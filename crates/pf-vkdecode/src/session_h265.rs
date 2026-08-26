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
//! ⚠⚠⚠ **The Std sets' heap blocks must outlive the parameters OBJECT, not just the
//! call that hands them over.** Vulkan reads as though parameter data were captured
//! by `vkCreateVideoSessionParametersKHR`, and all three codecs in this crate
//! assumed it. NVIDIA 610.57.04 does not: for AV1 it was measured keeping
//! `StdVideoAV1SequenceHeader::pColorConfig` and dereferencing it when a decode is
//! RECORDED, which decoded every frame against recycled heap ([`crate::session_av1`]
//! carries the measurement). H.265 embeds MORE such pointers than any other codec
//! here — the SPS alone carries seven — so [`StoredParamsH265`] holds the object and
//! its backings in ONE value with one lifetime, and both the recreate path and
//! `Drop` destroy the object before that value is released. The OUTER pointers
//! (`pStdVPSs`/`pStdSPSs`/`pStdPPSs`) are held the same way and for the same reason
//! — [`crate::session`]'s module docs carry the argument and the line it draws.
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
use crate::params_h265::OwnedStdH265Pps;
use crate::params_h265::OwnedStdH265Sps;
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
    /// The device's `maxLevelIdc` for this profile (Std code point). Every VPS/SPS
    /// handed to the parameters object has its declared level clamped to this —
    /// over-declared levels are common (AMF stamps 6.2 on 4K streams) and a set
    /// above the ceiling is invalid usage, while the stream's real demands are
    /// already enforced by `max_coded_extent` / `max_dpb_slots`.
    pub max_level_idc: hh::StdVideoH265LevelIdc,
}

/// A live parameters object **and every Std parameter set it was given**, in one
/// field — because the two may not drift apart. `session::StoredParams` one codec
/// over, with the VPS leg H.264 does not have.
///
/// The wrapper is not decoration and not defensive: a driver in this fleet keeps
/// the embedded pointers out of a Std set and dereferences them long after the call
/// that handed them over returned (module docs), so releasing the backing early
/// hands it freed memory. One value rather than two fields makes "an object whose
/// backing is gone" unrepresentable, which is the only shape of this bug — and the
/// shape a `let owned = …;` local silently had. H.265 has the most to lose: its Std
/// SPS points at a profile/tier/level block, a DPB-manager block, scaling lists,
/// the short-term RPS candidate array and the long-term SPS candidates.
///
/// What is pinned, precisely: the wrappers' BOXED blocks, which is what the driver
/// was measured retaining. The contiguous array of outer `StdVideoH265*` structs
/// each call receives is a short-lived temporary, and the driver copies THAT before
/// returning — which is what the AV1 fix itself rests on, its Std header being moved
/// into storage after the create call on a rung that is now 250/250 bit-exact. So
/// moving these wrappers, or reallocating the `Vec`s holding them, disturbs nothing
/// the driver kept; `params_h265::moving_the_wrapper_leaves_the_driver_s_pointers_put`
/// pins the half that matters.
struct StoredParamsH265 {
    object: vk::VideoSessionParametersKHR,
    /// One entry per set the OBJECT stores, held for the object's whole life.
    /// Never read by this crate after the create/update call; the DRIVER reads the
    /// blocks they own.
    vps: Vec<OwnedStdH265Vps>,
    sps: Vec<OwnedStdH265Sps>,
    pps: Vec<OwnedStdH265Pps>,
    /// The contiguous Std ARRAYS the create call was handed as
    /// `pStdVPSs`/`pStdSPSs`/`pStdPPSs` — the OUTER pointers, held for the object's
    /// life for the reason the wrappers are ([`crate::session`]'s `StoredParams`
    /// carries the argument). Built by [`Self::assemble`] at their final address.
    std_vps: Vec<hh::StdVideoH265VideoParameterSet>,
    std_sps: Vec<hh::StdVideoH265SequenceParameterSet>,
    std_pps: Vec<hh::StdVideoH265PictureParameterSet>,
}

impl StoredParamsH265 {
    /// The wrappers plus the contiguous Std arrays the create call reads its
    /// `pStdVPSs`/`pStdSPSs`/`pStdPPSs` out of, with a NULL object the caller fills
    /// in once `vkCreateVideoSessionParametersKHR` has succeeded
    /// ([`crate::session`]'s `StoredParams::assemble` for why it happens here).
    fn assemble(
        vps: Vec<OwnedStdH265Vps>,
        sps: Vec<OwnedStdH265Sps>,
        pps: Vec<OwnedStdH265Pps>,
    ) -> Self {
        // COPIES of each wrapper's Std struct (it is `Copy`); the embedded pointers
        // they carry still address the wrappers' own boxed blocks, which is why
        // both halves have to be kept.
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

    /// The placeholder a half-built session holds. `vkDestroyVideoSessionParametersKHR`
    /// ignores a NULL handle, so a [`VideoSessionH265::create`] that fails before
    /// the object exists still drops cleanly.
    fn none() -> Self {
        Self::assemble(Vec::new(), Vec::new(), Vec::new())
    }

    /// Take over sets an `Add` just handed to the live object — they belong to the
    /// OBJECT now, so their blocks live as long as it does rather than as long as
    /// the update call. Only ever reached after that call SUCCEEDED: a failed
    /// update stored nothing, and its wrappers are dropped instead.
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

/// The Vulkan half: session + bound memory + parameters object.
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
            parameters: StoredParamsH265::none(),
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
            built.parameters =
                built.create_parameters_object(Vec::new(), Vec::new(), Vec::new())?;
        }
        Ok(built)
    }

    /// Create a parameters object holding exactly `vps`/`sps`/`pps` (any may be
    /// empty), **fused with the wrappers whose heap blocks it points at**.
    ///
    /// Taking the wrappers BY VALUE rather than as Std slices is the point: there is
    /// no way to reach `vkCreateVideoSessionParametersKHR` from here without the
    /// resulting object taking ownership of everything it will go on dereferencing
    /// (module docs, [`StoredParamsH265`]).
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
        // Assembled FIRST so the arrays `pStdVPSs`/`pStdSPSs`/`pStdPPSs` will point
        // at are already where they will stay: `stored` is returned by value, and
        // moving a `Vec` moves its handle, not the block the driver was given.
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
        // SAFETY: fn contract; `ci` roots locals outliving the call, and everything
        // the driver may retain past it — the Std arrays AND the blocks their
        // embedded pointers address — is owned by `stored`, which is returned
        // rather than dropped here.
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
                // ⚠ The added sets now belong to the OBJECT, so their heap blocks
                // must too: an Add whose wrappers died at the end of this arm would
                // be the AV1 use-after-free with an update call in front of it.
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
                // The old object goes FIRST and its backings with it — installing
                // `fresh` through a local keeps the destroy ahead of the free,
                // which is the order a driver still holding the old pointers needs.
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
                // Explicit, because the ORDER is the whole point: every Std block
                // `old` owns is released only now, after the object that pointed at
                // them is gone.
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
        // instead of freeing them itself (`crate::session::BindFailure`). The Std
        // backings are freed after both, by the `parameters` field's own drop,
        // which Rust runs AFTER this body — the same reason `ensure_parameters`
        // destroys before it replaces.
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

    /// An `Add` hands NEW Std sets to an EXISTING parameters object, so their heap
    /// blocks must live as long as that OBJECT — not as long as the update call
    /// that carried them. [`StoredParamsH265::adopt`] is where the transfer
    /// happens, and this pins that it genuinely takes ownership: `ensure_parameters`
    /// drops its local wrappers the instant this returns, and a driver holding
    /// `pDecPicBufMgr` or `pProfileTierLevel` would be reading freed heap from the
    /// next frame on ([`crate::session_av1`] for the measurement that made this
    /// real).
    #[test]
    fn an_added_set_keeps_its_blocks_alive_past_the_update_call() {
        // The ledger fixtures never convert, so they leave `general_profile_idc`
        // at the unmappable 0; Std conversion needs a real one.
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

    /// …and it keeps the ADDRESS too, not merely the blocks.
    ///
    /// The Add path hands `vkUpdateVideoSessionParametersKHR` a
    /// `std::slice::from_ref(o.std())` — a one-element array that IS the wrapper's
    /// own Std struct — and then moves the wrapper into [`StoredParamsH265`]. The
    /// test above covers a driver retaining `pDecPicBufMgr` (an INNER pointer);
    /// this covers one retaining `pStdVPSs`/`pStdSPSs`/`pStdPPSs`, which the same
    /// wording in the spec permits just as much.
    #[test]
    fn an_added_set_keeps_the_address_the_update_call_was_given() {
        let mut base = (*authored_sps(0, 0, 64)).clone();
        base.profile_tier_level.general_profile_idc = 1; // Main
        let sps = Rc::new(base);
        let pps = authored_pps(&sps, 0, 0);
        let owned_vps = VpsSource::for_sps(&sps).to_std().expect("converts");
        let owned_sps = sps_to_std_h265(&sps).expect("converts");
        let owned_pps = pps_to_std_h265(&pps).expect("converts");
        // Exactly what `ensure_parameters` puts in `pStdVPSs`/`pStdSPSs`/`pStdPPSs`.
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

    /// The create path's OUTER pointers: `pStdVPSs`/`pStdSPSs`/`pStdPPSs` address
    /// contiguous COPIES of the wrappers' Std structs, and those arrays must
    /// outlive the create call the same way the wrappers do.
    ///
    /// [`StoredParamsH265::assemble`] builds them at their final address — inside
    /// the value the parameters object is returned in — so the pointer the driver
    /// is given never moves. Before this they were function-local `Vec`s, dropped
    /// the instant `create_parameters_object` returned.
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
        // Exactly what `create_parameters_object` hands the create call.
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
        // And their COPIES still address the wrappers' own live blocks.
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
