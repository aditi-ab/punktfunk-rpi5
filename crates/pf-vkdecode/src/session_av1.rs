//! `VkVideoSessionKHR` + `VkVideoSessionParametersKHR` lifecycle for AV1 —
//! [`crate::session_h265`] one codec over, and much the smaller of the two.
//!
//! AV1's parameter surface is ONE sequence header.
//! `VkVideoDecodeAV1SessionParametersCreateInfoKHR` carries a single
//! `pStdSequenceHeader` and there is no add-info structure at all — no PPS array,
//! no VPS array, and nothing `vkUpdateVideoSessionParametersKHR` can add. That
//! collapses the H.265 ledger's three-way decision table to two states, and BOTH
//! of them are forced by Vulkan rather than chosen here:
//!
//! - the stored header is byte-identical to the one this frame activates ⇒
//!   [`ParamsActionAv1::Current`], nothing to do;
//! - anything else — a first sequence header, or a content change under way —
//!   ⇒ [`ParamsActionAv1::Recreate`]. Vulkan cannot REPLACE a stored parameter
//!   set, and for AV1 it cannot ADD one either, so recreation is the only move.
//!
//! One consequence is worth stating because it differs from the other two codecs:
//! **the parameters object is not created with the session.** H.264 and H.265
//! create an empty object up front and Add sets into it; an AV1 parameters object
//! has no empty form (`pStdSequenceHeader` must be a valid pointer), so
//! [`VideoSessionAv1::create`] leaves the handle NULL and the first
//! [`VideoSessionAv1::ensure_parameters`] creates it. A decode recorded before
//! that would bind a NULL parameters object, which is why the decoder calls
//! `ensure_parameters` before every submission and nothing else may create the
//! session's coding scope.
//!
//! ⚠⚠⚠ **The Std sequence header's heap blocks must outlive the parameters
//! OBJECT, not just the create call.** Vulkan reads as though parameter data were
//! captured by `vkCreateVideoSessionParametersKHR`, and this module assumed it —
//! [`sequence_to_std`]'s wrapper was a local, dropped the moment the call
//! returned. NVIDIA 610.57.04 keeps the pointer instead and dereferences
//! `pColorConfig` when a decode is RECORDED, so every AV1 frame was decoded
//! against whatever the allocator had since put in those 24 bytes. Measured on an
//! RTX 5070 Ti: correct at create, `23 00 00 00 00 00 00 00 77 29 …` by the first
//! `vkCmdDecodeVideoKHR` — which reads as `mono_chrome = 1`, so the driver
//! deblocked the frame as monochrome and skipped `loop_filter_level[2..3]`
//! entirely. That is the whole of the AV1 rung's parity gap (250/250 frames
//! divergent; 0/250 with the backing held, [`StoredParamsAv1`]).
//!
//! ⚠⚠ That fix stabilised the INNER pointers only. `pStdSequenceHeader` — the
//! address `VkVideoDecodeAV1SessionParametersCreateInfoKHR` itself carries — was
//! still [`sequence_to_std`]'s stack local, dead the moment `ensure_parameters`
//! returned, and a driver retaining IT rather than `pColorConfig` would reproduce
//! the bug exactly. The Std struct is now boxed inside the wrapper, so the address
//! handed over is the one `StoredParamsAv1` keeps ([`crate::session`]'s module
//! docs carry the argument and the line it draws).
//!
//! `ParamsLedgerAv1` is the pure half of the decision (unit-tested);
//! [`VideoSessionAv1`] is the thin Vulkan half.

use std::rc::Rc;

use ash::vk;
use cros_codecs::codec::av1::parser::SequenceHeaderObu;
use tracing::debug;

use crate::caps::DecodeCaps;
use crate::caps_av1::Av1ProfileChain;
use crate::caps_av1::Av1ProfileKey;
use crate::device::DecodeDevice;
use crate::params_av1::sequence_to_std;
use crate::params_av1::OwnedStdAv1SequenceHeader;
use crate::session::bind_session_memory;
use crate::session::ResetArm;
use crate::session::SessionError;

/// What the ledger decided for one sequence-header activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsActionAv1 {
    /// The identical sequence header is already stored — nothing to do.
    Current,
    /// No object exists yet, or the stored header's content changed: create a
    /// fresh parameters object. There is deliberately no `Add` — AV1 session
    /// parameters hold exactly one sequence header and Vulkan offers no update
    /// path for it (module docs).
    Recreate,
}

/// Pure bookkeeping for the parameters object: which sequence header it holds, by
/// CONTENT.
///
/// By content rather than by pointer for the reason the other two ledgers give:
/// the parser re-parses the in-band sequence header at every keyframe, so a
/// perfectly unchanged stream hands out a fresh `Rc` several times a second, and
/// keying on identity would recreate the parameters object — and with it stall the
/// pipeline for a drain — at every one of them.
#[derive(Debug, Default)]
pub(crate) struct ParamsLedgerAv1 {
    sequence: Option<Rc<SequenceHeaderObu>>,
}

impl ParamsLedgerAv1 {
    /// Decide the action for activating `sequence`. Pure — mutate via
    /// [`Self::commit`].
    pub(crate) fn plan(&self, sequence: &Rc<SequenceHeaderObu>) -> ParamsActionAv1 {
        match &self.sequence {
            Some(stored) if **stored == **sequence => ParamsActionAv1::Current,
            _ => ParamsActionAv1::Recreate,
        }
    }

    /// Apply a decided action.
    pub(crate) fn commit(&mut self, action: ParamsActionAv1, sequence: &Rc<SequenceHeaderObu>) {
        match action {
            ParamsActionAv1::Current => {}
            ParamsActionAv1::Recreate => self.sequence = Some(Rc::clone(sequence)),
        }
    }
}

/// The session's create-time shape; a plan disagreeing with it forces a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfigAv1 {
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    /// The profile the session was created against — Std profile, sampling, bit
    /// depth AND the film-grain flag, every one of which a stream can renegotiate
    /// (a sequence header switching 8-bit → 10-bit, or turning film grain on, is a
    /// session rebuild, not a parameters update).
    pub profile: Av1ProfileKey,
}

/// A live parameters object **and the Std sequence header it was created from**,
/// in one field — because the two may not drift apart.
///
/// The wrapper is not decoration and not defensive: the driver dereferences the
/// header's `pColorConfig` long after the create call returned (module docs), so
/// dropping the backing early hands it freed memory. One field rather than two
/// makes "an object whose backing is gone" unrepresentable, which is the only
/// shape of this bug — and the shape a `let owned = …;` local silently had.
struct StoredParamsAv1 {
    object: vk::VideoSessionParametersKHR,
    /// Held for the OBJECT's whole life. Never read by this crate after the
    /// create call; the DRIVER reads it — potentially through `pStdSequenceHeader`
    /// itself, which is why the wrapper boxes its Std struct rather than holding it
    /// inline (module docs).
    _sequence: OwnedStdAv1SequenceHeader,
}

/// The Vulkan half: session + bound memory + parameters object.
pub(crate) struct VideoSessionAv1 {
    device: ash::Device,
    video_queue: ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    memory: Vec<vk::DeviceMemory>,
    /// `None` until the first [`Self::ensure_parameters`] — an AV1 parameters
    /// object has no empty form (module docs).
    parameters: Option<StoredParamsAv1>,
    ledger: ParamsLedgerAv1,
    pub(crate) config: SessionConfigAv1,
    /// The session has never run a coding scope: the first one records a
    /// `VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR` control before anything else.
    needs_reset: ResetArm,
}

impl VideoSessionAv1 {
    /// Create the session. The parameters object follows at the first
    /// [`Self::ensure_parameters`], which the decoder calls before every decode.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        config: SessionConfigAv1,
    ) -> Result<Self, SessionError> {
        let mut chain = Av1ProfileChain::new(config.profile);
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
            parameters: None,
            ledger: ParamsLedgerAv1::default(),
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
        }
        Ok(built)
    }

    /// The ledger's verdict for activating `sequence`, without mutating anything —
    /// the decoder consults this BEFORE [`Self::ensure_parameters`] so a
    /// [`ParamsActionAv1::Recreate`] over an EXISTING object can be preceded by a
    /// full in-flight drain (the destroy inside the recreate must never race a
    /// submitted decode).
    pub(crate) fn parameters_action(&self, sequence: &Rc<SequenceHeaderObu>) -> ParamsActionAv1 {
        self.ledger.plan(sequence)
    }

    /// Whether a parameters object exists at all. The decoder pairs this with
    /// [`Self::parameters_action`]: the FIRST `Recreate` of a session's life
    /// destroys nothing and needs no drain, every later one does.
    pub(crate) fn has_parameters(&self) -> bool {
        self.parameters.is_some()
    }

    /// Make the parameters object hold this frame's active sequence header.
    ///
    /// # Safety
    ///
    /// Live device; when [`Self::parameters_action`] says `Recreate` AND
    /// [`Self::has_parameters`] is true, the caller has ALREADY drained every
    /// in-flight decode — the old object is destroyed here, and a still-executing
    /// decode reading it would be use-after-free at the driver level.
    /// `Current` touches no object a submitted decode can be reading.
    pub(crate) unsafe fn ensure_parameters(
        &mut self,
        sequence: &Rc<SequenceHeaderObu>,
    ) -> Result<(), SessionError> {
        let action = self.ledger.plan(sequence);
        match action {
            ParamsActionAv1::Current => Ok(()),
            ParamsActionAv1::Recreate => {
                debug!(
                    first = !self.has_parameters(),
                    "creating AV1 session parameters (first activation or a \
                     sequence-header content change)"
                );
                // ⚠ The owned wrapper is MOVED INTO the stored parameters below
                // and lives as long as the object does — not merely across the
                // create call. Module docs carry the measurement; the short of it
                // is that a driver in this fleet dereferences `pColorConfig` at
                // every `vkCmdDecodeVideoKHR`, so an early drop decodes the whole
                // stream against recycled heap.
                let owned = sequence_to_std(sequence).map_err(SessionError::ParamsAv1)?;
                let mut av1 = vk::VideoDecodeAV1SessionParametersCreateInfoKHR::default()
                    .std_sequence_header(owned.std());
                let ci = vk::VideoSessionParametersCreateInfoKHR::default()
                    .video_session(self.session)
                    .push_next(&mut av1);
                let mut fresh = vk::VideoSessionParametersKHR::null();
                // SAFETY: live device + live session; `ci` roots locals (incl. the
                // OwnedStd backing, which outlives the call AND the object it
                // creates — see the module docs on why the second half matters).
                let r = unsafe {
                    (self.video_queue.fp().create_video_session_parameters_khr)(
                        self.device.handle(),
                        &ci,
                        std::ptr::null(),
                        &mut fresh,
                    )
                };
                if r != vk::Result::SUCCESS {
                    return Err(SessionError::Vk(r));
                }
                // The old object goes FIRST and its backing with it — taking the
                // whole `StoredParamsAv1` keeps the destroy ahead of the free,
                // which is the order a driver holding the pointer needs.
                if let Some(old) = self.parameters.take() {
                    // SAFETY: the fn-level contract — the caller drained every
                    // in-flight decode before a Recreate over an existing object
                    // reached here (checked via parameters_action +
                    // has_parameters), so no submitted work reads the old object;
                    // it is this session's own handle, on a live device.
                    unsafe {
                        (self.video_queue.fp().destroy_video_session_parameters_khr)(
                            self.device.handle(),
                            old.object,
                            std::ptr::null(),
                        );
                    }
                    // `old` (and its sequence-header blocks) drops here, after the
                    // object that pointed at them is gone.
                }
                self.parameters = Some(StoredParamsAv1 {
                    object: fresh,
                    _sequence: owned,
                });
                self.ledger.commit(action, sequence);
                Ok(())
            }
        }
    }

    pub(crate) fn session(&self) -> vk::VideoSessionKHR {
        self.session
    }

    pub(crate) fn parameters(&self) -> vk::VideoSessionParametersKHR {
        self.parameters
            .as_ref()
            .map_or(vk::VideoSessionParametersKHR::null(), |p| p.object)
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

impl Drop for VideoSessionAv1 {
    fn drop(&mut self) {
        // SAFETY: all handles are this session's own on the (contract-live) device;
        // the owning decoder drains GPU work before dropping state. The destroy
        // entry points ignore NULL handles, covering half-built sessions AND the
        // session that never got a parameters object. The ORDER is load-bearing,
        // not stylistic: memory bound into a session may not be freed while the
        // session lives, so the session is destroyed first — which is also why a
        // failed bind hands its allocations back here instead of freeing them
        // itself (`crate::session::BindFailure`). The sequence-header backing is
        // freed after both, by the field's own drop, for the same reason
        // `ensure_parameters` destroys before it replaces.
        unsafe {
            (self.video_queue.fp().destroy_video_session_parameters_khr)(
                self.device.handle(),
                self.parameters(),
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

    /// A sequence header carrying just the fields the ledger compares. The
    /// vendored AV1 parser has no builders, and `SequenceHeaderObu` derives
    /// `Default` + `PartialEq`, so the fixtures are authored by field.
    fn authored(max_frame_width_minus_1: u16, film_grain: bool) -> Rc<SequenceHeaderObu> {
        Rc::new(SequenceHeaderObu {
            max_frame_width_minus_1,
            max_frame_height_minus_1: 1079,
            film_grain_params_present: film_grain,
            ..Default::default()
        })
    }

    /// The OUTER pointer — `pStdSequenceHeader` itself — must still address the
    /// stored wrapper's Std struct after the wrapper has been moved into
    /// [`StoredParamsAv1`].
    ///
    /// [`crate::params_av1`]'s `moving_the_wrapper_leaves_the_driver_s_pointers_put`
    /// pins the INNER pointers (`pColorConfig`, `pTimingInfo`); this pins the one
    /// the create info carries. `ensure_parameters` converts into a local, hands
    /// `owned.std()` to `vkCreateVideoSessionParametersKHR`, and only THEN moves
    /// the wrapper into the stored value — so a driver retaining
    /// `pStdSequenceHeader` (rather than the `pColorConfig` this fleet was measured
    /// retaining) would read a moved-from slot, with the same silent signature as
    /// the original bug: plausible pictures, wrong content, no error and no
    /// counter. Boxing the Std struct inside the wrapper is what makes the two
    /// addresses equal, and this is the assertion that stops it being un-boxed.
    #[test]
    fn the_sequence_header_address_the_create_call_is_given_survives_being_stored() {
        let seq = authored(1919, false);
        let owned = sequence_to_std(&seq).expect("a plain 8-bit header converts");
        // Exactly what `ensure_parameters` puts in `pStdSequenceHeader`.
        let handed = std::ptr::from_ref(owned.std());
        // `ensure_parameters`' own final move: `self.parameters = Some(…)`.
        let parameters = Some(StoredParamsAv1 {
            object: vk::VideoSessionParametersKHR::null(),
            _sequence: owned,
        });
        let Some(stored) = parameters else {
            unreachable!("just installed")
        };
        assert_eq!(
            std::ptr::from_ref(stored._sequence.std()),
            handed,
            "the address handed to Vulkan must be the address the object keeps"
        );
        // SAFETY: `stored` owns the header — which is exactly the property here.
        let width = unsafe { (*handed).max_frame_width_minus_1 };
        assert_eq!(width, 1919, "the fixture's width, read back through it");
    }

    #[test]
    fn the_first_activation_recreates_because_there_is_no_empty_parameters_object() {
        let seq = authored(1919, false);
        let mut ledger = ParamsLedgerAv1::default();
        // Not `Add`: AV1 session parameters have no update path, and no object
        // exists yet — the session was created without one.
        assert_eq!(ledger.plan(&seq), ParamsActionAv1::Recreate);
        ledger.commit(ParamsActionAv1::Recreate, &seq);
        assert_eq!(ledger.plan(&seq), ParamsActionAv1::Current);
    }

    #[test]
    fn a_reparsed_identical_sequence_header_is_current_not_a_recreate() {
        // The parser re-parses the in-band sequence header at every keyframe:
        // same content, a NEW Rc. Keying on identity would drain and rebuild the
        // parameters object several times a second on a perfectly steady stream.
        let a = authored(1919, false);
        let b = authored(1919, false);
        assert!(!Rc::ptr_eq(&a, &b));

        let mut ledger = ParamsLedgerAv1::default();
        ledger.commit(ParamsActionAv1::Recreate, &a);
        assert_eq!(ledger.plan(&b), ParamsActionAv1::Current);
    }

    #[test]
    fn a_changed_sequence_header_recreates_and_the_new_one_is_then_current() {
        let small = authored(1279, false);
        let large = authored(1919, false);
        let mut ledger = ParamsLedgerAv1::default();
        ledger.commit(ParamsActionAv1::Recreate, &small);
        assert_eq!(ledger.plan(&small), ParamsActionAv1::Current);

        // A resize is a content change, so the object is rebuilt — and this is
        // the ONLY path AV1 has: there is no in-place replacement for a stored
        // sequence header.
        assert_eq!(ledger.plan(&large), ParamsActionAv1::Recreate);
        ledger.commit(ParamsActionAv1::Recreate, &large);
        assert_eq!(ledger.plan(&large), ParamsActionAv1::Current);
        assert_eq!(
            ledger.plan(&small),
            ParamsActionAv1::Recreate,
            "the ledger holds exactly one header — the old one is gone"
        );
    }

    #[test]
    fn turning_film_grain_on_is_a_content_change_the_ledger_sees() {
        // It is ALSO a profile change, which rebuilds the whole session
        // (SessionConfigAv1::profile) — but the ledger must not depend on the
        // session layer having noticed: a sequence header that differs only in
        // its grain flag is a different stored set, full stop.
        let plain = authored(1919, false);
        let grainy = authored(1919, true);
        assert_ne!(plain, grainy);
        let mut ledger = ParamsLedgerAv1::default();
        ledger.commit(ParamsActionAv1::Recreate, &plain);
        assert_eq!(ledger.plan(&grainy), ParamsActionAv1::Recreate);
    }

    #[test]
    fn committing_current_leaves_the_stored_header_alone() {
        // `commit(Current, ..)` is reachable on every steady-state frame; it must
        // be a genuine no-op rather than a silent re-store of an equal value.
        let a = authored(1919, false);
        let mut ledger = ParamsLedgerAv1::default();
        assert!(ledger.sequence.is_none());
        ledger.commit(ParamsActionAv1::Current, &a);
        assert!(
            ledger.sequence.is_none(),
            "Current must not install a header the object does not hold"
        );
        // And the next plan still says the object needs building.
        assert_eq!(ledger.plan(&a), ParamsActionAv1::Recreate);
    }
}
