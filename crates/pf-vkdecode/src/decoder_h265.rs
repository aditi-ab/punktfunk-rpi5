//! [`VkH265Decoder`]: the assembled native H.265 decoder — [`crate::decoder`] one
//! codec over, over pf-bitstream's H.265 planner and M3's CPU half.
//!
//! Per AU: `plan_au` → `plan_to_vk_h265` → slices-only upload into the bitstream
//! ring → record (barriers, `vkCmdBeginVideoCodingKHR` with every bound DPB slot,
//! the one-time session RESET control, a caps-gated `RESULT_STATUS_ONLY` query
//! bracketing `vkCmdDecodeVideoKHR`) → submit on the decode queue under the
//! caller's [`QueueLock`] with a per-image timeline signal.
//!
//! Everything codec-agnostic is SHARED with the H.264 decoder rather than
//! re-implemented: the picture pool and its zero-copy hand-off contract
//! ([`crate::images`]), the bitstream ring and its slices-only packing, the op ring
//! (command buffers + status queries), the pending/ready/graveyard bookkeeping,
//! and `settle_dpb`/`build_frame` from [`crate::decoder`]. What is genuinely
//! H.265's own lives here:
//!
//! - **The picture format is the stream's, not a constant.** Main decodes to NV12,
//!   Main 10 to P010, RExt 4:4:4 to the two-plane 4:4:4 formats — and the same
//!   facts shape the Vulkan profile every object is created against
//!   ([`H265ProfileKey`]). A device that cannot host the combination is refused
//!   BEFORE a session exists, so the ladder demotes cleanly.
//! - **`pReferenceSlots` order is load-bearing.**
//!   `StdVideoDecodeH265PictureInfo`'s `RefPicSetStCurrBefore`/`StCurrAfter`/
//!   `LtCurr` arrays hold INDICES INTO the decode op's reference-slot array
//!   ([`DecodePlanVkH265::refs`] order), not slot indices. The recording below
//!   therefore lays the op's references out in exactly `refs` order and FAILS
//!   CLOSED if any of them has no bound image — dropping one would silently
//!   re-point every later index at the wrong picture. (H.264 has no such arrays
//!   and only traces the case.)
//! - **Slice SEGMENT offsets, rebased.** The plan's offsets are AU-relative and
//!   start-code-inclusive; the ring carries the slice NALUs ALONE (non-VCL NALUs
//!   inside the decode range hang VCN firmware — the `vcn_unified_0` ring timeout),
//!   so `pSliceSegmentOffsets` gets `rebased_offsets`' ([`crate::ring`]) output.
//! - **RASL skips are not failures.** A RASL picture after an open-GOP CRA join is
//!   undecodable by definition (8.1.3 NOTE); [`VkH265Decoder::decode`] answers it
//!   like any other decode that produced no new picture — the next frame already
//!   queued, or `None` — leaving the planner and the DPB untouched. No re-anchor,
//!   no keyframe request: the very next AU plans normally.
//!
//! Codec dispatch (which decoder a stream gets) is the client wiring's job, not
//! this crate's: the public surface here mirrors [`crate::VkH264Decoder`]
//! method-for-method so the dispatch is a two-arm enum.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use ash::vk;
use ash::vk::native as hh;
use pf_bitstream::h265::AuPlan;
use pf_bitstream::h265::H265Planner;
use pf_bitstream::h265::PicId;
use pf_bitstream::h265::PlanError;
use pf_bitstream::h265::PlanWarning;
use tracing::debug;
use tracing::trace;

use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps_h265::derive_caps_h265;
use crate::caps_h265::query_h265_caps;
use crate::caps_h265::H265ProfileKey;
use crate::decoder::build_frame;
use crate::decoder::settle_dpb;
use crate::decoder::wait_timeline;
use crate::decoder::DecodeStatus;
use crate::decoder::DecodedVkFrame;
use crate::decoder::OpRing;
use crate::decoder::PendingPic;
use crate::decoder::RetiredPool;
use crate::decoder::VkDecodeError;
use crate::device::DecodeDevice;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::device::QueueSubmitGuard;
use crate::images::plan_pools;
use crate::images::DpbPool;
use crate::images::PicturePool;
use crate::params_h265::level_to_std as level_to_std_h265;
use crate::pic_h265::plan_to_vk_h265;
use crate::pic_h265::DecodePlanVkH265;
use crate::pic_h265::PlanToVkH265Error;
use crate::ring::rebased_offsets;
use crate::ring::BitstreamRing;
use crate::ring::RingLayout;
use crate::ring::UploadedAu;
use crate::ring::INITIAL_SLOT_SIZE;
use crate::ring::RING_SLOTS;
use crate::session_h265::ParamsActionH265;
use crate::session_h265::SessionConfigH265;
use crate::session_h265::VideoSessionH265;
use crate::session_h265::VpsSource;
use crate::slots::SlotMap;

/// Everything tied to ONE H.265 session generation. A stream renegotiation
/// (extent, DPB depth, profile — including a bit-depth or chroma-format switch)
/// retires it and builds fresh.
struct SessionStateH265 {
    session: VideoSessionH265,
    slots: SlotMap,
    /// Distinct mode's reference-only DPB backing; `None` in coincide mode (the
    /// picture pool backs the DPB there).
    dpb: Option<DpbPool>,
    pool: PicturePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Last-known Std reference info per DPB slot — `vkCmdBeginVideoCodingKHR`
    /// wants codec reference info for EVERY bound slot, including ones this AU's
    /// RPS does not reference; refreshed from each plan's setup/ref entries so
    /// marking transitions (short-term → long-term promotion, which in HEVC only
    /// ever happens through a LATER picture's `RefPicSetLtCurr`) propagate.
    slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>>,
    /// Coincide mode: which pool image each DPB slot currently binds (rebound at
    /// every activation — the decoupling that keeps delivered images safe).
    slot_image: Vec<Option<usize>>,
    /// Per command-buffer completion tokens (reuse gate).
    cmd_marks: Vec<Option<(vk::Semaphore, u64)>>,
    /// Per query-slot submission ordinals (staleness validation).
    query_marks: Vec<u64>,
    /// Submissions recorded on this session (cmd/query indexing).
    submitted: u64,
    /// The newest submission's completion token (session drain).
    last_submit: Option<(vk::Semaphore, u64)>,
    /// The STREAM's coded extent (renegotiation comparison).
    coded_extent: vk::Extent2D,
    /// The granularity-aligned allocation extent (picture resources + frames).
    image_extent: vk::Extent2D,
}

/// The post-failure recovery latch: set when an AU failed after its planning had
/// already advanced, consumed by the next `decode`, which flushes to the next
/// IRAP before planning anything new.
///
/// Why this exists at all — the fail-closed/recover split:
///
/// This decoder FAILS CLOSED, and that stays: when an AU cannot be carried
/// through to a submitted decode, it returns an error rather than substituting a
/// reference or decoding against a slot whose image is gone. H.264's
/// soft-degrade (trace the missing binding, drop that reference, decode anyway)
/// is not available here because `StdVideoDecodeH265PictureInfo`'s
/// `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr` arrays hold INDICES into the
/// decode op's reference array — dropping one entry re-points every later index
/// at the wrong picture, which is the corruption-hiding class this crate refuses.
///
/// But failing closed once must not wedge the stream FOREVER, and without this
/// latch it did: by the time an AU reaches a failure exit, `plan_to_vk_h265` has
/// already mutated the [`SlotMap`] (releases + the setup assignment) and the
/// coincide binding sync has already cleared the setup slot's image binding. The
/// planner and the slot map then both believe picture N is resident while no
/// image holds it, so every later AU referencing N fails in
/// [`build_scope`] with `UnboundReferenceSlot` — a transient consumer
/// backpressure (`NoFreeSlot` on AU N) turned into a permanently dead stream.
///
/// The recovery is a FLUSH TO THE NEXT IRAP, not H.264-style reference
/// substitution: `H265Planner::flush` drops the whole DPB and refuses everything
/// until the next IRAP, this decoder resets the slot ledger and image bindings to
/// match, and the integration layer — which already sets `want_keyframe` on EVERY
/// decode error — has an IDR on the way. So the stream re-anchors on real,
/// complete data instead of resuming over a DPB nobody can vouch for.
///
/// Its own type (the [`crate::session::ResetArm`] idiom one module over) so the
/// latch/consume cycle is unit-testable without a live device.
#[derive(Debug, Default)]
pub(crate) struct RecoveryLatch(bool);

impl RecoveryLatch {
    /// Record that recovery is owed. Idempotent: two failures in a row still owe
    /// exactly one flush.
    pub(crate) fn latch(&mut self) {
        self.0 = true;
    }

    /// Whether recovery is owed, CLEARING the latch — the recovery runs once per
    /// failure run, not on every later decode.
    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    /// Whether recovery is owed, without consuming it (state snapshots).
    pub(crate) fn is_latched(&self) -> bool {
        self.0
    }
}

/// The native Vulkan Video H.265 decoder. Mirrors [`crate::VkH264Decoder`]'s
/// public surface exactly.
pub struct VkH265Decoder {
    dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    planner: H265Planner,
    /// Caps per profile key, queried once per profile (a Main→Main 10 switch is a
    /// different key and re-queries).
    caps: Option<(H265ProfileKey, DecodeCaps)>,
    state: Option<SessionStateH265>,
    /// Decoded pictures awaiting their planner output verdict, keyed by [`PicId`].
    pending: BTreeMap<PicId, PendingPic>,
    /// Display-ready frames not yet handed out.
    ready: VecDeque<DecodedVkFrame>,
    /// Retired generations' pools with consumer-held images (die on their last
    /// release token).
    graveyard: Vec<RetiredPool>,
    /// The most recent plan's warnings ([`Self::take_warnings`]).
    last_warnings: Vec<PlanWarning>,
    /// The outstanding recovery point SEI, if any — see [`crate::recovery`].
    /// Named apart from [`Self::recovery`], which is this decoder's DPB-recovery
    /// latch: the two are unrelated (one is a fact about the stream's prediction
    /// structure, the other about this decoder's own wedged state).
    recovery_watch: crate::recovery::RecoveryWatch,
    /// Pictures planned so far — stamped onto each one as
    /// [`DecodedVkFrame::decode_order`]. Survives session rebuilds for the same
    /// reason the watch does.
    decoded: u64,
    /// Session generation: bumped on every rebuild, stamped into frames.
    generation: u64,
    device_lost: bool,
    /// Recovery owed after a failed AU whose planning had already advanced
    /// ([`RecoveryLatch`] docs for the whole argument).
    recovery: RecoveryLatch,
}

impl VkH265Decoder {
    /// Wrap the borrowed device. Sessions/pools are built lazily from the first
    /// AU's SPS (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The full [`DeviceHandles`] caller contract (liveness, enabled extensions
    /// and features, truthful queue families) — held for this decoder's whole
    /// lifetime, not just this call. The device must additionally have been
    /// created with `VK_KHR_video_decode_h265` enabled; that part of the contract
    /// is checked below AS FAR AS IT CAN BE — the check reads the decode queue
    /// family's advertised `videoCodecOperations`, which is the
    /// device's own claim about the family, not proof that the client enabled the
    /// extension at `vkCreateDevice`. (punktfunk's presenter enables h264 + h265 +
    /// av1, filtered by what the device supports — `pf-presenter/src/vk/setup.rs`
    /// — so the two coincide there.) Getting it wrong is undefined behaviour at
    /// session creation rather than an error, which is why the family check runs
    /// before anything is queried or created.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        // Before anything is queried or created: does this queue family actually
        // run H.265 decode ops? `query_h265_caps` would succeed on capable
        // hardware regardless (physical-device query), and the first
        // `vkCreateVideoSessionKHR` with a DECODE_H265 profile on a device that
        // never enabled the extension is UB — this is the ladder's clean demote.
        dev.require_codec_op(vk::VideoCodecOperationFlagsKHR::DECODE_H265, "H.265 decode")?;
        Ok(Self {
            dev,
            lock,
            planner: H265Planner::new(),
            caps: None,
            state: None,
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
            graveyard: Vec::new(),
            last_warnings: Vec::new(),
            recovery_watch: crate::recovery::RecoveryWatch::new(),
            decoded: 0,
            generation: 0,
            device_lost: false,
            recovery: RecoveryLatch::default(),
        })
    }

    /// Ask the device, BEFORE a single AU is fed, whether it can decode a stream of
    /// the negotiated (chroma format, bit depth) shape — the construction-time half
    /// of what the lazy `ensure_state` path would otherwise only discover at the
    /// first SPS.
    ///
    /// Why it exists: the session's picture format is the STREAM's, and a device
    /// that advertises H.265 decode need not advertise a picture format for every
    /// shape of it — 4:4:4 RExt is absent everywhere but NVIDIA, and 10-bit is
    /// absent on some older silicon. Discovering that lazily makes the refusal a
    /// mid-stream ERROR STREAK, which demotes past the FFmpeg rungs to
    /// VAAPI/D3D11VA/software; discovering it here makes it a construction failure,
    /// which the client's ladder answers by falling through to the next rung with
    /// the session's hardware decode intact. Same query, same derivation, same
    /// [`crate::CapsError`] — only the timing differs.
    ///
    /// The negotiated facts are a HINT (the in-band SPS is authoritative), so this
    /// is deliberately not a promise that decode will succeed: the level ceiling and
    /// an SPS that disagrees with the Welcome still surface at the first AU. What it
    /// does guarantee is that a shape the device provably cannot host never gets a
    /// session built for it.
    pub fn probe_stream_support(
        &self,
        chroma_format_idc: u8,
        bit_depth_luma_minus8: u8,
    ) -> Result<(), VkDecodeError> {
        let key = H265ProfileKey::from_negotiated(chroma_format_idc, bit_depth_luma_minus8)?;
        let wanted = key
            .output_format()
            .expect("from_negotiated gated the chroma/depth combination");
        // SAFETY: the constructor's `DeviceHandles` contract holds for this
        // decoder's whole lifetime, so the physical device is live — the same
        // proof `ensure_state`'s identical call carries.
        let raw = unsafe { query_h265_caps(&self.dev, key) }.map_err(VkDecodeError::from)?;
        derive_caps_h265(&raw, wanted)?;
        Ok(())
    }

    /// Decode one access unit. Returns the next display-ready frame, if the
    /// planner declared one.
    ///
    /// A RASL picture the planner refuses after an open-GOP join is NOT an error:
    /// it is undecodable BY DEFINITION (8.1.3 NOTE — its references precede the
    /// join), so this returns `Ok` with whatever was already display-ready (a
    /// frame an earlier AU decoded, or `None`) and leaves the planner, the DPB and
    /// the slot ledger untouched; the next AU plans normally. Treating it as an
    /// error would make every CRA join request a keyframe the host has no reason
    /// to send. The warning ledger IS cleared, per [`Self::take_warnings`].
    ///
    /// Never panics. `VkDecodeError::DeviceLost` latches: every later call fails
    /// fast until the owner rebuilds the decoder on fresh handles.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        if self.device_lost {
            return Err(VkDecodeError::DeviceLost);
        }
        let result = self.decode_inner(au);
        if matches!(result, Err(VkDecodeError::DeviceLost)) {
            self.device_lost = true;
        }
        result
    }

    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        // A previous AU failed after its planning had advanced: clear the stale
        // DPB residency BEFORE planning this one, or every AU referencing the
        // stranded picture fails forever ([`RecoveryLatch`] docs).
        if self.recovery.take() {
            self.recover_dpb();
        }
        // Cleared BEFORE planning for the same reason the RASL arm below clears it:
        // "cleared by the next decode" must hold for an AU that fails to plan at
        // all, or the previous AU's warnings could be re-read as fresh damage. The
        // ledger is drained after every successful decode (`take_warnings` is a
        // `mem::take`), so the failed-plan case is the only one that could carry
        // over — a hole closed by construction, not a fix for a field symptom.
        self.last_warnings.clear();
        let plan = match self.planner.plan_au(au) {
            Ok(plan) => plan,
            Err(PlanError::RaslSkipped { poc }) => {
                trace!(poc, "RASL picture after a CRA join — skipped, not failed");
                return Ok(self.ready.pop_front());
            }
            Err(e) => return Err(VkDecodeError::PlanH265(e)),
        };
        for warning in &plan.warnings {
            // The recovery verdict is the integration layer's
            // ([`Self::take_warnings`]); never silent here though.
            trace!(?warning, "plan warning");
        }
        self.last_warnings = plan.warnings.clone();
        // One picture per AU under this envelope: stamp its DECODE-order ordinal
        // before anything can reorder it (see `DecodedVkFrame::decode_order`).
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;
        // The recovery-point watch, folded ONCE per successfully planned AU and in
        // DECODE order — the order the SEI's POC delta is measured in. The mark
        // rides the pending picture into display order (crate::recovery).
        let recovery = self.recovery_watch.note_h265(
            plan.picture.pic_order_cnt,
            plan.picture.is_irap,
            plan.picture.recovery_point,
        );
        if recovery != crate::recovery::RecoveryMark::NONE {
            trace!(
                sei = recovery.sei_here,
                recovery_point = recovery.is_recovery_point,
                poc = plan.picture.pic_order_cnt,
                "recovery point SEI"
            );
        }

        // From here the PLANNER has already advanced past this AU — its DPB holds
        // the picture whatever happens next — so any failure below leaves the
        // planner's DPB and this decoder's slot/image ledgers able to disagree.
        // Latch the recovery for the next decode rather than returning into a
        // permanently wedged state. (Deliberately wider than the paths that
        // mutate the SlotMap: a failure BEFORE `plan_to_vk_h265` mutates it —
        // `UnresolvedReference`, an `ensure_state` refusal — strands the picture
        // the other way round, planner-resident with no slot at all, and wedges
        // just as hard. One flush cures both.)
        let result = self.decode_planned(&plan, au, recovery, decode_order);
        if result.is_err() {
            self.recovery.latch();
        }
        result
    }

    /// The submission half of one decode, from the point the planner has already
    /// advanced. Split out so [`Self::decode_inner`] can latch recovery on ANY
    /// failure past that line without threading a flag through every exit.
    /// `au` is the same buffer `plan`'s slice ranges index into; `recovery` is the
    /// recovery-point verdict already folded for this AU and `decode_order` its
    /// decode-order ordinal (both advance in decode order, so neither can be
    /// derived here — this path is not reached for every planned AU).
    fn decode_planned(
        &mut self,
        plan: &AuPlan,
        au: &[u8],
        recovery: crate::recovery::RecoveryMark,
        decode_order: u64,
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        self.ensure_state(plan)?;

        // The VPS this SPS activates: the stream's own, or the fallback identity
        // for a stream joined after its VPS NALU (session_h265 module docs).
        let vps = VpsSource::for_sps(&plan.sps);

        // Convert, with ONE rebuild retry on CapacityMismatch — the designed
        // trigger for a DPB-depth renegotiation (pic_h265.rs docs).
        let mut vk_plan: Option<DecodePlanVkH265> = None;
        for attempt in 0..2 {
            // A parameters RECREATE destroys the old object, which an in-flight
            // decode may still be executing against: drain first.
            if self
                .state
                .as_ref()
                .expect("ensure_state built it")
                .session
                .parameters_action(&vps, &plan.sps, &plan.pps)
                == ParamsActionH265::Recreate
            {
                self.drain_gpu()?;
            }
            let state = self.state.as_mut().expect("ensure_state built it");
            // SAFETY: live device (constructor contract); the drain above
            // satisfies ensure_parameters' Recreate contract, and Current/Add
            // touch nothing a submitted decode reads.
            unsafe {
                state
                    .session
                    .ensure_parameters(&vps, &plan.sps, &plan.pps)?
            };
            match plan_to_vk_h265(plan, &mut state.slots) {
                Ok(converted) => {
                    vk_plan = Some(converted);
                    break;
                }
                Err(PlanToVkH265Error::CapacityMismatch { required, capacity }) if attempt == 0 => {
                    debug!(
                        required,
                        capacity, "DPB depth renegotiated — rebuilding session"
                    );
                    self.rebuild_state(plan)?;
                }
                Err(e) => return Err(VkDecodeError::ConvertH265(e)),
            }
        }
        let vk_plan = vk_plan.expect("the rebuilt session matches its own plan");

        let state = self.state.as_mut().expect("ensured above");
        // The per-AU active-reference gate: the session was created with
        // maxActiveReferencePictures; binding more in one decode op would be a
        // silent VUID violation on the drivers that matter most.
        let max_active = state.session.config.max_active_references as usize;
        if vk_plan.refs.len() > max_active {
            return Err(VkDecodeError::Unsupported(format!(
                "AU references {} pictures, session allows {max_active} active references",
                vk_plan.refs.len()
            )));
        }

        // Coincide binding sync: slots the planner released no longer bind their
        // images (the pictures may still be pending/held — untouched), and the
        // setup slot's PREVIOUS binding is cleared before it binds fresh.
        let setup = usize::from(vk_plan.setup_slot);
        if state.dpb.is_none() {
            let mut held = vec![false; state.slot_image.len()];
            for (slot, _id) in state.slots.held() {
                held[usize::from(slot)] = true;
            }
            for (slot, binding) in state.slot_image.iter_mut().enumerate() {
                if let Some(picture) = *binding {
                    if !held[slot] || slot == setup {
                        state.pool.pictures[picture].bound = false;
                        *binding = None;
                    }
                }
            }
        }

        // The decode target: a FREE pool image (never one a consumer holds — the
        // whole point of the pool model).
        let Some(dst) = state.pool.free_index() else {
            debug!(
                held = state.pool.held_total(),
                "picture pool exhausted — release_frame owed"
            );
            return Err(VkDecodeError::NoFreeSlot);
        };

        // Cross-queue waits (the AVVkFrame contract): the dst image's last known
        // timeline value (covers a presenter write-back after release), plus —
        // coincide mode — every referenced image's value, so reference reads
        // order after any presenter layout restore already reported back.
        let mut waits: Vec<(vk::Semaphore, u64)> = Vec::new();
        {
            let dst_pic = &state.pool.pictures[dst];
            if dst_pic.value > 0 {
                waits.push((dst_pic.semaphore, dst_pic.value));
            }
        }
        if state.dpb.is_none() {
            for r in &vk_plan.refs {
                if let Some(picture) = state.slot_image[usize::from(r.slot)] {
                    let pic = &state.pool.pictures[picture];
                    if pic.value > 0 && !waits.iter().any(|(sem, _)| *sem == pic.semaphore) {
                        waits.push((pic.semaphore, pic.value));
                    }
                }
            }
        }
        let signal_value = state.pool.pictures[dst].value + 1;

        // Command buffer + query slot for this submission.
        let submission = state.submitted;
        let cmd_index = (submission % state.ops.cmds.len() as u64) as usize;
        if let Some((sem, value)) = state.cmd_marks[cmd_index] {
            // SAFETY: live device; the token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "command buffer reuse")? };
        }
        let query_index = (submission % u64::from(state.ops.query_count)) as u32;

        // Upload the AU (recycles/grows against submission-completion tokens).
        let device = self.dev.ash().clone();
        let mut poll = |token: &(vk::Semaphore, u64)| -> Result<bool, VkDecodeError> {
            // SAFETY: live device; the token's semaphore is a pool semaphore.
            let current = unsafe { device.get_semaphore_counter_value(token.0) }
                .map_err(VkDecodeError::from)?;
            Ok(current >= token.1)
        };
        let device2 = self.dev.ash().clone();
        let mut wait = |token: &(vk::Semaphore, u64)| -> Result<(), VkDecodeError> {
            // SAFETY: as above.
            unsafe { wait_timeline(&device2, token.0, token.1, "bitstream slot drain") }
        };
        // SLICE SEGMENT NALUs only, concatenated — an HEVC AU opens with AUD/SEI
        // (and, at IRAPs, VPS/SPS/PPS) NALUs, and feeding those to the VCN
        // firmware inside the decode range HANGS it (the .25 `vcn_unified_0 ring
        // timeout` the H.264 path was built around; the parameter sets ride the
        // session parameters object instead). The plan's AU-relative offsets are
        // rebased into the packed buffer.
        let segments: Vec<std::ops::Range<usize>> =
            plan.slices.iter().map(|s| s.data.clone()).collect();
        // One rebased offset per slice segment, in plan order — the same count and
        // order as `vk_plan.slice_offsets` (both are built by walking
        // `plan.slices`), so `pSliceSegmentOffsets` and `sliceSegmentCount` agree
        // by construction rather than by check.
        let Some(slice_offsets) = rebased_offsets(&segments) else {
            return Err(VkDecodeError::Unsupported(
                "packed slice data exceeds the u32 offsets Vulkan submits".into(),
            ));
        };
        // SAFETY: live device; the segments are the plan's own in-bounds slice
        // ranges; every pending token is the completion signal of the submission
        // that consumed the slot.
        let upload = unsafe {
            state
                .ring
                .upload(&self.dev, au, &segments, &mut poll, &mut wait)?
        };

        // Record + submit, signalling the dst image's next timeline value.
        // SAFETY: live device; every handle recorded below belongs to this
        // session generation, and the packed slices sit uploaded in the ring slot.
        unsafe {
            record_and_submit_h265(
                &self.dev,
                &*self.lock,
                state,
                &vk_plan,
                &slice_offsets,
                &upload,
                dst,
                cmd_index,
                query_index,
                &waits,
                signal_value,
            )?;
        }

        // Post-submit bookkeeping.
        let dst_sem = state.pool.pictures[dst].semaphore;
        state.pool.pictures[dst].value = signal_value;
        state.pool.pictures[dst].pending = true;
        if state.dpb.is_none() {
            state.pool.pictures[dst].bound = true;
            state.slot_image[setup] = Some(dst);
        }
        state.cmd_marks[cmd_index] = Some((dst_sem, signal_value));
        state.query_marks[query_index as usize] = submission;
        state.submitted += 1;
        state.last_submit = Some((dst_sem, signal_value));
        state
            .ring
            .pending
            .set_pending(upload.slot, (dst_sem, signal_value));

        // Refresh the per-slot reference cache from this AU's facts.
        state.slot_refs[setup] = Some(vk_plan.setup_ref);
        for r in &vk_plan.refs {
            state.slot_refs[usize::from(r.slot)] = Some(r.std);
        }

        self.pending.insert(
            vk_plan.setup_id,
            PendingPic {
                image: dst,
                submission,
                query_slot: query_index,
                timeline_value: signal_value,
                crop: plan.picture.display_crop,
                colour: plan.picture.colour,
                poc: plan.picture.pic_order_cnt,
                is_idr: plan.picture.is_idr,
                recovery,
                decode_order,
            },
        );

        // The plan's DPB verdicts over the pending map: outputs become ready
        // frames (their images move pending → held until released);
        // removed-but-never-output pictures free their images.
        let (ready, dropped) = settle_dpb(&mut self.pending, &plan.dpb);
        let state = self.state.as_mut().expect("ensured above");
        for entry in ready {
            let frame = build_frame(
                &mut state.pool,
                state.dpb.is_none(),
                state.image_extent,
                &entry,
                self.generation,
            );
            self.ready.push_back(frame);
        }
        for entry in dropped {
            debug!(
                poc = entry.poc,
                "picture removed without output — freeing its image"
            );
            state.pool.pictures[entry.image].pending = false;
        }
        Ok(self.ready.pop_front())
    }

    /// Hand a delivered frame back. `presenter_signaled` reports whether the
    /// consumer SAMPLED the image (and therefore enqueued the `value + 1`
    /// timeline signal per the [`DecodedVkFrame`] contract) — the decoder then
    /// waits that write-back before the image's next use. Every frame
    /// `decode`/`take_ready` returns must come back exactly once, including
    /// stale-generation frames (their retired pool dies on its last token).
    pub fn release_frame(
        &mut self,
        frame: &DecodedVkFrame,
        presenter_signaled: bool,
    ) -> Result<(), VkDecodeError> {
        let pool = if frame.generation == self.generation {
            match &mut self.state {
                Some(state) => &mut state.pool,
                None => {
                    return Err(VkDecodeError::StaleFrame {
                        frame_generation: frame.generation,
                        current_generation: self.generation,
                    })
                }
            }
        } else {
            match self
                .graveyard
                .iter_mut()
                .find(|r| r.generation == frame.generation)
            {
                Some(retired) => &mut retired.pool,
                None => {
                    return Err(VkDecodeError::StaleFrame {
                        frame_generation: frame.generation,
                        current_generation: self.generation,
                    })
                }
            }
        };
        let index = frame.picture as usize;
        if index >= pool.pictures.len() {
            return Err(VkDecodeError::StaleFrame {
                frame_generation: frame.generation,
                current_generation: self.generation,
            });
        }
        let picture = &mut pool.pictures[index];
        match picture.held.checked_sub(1) {
            Some(remaining) => picture.held = remaining,
            None => {
                debug!(index, "frame released more often than delivered");
                return Ok(());
            }
        }
        if presenter_signaled {
            picture.value = picture.value.max(frame.value + 1);
        }
        // A retired pool dies on its last token (presenter fence-waited before
        // the token per the release contract; decode work drained at retirement).
        if frame.generation != self.generation {
            self.graveyard
                .retain(|r| r.generation != frame.generation || r.pool.held_total() > 0);
        }
        Ok(())
    }

    /// A display-ready frame beyond the one `decode` returned, if any. Drain after
    /// every decode; frames left here still occupy pool images.
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// The warnings of the most recent successfully planned AU (concealment
    /// signals — the integration layer's want_keyframe hook). Cleared by the
    /// next `decode`.
    pub fn take_warnings(&mut self) -> Vec<PlanWarning> {
        std::mem::take(&mut self.last_warnings)
    }

    /// The current session generation ([`DecodedVkFrame::generation`] of newly
    /// delivered frames).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The DECODE-order ordinal of the most recently planned picture — the
    /// watermark a consumer compares [`DecodedVkFrame::decode_order`] against to
    /// tell a frame decoded before a loss from one decoded after it. Especially
    /// load-bearing here: [`Self::recover_dpb`] flushes every buffered picture
    /// into `ready` at once, so a pre-loss picture routinely reaches the consumer
    /// after the loss that flushed it. 0 before the first AU plans.
    pub fn decode_order(&self) -> u64 {
        self.decoded
    }

    /// One-line state snapshot for failure paths and field logs (not a stable
    /// format).
    pub fn debug_snapshot(&self) -> String {
        // A latched recovery is the single most useful thing to see next to a
        // failure: it says the NEXT decode flushes to an IRAP rather than
        // resuming (RecoveryLatch docs).
        let recovery = if self.recovery.is_latched() {
            " recovery=owed"
        } else {
            ""
        };
        match &self.state {
            None => format!("gen={}{recovery} <no session>", self.generation),
            Some(state) => {
                let occupancy: Vec<String> = state
                    .pool
                    .pictures
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        format!(
                            "{i}:{}{}h{}",
                            if p.bound { "B" } else { "-" },
                            if p.pending { "P" } else { "-" },
                            p.held
                        )
                    })
                    .collect();
                format!(
                    "h265 gen={}{recovery} mode={} slots_held={}/{} pool=[{}] pending={} \
                     ready={} graveyard={}",
                    self.generation,
                    if state.dpb.is_none() {
                        "coincide"
                    } else {
                        "distinct"
                    },
                    state.slots.active(),
                    state.slots.capacity(),
                    occupancy.join(" "),
                    self.pending.len(),
                    self.ready.len(),
                    self.graveyard.len(),
                )
            }
        }
    }

    /// Read `frame`'s decode status WITHOUT waiting.
    ///
    /// [`DecodeStatus::Failed`] covers driver-reported errors AND a query slot
    /// re-armed before it was read (the status is then unprovable — same
    /// conservative verdict).
    ///
    /// On drivers whose decode family lacks `queryResultStatusSupport` (RADV)
    /// there is no per-op verdict to read: `Ok` then means "the decode op
    /// COMPLETED on the timeline" — the same information FFmpeg has on every
    /// driver, no worse.
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    /// Does this decode queue family answer per-op `RESULT_STATUS` queries at all?
    /// See [`crate::VkH264Decoder::status_queries`] — the fact is the DEVICE's, identical
    /// for both codecs, and it is what tells a clean integrity report apart from
    /// an undetectable one.
    pub fn status_queries(&self) -> bool {
        self.dev.result_status_queries()
    }

    /// [`Self::poll_status`], but WAITs for the op to complete first.
    pub fn wait_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, true)
    }

    fn read_status(&mut self, frame: &DecodedVkFrame, block: bool) -> DecodeStatus {
        if frame.generation != self.generation {
            trace!(
                frame_generation = frame.generation,
                current = self.generation,
                "status asked for a stale-generation frame — Failed, without \
                 touching the new pools"
            );
            return DecodeStatus::Failed;
        }
        let Some(state) = &self.state else {
            return DecodeStatus::Failed;
        };
        let Some(query_pool) = state.ops.query_pool else {
            // No queries on this driver: the verdict degrades to timeline
            // completion (poll_status docs).
            if block {
                // SAFETY: live device; pool-owned semaphore.
                return match unsafe {
                    wait_timeline(self.dev.ash(), frame.semaphore, frame.value, "status wait")
                } {
                    Ok(()) => DecodeStatus::Ok,
                    Err(VkDecodeError::DeviceLost) => {
                        self.device_lost = true;
                        DecodeStatus::Failed
                    }
                    Err(_) => DecodeStatus::Failed,
                };
            }
            // SAFETY: live device; pool-owned semaphore.
            return match unsafe { self.dev.ash().get_semaphore_counter_value(frame.semaphore) } {
                Ok(current) if current >= frame.value => DecodeStatus::Ok,
                Ok(_) => DecodeStatus::Pending,
                Err(vk::Result::ERROR_DEVICE_LOST) => {
                    self.device_lost = true;
                    DecodeStatus::Failed
                }
                Err(_) => DecodeStatus::Failed,
            };
        };
        let slot = frame.query_slot as usize;
        if slot >= state.query_marks.len() || state.query_marks[slot] != frame.submission {
            trace!(
                slot,
                "status query slot re-armed before it was read — unprovable, reported Failed"
            );
            return DecodeStatus::Failed;
        }
        let flags = if block {
            vk::QueryResultFlags::WAIT | vk::QueryResultFlags::WITH_STATUS_KHR
        } else {
            vk::QueryResultFlags::WITH_STATUS_KHR
        };
        let mut status = [0i32; 1];
        // SAFETY: live device; the query pool is this session generation's own and
        // `frame.query_slot` indexes within its count (checked above against the
        // marks array it is sized to).
        let result = unsafe {
            self.dev
                .ash()
                .get_query_pool_results(query_pool, frame.query_slot, &mut status, flags)
        };
        match result {
            // VkQueryResultStatusKHR: >0 complete, 0 not ready, <0 error.
            Ok(()) if status[0] > 0 => DecodeStatus::Ok,
            Ok(()) if status[0] == 0 => DecodeStatus::Pending,
            Ok(()) => DecodeStatus::Failed,
            Err(vk::Result::NOT_READY) => DecodeStatus::Pending,
            Err(vk::Result::ERROR_DEVICE_LOST) => {
                self.device_lost = true;
                DecodeStatus::Failed
            }
            Err(r) => {
                debug!(?r, "status query read failed");
                DecodeStatus::Failed
            }
        }
    }

    /// Wait — bounded by `timeout_ns` — for a delivered frame's decode-complete
    /// signal. Pure measurement (the integration layer's sampled decode-latency
    /// stat): touches no decoder state. `frame` must be unreleased, which pins its
    /// pool — and with it the semaphore — alive.
    pub fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        if frame.generation != self.generation {
            return false;
        }
        let semaphores = [frame.semaphore];
        let values = [frame.value];
        let info = vk::SemaphoreWaitInfo::default()
            .semaphores(&semaphores)
            .values(&values);
        // SAFETY: live device (constructor contract); the semaphore is a pool
        // semaphore the unreleased frame keeps alive (fn docs); the info arrays
        // are locals outliving the call.
        unsafe { self.dev.ash().wait_semaphores(&info, timeout_ns) }.is_ok()
    }

    /// Drain the planner (teardown / stream discontinuity): every buffered
    /// picture becomes display-ready via [`Self::take_ready`] (zero-copy — the
    /// images already hold the content), all DPB slots free, and any picture
    /// removed without ever reaching output frees its image.
    pub fn flush(&mut self) {
        let update = self.planner.flush();
        let (ready, dropped) = settle_dpb(&mut self.pending, &update);
        if let Some(state) = &mut self.state {
            state.slots.apply(&update);
            for entry in ready {
                let frame = build_frame(
                    &mut state.pool,
                    state.dpb.is_none(),
                    state.image_extent,
                    &entry,
                    self.generation,
                );
                self.ready.push_back(frame);
            }
            for entry in dropped {
                state.pool.pictures[entry.image].pending = false;
            }
            // Defensive: a pending picture neither output nor removed should not
            // exist after a flush; free any leftover.
            for (_, entry) in std::mem::take(&mut self.pending) {
                debug!(poc = entry.poc, "pending picture survived a flush — freed");
                state.pool.pictures[entry.image].pending = false;
            }
        } else {
            self.pending.clear();
        }
    }

    /// Clear the DPB state a failed AU left behind, so planning resumes at the
    /// next IRAP instead of erroring on residency nothing can honour.
    ///
    /// Three ledgers have to agree and, after a post-planning failure, do not:
    /// the PLANNER's DPB, this decoder's [`SlotMap`], and the slot→image
    /// bindings. [`Self::flush`] settles the first (and hands back any picture
    /// that did reach output — those frames are real and are still delivered),
    /// then [`reset_slot_bindings`] empties the other two. Pool images the stale
    /// bindings pinned go back on the free list; images a consumer still HOLDS
    /// stay pinned by their own `held` counts, exactly as they would across a
    /// session rebuild.
    ///
    /// Deliberately not a session rebuild: the session, pools and ring are all
    /// still valid — only the DPB bookkeeping is stale — and a rebuild would
    /// churn every image allocation for a condition an IDR fixes anyway.
    fn recover_dpb(&mut self) {
        debug!(
            snapshot = %self.debug_snapshot(),
            "recovering from a failed AU — flushing the H.265 DPB to the next IRAP"
        );
        self.flush();
        if let Some(state) = &mut self.state {
            let unbound = reset_slot_bindings(
                &mut state.slots,
                &mut state.slot_image,
                &mut state.slot_refs,
            );
            for picture in unbound {
                state.pool.pictures[picture].bound = false;
            }
        }
    }

    /// Session/caps for THIS plan exist and match its extent + profile, and the
    /// stream sits inside the device's level ceiling. DPB-depth mismatches
    /// surface later as `plan_to_vk_h265`'s `CapacityMismatch` (the designed
    /// trigger) and take the same rebuild path.
    fn ensure_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        let key = profile_key_for(plan)?;
        if self.caps.as_ref().map(|(k, _)| *k) != Some(key) {
            let wanted = key
                .output_format()
                .expect("from_stream gated the chroma/depth combination");
            // SAFETY: live device (constructor contract).
            let raw = unsafe { query_h265_caps(&self.dev, key) }.map_err(VkDecodeError::from)?;
            self.caps = Some((key, derive_caps_h265(&raw, wanted)?));
        }
        // The level gate: a stream above the device's maxLevelIdc is refused up
        // front (within one codec the Std code points ascend with the level, so
        // the comparison is numeric), never submitted on a hope. The ceiling came
        // from an H.265 caps query, so it is compared against an H.265 code point
        // — the pairing MaxLevelIdc's tag exists to keep honest.
        let caps_max_level = self.caps.as_ref().expect("queried above").1.max_level_idc;
        let stream_level = level_to_std_h265(plan.picture.level_idc);
        if stream_level > caps_max_level.code_point() {
            return Err(VkDecodeError::Unsupported(format!(
                "stream level (Std code point {stream_level}) above the device's \
                 maxLevelIdc ({caps_max_level})"
            )));
        }
        let coded = vk::Extent2D {
            width: plan.picture.coded_width,
            height: plan.picture.coded_height,
        };
        match &self.state {
            Some(state) if state.coded_extent == coded && state.session.config.profile == key => {
                Ok(())
            }
            _ => self.rebuild_state(plan),
        }
    }

    /// Tear down the current session generation (draining its decode work,
    /// retiring its picture pool to the graveyard when the consumer still holds
    /// images) and build a fresh one shaped by `plan`, bumping
    /// [`Self::generation`] so frames of the old one route to the graveyard.
    ///
    /// The renegotiation-safety argument is [`crate::VkH264Decoder`]'s, unchanged:
    /// pools with consumer holds retire INTACT to the graveyard, every frame and
    /// token carries its generation, and the session objects (query pool included)
    /// die only after [`Self::drain_gpu`] with no consumer-facing handle pointing
    /// at them.
    fn rebuild_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        self.drain_gpu()?;
        if let Some(state) = self.state.take() {
            debug!("rebuilding H.265 decode session (stream renegotiation)");
            let SessionStateH265 { mut pool, .. } = state;
            for frame in self.ready.drain(..) {
                let picture = &mut pool.pictures[frame.picture as usize];
                picture.held = picture.held.saturating_sub(1);
            }
            for (_, entry) in std::mem::take(&mut self.pending) {
                pool.pictures[entry.image].pending = false;
            }
            for picture in &mut pool.pictures {
                picture.bound = false;
            }
            let held = pool.held_total();
            if held > 0 {
                debug!(
                    held,
                    generation = self.generation,
                    "consumer still holds images of the retired generation — graveyarding"
                );
                self.graveyard.push(RetiredPool {
                    generation: self.generation,
                    pool,
                });
            }
        }
        self.generation += 1;

        let (key, caps) = self.caps.as_ref().expect("ensure_state queried caps");
        let key = *key;
        let required_slots = plan.picture.max_dpb_frames as u32 + 1;
        if required_slots > caps.max_dpb_slots {
            return Err(VkDecodeError::Unsupported(format!(
                "stream needs {required_slots} DPB slots, device caps at {}",
                caps.max_dpb_slots
            )));
        }
        let coded = vk::Extent2D {
            width: plan.picture.coded_width,
            height: plan.picture.coded_height,
        };
        // Bounds-checked at the ALLOCATION extent (granularity-rounded): that is
        // what the images are created at and what maxCodedExtent must cover.
        let image_extent = caps.aligned_extent(coded);
        if coded.width < caps.min_coded_extent.width
            || coded.height < caps.min_coded_extent.height
            || image_extent.width > caps.max_coded_extent.width
            || image_extent.height > caps.max_coded_extent.height
        {
            return Err(VkDecodeError::Unsupported(format!(
                "coded extent {}x{} (allocated {}x{}) outside device range {}x{}..{}x{}",
                coded.width,
                coded.height,
                image_extent.width,
                image_extent.height,
                caps.min_coded_extent.width,
                caps.min_coded_extent.height,
                caps.max_coded_extent.width,
                caps.max_coded_extent.height
            )));
        }

        let config = SessionConfigH265 {
            max_coded_extent: image_extent,
            max_dpb_slots: required_slots,
            max_active_references: (required_slots - 1).min(caps.max_active_references),
            profile: key,
        };
        let mut pool_plan = plan_pools(caps, required_slots);
        // TEST-ONLY readback hook, exactly as the H.264 decoder's: the parity
        // test copies decoded pictures back to hash them, and
        // `vkCmdCopyImageToBuffer` needs TRANSFER_SRC on the source — a bit the
        // zero-copy production pools deliberately do not carry.
        if std::env::var("PF_VKD_TEST_READBACK").is_ok_and(|v| v == "1") {
            pool_plan.picture_usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let decode_profile = DecodeProfile::H265(key);
        // SAFETY: live device per the constructor contract, for every create in
        // this block; each created half is owned by a Drop type the moment it
        // exists, so a mid-build failure unwinds cleanly.
        let state = unsafe {
            let session = VideoSessionH265::create(&self.dev, caps, config)?;
            let dpb = if caps.coincide {
                None
            } else {
                Some(
                    DpbPool::create(&self.dev, caps, &pool_plan, image_extent, decode_profile)
                        .map_err(VkDecodeError::from)?,
                )
            };
            let pool =
                PicturePool::create(&self.dev, caps, &pool_plan, image_extent, decode_profile)
                    .map_err(VkDecodeError::from)?;
            let ring = BitstreamRing::create(
                &self.dev,
                RingLayout::new(
                    INITIAL_SLOT_SIZE,
                    RING_SLOTS,
                    caps.min_bitstream_offset_alignment,
                    caps.min_bitstream_size_alignment,
                ),
                decode_profile,
            )
            .map_err(VkDecodeError::from)?;
            let ops = OpRing::create(
                &self.dev,
                decode_profile,
                pool_plan.picture_count,
                RING_SLOTS,
            )
            .map_err(VkDecodeError::from)?;
            SessionStateH265 {
                session,
                slots: SlotMap::new(plan.picture.max_dpb_frames),
                slot_refs: vec![None; required_slots as usize],
                slot_image: vec![None; required_slots as usize],
                cmd_marks: vec![None; RING_SLOTS as usize],
                query_marks: vec![u64::MAX; pool_plan.picture_count as usize],
                submitted: 0,
                last_submit: None,
                coded_extent: coded,
                image_extent,
                dpb,
                pool,
                ring,
                ops,
            }
        };
        self.state = Some(state);
        Ok(())
    }

    /// Wait out every in-flight decode submission of the current session.
    fn drain_gpu(&mut self) -> Result<(), VkDecodeError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        if let Some((sem, value)) = state.last_submit {
            // SAFETY: live device; the token is a pool image's semaphore.
            unsafe { wait_timeline(self.dev.ash(), sem, value, "session drain")? };
        }
        Ok(())
    }
}

impl Drop for VkH265Decoder {
    fn drop(&mut self) {
        // Best-effort decode drain so the pools' Drop impls never destroy
        // in-flight decode work; a wedged driver falls through after the bounded
        // timeout. Presenter-side sampling of graveyarded/held images is the
        // CALLER's teardown contract (the H.264 decoder's Drop docs).
        if let Err(e) = self.drain_gpu() {
            debug!(error = %e, "drain on drop failed; tearing down anyway");
        }
        if !self.graveyard.is_empty() {
            debug!(
                pools = self.graveyard.len(),
                "graveyard not fully token-drained at decoder drop — destroying anyway \
                 (upstream teardown forfeited its bounded wait)"
            );
        }
    }
}

/// The Vulkan profile this AU's stream needs — profile idc plus the chroma format
/// and bit depths, all of which the SPS carries and the profile must restate.
/// (`separate_colour_plane_flag` comes off the active SPS rather than the picture
/// plan, which does not carry it: the planner has no use for it, the profile gate
/// does.)
fn profile_key_for(plan: &AuPlan) -> Result<H265ProfileKey, VkDecodeError> {
    H265ProfileKey::from_stream(
        plan.picture.general_profile_idc,
        plan.picture.chroma_format_idc,
        plan.sps.separate_colour_plane_flag,
        plan.picture.bit_depth_luma_minus8,
        plan.picture.bit_depth_chroma_minus8,
    )
    .map_err(VkDecodeError::ParamsH265)
}

/// Empty the three per-slot ledgers a recovery resets: DPB residency, the
/// slot→image bindings and the cached per-slot reference info. Returns the pool
/// image indices the cleared bindings were pinning, for the caller to unbind
/// (pure over the ledgers so the recovery is testable without a device — the pool
/// is the one piece that needs one).
///
/// All three are emptied TOGETHER on purpose: leaving reference info behind would
/// let [`build_scope`] bind a slot the planner no longer knows about, which is the
/// same "plausible-looking picture in the wrong place" the unbound-reference
/// refusal exists to prevent.
fn reset_slot_bindings(
    slots: &mut SlotMap,
    slot_image: &mut [Option<usize>],
    slot_refs: &mut [Option<hh::StdVideoDecodeH265ReferenceInfo>],
) -> Vec<usize> {
    // `release` is the only way a slot is freed (SlotMap docs); the collect is
    // because `held` borrows the map the releases mutate.
    for (_slot, id) in slots.held().collect::<Vec<_>>() {
        slots.release(id);
    }
    let unbound = slot_image.iter_mut().filter_map(Option::take).collect();
    for cached in slot_refs.iter_mut() {
        *cached = None;
    }
    unbound
}

/// The picture resource view bound for DPB `slot`: the bound pool image
/// (coincide) or the DPB array layer (distinct).
fn slot_view(state: &SessionStateH265, slot: u8) -> Option<vk::ImageView> {
    match &state.dpb {
        Some(dpb) => Some(dpb.dpb_view(slot)),
        None => state.slot_image[usize::from(slot)].map(|p| state.pool.pictures[p].view),
    }
}

/// One entry of a coding scope's bound-slot list: the DPB slot index it binds
/// (`-1` for the setup ACTIVATION entry), the picture resource view, and the
/// codec reference info that slot's association carries.
/// (No derived equality: `StdVideoDecodeH265ReferenceInfo` is a plain-C bindgen
/// struct without it. Assertions compare the fields that carry meaning.)
#[derive(Debug, Clone, Copy)]
struct ScopeEntry {
    slot_index: i32,
    view: vk::ImageView,
    std: hh::StdVideoDecodeH265ReferenceInfo,
}

/// Build the coding scope's bound-slot list and say how many leading entries are
/// THIS AU's references.
///
/// The ordering is the whole point, and it is a Vulkan contract rather than a
/// preference: `StdVideoDecodeH265PictureInfo`'s `RefPicSetStCurrBefore`,
/// `RefPicSetStCurrAfter` and `RefPicSetLtCurr` hold indices into the decode op's
/// `pReferenceSlots` array, and [`crate::pic_h265`] built those indices against
/// [`DecodePlanVkH265::refs`]. So:
///
/// 1. every entry of `refs`, IN ORDER — the decode op takes exactly this prefix;
/// 2. every other still-held slot, so its association survives the scope (their
///    resources must stay bound even when this AU does not reference them);
/// 3. the setup slot as the activation entry, slot index `-1`.
///
/// A reference whose slot binds no image is a hard error, never a skip: compacting
/// the array would shift every later index onto the wrong picture and produce
/// output that looks plausible and is wrong.
fn build_scope(
    refs: &[crate::pic_h265::VkRefH265],
    held_slots: impl Iterator<Item = u8>,
    setup_slot: u8,
    setup_view: vk::ImageView,
    setup_ref: hh::StdVideoDecodeH265ReferenceInfo,
    slot_refs: &[Option<hh::StdVideoDecodeH265ReferenceInfo>],
    view_of: impl Fn(u8) -> Option<vk::ImageView>,
) -> Result<(Vec<ScopeEntry>, usize), VkDecodeError> {
    let mut scope: Vec<ScopeEntry> = Vec::with_capacity(refs.len() + slot_refs.len() + 1);
    for r in refs {
        match view_of(r.slot) {
            Some(view) => scope.push(ScopeEntry {
                slot_index: i32::from(r.slot),
                view,
                std: r.std,
            }),
            None => return Err(VkDecodeError::UnboundReferenceSlot { slot: r.slot }),
        }
    }
    let reference_count = scope.len();
    for slot in held_slots {
        if slot == setup_slot || refs.iter().any(|r| r.slot == slot) {
            continue;
        }
        match (
            slot_refs.get(usize::from(slot)).copied().flatten(),
            view_of(slot),
        ) {
            (Some(std), Some(view)) => scope.push(ScopeEntry {
                slot_index: i32::from(slot),
                view,
                std,
            }),
            // Unreachable in practice: every held slot was a setup slot once.
            _ => trace!(
                slot,
                "held slot without reference info/binding — left unbound"
            ),
        }
    }
    scope.push(ScopeEntry {
        slot_index: -1,
        view: setup_view,
        std: setup_ref,
    });
    Ok((scope, reference_count))
}

/// Record one H.265 decode op into the chosen command buffer and submit it under
/// the queue lock: image waits per the pool contract, the dst image's timeline
/// signal at `signal_value`.
///
/// # Safety
///
/// Live device; `state` is the current session generation with `vk_plan` derived
/// against its `SlotMap`, `dst` a free pool image, the AU resident in `upload`'s
/// ring slot, and the command buffer's previous submission completed (caller
/// waited its mark).
#[allow(clippy::too_many_arguments)]
unsafe fn record_and_submit_h265(
    dev: &DecodeDevice,
    lock: &dyn QueueLock,
    state: &mut SessionStateH265,
    vk_plan: &DecodePlanVkH265,
    slice_offsets: &[u32],
    upload: &UploadedAu,
    dst: usize,
    cmd_index: usize,
    query_index: u32,
    waits: &[(vk::Semaphore, u64)],
    signal_value: u64,
) -> Result<(), VkDecodeError> {
    let device = dev.ash();
    let cmd = state.ops.cmds[cmd_index];
    let coded_extent = state.coded_extent;
    let coincide = state.dpb.is_none();

    // ---- the reference layout, decided BEFORE anything is recorded ----
    // `refs` order is the contract (build_scope docs): the Std picture info's RPS
    // arrays index into this exact array, so a missing entry is fatal, not
    // skippable — and it must fail before the command buffer is even begun.
    let setup_view = if coincide {
        state.pool.pictures[dst].view
    } else {
        state
            .dpb
            .as_ref()
            .expect("distinct mode")
            .dpb_view(vk_plan.setup_slot)
    };
    let held_slots: Vec<u8> = state.slots.held().map(|(slot, _id)| slot).collect();
    let (scope, reference_count) = build_scope(
        &vk_plan.refs,
        held_slots.into_iter(),
        vk_plan.setup_slot,
        setup_view,
        vk_plan.setup_ref,
        &state.slot_refs,
        |slot| slot_view(state, slot),
    )?;

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the buffer's previous submission completed (fn contract) and its
    // pool allows per-buffer reset, so begin implicitly resets it.
    unsafe {
        device
            .begin_command_buffer(cmd, &begin_info)
            .map_err(VkDecodeError::from)?
    };

    // ---- barriers (outside the video coding scope) ----
    // Prior reconstructions must be visible to this op's reference reads.
    let memory_barriers = [vk::MemoryBarrier2::default()
        .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        .src_access_mask(vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR)
        .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        .dst_access_mask(
            vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
        )];
    // Decode targets are fully overwritten: discard via UNDEFINED with an
    // execution+memory dependency on earlier ops that touched them.
    let decode_layer_barrier = |image: vk::Image, layer: u32, new_layout: vk::ImageLayout| {
        vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .src_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            )
            .dst_stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
            .dst_access_mask(
                vk::AccessFlags2::VIDEO_DECODE_READ_KHR | vk::AccessFlags2::VIDEO_DECODE_WRITE_KHR,
            )
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(new_layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: layer,
                layer_count: 1,
            })
    };
    let dst_image = state.pool.pictures[dst].image;
    let mut image_barriers = Vec::new();
    if coincide {
        // The dst pool image IS the setup DPB picture.
        image_barriers.push(decode_layer_barrier(
            dst_image,
            0,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        ));
    } else {
        let dpb = state.dpb.as_ref().expect("distinct mode");
        let (setup_image, setup_layer) = dpb.dpb_target(vk_plan.setup_slot);
        image_barriers.push(decode_layer_barrier(
            setup_image,
            setup_layer,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
        ));
        image_barriers.push(decode_layer_barrier(
            dst_image,
            0,
            vk::ImageLayout::VIDEO_DECODE_DST_KHR,
        ));
    }
    let dependency = vk::DependencyInfo::default()
        .memory_barriers(&memory_barriers)
        .image_memory_barriers(&image_barriers);
    // SAFETY: recording into the begun buffer; synchronization2 is enabled per
    // the DeviceHandles feature contract.
    unsafe { device.cmd_pipeline_barrier2(cmd, &dependency) };

    // This op's status query slot, reset before the coding scope (encoder idiom).
    // None on drivers without queryResultStatusSupport (RADV — recording a query
    // there hangs the VCN; OpRing docs). NEVER remove this gate.
    if let Some(query_pool) = state.ops.query_pool {
        // SAFETY: recording; `query_index` is within the pool's count (fn contract).
        unsafe { device.cmd_reset_query_pool(cmd, query_pool, query_index, 1) };
    }

    // ---- bound-slot staging ----
    // Staged arrays over the scope decided above: resources → std infos → codec
    // slot infos → slot infos. Each vector is fully built before the next borrows
    // it, so nothing reallocates under a stored pointer.
    let resources: Vec<vk::VideoPictureResourceInfoKHR<'_>> = scope
        .iter()
        .map(|entry| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded_extent)
                .base_array_layer(0)
                .image_view_binding(entry.view)
        })
        .collect();
    let std_refs: Vec<hh::StdVideoDecodeH265ReferenceInfo> =
        scope.iter().map(|entry| entry.std).collect();
    let mut dpb_infos: Vec<vk::VideoDecodeH265DpbSlotInfoKHR<'_>> = std_refs
        .iter()
        .map(|std| vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(std))
        .collect();
    let mut begin_slots: Vec<vk::VideoReferenceSlotInfoKHR<'_>> = Vec::with_capacity(scope.len());
    for (index, entry) in scope.iter().enumerate() {
        begin_slots.push(
            vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(entry.slot_index)
                .picture_resource(&resources[index]),
        );
    }
    for (slot_info, dpb_info) in begin_slots.iter_mut().zip(dpb_infos.iter_mut()) {
        *slot_info = (*slot_info).push_next(dpb_info);
    }
    // The decode op's reference list: exactly this AU's references, in `refs`
    // order — `RefPicSetStCurrBefore`/`StCurrAfter`/`LtCurr` index into THIS
    // array (module docs), which is why the entries were built refs-first and why
    // a missing binding failed the whole op above rather than compacting.
    let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
        begin_slots[..reference_count].to_vec();

    // The setup slot as the decode op sees it: its REAL index (the begin list's
    // twin entry carries -1), same resource, its own codec info chain.
    let setup_std = vk_plan.setup_ref;
    let mut setup_dpb = vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_resource = resources[scope.len() - 1];
    let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(i32::from(vk_plan.setup_slot))
        .picture_resource(&setup_resource)
        .push_next(&mut setup_dpb);

    // Decode destination: the setup picture itself (coincide) or the pool image
    // (distinct).
    let dst_resource = if coincide {
        setup_resource
    } else {
        vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(coded_extent)
            .base_array_layer(0)
            .image_view_binding(state.pool.pictures[dst].view)
    };

    let std_pic = vk_plan.std_pic;
    // Offsets rebased into the packed slices-only buffer (NOT the plan's
    // AU-absolute offsets — the AU's non-slice NALUs were never uploaded).
    let mut h265_pic = vk::VideoDecodeH265PictureInfoKHR::default()
        .std_picture_info(&std_pic)
        .slice_segment_offsets(slice_offsets);
    let mut decode_info = vk::VideoDecodeInfoKHR::default()
        .src_buffer(state.ring.buffer())
        .src_buffer_offset(upload.offset)
        .src_buffer_range(upload.range)
        .dst_picture_resource(dst_resource)
        .setup_reference_slot(&setup_slot_info)
        .push_next(&mut h265_pic);
    if reference_count > 0 {
        decode_info = decode_info.reference_slots(&decode_refs);
    }

    let begin_coding = vk::VideoBeginCodingInfoKHR::default()
        .video_session(state.session.session())
        .video_session_parameters(state.session.parameters())
        .reference_slots(&begin_slots);
    // The one-shot session RESET, consumed HERE but re-armed on every error path
    // below — a RESET recorded into a command buffer that never reaches the
    // queue initialized nothing, and the next successful recording must carry it
    // or the session runs its whole life uninitialized.
    let did_reset = state.session.take_needs_reset();
    // SAFETY: recording into the begun buffer, through end_command_buffer; every
    // pointed-to struct above is a local (or session-state field) that outlives
    // the calls; the session/parameters handles are this generation's own.
    let recorded: Result<(), vk::Result> = unsafe {
        (dev.video_queue().fp().cmd_begin_video_coding_khr)(cmd, &begin_coding);
        if did_reset {
            // Session first-use initialization — ONCE, before its first decode.
            let control = vk::VideoCodingControlInfoKHR::default()
                .flags(vk::VideoCodingControlFlagsKHR::RESET);
            (dev.video_queue().fp().cmd_control_video_coding_khr)(cmd, &control);
        }
        if let Some(query_pool) = state.ops.query_pool {
            device.cmd_begin_query(cmd, query_pool, query_index, vk::QueryControlFlags::empty());
        }
        (dev.video_decode_queue().fp().cmd_decode_video_khr)(cmd, &decode_info);
        if let Some(query_pool) = state.ops.query_pool {
            device.cmd_end_query(cmd, query_pool, query_index);
        }
        (dev.video_queue().fp().cmd_end_video_coding_khr)(
            cmd,
            &vk::VideoEndCodingInfoKHR::default(),
        );
        device.end_command_buffer(cmd)
    };
    if let Err(e) = recorded {
        if did_reset {
            state.session.re_arm_reset();
        }
        return Err(VkDecodeError::from(e));
    }

    // ---- submit, under the caller's queue lock ----
    let cmd_infos = [vk::CommandBufferSubmitInfo::default().command_buffer(cmd)];
    let wait_infos: Vec<vk::SemaphoreSubmitInfo<'_>> = waits
        .iter()
        .map(|&(semaphore, value)| {
            vk::SemaphoreSubmitInfo::default()
                .semaphore(semaphore)
                .value(value)
                .stage_mask(vk::PipelineStageFlags2::VIDEO_DECODE_KHR)
        })
        .collect();
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(state.pool.pictures[dst].semaphore)
        .value(signal_value)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cmd_infos)
        .wait_semaphore_infos(&wait_infos)
        .signal_semaphore_infos(&signals)];
    let guard = QueueSubmitGuard::acquire(lock);
    // SAFETY: the decode queue is the device's own (DeviceHandles contract) and
    // externally synchronized by the guard; the submit arrays are locals.
    let result = unsafe { device.queue_submit2(dev.decode_queue(), &submits, vk::Fence::null()) };
    drop(guard);
    if let Err(e) = result {
        // The recorded RESET never executed: the next recording must redo it.
        if did_reset {
            state.session.re_arm_reset();
        }
        return Err(VkDecodeError::from(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;

    use super::*;
    use crate::pic_h265::VkRefH265;

    /// A reference-info value carrying just the two fields the assertions read.
    fn std_ref(poc: i32, long_term: bool) -> hh::StdVideoDecodeH265ReferenceInfo {
        // SAFETY: StdVideoDecodeH265ReferenceInfo is a plain-C bindgen struct of a
        // bitfield word and one integer; all-zero is valid for every field.
        let mut std: hh::StdVideoDecodeH265ReferenceInfo = unsafe { std::mem::zeroed() };
        std.PicOrderCntVal = poc;
        std.flags
            .set_used_for_long_term_reference(u32::from(long_term));
        std
    }

    fn vk_ref(slot: u8, poc: i32, long_term: bool) -> VkRefH265 {
        VkRefH265 {
            slot,
            std: std_ref(poc, long_term),
            id: u64::from(slot) + 100,
        }
    }

    /// A distinguishable fake view per slot (never dereferenced — the scope only
    /// carries handles around).
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    #[test]
    fn the_scopes_leading_entries_are_the_refs_in_plan_order() {
        // The plan's refs are NOT in slot order (they are in RPS set order:
        // StCurrBefore, StCurrAfter, LtCurr) — and the Std index arrays point at
        // positions in THAT order, so the scope must not sort or dedup them.
        let refs = vec![
            vk_ref(5, 40, false),
            vk_ref(1, 60, false),
            vk_ref(3, 8, true),
        ];
        let slot_refs = vec![Some(std_ref(0, false)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3, 5, 7].into_iter(),
            2,
            fake_view(2),
            std_ref(50, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();

        assert_eq!(reference_count, 3, "exactly this AU's references lead");
        assert_eq!(
            scope[..reference_count]
                .iter()
                .map(|e| e.slot_index)
                .collect::<Vec<_>>(),
            vec![5, 1, 3],
            "plan order, not slot order — the RPS index arrays depend on it"
        );
        for (entry, r) in scope.iter().zip(&refs) {
            assert_eq!(entry.view, fake_view(r.slot));
            assert_eq!(entry.std.PicOrderCntVal, r.std.PicOrderCntVal);
            assert_eq!(
                entry.std.flags.used_for_long_term_reference(),
                r.std.flags.used_for_long_term_reference(),
                "the long-term marking rides with the binding"
            );
        }

        // Then the other still-held slot (7), then the setup ACTIVATION entry.
        assert_eq!(scope[3].slot_index, 7);
        let last = scope.last().unwrap();
        assert_eq!(
            last.slot_index, -1,
            "the setup slot binds its resource without a current association"
        );
        assert_eq!(last.view, fake_view(2));
        assert_eq!(last.std.PicOrderCntVal, 50);
        assert_eq!(
            scope.len(),
            5,
            "3 refs + 1 other held slot + the activation"
        );
    }

    #[test]
    fn a_reference_slot_without_a_bound_image_fails_the_whole_op() {
        // Compacting past it would shift every later RefPicSetStCurr* index onto
        // the wrong picture — plausible-looking, wrong output. Fail closed.
        let refs = vec![vk_ref(4, 10, false), vk_ref(6, 20, false)];
        let slot_refs = vec![Some(std_ref(0, false)); 8];
        let err = build_scope(
            &refs,
            [4u8, 6].into_iter(),
            0,
            fake_view(0),
            std_ref(30, false),
            &slot_refs,
            |slot| (slot != 6).then(|| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 6 }),
            "{err}"
        );
    }

    #[test]
    fn held_slots_are_bound_once_and_the_setup_slot_never_twice() {
        // Slot 3 is BOTH a reference and still held; slot 2 is the setup slot and
        // also held (the previous picture in it). Neither may appear twice: a
        // duplicate slot index in one coding scope is invalid, and a second entry
        // for a reference would also break the index arrays.
        let refs = vec![vk_ref(3, 12, false)];
        let slot_refs = vec![Some(std_ref(99, false)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 2, 3].into_iter(),
            2,
            fake_view(2),
            std_ref(24, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 1);
        let indices: Vec<i32> = scope.iter().map(|e| e.slot_index).collect();
        assert_eq!(indices, vec![3, 1, -1]);
        assert_eq!(
            indices.iter().filter(|&&i| i == 3).count(),
            1,
            "a referenced slot is bound exactly once"
        );
        assert!(
            !indices.contains(&2),
            "the setup slot is bound only as the -1 activation entry"
        );
    }

    #[test]
    fn a_held_slot_with_no_cached_reference_info_is_left_unbound_not_faked() {
        // Only reachable if a slot was never a setup slot on this session; the
        // scope drops it rather than binding zeroed reference info (which would
        // claim POC 0, short-term, for a picture that is neither).
        let refs: Vec<VkRefH265> = Vec::new();
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>> = vec![None; 4];
        slot_refs[1] = Some(std_ref(7, false));
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3].into_iter(),
            0,
            fake_view(0),
            std_ref(9, false),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "an IRAP references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![1, -1],
            "slot 3 had no cached info and is simply not bound"
        );
    }

    #[test]
    fn a_post_mutation_failure_wedges_every_later_au_until_the_ledgers_are_reset() {
        // The exact shape the recovery exists for. An AU planned, `plan_to_vk_h265`
        // assigned it slot 2 in the SlotMap, the coincide binding sync cleared
        // slot 2's image binding — and THEN the decode failed (pool exhausted, ring
        // upload, submit, any of them). Nothing restores the state, so the planner
        // and the SlotMap both believe the picture is resident while no image holds
        // it.
        let mut slots = SlotMap::new(3);
        slots.assign(100).unwrap(); // slot 0, an older reference, image bound
        slots.assign(200).unwrap(); // slot 1, another, image bound
        slots.assign(300).unwrap(); // slot 2, THIS AU's setup — binding cleared
        let mut slot_image: Vec<Option<usize>> = vec![Some(7), Some(8), None, None];
        let mut slot_refs: Vec<Option<hh::StdVideoDecodeH265ReferenceInfo>> =
            vec![Some(std_ref(10, false)); 4];

        // Every later AU that references slot 2 fails, forever: build_scope will
        // not silently compact past an unbound reference (the RPS index arrays).
        let err = build_scope(
            &[vk_ref(2, 30, false)],
            [0u8, 1, 2].into_iter(),
            0,
            fake_view(0),
            std_ref(40, false),
            &slot_refs,
            |slot| slot_image[usize::from(slot)].map(|_| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 2 }),
            "{err}"
        );

        // The recovery: flush to the next IRAP and empty all three ledgers.
        let unbound = reset_slot_bindings(&mut slots, &mut slot_image, &mut slot_refs);
        assert_eq!(
            unbound,
            vec![7, 8],
            "the pool images the stale bindings pinned go back on the free list"
        );
        assert_eq!(slots.active(), 0, "no picture is DPB-resident any more");
        assert_eq!(
            slots.capacity(),
            4,
            "capacity survives — no session rebuild"
        );
        assert!(slot_image.iter().all(Option::is_none));
        assert!(
            slot_refs.iter().all(Option::is_none),
            "cached reference info goes too, or build_scope could bind a slot the \
             planner no longer knows about"
        );

        // And the IRAP that follows plans against an empty DPB: it references
        // nothing, takes the lowest slot, and its scope builds.
        let setup_slot = slots.assign(400).unwrap();
        assert_eq!(setup_slot, 0, "the freed slots are assignable again");
        slot_image[usize::from(setup_slot)] = Some(9);
        let (scope, reference_count) = build_scope(
            &[],
            slots.held().map(|(slot, _id)| slot),
            setup_slot,
            fake_view(setup_slot),
            std_ref(0, false),
            &slot_refs,
            |slot| slot_image[usize::from(slot)].map(|_| fake_view(slot)),
        )
        .unwrap();
        assert_eq!(reference_count, 0, "an IRAP references nothing");
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![-1],
            "only the setup activation entry — the stream is decoding again"
        );
    }

    #[test]
    fn the_recovery_latch_is_owed_once_and_consumed_by_exactly_one_decode() {
        // Two failures in a row still owe ONE flush, and the decode that performs
        // it clears the debt — otherwise every later decode would re-flush and the
        // stream could never build a DPB again.
        let mut latch = RecoveryLatch::default();
        assert!(!latch.is_latched(), "a fresh decoder owes nothing");
        assert!(!latch.take());

        latch.latch();
        latch.latch();
        assert!(
            latch.is_latched(),
            "visible in debug_snapshot before it runs"
        );
        assert!(latch.take(), "the next decode recovers");
        assert!(!latch.is_latched());
        assert!(!latch.take(), "and the one after that just decodes");
    }

    #[test]
    fn the_picture_info_carries_the_rebased_offsets_and_the_h265_std_struct() {
        // The submission-final wiring, without a device: `pSliceSegmentOffsets`
        // must be the REBASED array (one entry per slice segment, counted by
        // ash from the slice's length), and the picture info must point at the
        // plan's own Std struct.
        let segments = [40..900, 900..1500, 1500..2000];
        let offsets = rebased_offsets(&segments).unwrap();
        assert_eq!(offsets, vec![0, 860, 1460]);

        // SAFETY: StdVideoDecodeH265PictureInfo is a plain-C bindgen struct of a
        // bitfield word, integers and byte arrays; all-zero is valid.
        let mut std_pic: hh::StdVideoDecodeH265PictureInfo = unsafe { std::mem::zeroed() };
        std_pic.PicOrderCntVal = 42;
        let picture_info = vk::VideoDecodeH265PictureInfoKHR::default()
            .std_picture_info(&std_pic)
            .slice_segment_offsets(&offsets);
        assert_eq!(picture_info.slice_segment_count, 3);
        assert_eq!(
            picture_info.s_type,
            vk::StructureType::VIDEO_DECODE_H265_PICTURE_INFO_KHR
        );
        // SAFETY: the two pointers were just taken from `offsets` and `std_pic`,
        // both alive for this scope.
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(picture_info.p_slice_segment_offsets, 3),
                &offsets[..]
            );
            assert_eq!((*picture_info.p_std_picture_info).PicOrderCntVal, 42);
        }

        // And a DPB slot info chains the H.265 reference info, not the H.264 one.
        let std = std_ref(17, true);
        let dpb_info = vk::VideoDecodeH265DpbSlotInfoKHR::default().std_reference_info(&std);
        assert_eq!(
            dpb_info.s_type,
            vk::StructureType::VIDEO_DECODE_H265_DPB_SLOT_INFO_KHR
        );
        // SAFETY: the pointer was just taken from `std`, alive for this scope.
        unsafe {
            assert_eq!((*dpb_info.p_std_reference_info).PicOrderCntVal, 17);
            assert_eq!(
                (*dpb_info.p_std_reference_info)
                    .flags
                    .used_for_long_term_reference(),
                1
            );
        }
    }
}
