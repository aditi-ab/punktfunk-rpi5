//! `VkVideoSessionKHR` + `VkVideoSessionParametersKHR` lifecycle for H.265 —
//! [`crate::session`] one codec over, plus the VPS H.264 does not have.
//!
//! Three arrays (VPS, SPS, PPS); a decode names all three by id, so the object
//! must hold the VPS its SPS names. A new id is ADDED with `updateSequenceCount`
//! = previous + 1 (one call may carry all three and counts as one). A stored id
//! whose content changed, or a capacity overflow, RECREATES the object (Vulkan
//! cannot replace a stored set). A DPB or coded-extent resize recreates the
//! session. A mid-flight join can carry an SPS whose VPS never arrived: the
//! ledger stores the parsed VPS or the SPS the fallback was synthesized from
//! ([`fallback_vps_from_sps`]); the real VPS is a content change and recreates.
//!
//! Std heap blocks must outlive the parameters OBJECT: a driver retains the
//! embedded pointers past create/update ([`crate::session_av1`]).
//! [`StoredParamsH265`] holds object and backings together; Drop and recreate
//! destroy the object first. Outer `pStd*` arrays are held the same way
//! ([`crate::session`]).
//!
//! `ParamsLedgerH265` is the pure half (unit-tested); `VideoSessionH265` is the
//! thin Vulkan half.

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
use crate::params_h265::OwnedStdH265Pps;
use crate::params_h265::OwnedStdH265Sps;
use crate::params_h265::OwnedStdH265Vps;
use crate::params_h265::Pps;
use crate::params_h265::Sps;
use crate::params_h265::Vps;
use crate::session::bind_session_memory;
use crate::session::ResetArm;
use crate::session::SessionError;

/// Hosts emit one VPS + one SPS + one PPS per stream; headroom absorbs id churn
/// without recreation. Overflow recreates rather than failing.
pub(crate) const MAX_STD_VPS: usize = 4;
pub(crate) const MAX_STD_SPS: usize = 4;
pub(crate) const MAX_STD_PPS: usize = 8;

/// VPS identity for an activation. Compared whole, not by id: `Parsed` and
/// `FromSps` under the same id are never equal, so a real VPS arriving after a
/// fallback is a content change and recreates the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum VpsSource {
    Parsed(Rc<Vps>),
    /// No VPS NALU seen; identity is the SPS [`fallback_vps_from_sps`] would use.
    FromSps(Rc<Sps>),
}

impl VpsSource {
    pub(crate) fn for_sps(sps: &Rc<Sps>) -> Self {
        match &sps.vps {
            Some(vps) => VpsSource::Parsed(Rc::clone(vps)),
            None => VpsSource::FromSps(Rc::clone(sps)),
        }
    }

    pub(crate) fn id(&self) -> u8 {
        match self {
            VpsSource::Parsed(vps) => vps.video_parameter_set_id,
            VpsSource::FromSps(sps) => sps.video_parameter_set_id,
        }
    }

    pub(crate) fn to_std(&self) -> Result<OwnedStdH265Vps, H265ParamsError> {
        match self {
            VpsSource::Parsed(vps) => vps_to_std_h265(vps),
            VpsSource::FromSps(sps) => fallback_vps_from_sps(sps),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsActionH265 {
    Current,
    /// At least one set is new; one update call (seq += 1) adds what is missing.
    Add {
        add_vps: bool,
        add_sps: bool,
        add_pps: bool,
    },
    /// Stored id changed content, or capacity would overflow. Vulkan cannot
    /// replace or evict a stored set.
    Recreate,
}

/// Which sets the object holds, by id AND content. The parser re-parses in-band
/// sets every IRAP, so pointer identity means nothing.
#[derive(Debug, Default)]
pub(crate) struct ParamsLedgerH265 {
    vps: Vec<(u8, VpsSource)>,
    sps: Vec<(u8, Rc<Sps>)>,
    /// Keyed `(sps_id, pps_id)` — the pair Vulkan resolves a stored PPS by.
    pps: Vec<((u8, u8), Rc<Pps>)>,
    update_seq: u32,
}

impl ParamsLedgerH265 {
    pub(crate) fn plan(&self, vps: &VpsSource, sps: &Rc<Sps>, pps: &Rc<Pps>) -> ParamsActionH265 {
        let vps_key = vps.id();
        let sps_key = sps.seq_parameter_set_id;
        let pps_key = (pps.seq_parameter_set_id, pps.pic_parameter_set_id);

        let stored_vps = self.vps.iter().find(|(id, _)| *id == vps_key);
        let stored_sps = self.sps.iter().find(|(id, _)| *id == sps_key);
        let stored_pps = self.pps.iter().find(|(id, _)| *id == pps_key);
        // Content change under a stored id, including fallback VPS → real.
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

    /// `Add` bumps the sequence by exactly one (one call may carry all three).
    /// `Recreate` keeps only the current triple and resets the counter to zero;
    /// any other still-referenced id re-Adds on next activation.
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
    /// Profile idc, chroma format, and both bit depths. A mid-stream switch
    /// (Main → Main 10) rebuilds the session; it is not an update.
    pub profile: H265ProfileKey,
    /// Device `maxLevelIdc` for this profile. Every VPS/SPS is clamped to it: a
    /// set above the ceiling is invalid usage; coded extent and DPB already
    /// enforce the stream's real demands.
    pub max_level_idc: hh::StdVideoH265LevelIdc,
}

/// Parameters object and every Std set it was given, in one value — the two may
/// not drift. A driver retains the embedded pointers past create/update (module
/// docs); a `let owned = …;` local frees that heap. H.265's SPS alone points at
/// seven such blocks.
///
/// What is pinned: the wrappers' boxed blocks. Outer `StdVideoH265*` arrays are
/// copies whose embedded pointers still address those blocks; moving a wrapper
/// or reallocating the `Vec` does not move the boxes.
/// `params_h265::moving_the_wrapper_leaves_the_driver_s_pointers_put` pins that.
struct StoredParamsH265 {
    object: vk::VideoSessionParametersKHR,
    /// One entry per stored set, held for the object's life. This crate never
    /// reads them after create/update; the driver reads the blocks they own.
    vps: Vec<OwnedStdH265Vps>,
    sps: Vec<OwnedStdH265Sps>,
    pps: Vec<OwnedStdH265Pps>,
    /// Outer `pStdVPSs`/`pStdSPSs`/`pStdPPSs` arrays, held for the object's life
    /// for the same reason as the wrappers ([`crate::session`]). Assembled at
    /// their final address.
    std_vps: Vec<hh::StdVideoH265VideoParameterSet>,
    std_sps: Vec<hh::StdVideoH265SequenceParameterSet>,
    std_pps: Vec<hh::StdVideoH265PictureParameterSet>,
}

impl StoredParamsH265 {
    /// Wrappers plus the Std arrays `pStd*` will point at, object still NULL.
    /// Assembled here so those arrays already sit at their final address.
    fn assemble(
        vps: Vec<OwnedStdH265Vps>,
        sps: Vec<OwnedStdH265Sps>,
        pps: Vec<OwnedStdH265Pps>,
    ) -> Self {
        // Copies of each wrapper's Std struct (`Copy`); embedded pointers still
        // address the wrappers' boxed blocks, so both halves stay.
        let std_vps = vps.iter().map(|o| *o.std()).collect();
        let std_sps = sps.iter().map(|o| *o.std()).collect();
        let std_pps = pps.iter().map(|o| *o.std()).collect();
        Self {
            object: vk::VideoSessionParametersKHR::null(),
            vps,
            sps,
            pps,
            std_vps,
            std_sps,
            std_pps,
        }
    }

    /// Empty placeholder. Destroy ignores a NULL handle, so a create that fails
    /// before the object exists still drops cleanly.
    fn none() -> Self {
        Self::assemble(Vec::new(), Vec::new(), Vec::new())
    }

    /// Blocks must outlive the object, not the update call. Reached only after
    /// that call succeeded; a failed update stored nothing and its wrappers drop.
    fn adopt(
        &mut self,
        vps: Option<OwnedStdH265Vps>,
        sps: Option<OwnedStdH265Sps>,
        pps: Option<OwnedStdH265Pps>,
    ) {
        self.vps.extend(vps);
        self.sps.extend(sps);
        self.pps.extend(pps);
    }
}

pub(crate) struct VideoSessionH265 {
    device: ash::Device,
    video_queue: ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    memory: Vec<vk::DeviceMemory>,
    parameters: StoredParamsH265,
    ledger: ParamsLedgerH265,
    pub(crate) config: SessionConfigH265,
    /// The session has never run a coding scope: the first one records a
    /// `VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR` control before anything else.
    needs_reset: ResetArm,
}

impl VideoSessionH265 {
    /// Session plus an empty parameters object. Sets arrive via
    /// [`Self::ensure_parameters`] before the first decode.
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
            parameters: StoredParamsH265::none(),
            ledger: ParamsLedgerH265::default(),
            config,
            needs_reset: ResetArm::armed(),
        };
        // SAFETY: fn contract; on error `built` drops and unwinds the session +
        // whatever memory was bound.
        unsafe {
            // A bind failure hands allocations back: parking them in `built` is
            // what makes the early return destroy the session before freeing
            // them (BindFailure docs — Vulkan defines no partial-bind rollback).
            match bind_session_memory(dev, session) {
                Ok(memory) => built.memory = memory,
                Err(failure) => {
                    built.memory = failure.allocations;
                    return Err(failure.error);
                }
            }
            built.parameters =
                built.create_parameters_object(Vec::new(), Vec::new(), Vec::new())?;
        }
        Ok(built)
    }

    /// Parameters object holding exactly `vps`/`sps`/`pps` (any may be empty),
    /// fused with the wrappers whose heap blocks it points at. Wrappers are
    /// taken by value so the object owns everything the driver may dereference.
    ///
    /// # Safety
    ///
    /// Live device + live session.
    unsafe fn create_parameters_object(
        &self,
        vps: Vec<OwnedStdH265Vps>,
        sps: Vec<OwnedStdH265Sps>,
        pps: Vec<OwnedStdH265Pps>,
    ) -> Result<StoredParamsH265, SessionError> {
        // Assemble first so `pStd*` already sit at their final address: returning
        // `stored` moves the Vec handle, not the block the driver was given.
        let mut stored = StoredParamsH265::assemble(vps, sps, pps);
        let add = vk::VideoDecodeH265SessionParametersAddInfoKHR::default()
            .std_vp_ss(&stored.std_vps)
            .std_sp_ss(&stored.std_sps)
            .std_pp_ss(&stored.std_pps);
        let mut h265 = vk::VideoDecodeH265SessionParametersCreateInfoKHR::default()
            .max_std_vps_count(MAX_STD_VPS as u32)
            .max_std_sps_count(MAX_STD_SPS as u32)
            .max_std_pps_count(MAX_STD_PPS as u32)
            .parameters_add_info(&add);
        let ci = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(self.session)
            .push_next(&mut h265);
        let mut object = vk::VideoSessionParametersKHR::null();
        // SAFETY: fn contract; `ci` roots locals outliving the call. The Std
        // arrays and the blocks their embedded pointers address are owned by
        // `stored`, which is returned rather than dropped here.
        let r = unsafe {
            (self.video_queue.fp().create_video_session_parameters_khr)(
                self.device.handle(),
                &ci,
                std::ptr::null(),
                &mut object,
            )
        };
        if r != vk::Result::SUCCESS {
            return Err(SessionError::Vk(r));
        }
        stored.object = object;
        Ok(stored)
    }

    /// Ledger verdict without mutating. The decoder consults this before
    /// [`Self::ensure_parameters`] so a Recreate can drain in-flight work first:
    /// the destroy must not race a submitted decode.
    pub(crate) fn parameters_action(
        &self,
        vps: &VpsSource,
        sps: &Rc<Sps>,
        pps: &Rc<Pps>,
    ) -> ParamsActionH265 {
        self.ledger.plan(vps, sps, pps)
    }

    /// Activate this AU's (VPS, SPS, PPS), Adding or Recreating per the ledger.
    ///
    /// # Safety
    ///
    /// Live device; when [`Self::parameters_action`] says `Recreate`, the caller
    /// has already drained every in-flight decode — the old object is destroyed
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
                // Wrappers stay alive past the update call: Std structs embed
                // pointers into their heap blocks.
                let mut owned_vps = if add_vps { Some(vps.to_std()?) } else { None };
                let mut owned_sps = if add_sps {
                    Some(sps_to_std_h265(sps)?)
                } else {
                    None
                };
                if let Some(v) = owned_vps.as_mut() {
                    v.clamp_level(self.config.max_level_idc);
                }
                if let Some(s) = owned_sps.as_mut() {
                    s.clamp_level(self.config.max_level_idc);
                }
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
                // (incl. the OwnedStd backings) outliving the call — and the
                // backings go on outliving it, adopted below.
                let r = unsafe {
                    (self.video_queue.fp().update_video_session_parameters_khr)(
                        self.device.handle(),
                        self.parameters.object,
                        &update,
                    )
                };
                if r != vk::Result::SUCCESS {
                    return Err(SessionError::Vk(r));
                }
                // Added sets now belong to the object; adopt so their heap
                // outlives the update call, not this arm.
                self.parameters.adopt(owned_vps, owned_sps, owned_pps);
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
                let mut owned_vps = vps.to_std()?;
                let mut owned_sps = sps_to_std_h265(sps)?;
                owned_vps.clamp_level(self.config.max_level_idc);
                owned_sps.clamp_level(self.config.max_level_idc);
                let owned_pps = pps_to_std_h265(pps)?;
                // SAFETY: fn contract — live device + live session. The wrappers
                // are MOVED IN and come back owned by the fresh object, so they
                // live as long as it does rather than merely across the call.
                let fresh = unsafe {
                    self.create_parameters_object(
                        vec![owned_vps],
                        vec![owned_sps],
                        vec![owned_pps],
                    )?
                };
                // Replace first so destroy of `old` runs before its backings
                // free — the order a driver still holding the old pointers needs.
                let old = std::mem::replace(&mut self.parameters, fresh);
                // SAFETY: the fn-level contract — the caller drained every
                // in-flight decode before a Recreate reached here (checked via
                // parameters_action), so no submitted work reads the old object;
                // it is this session's own handle, on a live device.
                unsafe {
                    (self.video_queue.fp().destroy_video_session_parameters_khr)(
                        self.device.handle(),
                        old.object,
                        std::ptr::null(),
                    );
                }
                // Drop only after destroy: Std blocks `old` owns must outlive
                // the object that pointed at them.
                drop(old);
                self.ledger.commit(action, vps, sps, pps);
                Ok(())
            }
        }
    }

    pub(crate) fn session(&self) -> vk::VideoSessionKHR {
        self.session
    }

    pub(crate) fn parameters(&self) -> vk::VideoSessionParametersKHR {
        self.parameters.object
    }

    /// Next coding scope must record the initialization RESET. `true` once per
    /// session, only if that command buffer reaches the queue: a record/submit
    /// failure after this returned `true` must call [`Self::re_arm_reset`].
    pub(crate) fn take_needs_reset(&mut self) -> bool {
        self.needs_reset.take()
    }

    pub(crate) fn re_arm_reset(&mut self) {
        self.needs_reset.re_arm();
    }
}

impl Drop for VideoSessionH265 {
    fn drop(&mut self) {
        // SAFETY: all handles are this session's own on a live device; the
        // decoder drains GPU work first. Destroy ignores NULL (half-built
        // sessions). Memory bound into a session must not be freed while the
        // session lives, so destroy the session first — a failed bind parks
        // allocations here (`crate::session::BindFailure`). Std backings drop
        // after this body, after the parameters object is destroyed.
        unsafe {
            (self.video_queue.fp().destroy_video_session_parameters_khr)(
                self.device.handle(),
                self.parameters.object,
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

    /// H.265 has no set builders and `Pps` has no `Default`. Only the fields the
    /// ledger keys or compares carry meaning.
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
            column_width_minus1: [0; 20],
            row_height_minus1: [0; 22],
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

    /// An `Add` hands new Std sets to a live object; their heap must outlive that
    /// object, not the update call. [`StoredParamsH265::adopt`] takes ownership —
    /// `ensure_parameters` drops its locals the instant this returns.
    #[test]
    fn an_added_set_keeps_its_blocks_alive_past_the_update_call() {
        // Fixtures leave `general_profile_idc` at unmappable 0; Std conversion
        // needs a real profile.
        let mut base = (*authored_sps(0, 0, 64)).clone();
        base.profile_tier_level.general_profile_idc = 1; // Main
        base.max_dec_pic_buffering_minus1 = [5, 0, 0, 0, 0, 0, 0];
        let sps = Rc::new(base);
        let owned_vps = VpsSource::for_sps(&sps).to_std().expect("converts");
        let owned_sps = sps_to_std_h265(&sps).expect("converts");
        let vps_ptl = owned_vps.std().pProfileTierLevel;
        let sps_dpb = owned_sps.std().pDecPicBufMgr;
        assert!(!vps_ptl.is_null() && !sps_dpb.is_null());

        let mut stored = StoredParamsH265::none();
        stored.adopt(Some(owned_vps), Some(owned_sps), None);
        assert_eq!(
            (stored.vps.len(), stored.sps.len(), stored.pps.len()),
            (1, 1, 0),
            "the object took the sets themselves, not borrows of them"
        );
        assert_eq!(stored.vps[0].std().pProfileTierLevel, vps_ptl);
        assert_eq!(stored.sps[0].std().pDecPicBufMgr, sps_dpb);
        // SAFETY: `stored` owns both blocks — which is exactly the property here.
        let dpb = unsafe { (*sps_dpb).max_dec_pic_buffering_minus1[0] };
        assert_eq!(dpb, 5, "the fixture's DPB sizing, read back live");
    }

    /// The Add path hands `from_ref(o.std())` — the wrapper's own Std struct —
    /// then moves the wrapper into [`StoredParamsH265`]. A driver may retain the
    /// outer `pStd*` addresses, not only the inner `pDecPicBufMgr` pointers.
    #[test]
    fn an_added_set_keeps_the_address_the_update_call_was_given() {
        let mut base = (*authored_sps(0, 0, 64)).clone();
        base.profile_tier_level.general_profile_idc = 1; // Main
        let sps = Rc::new(base);
        let pps = authored_pps(&sps, 0, 0);
        let owned_vps = VpsSource::for_sps(&sps).to_std().expect("converts");
        let owned_sps = sps_to_std_h265(&sps).expect("converts");
        let owned_pps = pps_to_std_h265(&pps).expect("converts");
        let handed_vps = std::ptr::from_ref(owned_vps.std());
        let handed_sps = std::ptr::from_ref(owned_sps.std());
        let handed_pps = std::ptr::from_ref(owned_pps.std());

        let mut stored = StoredParamsH265::none();
        stored.adopt(Some(owned_vps), Some(owned_sps), Some(owned_pps));
        assert_eq!(
            (
                std::ptr::from_ref(stored.vps[0].std()),
                std::ptr::from_ref(stored.sps[0].std()),
                std::ptr::from_ref(stored.pps[0].std()),
            ),
            (handed_vps, handed_sps, handed_pps),
            "the addresses handed to Vulkan must be the ones the object keeps"
        );
        // SAFETY: `stored` owns all three — which is exactly the property here.
        let ids = unsafe {
            (
                (*handed_vps).vps_video_parameter_set_id,
                (*handed_sps).sps_seq_parameter_set_id,
                (*handed_pps).pps_pic_parameter_set_id,
            )
        };
        assert_eq!(ids, (0, 0, 0), "read back through the driver's pointers");
    }

    /// Create-path `pStd*` arrays are copies of the wrappers' Std structs,
    /// assembled at their final address inside the returned value so the pointer
    /// the driver is given never moves.
    #[test]
    fn the_std_arrays_the_create_call_is_given_are_the_ones_the_object_keeps() {
        let mut base = (*authored_sps(0, 0, 64)).clone();
        base.profile_tier_level.general_profile_idc = 1; // Main
        let sps = Rc::new(base);
        let pps = authored_pps(&sps, 0, 0);
        let owned_vps = VpsSource::for_sps(&sps).to_std().expect("converts");
        let owned_sps = sps_to_std_h265(&sps).expect("converts");
        let owned_pps = pps_to_std_h265(&pps).expect("converts");

        let stored = StoredParamsH265::assemble(vec![owned_vps], vec![owned_sps], vec![owned_pps]);
        let handed = (
            stored.std_vps.as_ptr(),
            stored.std_sps.as_ptr(),
            stored.std_pps.as_ptr(),
        );
        // The move `create_parameters_object` ends with: `Ok(stored)`.
        let stored = std::hint::black_box(stored);
        assert_eq!(
            (
                stored.std_vps.len(),
                stored.std_sps.len(),
                stored.std_pps.len()
            ),
            (1, 1, 1)
        );
        assert_eq!(
            (
                stored.std_vps.as_ptr(),
                stored.std_sps.as_ptr(),
                stored.std_pps.as_ptr(),
            ),
            handed,
            "the arrays handed to Vulkan must be the ones the object keeps"
        );
        let (ptl, dpb) = (
            stored.std_vps[0].pProfileTierLevel,
            stored.std_sps[0].pDecPicBufMgr,
        );
        assert!(!ptl.is_null() && !dpb.is_null(), "both are always attached");
        assert_eq!(ptl, stored.vps[0].std().pProfileTierLevel);
        assert_eq!(dpb, stored.sps[0].std().pDecPicBufMgr);
        // SAFETY: `stored` owns both blocks — which is exactly the property here.
        let profile_idc = unsafe { (*ptl).general_profile_idc };
        assert_eq!(profile_idc, 1, "the fixture's Main profile, read back live");
        assert_eq!(
            stored.std_pps[0].pps_pic_parameter_set_id,
            stored.pps[0].std().pps_pic_parameter_set_id,
            "the PPS copy is the wrapper's, field for field"
        );
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
        // SPS arrived without its VPS; identity is the SPS the fallback uses.
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
        assert_eq!(
            ledger.plan(&fallback, &sps_no_vps, &pps),
            ParamsActionH265::Current
        );

        // Real VPS, same id. Vulkan cannot replace a stored set, so
        // fallback→real recreates. (A real stream also changes the SPS; the VPS
        // leg alone is in `changed_vps_content_under_a_stored_id_recreates_with_the_sps_and_pps_untouched`.)
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

    /// VPS not attached to an SPS. The ledger keys the three arrays independently;
    /// a real stream's SPS carries its VPS, so a VPS change would also change the SPS.
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

        // Same VPS id, different content. SPS and PPS would be Current alone;
        // Vulkan has no in-place replacement, so the object still recreates.
        let vps2 = standalone_vps(0, 1);
        assert_eq!(vps2.id(), vps.id());
        assert_ne!(vps2, vps);
        assert_eq!(ledger.plan(&vps2, &sps, &pps), ParamsActionH265::Recreate);

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

        let pps2 = authored_pps(&sps, 0, 4);
        let action = ledger.plan(&vps, &sps, &pps2);
        assert_eq!(action, ParamsActionH265::Recreate);
        ledger.commit(action, &vps, &sps, &pps2);
        assert_eq!(ledger.next_update_seq(), 1);
        assert_eq!(ledger.plan(&vps, &sps, &pps2), ParamsActionH265::Current);

        // Same SPS id, different content. A resize the extent check would also
        // catch — the ledger must not depend on that.
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

        // One past capacity recreates. The evicted first PPS then re-Adds;
        // recreate kept only the current triple.
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
