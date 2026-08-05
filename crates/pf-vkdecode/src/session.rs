//! `VkVideoSessionKHR` + `VkVideoSessionParametersKHR` lifecycle.
//!
//! The session is created from the STREAM's facts (the SPS's coded extent and DPB
//! depth), its memory requirements bound exactly like the encoder does, and its
//! parameters object holds WP-A's converted `StdVideoH264*ParameterSet`s. Parameter
//! versioning follows Vulkan's rules precisely:
//!
//! - a NEW (sps-id / pps-id) is ADDED via `vkUpdateVideoSessionParametersKHR` with
//!   `updateSequenceCount` = previous + 1 (the spec's exact-increment rule);
//! - an EXISTING id whose content changed cannot be updated in place — the object
//!   is RECREATED (Vulkan forbids replacing a stored parameter set), as is an
//!   object whose capacity would overflow;
//! - a stream renegotiation that resizes the DPB or the coded extent recreates the
//!   whole session — `plan_to_vk`'s `CapacityMismatch` is the trigger the decoder
//!   sees for the DPB half, the extent comparison covers the other.
//!
//! [`ParamsLedger`] is the pure half of that decision table (unit-tested);
//! [`VideoSession`] is the thin Vulkan half.

use std::rc::Rc;

use ash::vk;
use ash::vk::native as hh;
use cros_codecs::codec::h264::parser::Pps;
use cros_codecs::codec::h264::parser::Sps;
use tracing::debug;

use crate::caps::DecodeCaps;
use crate::caps::H264ProfileChain;
use crate::device::find_memory_type_preferring;
use crate::device::AllocError;
use crate::device::DecodeDevice;
use crate::params::pps_to_std;
use crate::params::sps_to_std;
use crate::params::ParamsError;
use crate::params_h265::H265ParamsError;

/// Parameter-object capacity. Punktfunk hosts emit one SPS + one PPS per stream;
/// the headroom absorbs id churn across renegotiations without recreation, and an
/// overflow beyond it recreates rather than fails.
pub(crate) const MAX_STD_SPS: usize = 4;
pub(crate) const MAX_STD_PPS: usize = 8;

/// What the ledger decided for one (SPS, PPS) activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsAction {
    /// Both sets are already stored with identical content — nothing to do.
    Current,
    /// At least one set is new; one update call (seq += 1) adds what is missing.
    Add { add_sps: bool, add_pps: bool },
    /// A stored id changed content, or capacity would overflow: recreate the
    /// parameters object (Vulkan cannot replace or evict a stored set).
    Recreate,
}

/// Pure bookkeeping for the parameters object: which sets it holds (by id AND
/// content — the parser re-parses in-band parameter sets every keyframe, so
/// pointer identity means nothing) and the update sequence counter.
#[derive(Debug, Default)]
pub(crate) struct ParamsLedger {
    sps: Vec<(u8, Rc<Sps>)>,
    pps: Vec<((u8, u8), Rc<Pps>)>,
    update_seq: u32,
}

impl ParamsLedger {
    /// Decide the action for activating (`sps`, `pps`). Pure — mutate via
    /// [`Self::commit`].
    pub(crate) fn plan(&self, sps: &Rc<Sps>, pps: &Rc<Pps>) -> ParamsAction {
        let sps_key = sps.seq_parameter_set_id;
        let pps_key = (pps.seq_parameter_set_id, pps.pic_parameter_set_id);

        let stored_sps = self.sps.iter().find(|(id, _)| *id == sps_key);
        let stored_pps = self.pps.iter().find(|(id, _)| *id == pps_key);
        if let Some((_, stored)) = stored_sps {
            if **stored != **sps {
                return ParamsAction::Recreate;
            }
        }
        if let Some((_, stored)) = stored_pps {
            if **stored != **pps {
                return ParamsAction::Recreate;
            }
        }
        let add_sps = stored_sps.is_none();
        let add_pps = stored_pps.is_none();
        if !add_sps && !add_pps {
            return ParamsAction::Current;
        }
        if (add_sps && self.sps.len() >= MAX_STD_SPS) || (add_pps && self.pps.len() >= MAX_STD_PPS)
        {
            return ParamsAction::Recreate;
        }
        ParamsAction::Add { add_sps, add_pps }
    }

    /// Apply a decided action. `Add` bumps the sequence count by EXACTLY one (the
    /// Vulkan update rule — one call may carry both sets); `Recreate` resets the
    /// ledger to just the current pair with a fresh object's zero counter (any
    /// other id the stream still references simply re-Adds on next activation).
    pub(crate) fn commit(&mut self, action: ParamsAction, sps: &Rc<Sps>, pps: &Rc<Pps>) {
        match action {
            ParamsAction::Current => {}
            ParamsAction::Add { add_sps, add_pps } => {
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
            ParamsAction::Recreate => {
                self.sps.clear();
                self.pps.clear();
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
pub struct SessionConfig {
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    /// The Std profile the session was created against (a profile change is a
    /// renegotiation too).
    pub std_profile_idc: hh::StdVideoH264ProfileIdc,
}

/// Session creation/parameter failures the decoder maps into its error type.
#[derive(Debug)]
pub(crate) enum SessionError {
    Vk(vk::Result),
    Params(ParamsError),
    /// An H.265 parameter set has no Std representation (the H.265 session's
    /// counterpart of [`SessionError::Params`]).
    ParamsH265(H265ParamsError),
    /// Session memory binding found no matching memory type (never a fallback).
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl From<vk::Result> for SessionError {
    fn from(r: vk::Result) -> Self {
        SessionError::Vk(r)
    }
}

impl From<ParamsError> for SessionError {
    fn from(e: ParamsError) -> Self {
        SessionError::Params(e)
    }
}

impl From<H265ParamsError> for SessionError {
    fn from(e: H265ParamsError) -> Self {
        SessionError::ParamsH265(e)
    }
}

impl From<AllocError> for SessionError {
    fn from(e: AllocError) -> Self {
        match e {
            AllocError::Vk(r) => SessionError::Vk(r),
            AllocError::NoMemoryType { type_bits, flags } => {
                SessionError::NoMemoryType { type_bits, flags }
            }
        }
    }
}

/// A [`bind_session_memory`] failure and whatever allocations the CALLER must now
/// take over.
///
/// The distinction is a lifetime rule, not bookkeeping taste. Vulkan defines no
/// partial-bind rollback: once `vkBindVideoSessionMemoryKHR` has been called, some
/// bind indices may have taken, and memory bound into a live session may NOT be
/// freed while that session exists. So:
///
/// - an ALLOCATE failure happens before any bind — nothing is attached to the
///   session, the function frees everything itself, and `allocations` is empty;
/// - a BIND failure hands the allocations back UNFREED, because the caller's
///   session object must be destroyed FIRST. The caller parks them where its own
///   `Drop` frees them after the destroy (that is exactly [`VideoSession`]'s and
///   [`crate::session_h265::VideoSessionH265`]'s field order).
pub(crate) struct BindFailure {
    /// Allocations that may be bound into the session — free them only AFTER the
    /// session is destroyed. Empty when the failure preceded any bind.
    pub(crate) allocations: Vec<vk::DeviceMemory>,
    pub(crate) error: SessionError,
}

/// Query and bind one video session's memory requirements (the encoder's exact
/// shape), returning the allocations the session now owns. Codec-agnostic —
/// `VkVideoSessionKHR` memory binding says nothing about H.264 vs H.265 — so both
/// session types call this, and the NVIDIA placement rationale below lives once.
///
/// Failure hands back a [`BindFailure`] whose `allocations` the caller must adopt
/// (see its docs for the destroy-before-free rule); an allocate-stage failure
/// frees eagerly and hands back none.
///
/// # Safety
///
/// `dev` wraps live handles ([`crate::DeviceHandles`] contract) and `session` is a
/// live, not-yet-memory-bound session created on it.
pub(crate) unsafe fn bind_session_memory(
    dev: &DecodeDevice,
    session: vk::VideoSessionKHR,
) -> Result<Vec<vk::DeviceMemory>, BindFailure> {
    let device = dev.ash();
    let get = dev
        .video_queue()
        .fp()
        .get_video_session_memory_requirements_khr;
    let mut count = 0u32;
    // SAFETY: live device + session (fn contract); null pointer is the
    // count-query form.
    let _ = unsafe { get(device.handle(), session, &mut count, std::ptr::null_mut()) };
    let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
    // SAFETY: as above with an array of the reported count.
    let _ = unsafe { get(device.handle(), session, &mut count, reqs.as_mut_ptr()) };

    let props = dev.memory_properties();
    let mut allocated: Vec<vk::DeviceMemory> = Vec::with_capacity(reqs.len());
    let mut binds = Vec::with_capacity(reqs.len());
    // Free everything allocated so far — for the ALLOCATE-stage exits only, which
    // are reached before `vkBindVideoSessionMemoryKHR` is ever called. The
    // BIND-stage exit must NOT come through here (BindFailure docs).
    let unwind = |device: &ash::Device, allocated: &[vk::DeviceMemory]| {
        for &memory in allocated {
            // SAFETY: allocations made in this function on this live device, none
            // of which the bind call has been reached for — so none can be bound
            // into any session, and freeing them here cannot outlive-order a
            // session destroy.
            unsafe { device.free_memory(memory, None) };
        }
    };
    for rq in &reqs {
        let mr = rq.memory_requirements;
        // DEVICE_LOCAL preferred, any type from `memoryTypeBits` accepted:
        // NVIDIA (610.88) constrains some session bindings to host-visible-only
        // types, and the driver knows where its own session state belongs. NEVER
        // a hard DEVICE_LOCAL requirement — that shape is unsatisfiable there.
        let type_index = match find_memory_type_preferring(
            &props,
            mr.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) {
            Ok(index) => index,
            Err(e) => {
                unwind(device, &allocated);
                return Err(BindFailure {
                    allocations: Vec::new(),
                    error: e.into(),
                });
            }
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(mr.size)
            .memory_type_index(type_index);
        // SAFETY: live device (fn contract).
        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(memory) => memory,
            Err(e) => {
                unwind(device, &allocated);
                return Err(BindFailure {
                    allocations: Vec::new(),
                    error: SessionError::Vk(e),
                });
            }
        };
        allocated.push(memory);
        binds.push(
            vk::BindVideoSessionMemoryInfoKHR::default()
                .memory_bind_index(rq.memory_bind_index)
                .memory(memory)
                .memory_offset(0)
                .memory_size(mr.size),
        );
    }
    // SAFETY: session + freshly allocated memory, one bind per requirement.
    let r = unsafe {
        (dev.video_queue().fp().bind_video_session_memory_khr)(
            device.handle(),
            session,
            binds.len() as u32,
            binds.as_ptr(),
        )
    };
    if r != vk::Result::SUCCESS {
        // NOT freed here: a partial bind may have attached some of these to
        // `session`, and Vulkan has no rollback for that. They go back to the
        // caller, whose session object destroys BEFORE freeing them.
        return Err(BindFailure {
            allocations: allocated,
            error: SessionError::Vk(r),
        });
    }
    Ok(allocated)
}

/// The Vulkan half: session + bound memory + parameters object.
pub(crate) struct VideoSession {
    device: ash::Device,
    video_queue: ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    memory: Vec<vk::DeviceMemory>,
    parameters: vk::VideoSessionParametersKHR,
    ledger: ParamsLedger,
    pub(crate) config: SessionConfig,
    /// The session has never run a coding scope: the first one records a
    /// `VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR` control before anything else (the
    /// spec's initialization requirement; same shape as the encoder's first-frame
    /// RESET install).
    needs_reset: ResetArm,
}

impl VideoSession {
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
        config: SessionConfig,
    ) -> Result<Self, SessionError> {
        let mut chain = H264ProfileChain::new(config.std_profile_idc);
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
            ledger: ParamsLedger::default(),
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
            built.parameters = built.create_parameters_object(&[], &[])?;
        }
        Ok(built)
    }

    /// Create a parameters object holding exactly `sps`/`pps` (either may be empty).
    ///
    /// # Safety
    ///
    /// Live device + live session; the Std slices' backing (the `OwnedStd*`
    /// wrappers) outlives this call — Vulkan copies all parameter data before
    /// returning.
    unsafe fn create_parameters_object(
        &self,
        sps: &[hh::StdVideoH264SequenceParameterSet],
        pps: &[hh::StdVideoH264PictureParameterSet],
    ) -> Result<vk::VideoSessionParametersKHR, SessionError> {
        let add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(sps)
            .std_pp_ss(pps);
        let mut h264 = vk::VideoDecodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(MAX_STD_SPS as u32)
            .max_std_pps_count(MAX_STD_PPS as u32)
            .parameters_add_info(&add);
        let ci = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(self.session)
            .push_next(&mut h264);
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

    /// The ledger's verdict for activating (`sps`, `pps`), without mutating
    /// anything — the decoder consults this BEFORE [`Self::ensure_parameters`] so
    /// a [`ParamsAction::Recreate`] can be preceded by a full in-flight drain
    /// (the destroy inside the recreate must never race a submitted decode).
    pub(crate) fn parameters_action(&self, sps: &Rc<Sps>, pps: &Rc<Pps>) -> ParamsAction {
        self.ledger.plan(sps, pps)
    }

    /// Make the parameters object hold this AU's activated (SPS, PPS), converting
    /// through WP-A and Adding/Recreating per the ledger's decision.
    ///
    /// # Safety
    ///
    /// Live device; when [`Self::parameters_action`] says `Recreate`, the caller
    /// has ALREADY drained every in-flight decode (waited each output slot's
    /// newest submitted timeline value) — the old object is destroyed here, and a
    /// still-executing decode reading it would be use-after-free at the driver
    /// level. The decoder enforces exactly that ordering in `decode_inner`;
    /// `Current`/`Add` touch no object a submitted decode can be reading.
    pub(crate) unsafe fn ensure_parameters(
        &mut self,
        sps: &Rc<Sps>,
        pps: &Rc<Pps>,
    ) -> Result<(), SessionError> {
        let action = self.ledger.plan(sps, pps);
        match action {
            ParamsAction::Current => Ok(()),
            ParamsAction::Add { add_sps, add_pps } => {
                let owned_sps = if add_sps {
                    Some(sps_to_std(sps)?)
                } else {
                    None
                };
                let owned_pps = if add_pps {
                    Some(pps_to_std(pps)?)
                } else {
                    None
                };
                let sps_slice: &[hh::StdVideoH264SequenceParameterSet] = match &owned_sps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let pps_slice: &[hh::StdVideoH264PictureParameterSet] = match &owned_pps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let mut add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
                    .std_sp_ss(sps_slice)
                    .std_pp_ss(pps_slice);
                let update = vk::VideoSessionParametersUpdateInfoKHR::default()
                    .update_sequence_count(self.ledger.next_update_seq())
                    .push_next(&mut add);
                // SAFETY: live device + parameters object; `update` roots locals
                // (incl. the OwnedStd backings) outliving the call, and Vulkan
                // copies parameter data before returning.
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
                self.ledger.commit(action, sps, pps);
                Ok(())
            }
            ParamsAction::Recreate => {
                debug!(
                    sps_id = sps.seq_parameter_set_id,
                    pps_id = pps.pic_parameter_set_id,
                    "recreating session parameters (content change or capacity)"
                );
                let owned_sps = sps_to_std(sps)?;
                let owned_pps = pps_to_std(pps)?;
                // SAFETY: fn contract (the OwnedStd backings live across the call).
                let fresh = unsafe {
                    self.create_parameters_object(
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
                self.ledger.commit(action, sps, pps);
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

/// The one-shot session-RESET arm, its own type so the take/re-arm cycle is
/// testable without a live session object.
#[derive(Debug)]
pub(crate) struct ResetArm(bool);

impl ResetArm {
    pub(crate) fn armed() -> Self {
        Self(true)
    }

    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    pub(crate) fn re_arm(&mut self) {
        self.0 = true;
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        // SAFETY: all handles are this session's own on the (contract-live) device;
        // the owning decoder drains GPU work before dropping state. The destroy
        // entry points ignore NULL handles, covering half-built sessions. The
        // ORDER is load-bearing, not stylistic: memory bound into a session may
        // not be freed while the session lives, so the session is destroyed first
        // — which is also why a failed bind hands its allocations back here
        // instead of freeing them itself ([`BindFailure`]).
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
    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use pf_bitstream::h264::Level;

    use super::*;

    fn authored(sps_id: u8, pps_id: u8, qp: u8) -> (Rc<Sps>, Rc<Pps>) {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(sps_id)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .resolution(64, 64)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(pps_id)
            .pic_init_qp(qp)
            .build();
        (sps, pps)
    }

    #[test]
    fn a_reactivated_identical_pair_is_current_even_across_reparses() {
        let (sps_a, pps_a) = authored(0, 0, 26);
        // The parser re-parses in-band sets each keyframe: same content, NEW Rcs.
        let (sps_b, pps_b) = authored(0, 0, 26);
        assert!(!Rc::ptr_eq(&sps_a, &sps_b));

        let mut ledger = ParamsLedger::default();
        let first = ledger.plan(&sps_a, &pps_a);
        assert_eq!(
            first,
            ParamsAction::Add {
                add_sps: true,
                add_pps: true
            }
        );
        ledger.commit(first, &sps_a, &pps_a);
        assert_eq!(ledger.plan(&sps_b, &pps_b), ParamsAction::Current);
    }

    #[test]
    fn a_new_pps_id_over_a_stored_sps_adds_only_the_pps() {
        let (sps, pps0) = authored(0, 0, 26);
        let pps1 = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(1)
            .pic_init_qp(26)
            .build();

        let mut ledger = ParamsLedger::default();
        let a = ledger.plan(&sps, &pps0);
        ledger.commit(a, &sps, &pps0);
        assert_eq!(
            ledger.plan(&sps, &pps1),
            ParamsAction::Add {
                add_sps: false,
                add_pps: true
            }
        );
    }

    #[test]
    fn changed_content_under_a_stored_id_recreates_and_resets_the_sequence() {
        let (sps, pps) = authored(0, 0, 26);
        let mut ledger = ParamsLedger::default();
        let a = ledger.plan(&sps, &pps);
        ledger.commit(a, &sps, &pps);
        assert_eq!(ledger.next_update_seq(), 2, "one Add happened");

        // Same ids, different content (qp changed): Vulkan cannot replace a
        // stored set, so this must recreate.
        let (sps2, pps2) = authored(0, 0, 30);
        let action = ledger.plan(&sps2, &pps2);
        assert_eq!(action, ParamsAction::Recreate);
        ledger.commit(action, &sps2, &pps2);
        assert_eq!(
            ledger.next_update_seq(),
            1,
            "a fresh object restarts its counter"
        );
        // And the pair is now Current under the new content.
        assert_eq!(ledger.plan(&sps2, &pps2), ParamsAction::Current);
    }

    #[test]
    fn capacity_overflow_recreates_with_just_the_current_pair() {
        let mut ledger = ParamsLedger::default();
        // Fill the PPS capacity under one SPS.
        let (sps, first) = authored(0, 0, 26);
        let a = ledger.plan(&sps, &first);
        ledger.commit(a, &sps, &first);
        for pps_id in 1..MAX_STD_PPS as u8 {
            let pps = PpsBuilder::new(Rc::clone(&sps))
                .pic_parameter_set_id(pps_id)
                .pic_init_qp(26)
                .build();
            let a = ledger.plan(&sps, &pps);
            assert!(matches!(a, ParamsAction::Add { .. }));
            ledger.commit(a, &sps, &pps);
        }
        assert_eq!(ledger.next_update_seq() - 1, MAX_STD_PPS as u32);

        // One past capacity: recreate; afterwards the evicted first PPS re-Adds.
        let overflow = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(MAX_STD_PPS as u8)
            .pic_init_qp(26)
            .build();
        let action = ledger.plan(&sps, &overflow);
        assert_eq!(action, ParamsAction::Recreate);
        ledger.commit(action, &sps, &overflow);
        assert_eq!(
            ledger.plan(&sps, &first),
            ParamsAction::Add {
                add_sps: false,
                add_pps: true
            },
            "sets evicted by a recreate re-add on next activation"
        );
    }

    #[test]
    fn the_reset_arm_fires_once_unless_the_failed_submit_re_arms_it() {
        let mut arm = ResetArm::armed();
        assert!(arm.take(), "a fresh session needs its RESET");
        assert!(
            !arm.take(),
            "consumed — the next scope must NOT reset again"
        );

        // The recorded RESET never reached the queue (end/submit failed): the
        // re-arm makes the next successful recording carry it instead.
        arm.re_arm();
        assert!(arm.take());
        assert!(!arm.take());
    }

    #[test]
    fn update_sequence_counts_one_per_add_call_not_per_set() {
        let (sps, pps) = authored(0, 0, 26);
        let mut ledger = ParamsLedger::default();
        assert_eq!(ledger.next_update_seq(), 1);
        // One call carries BOTH sets: the counter moves by exactly one.
        let a = ledger.plan(&sps, &pps);
        assert_eq!(
            a,
            ParamsAction::Add {
                add_sps: true,
                add_pps: true
            }
        );
        ledger.commit(a, &sps, &pps);
        assert_eq!(ledger.next_update_seq(), 2);
    }
}
