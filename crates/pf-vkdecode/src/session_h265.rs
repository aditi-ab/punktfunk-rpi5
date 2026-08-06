//! `VkVideoSessionKHR` + `VkVideoSessionParametersKHR` lifecycle for H.265 —
//! [`crate::session`] one codec over, with the leg H.264 does not have: the VPS.
//!
//! Vulkan's H.265 session parameters hold THREE parameter-set arrays (VPS, SPS,
//! PPS) behind `VkVideoDecodeH265SessionParametersAddInfoKHR`, and
//! `StdVideoDecodeH265PictureInfo` names all three by id
//! (`sps_video_parameter_set_id`, `pps_seq_parameter_set_id`,
//! `pps_pic_parameter_set_id`) — so the object must hold the VPS its SPS names, or
//! the decode op resolves nothing. The versioning rules are Vulkan's, unchanged
//! from the H.264 ledger:
//!
//! - a NEW (vps-id / sps-id / pps-id) is ADDED via
//!   `vkUpdateVideoSessionParametersKHR` with `updateSequenceCount` = previous + 1
//!   (the spec's exact-increment rule — ONE call may carry all three sets, and it
//!   counts as ONE);
//! - an EXISTING id whose content changed cannot be updated in place — the object
//!   is RECREATED (Vulkan forbids replacing a stored parameter set), as is an
//!   object whose capacity would overflow;
//! - a stream renegotiation that resizes the DPB or the coded extent recreates the
//!   whole session — `plan_to_vk_h265`'s `CapacityMismatch` is the trigger for the
//!   DPB half, the extent comparison covers the other.
//!
//! **The missing-VPS case is real and handled, not assumed away.** The vendored
//! parser attaches a VPS to an SPS only when it actually saw the VPS NALU; a
//! stream joined mid-flight (punktfunk clients join live sessions) can therefore
//! carry an SPS whose VPS never arrived. `VpsSource` makes that a first-class
//! state: the ledger stores either the parsed VPS or the SPS the fallback was
//! synthesized from ([`fallback_vps_from_sps`]), and dedups on THAT — so when the
//! real VPS finally arrives, the content differs from the fallback and the object
//! RECREATES onto the real one, exactly as any other content change would.
//!
//! `ParamsLedgerH265` is the pure half of that decision table (unit-tested);
//! `VideoSessionH265` is the thin Vulkan half.

use std::rc::Rc;

use ash::vk;
use ash::vk::native as hh;
use tracing::debug;

use crate::caps::DecodeCaps;
use crate::caps_h265::H265ProfileChain;
use crate::caps_h265::H265ProfileKey;
use crate::device::DecodeDevice;
use crate::params_h265::fallback_vps_from_sps;
use crate::params_h265::pps_to_std_h265;
use crate::params_h265::sps_to_std_h265;
use crate::params_h265::vps_to_std_h265;
use crate::params_h265::H265ParamsError;
use crate::params_h265::OwnedStdH265Vps;
use crate::params_h265::Pps;
use crate::params_h265::Sps;
use crate::params_h265::Vps;
use crate::session::bind_session_memory;
use crate::session::ResetArm;
use crate::session::SessionError;

/// Parameter-object capacity. Punktfunk hosts emit one VPS + one SPS + one PPS per
/// stream; the headroom absorbs id churn across renegotiations without recreation,
/// and an overflow beyond it recreates rather than fails.
pub(crate) const MAX_STD_VPS: usize = 4;
pub(crate) const MAX_STD_SPS: usize = 4;
pub(crate) const MAX_STD_PPS: usize = 8;

/// Where the VPS an activation needs comes from — and, equally, the ledger's
/// identity for it.
///
/// Comparing whole values (not just ids) is what makes the fallback→real
/// transition correct: `Parsed` and `FromSps` under the same id are never equal,
/// so the arrival of the genuine VPS is a content change and recreates the object
/// instead of silently keeping the synthesized stand-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VpsSource {
    /// The stream's own VPS, as the parser attached it to the SPS.
    Parsed(Rc<Vps>),
    /// No VPS NALU was ever seen; the SPS it would be synthesized from
    /// ([`fallback_vps_from_sps`]) stands in as the identity.
    FromSps(Rc<Sps>),
}

impl VpsSource {
    /// The VPS an SPS activation needs.
    pub(crate) fn for_sps(sps: &Rc<Sps>) -> Self {
        match &sps.vps {
            Some(vps) => VpsSource::Parsed(Rc::clone(vps)),
            None => VpsSource::FromSps(Rc::clone(sps)),
        }
    }

    /// `vps_video_parameter_set_id` — the id the parameters object stores it under
    /// and the SPS names it by.
    pub(crate) fn id(&self) -> u8 {
        match self {
            VpsSource::Parsed(vps) => vps.video_parameter_set_id,
            VpsSource::FromSps(sps) => sps.video_parameter_set_id,
        }
    }

    /// Convert to the Std struct (owning wrapper).
    pub(crate) fn to_std(&self) -> Result<OwnedStdH265Vps, H265ParamsError> {
        match self {
            VpsSource::Parsed(vps) => vps_to_std_h265(vps),
            VpsSource::FromSps(sps) => fallback_vps_from_sps(sps),
        }
    }
}

/// What the ledger decided for one (VPS, SPS, PPS) activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsActionH265 {
    /// All three sets are already stored with identical content — nothing to do.
    Current,
    /// At least one set is new; ONE update call (seq += 1) adds what is missing.
    Add {
        add_vps: bool,
        add_sps: bool,
        add_pps: bool,
    },
    /// A stored id changed content, or capacity would overflow: recreate the
    /// parameters object (Vulkan cannot replace or evict a stored set).
    Recreate,
}

/// Pure bookkeeping for the parameters object: which sets it holds (by id AND
/// content — the parser re-parses in-band parameter sets every IRAP, so pointer
/// identity means nothing) and the update sequence counter.
#[derive(Debug, Default)]
pub(crate) struct ParamsLedgerH265 {
    vps: Vec<(u8, VpsSource)>,
    sps: Vec<(u8, Rc<Sps>)>,
    /// Keyed `(seq_parameter_set_id, pic_parameter_set_id)`, the pair Vulkan
    /// resolves a stored PPS by.
    pps: Vec<((u8, u8), Rc<Pps>)>,
    update_seq: u32,
}

impl ParamsLedgerH265 {
    /// Decide the action for activating (`vps`, `sps`, `pps`). Pure — mutate via
    /// [`Self::commit`].
    pub(crate) fn plan(&self, vps: &VpsSource, sps: &Rc<Sps>, pps: &Rc<Pps>) -> ParamsActionH265 {
        let vps_key = vps.id();
        let sps_key = sps.seq_parameter_set_id;
        let pps_key = (pps.seq_parameter_set_id, pps.pic_parameter_set_id);

        let stored_vps = self.vps.iter().find(|(id, _)| *id == vps_key);
        let stored_sps = self.sps.iter().find(|(id, _)| *id == sps_key);
        let stored_pps = self.pps.iter().find(|(id, _)| *id == pps_key);
        // Content changes under a stored id — including a fallback VPS being
        // superseded by the real one (VpsSource docs).
        if let Some((_, stored)) = stored_vps {
            if stored != vps {
                return ParamsActionH265::Recreate;
            }
        }
        if let Some((_, stored)) = stored_sps {
            if **stored != **sps {
                return ParamsActionH265::Recreate;
            }
        }
        if let Some((_, stored)) = stored_pps {
            if **stored != **pps {
                return ParamsActionH265::Recreate;
            }
        }
        let add_vps = stored_vps.is_none();
        let add_sps = stored_sps.is_none();
        let add_pps = stored_pps.is_none();
        if !add_vps && !add_sps && !add_pps {
            return ParamsActionH265::Current;
        }
        if (add_vps && self.vps.len() >= MAX_STD_VPS)
            || (add_sps && self.sps.len() >= MAX_STD_SPS)
            || (add_pps && self.pps.len() >= MAX_STD_PPS)
        {
            return ParamsActionH265::Recreate;
        }
        ParamsActionH265::Add {
            add_vps,
            add_sps,
            add_pps,
        }
    }

    /// Apply a decided action. `Add` bumps the sequence count by EXACTLY one (the
    /// Vulkan update rule — one call may carry all three sets); `Recreate` resets
    /// the ledger to just the current triple with a fresh object's zero counter
    /// (any other id the stream still references simply re-Adds on next
    /// activation).
    pub(crate) fn commit(
        &mut self,
        action: ParamsActionH265,
        vps: &VpsSource,
        sps: &Rc<Sps>,
        pps: &Rc<Pps>,
    ) {
        match action {
            ParamsActionH265::Current => {}
            ParamsActionH265::Add {
                add_vps,
                add_sps,
                add_pps,
            } => {
                if add_vps {
                    self.vps.push((vps.id(), vps.clone()));
                }
                if add_sps {
                    self.sps.push((sps.seq_parameter_set_id, Rc::clone(sps)));
                }
                if add_pps {
                    self.pps.push((
                        (pps.seq_parameter_set_id, pps.pic_parameter_set_id),
                        Rc::clone(pps),
                    ));
                }
                self.update_seq += 1;
            }
            ParamsActionH265::Recreate => {
                self.vps.clear();
                self.sps.clear();
                self.pps.clear();
                self.vps.push((vps.id(), vps.clone()));
                self.sps.push((sps.seq_parameter_set_id, Rc::clone(sps)));
                self.pps.push((
                    (pps.seq_parameter_set_id, pps.pic_parameter_set_id),
                    Rc::clone(pps),
                ));
                self.update_seq = 0;
            }
        }
    }

    /// The sequence count the NEXT `vkUpdateVideoSessionParametersKHR` must carry.
    pub(crate) fn next_update_seq(&self) -> u32 {
        self.update_seq + 1
    }
}

/// The session's create-time shape; a plan disagreeing with it forces a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfigH265 {
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    /// The profile the session was created against — profile idc AND the chroma
    /// format / bit depths, all four of which a stream can renegotiate (an SPS
    /// switching Main→Main 10 mid-stream is a session rebuild, not an update).
    pub profile: H265ProfileKey,
}

/// The Vulkan half: session + bound memory + parameters object.
pub(crate) struct VideoSessionH265 {
    device: ash::Device,
    video_queue: ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    memory: Vec<vk::DeviceMemory>,
    parameters: vk::VideoSessionParametersKHR,
    ledger: ParamsLedgerH265,
    pub(crate) config: SessionConfigH265,
    /// The session has never run a coding scope: the first one records a
    /// `VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR` control before anything else.
    needs_reset: ResetArm,
}

impl VideoSessionH265 {
    /// Create the session + an EMPTY parameters object (sets arrive via
    /// [`Self::ensure_parameters`], which the decoder calls before the first
    /// decode).
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        config: SessionConfigH265,
    ) -> Result<Self, SessionError> {
        let mut chain = H265ProfileChain::new(config.profile);
        let profile = chain.wire();
        let std_header_version = caps.std_header_version;
        let session_ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(dev.decode_qf())
            .video_profile(profile)
            .picture_format(caps.output_format)
            .max_coded_extent(config.max_coded_extent)
            .reference_picture_format(caps.dpb_format)
            .max_dpb_slots(config.max_dpb_slots)
            .max_active_reference_pictures(config.max_active_references)
            .std_header_version(&std_header_version);
        let mut session = vk::VideoSessionKHR::null();
        // SAFETY: live device; `session_ci` roots locals (chain, header version)
        // that outlive the call.
        let r = unsafe {
            (dev.video_queue().fp().create_video_session_khr)(
                dev.ash().handle(),
                &session_ci,
                std::ptr::null(),
                &mut session,
            )
        };
        if r != vk::Result::SUCCESS {
            return Err(SessionError::Vk(r));
        }

        let mut built = Self {
            device: dev.ash().clone(),
            video_queue: dev.video_queue().clone(),
            session,
            memory: Vec::new(),
            parameters: vk::VideoSessionParametersKHR::null(),
            ledger: ParamsLedgerH265::default(),
            config,
            needs_reset: ResetArm::armed(),
        };
        // SAFETY: fn contract; on error `built` drops and unwinds the session +
        // whatever memory was bound.
        unsafe {
            // A bind failure hands its allocations BACK: parking them in `built`
            // is what makes the early return destroy the session before freeing
            // them (BindFailure docs — Vulkan defines no partial-bind rollback).
            match bind_session_memory(dev, session) {
                Ok(memory) => built.memory = memory,
                Err(failure) => {
                    built.memory = failure.allocations;
                    return Err(failure.error);
                }
            }
            built.parameters = built.create_parameters_object(&[], &[], &[])?;
        }
        Ok(built)
    }

    /// Create a parameters object holding exactly `vps`/`sps`/`pps` (any may be
    /// empty).
    ///
    /// # Safety
    ///
    /// Live device + live session; the Std slices' backing (the `OwnedStd*`
    /// wrappers, INCLUDING the heap blocks their embedded pointers target)
    /// outlives this call.
    ///
    /// ⚠ This used to end "— Vulkan copies all parameter data before returning",
    /// and that is **not universally true**: see [`crate::session::VideoSession`]'s
    /// twin of this comment and [`crate::session_av1`], where a driver was measured
    /// dereferencing a retained pointer long after the create call. H.265's Std SPS
    /// carries SEVEN embedded pointers, more than any other set in this crate.
    unsafe fn create_parameters_object(
        &self,
        vps: &[hh::StdVideoH265VideoParameterSet],
        sps: &[hh::StdVideoH265SequenceParameterSet],
        pps: &[hh::StdVideoH265PictureParameterSet],
    ) -> Result<vk::VideoSessionParametersKHR, SessionError> {
        let add = vk::VideoDecodeH265SessionParametersAddInfoKHR::default()
            .std_vp_ss(vps)
            .std_sp_ss(sps)
            .std_pp_ss(pps);
        let mut h265 = vk::VideoDecodeH265SessionParametersCreateInfoKHR::default()
            .max_std_vps_count(MAX_STD_VPS as u32)
            .max_std_sps_count(MAX_STD_SPS as u32)
            .max_std_pps_count(MAX_STD_PPS as u32)
            .parameters_add_info(&add);
        let ci = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(self.session)
            .push_next(&mut h265);
        let mut parameters = vk::VideoSessionParametersKHR::null();
        // SAFETY: fn contract; `ci` roots locals outliving the call.
        let r = unsafe {
            (self.video_queue.fp().create_video_session_parameters_khr)(
                self.device.handle(),
                &ci,
                std::ptr::null(),
                &mut parameters,
            )
        };
        if r != vk::Result::SUCCESS {
            return Err(SessionError::Vk(r));
        }
        Ok(parameters)
    }

    /// The ledger's verdict for activating (`vps`, `sps`, `pps`), without mutating
    /// anything — the decoder consults this BEFORE [`Self::ensure_parameters`] so
    /// a [`ParamsActionH265::Recreate`] can be preceded by a full in-flight drain
    /// (the destroy inside the recreate must never race a submitted decode).
    pub(crate) fn parameters_action(
        &self,
        vps: &VpsSource,
        sps: &Rc<Sps>,
        pps: &Rc<Pps>,
    ) -> ParamsActionH265 {
        self.ledger.plan(vps, sps, pps)
    }

    /// Make the parameters object hold this AU's activated (VPS, SPS, PPS),
    /// converting through the params module and Adding/Recreating per the ledger.
    ///
    /// # Safety
    ///
    /// Live device; when [`Self::parameters_action`] says `Recreate`, the caller
    /// has ALREADY drained every in-flight decode — the old object is destroyed
    /// here, and a still-executing decode reading it would be use-after-free at
    /// the driver level. `Current`/`Add` touch no object a submitted decode can be
    /// reading.
    pub(crate) unsafe fn ensure_parameters(
        &mut self,
        vps: &VpsSource,
        sps: &Rc<Sps>,
        pps: &Rc<Pps>,
    ) -> Result<(), SessionError> {
        let action = self.ledger.plan(vps, sps, pps);
        match action {
            ParamsActionH265::Current => Ok(()),
            ParamsActionH265::Add {
                add_vps,
                add_sps,
                add_pps,
            } => {
                // Every owned wrapper below stays alive until after the update
                // call: the Std structs embed pointers into their heap blocks.
                let owned_vps = if add_vps { Some(vps.to_std()?) } else { None };
                let owned_sps = if add_sps {
                    Some(sps_to_std_h265(sps)?)
                } else {
                    None
                };
                let owned_pps = if add_pps {
                    Some(pps_to_std_h265(pps)?)
                } else {
                    None
                };
                let vps_slice: &[hh::StdVideoH265VideoParameterSet] = match &owned_vps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let sps_slice: &[hh::StdVideoH265SequenceParameterSet] = match &owned_sps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let pps_slice: &[hh::StdVideoH265PictureParameterSet] = match &owned_pps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let mut add = vk::VideoDecodeH265SessionParametersAddInfoKHR::default()
                    .std_vp_ss(vps_slice)
                    .std_sp_ss(sps_slice)
                    .std_pp_ss(pps_slice);
                let update = vk::VideoSessionParametersUpdateInfoKHR::default()
                    .update_sequence_count(self.ledger.next_update_seq())
                    .push_next(&mut add);
                // SAFETY: live device + parameters object; `update` roots locals
                // (incl. the OwnedStd backings) outliving the call. See
                // `create_parameters_object` on why "and the driver copies them"
                // is an observation about this fleet rather than a guarantee.
                let r = unsafe {
                    (self.video_queue.fp().update_video_session_parameters_khr)(
                        self.device.handle(),
                        self.parameters,
                        &update,
                    )
                };
                if r != vk::Result::SUCCESS {
                    return Err(SessionError::Vk(r));
                }
                self.ledger.commit(action, vps, sps, pps);
                Ok(())
            }
            ParamsActionH265::Recreate => {
                debug!(
                    vps_id = vps.id(),
                    sps_id = sps.seq_parameter_set_id,
                    pps_id = pps.pic_parameter_set_id,
                    "recreating H.265 session parameters (content change or capacity)"
                );
                let owned_vps = vps.to_std()?;
                let owned_sps = sps_to_std_h265(sps)?;
                let owned_pps = pps_to_std_h265(pps)?;
                // SAFETY: fn contract (the OwnedStd backings live across the call).
                let fresh = unsafe {
                    self.create_parameters_object(
                        std::slice::from_ref(owned_vps.std()),
                        std::slice::from_ref(owned_sps.std()),
                        std::slice::from_ref(owned_pps.std()),
                    )?
                };
                // SAFETY: the fn-level contract — the caller drained every
                // in-flight decode before a Recreate reached here (checked via
                // parameters_action), so no submitted work reads the old object;
                // it is this session's own handle.
                unsafe {
                    (self.video_queue.fp().destroy_video_session_parameters_khr)(
                        self.device.handle(),
                        self.parameters,
                        std::ptr::null(),
                    );
                }
                self.parameters = fresh;
                self.ledger.commit(action, vps, sps, pps);
                Ok(())
            }
        }
    }

    pub(crate) fn session(&self) -> vk::VideoSessionKHR {
        self.session
    }

    pub(crate) fn parameters(&self) -> vk::VideoSessionParametersKHR {
        self.parameters
    }

    /// Whether the next coding scope must record the initialization RESET —
    /// `true` exactly once per session, PROVIDED the command buffer that recorded
    /// it actually reaches the queue: a recording/submit failure after this
    /// returned `true` must call [`Self::re_arm_reset`], or the session would run
    /// its whole life uninitialized.
    pub(crate) fn take_needs_reset(&mut self) -> bool {
        self.needs_reset.take()
    }

    /// Undo a consumed [`Self::take_needs_reset`] whose RESET never reached the
    /// queue (end/submit failed after recording it).
    pub(crate) fn re_arm_reset(&mut self) {
        self.needs_reset.re_arm();
    }
}

impl Drop for VideoSessionH265 {
    fn drop(&mut self) {
        // SAFETY: all handles are this session's own on the (contract-live) device;
        // the owning decoder drains GPU work before dropping state. The destroy
        // entry points ignore NULL handles, covering half-built sessions. The
        // ORDER is load-bearing, not stylistic: memory bound into a session may
        // not be freed while the session lives, so the session is destroyed first
        // — which is also why a failed bind hands its allocations back here
        // instead of freeing them itself (`crate::session::BindFailure`).
        unsafe {
            (self.video_queue.fp().destroy_video_session_parameters_khr)(
                self.device.handle(),
                self.parameters,
                std::ptr::null(),
            );
            (self.video_queue.fp().destroy_video_session_khr)(
                self.device.handle(),
                self.session,
                std::ptr::null(),
            );
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored H.265 parser has no set builders (unlike its H.264 half), and
    /// `Pps` derives no `Default` — so fixtures are written out once here. Only
    /// the fields the ledger keys or compares carry meaning.
    fn authored_sps(sps_id: u8, vps_id: u8, width: u16) -> Rc<Sps> {
        Rc::new(Sps {
            video_parameter_set_id: vps_id,
            seq_parameter_set_id: sps_id,
            chroma_format_idc: 1,
            pic_width_in_luma_samples: width,
            pic_height_in_luma_samples: 64,
            ..Default::default()
        })
    }

    /// The same SPS with the stream's VPS attached, as the parser would.
    fn with_vps(sps: &Rc<Sps>, vps_id: u8, max_layers_minus1: u8) -> Rc<Sps> {
        let vps = Vps {
            video_parameter_set_id: vps_id,
            max_layers_minus1,
            ..Default::default()
        };
        Rc::new(Sps {
            vps: Some(Rc::new(vps)),
            ..(**sps).clone()
        })
    }

    fn authored_pps(sps: &Rc<Sps>, pps_id: u8, init_qp_minus26: i8) -> Rc<Pps> {
        Rc::new(Pps {
            pic_parameter_set_id: pps_id,
            seq_parameter_set_id: sps.seq_parameter_set_id,
            dependent_slice_segments_enabled_flag: false,
            output_flag_present_flag: false,
            num_extra_slice_header_bits: 0,
            sign_data_hiding_enabled_flag: false,
            cabac_init_present_flag: false,
            num_ref_idx_l0_default_active_minus1: 0,
            num_ref_idx_l1_default_active_minus1: 0,
            init_qp_minus26,
            constrained_intra_pred_flag: false,
            transform_skip_enabled_flag: false,
            cu_qp_delta_enabled_flag: false,
            diff_cu_qp_delta_depth: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            slice_chroma_qp_offsets_present_flag: false,
            weighted_pred_flag: false,
            weighted_bipred_flag: false,
            transquant_bypass_enabled_flag: false,
            tiles_enabled_flag: false,
            entropy_coding_sync_enabled_flag: false,
            num_tile_columns_minus1: 0,
            num_tile_rows_minus1: 0,
            uniform_spacing_flag: true,
            column_width_minus1: [0; 19],
            row_height_minus1: [0; 21],
            loop_filter_across_tiles_enabled_flag: true,
            loop_filter_across_slices_enabled_flag: false,
            deblocking_filter_control_present_flag: false,
            deblocking_filter_override_enabled_flag: false,
            deblocking_filter_disabled_flag: false,
            beta_offset_div2: 0,
            tc_offset_div2: 0,
            scaling_list_data_present_flag: false,
            scaling_list: Default::default(),
            lists_modification_present_flag: false,
            log2_parallel_merge_level_minus2: 0,
            slice_segment_header_extension_present_flag: false,
            extension_present_flag: false,
            range_extension_flag: false,
            range_extension: Default::default(),
            scc_extension_flag: false,
            scc_extension: Default::default(),
            qp_bd_offset_y: 0,
            sps: Rc::clone(sps),
        })
    }

    #[test]
    fn the_first_activation_adds_all_three_sets_in_one_update_call() {
        let sps = with_vps(&authored_sps(0, 0, 64), 0, 0);
        let vps = VpsSource::for_sps(&sps);
        let pps = authored_pps(&sps, 0, 0);

        let mut ledger = ParamsLedgerH265::default();
        assert_eq!(ledger.next_update_seq(), 1);
        let action = ledger.plan(&vps, &sps, &pps);
        assert_eq!(
            action,
            ParamsActionH265::Add {
                add_vps: true,
                add_sps: true,
                add_pps: true
            }
        );
        ledger.commit(action, &vps, &sps, &pps);
        // ONE call carried all three: the counter moves by exactly one.
        assert_eq!(ledger.next_update_seq(), 2);
        assert_eq!(ledger.plan(&vps, &sps, &pps), ParamsActionH265::Current);
    }

    #[test]
    fn a_reactivated_identical_triple_is_current_even_across_reparses() {
        // The parser re-parses in-band sets at every IRAP: same content, NEW Rcs.
        let sps_a = with_vps(&authored_sps(0, 0, 64), 0, 0);
        let sps_b = with_vps(&authored_sps(0, 0, 64), 0, 0);
        assert!(!Rc::ptr_eq(&sps_a, &sps_b));
        let (vps_a, vps_b) = (VpsSource::for_sps(&sps_a), VpsSource::for_sps(&sps_b));
        assert_eq!(vps_a, vps_b, "identical content, distinct allocations");
        let pps_a = authored_pps(&sps_a, 0, 0);
        let pps_b = authored_pps(&sps_b, 0, 0);

        let mut ledger = ParamsLedgerH265::default();
        let action = ledger.plan(&vps_a, &sps_a, &pps_a);
        ledger.commit(action, &vps_a, &sps_a, &pps_a);
        assert_eq!(
            ledger.plan(&vps_b, &sps_b, &pps_b),
            ParamsActionH265::Current
        );
    }

    #[test]
    fn a_new_pps_id_over_stored_vps_and_sps_adds_only_the_pps() {
        let sps = with_vps(&authored_sps(0, 0, 64), 0, 0);
        let vps = VpsSource::for_sps(&sps);
        let pps0 = authored_pps(&sps, 0, 0);
        let pps1 = authored_pps(&sps, 1, 0);

        let mut ledger = ParamsLedgerH265::default();
        let action = ledger.plan(&vps, &sps, &pps0);
        ledger.commit(action, &vps, &sps, &pps0);
        assert_eq!(
            ledger.plan(&vps, &sps, &pps1),
            ParamsActionH265::Add {
                add_vps: false,
                add_sps: false,
                add_pps: true
            }
        );
    }

    #[test]
    fn a_stream_with_no_vps_nalu_stores_the_fallback_and_recreates_when_the_real_one_arrives() {
        // Joined mid-stream: the SPS arrived without its VPS, so the ledger's
        // identity is the SPS the fallback would be synthesized from.
        let sps_no_vps = authored_sps(0, 0, 64);
        assert!(sps_no_vps.vps.is_none());
        let fallback = VpsSource::for_sps(&sps_no_vps);
        assert!(matches!(fallback, VpsSource::FromSps(_)));
        assert_eq!(fallback.id(), 0, "the id comes off the SPS's vps id");
        let pps = authored_pps(&sps_no_vps, 0, 0);

        let mut ledger = ParamsLedgerH265::default();
        let action = ledger.plan(&fallback, &sps_no_vps, &pps);
        assert_eq!(
            action,
            ParamsActionH265::Add {
                add_vps: true,
                add_sps: true,
                add_pps: true
            }
        );
        ledger.commit(action, &fallback, &sps_no_vps, &pps);
        // Re-activating the same VPS-less SPS is Current, not a churn of Adds.
        assert_eq!(
            ledger.plan(&fallback, &sps_no_vps, &pps),
            ParamsActionH265::Current
        );

        // The real VPS finally arrives (same id 0). Vulkan cannot replace a
        // stored set, so the fallback→real transition RECREATES. (In a real
        // stream the SPS changes with it — it now carries the VPS — so the
        // recreate is over-determined; the VPS leg on its own is isolated in
        // `changed_vps_content_under_a_stored_id_recreates_with_the_sps_and_pps_untouched`.)
        let sps_with_vps = with_vps(&sps_no_vps, 0, 0);
        let real = VpsSource::for_sps(&sps_with_vps);
        assert!(matches!(real, VpsSource::Parsed(_)));
        assert_ne!(real, fallback, "a synthesized VPS is not the parsed one");
        let pps = authored_pps(&sps_with_vps, 0, 0);
        let action = ledger.plan(&real, &sps_with_vps, &pps);
        assert_eq!(action, ParamsActionH265::Recreate);
        ledger.commit(action, &real, &sps_with_vps, &pps);
        assert_eq!(
            ledger.next_update_seq(),
            1,
            "a fresh object restarts its counter"
        );
        assert_eq!(
            ledger.plan(&real, &sps_with_vps, &pps),
            ParamsActionH265::Current
        );
    }

    /// A VPS built directly, decoupled from any SPS. The ledger keys its three
    /// arrays independently, so this is how the VPS leg is exercised ALONE — in a
    /// real stream an SPS carries its VPS, so any VPS change drags the SPS's
    /// content along and the recreate becomes over-determined.
    fn standalone_vps(vps_id: u8, max_layers_minus1: u8) -> VpsSource {
        VpsSource::Parsed(Rc::new(Vps {
            video_parameter_set_id: vps_id,
            max_layers_minus1,
            ..Default::default()
        }))
    }

    #[test]
    fn changed_vps_content_under_a_stored_id_recreates_with_the_sps_and_pps_untouched() {
        let sps = authored_sps(0, 0, 64);
        let pps = authored_pps(&sps, 0, 0);
        let vps = standalone_vps(0, 0);
        let mut ledger = ParamsLedgerH265::default();
        let action = ledger.plan(&vps, &sps, &pps);
        ledger.commit(action, &vps, &sps, &pps);
        assert_eq!(ledger.plan(&vps, &sps, &pps), ParamsActionH265::Current);

        // Same VPS id, different content: SPS and PPS are byte-identical and
        // would be `Current` on their own, yet the object still has to be
        // rebuilt — Vulkan has no in-place replacement for a stored set.
        let vps2 = standalone_vps(0, 1);
        assert_eq!(vps2.id(), vps.id());
        assert_ne!(vps2, vps);
        assert_eq!(ledger.plan(&vps2, &sps, &pps), ParamsActionH265::Recreate);

        // A NEW vps id over the same SPS/PPS is an Add of the VPS alone.
        let vps3 = standalone_vps(1, 0);
        assert_eq!(
            ledger.plan(&vps3, &sps, &pps),
            ParamsActionH265::Add {
                add_vps: true,
                add_sps: false,
                add_pps: false
            }
        );
    }

    #[test]
    fn changed_sps_or_pps_content_under_a_stored_id_recreates_and_resets_the_sequence() {
        let sps = with_vps(&authored_sps(0, 0, 64), 0, 0);
        let vps = VpsSource::for_sps(&sps);
        let pps = authored_pps(&sps, 0, 0);
        let mut ledger = ParamsLedgerH265::default();
        let action = ledger.plan(&vps, &sps, &pps);
        ledger.commit(action, &vps, &sps, &pps);
        assert_eq!(ledger.next_update_seq(), 2, "one Add happened");

        // Same PPS id, different content (init qp changed).
        let pps2 = authored_pps(&sps, 0, 4);
        let action = ledger.plan(&vps, &sps, &pps2);
        assert_eq!(action, ParamsActionH265::Recreate);
        ledger.commit(action, &vps, &sps, &pps2);
        assert_eq!(ledger.next_update_seq(), 1);
        assert_eq!(ledger.plan(&vps, &sps, &pps2), ParamsActionH265::Current);

        // Same SPS id, different content (a resize the extent check would also
        // catch — but the ledger must not depend on that).
        let sps2 = with_vps(&authored_sps(0, 0, 128), 0, 0);
        let vps2 = VpsSource::for_sps(&sps2);
        let pps3 = authored_pps(&sps2, 0, 4);
        assert_eq!(ledger.plan(&vps2, &sps2, &pps3), ParamsActionH265::Recreate);
    }

    #[test]
    fn capacity_overflow_on_any_of_the_three_arrays_recreates_with_just_the_current_triple() {
        let sps = with_vps(&authored_sps(0, 0, 64), 0, 0);
        let vps = VpsSource::for_sps(&sps);
        let mut ledger = ParamsLedgerH265::default();

        // Fill the PPS capacity under one VPS/SPS pair.
        let first = authored_pps(&sps, 0, 0);
        let action = ledger.plan(&vps, &sps, &first);
        ledger.commit(action, &vps, &sps, &first);
        for pps_id in 1..MAX_STD_PPS as u8 {
            let pps = authored_pps(&sps, pps_id, 0);
            let action = ledger.plan(&vps, &sps, &pps);
            assert!(matches!(action, ParamsActionH265::Add { .. }));
            ledger.commit(action, &vps, &sps, &pps);
        }
        assert_eq!(ledger.next_update_seq() - 1, MAX_STD_PPS as u32);

        // One past capacity: recreate; afterwards the evicted first PPS re-Adds
        // (and the VPS/SPS with it — the recreate kept only the current triple).
        let overflow = authored_pps(&sps, MAX_STD_PPS as u8, 0);
        let action = ledger.plan(&vps, &sps, &overflow);
        assert_eq!(action, ParamsActionH265::Recreate);
        ledger.commit(action, &vps, &sps, &overflow);
        assert_eq!(
            ledger.plan(&vps, &sps, &first),
            ParamsActionH265::Add {
                add_vps: false,
                add_sps: false,
                add_pps: true
            },
            "sets evicted by a recreate re-add on next activation"
        );

        // The VPS array overflows the same way, and on its own: MAX_STD_VPS
        // distinct ids fit over one fixed SPS/PPS pair, the next one recreates.
        let mut ledger = ParamsLedgerH265::default();
        let sps = authored_sps(0, 0, 64);
        let pps = authored_pps(&sps, 0, 0);
        for vps_id in 0..MAX_STD_VPS as u8 {
            let vps = standalone_vps(vps_id, 0);
            let action = ledger.plan(&vps, &sps, &pps);
            assert!(matches!(
                action,
                ParamsActionH265::Add { add_vps: true, .. }
            ));
            ledger.commit(action, &vps, &sps, &pps);
        }
        let overflow_vps = standalone_vps(MAX_STD_VPS as u8, 0);
        assert_eq!(
            ledger.plan(&overflow_vps, &sps, &pps),
            ParamsActionH265::Recreate,
            "a fifth VPS id overflows the array even though SPS and PPS are current"
        );
    }
}
