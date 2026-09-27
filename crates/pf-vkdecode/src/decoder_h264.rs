//! Native Vulkan Video H.264 decoder: planner, session, and decode-queue submit.
//!
//! Each AU is planned, converted, packed into the bitstream ring, recorded
//! (bound DPB slots, one-time session RESET, a `RESULT_STATUS_ONLY` query
//! around `vkCmdDecodeVideoKHR`), then submitted under the caller's
//! [`QueueLock`] with a per-picture timeline signal. The pool and timeline
//! contract is [`crate::decoder`]'s.
//!
//! Every decode op has a query slot. [`VkH264Decoder::poll_status`] reads it
//! without waiting; a non-COMPLETE result is the concealment signal. FFmpeg
//! runs `nb_queries = 0` and cannot see driver-reported corruption.
//!
//! Residual: sampling a still-live reference while a decode reads it writes
//! presenter layout metadata; `VK_KHR_unified_image_layouts` drops that trip.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use ash::vk;
use ash::vk::native as hh;
use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::H264Planner;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::PlanWarning;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::caps::derive_caps;
use crate::caps::query_caps;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps::NV12;
use crate::decoder::core::build_frame;
use crate::decoder::core::build_scope;
use crate::decoder::core::reset_slot_bindings;
use crate::decoder::core::settle_dpb;
use crate::decoder::core::sync_slot_bindings;
use crate::decoder::core::wait_timeline;
use crate::decoder::core::OpRing;
use crate::decoder::core::PendingPic;
use crate::decoder::core::RecoveryLatch;
use crate::decoder::core::RetiredPool;
use crate::decoder::core::ScopeRef;
use crate::decoder::DecodeStatus;
use crate::decoder::DecodedVkFrame;
use crate::decoder::VkDecodeError;
use crate::device::DecodeDevice;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::device::QueueSubmitGuard;
use crate::images::plan_pools;
use crate::images::DpbPool;
use crate::images::PicturePool;
use crate::params::level_to_std;
use crate::params::ParamsError;
use crate::pic::plan_to_vk;
use crate::pic::DecodePlanVk;
use crate::pic::PlanToVkError;
use crate::ring::pack_slices;
use crate::ring::BitstreamRing;
use crate::ring::RingLayout;
use crate::ring::UploadedAu;
use crate::ring::INITIAL_SLOT_SIZE;
use crate::ring::RING_SLOTS;
use crate::session::ParamsAction;
use crate::session::SessionConfig;
use crate::session::VideoSession;
use crate::slots::SlotMap;

/// One session generation. Extent / DPB / profile renegotiation retires it.
struct SessionState {
    session: VideoSession,
    slots: SlotMap,
    /// Distinct-mode reference-only DPB; `None` in coincide (picture pool backs
    /// the DPB).
    dpb: Option<DpbPool>,
    pool: PicturePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Last Std reference info per DPB slot. `vkCmdBeginVideoCodingKHR` wants
    /// codec info for every bound slot, including ones this AU does not
    /// reference. Refreshed from each plan so MMCO long-term promotions land.
    slot_refs: Vec<Option<hh::StdVideoDecodeH264ReferenceInfo>>,
    /// Coincide: pool picture bound to each DPB slot (rebound at activation).
    slot_image: Vec<Option<usize>>,
    /// Per command-buffer completion tokens (reuse gate).
    cmd_marks: Vec<Option<(vk::Semaphore, u64)>>,
    /// Per query-slot submission ordinals (staleness validation).
    query_marks: Vec<u64>,
    /// Submissions on this session (cmd/query indexing).
    submitted: u64,
    /// Newest submission's completion token (session drain).
    last_submit: Option<(vk::Semaphore, u64)>,
    /// Stream coded extent (renegotiation comparison).
    coded_extent: vk::Extent2D,
    /// Granularity-aligned allocation extent (picture resources + frames).
    image_extent: vk::Extent2D,
}

pub struct VkH264Decoder {
    dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    planner: H264Planner,
    /// Caps per Std profile idc, queried once per profile.
    caps: Option<(hh::StdVideoH264ProfileIdc, DecodeCaps)>,
    state: Option<SessionState>,
    pending: BTreeMap<PicId, PendingPic>,
    /// Display-ready, not yet handed out. Zero-reorder: at most one per AU;
    /// deeper only around discontinuities/flushes.
    ready: VecDeque<DecodedVkFrame>,
    /// Retired generations' pools with consumer-held images (die on last token).
    graveyard: Vec<RetiredPool>,
    /// Most recent plan warnings ([`Self::take_warnings`]).
    last_warnings: Vec<PlanWarning>,
    /// Outstanding recovery-point SEI ([`crate::recovery`]). Survives session
    /// rebuilds: a fact about the stream, not Vulkan objects. Distinct from
    /// [`Self::recovery`] (DPB-recovery latch).
    recovery_watch: crate::recovery::RecoveryWatch,
    /// Post-failure DPB recovery owed; see [`RecoveryLatch`].
    recovery: RecoveryLatch,
    /// Pictures planned so far — stamped as [`DecodedVkFrame::decode_order`].
    /// Survives session rebuilds for the same reason the watch does.
    decoded: u64,
    /// Bumped on every rebuild; stamped into frames.
    generation: u64,
    device_lost: bool,
    /// Over-declared-level warning already fired (once per decoder; the SPS
    /// does not change per AU).
    level_clamp_warned: bool,
}

impl VkH264Decoder {
    /// Wrap the borrowed device. Sessions and pools are built lazily from the
    /// first AU's SPS (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// Full [`DeviceHandles`] contract (liveness, enabled extensions and
    /// features, truthful queue families) for this decoder's lifetime. The
    /// device must have `VK_KHR_video_decode_h264` enabled; that part is
    /// checked below because a miss is UB at session creation, not an error.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        // Queue family must actually run H.264 decode. Caps would answer for
        // the hardware even if the extension was never enabled (`device.rs`).
        dev.require_codec_op(vk::VideoCodecOperationFlagsKHR::DECODE_H264, "H.264 decode")?;
        // The picture-pool arrangement is a device fact, not the stream's: ask with
        // the profile every host encodes, so an unusable device refuses the rung
        // before its first AU. A stream in another profile re-queries at its SPS.
        // SAFETY: live device (the `wrap` contract above).
        let raw = unsafe { query_caps(&dev, DecodeProfile::H264(H264_PROFILE_HIGH)) }
            .map_err(VkDecodeError::from)?;
        let caps = Some((H264_PROFILE_HIGH, derive_caps(&raw, NV12)?));
        Ok(Self {
            dev,
            lock,
            planner: H264Planner::new(),
            caps,
            state: None,
            pending: BTreeMap::new(),
            ready: VecDeque::new(),
            graveyard: Vec::new(),
            last_warnings: Vec::new(),
            recovery_watch: crate::recovery::RecoveryWatch::new(),
            recovery: Default::default(),
            decoded: 0,
            generation: 0,
            device_lost: false,
            level_clamp_warned: false,
        })
    }

    /// Decode one access unit. Returns the next display-ready frame if the
    /// planner declared one (zero-reorder: the AU's own picture).
    ///
    /// Never panics. `VkDecodeError::DeviceLost` latches: later calls fail fast
    /// until the owner rebuilds on fresh handles.
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
        // Previous AU failed after planning: clear stale DPB residency before
        // planning this one, or every AU that references the stranded picture
        // fails forever ([`RecoveryLatch`] docs).
        if self.recovery.take() {
            self.recover_dpb();
        }
        // `take_warnings` is "cleared by the next decode". Clear before planning
        // so a failed plan cannot leave the previous AU's warnings to be re-read
        // as damage. Successful decode already `mem::take`s them.
        self.last_warnings.clear();
        let plan = self.planner.plan_au(au)?;
        for warning in &plan.warnings {
            // Recovery verdict is the integration layer's; still not silent here.
            trace!(?warning, "plan warning");
        }
        self.last_warnings = plan.warnings.clone();
        // One picture per AU: stamp decode-order before anything can reorder it.
        self.decoded = self.decoded.saturating_add(1);
        let decode_order = self.decoded;
        // Fold recovery-point SEI once per planned AU, in decode order (the
        // SEI's count). The mark rides the pending picture to display order.
        let recovery = self.recovery_watch.note_h264(
            plan.picture.frame_num,
            plan.picture.is_idr,
            plan.picture.recovery_point,
        );
        if recovery != crate::recovery::RecoveryMark::NONE {
            trace!(
                sei = recovery.sei_here,
                recovery_point = recovery.is_recovery_point,
                frame_num = plan.picture.frame_num,
                "recovery point SEI"
            );
        }

        // Planner has advanced; its DPB holds this picture. A later failure
        // can disagree with the slot/image ledgers — latch recovery. Wider
        // than SlotMap mutations: `ensure_state` / `NoFreeSlot` strands the
        // picture planner-resident with no slot. One flush cures both.
        let result = self.decode_planned(&plan, au, recovery, decode_order);
        if result.is_err() {
            self.recovery.latch();
        }
        result
    }

    /// Submit one already-planned AU. Split so [`Self::decode_inner`] can latch
    /// recovery on any failure past that line without a flag on every exit.
    /// `au` is the buffer `plan`'s slice ranges index; `recovery` and
    /// `decode_order` are already folded (this path is not every planned AU).
    fn decode_planned(
        &mut self,
        plan: &AuPlan,
        au: &[u8],
        recovery: crate::recovery::RecoveryMark,
        decode_order: u64,
    ) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        self.ensure_state(plan)?;
        let sps_id = plan.sps.seq_parameter_set_id;

        // One rebuild retry on CapacityMismatch — DPB-depth renegotiation
        // (`pic.rs`).
        let mut vk_plan: Option<DecodePlanVk> = None;
        for attempt in 0..2 {
            // Recreate destroys the old parameters object; an in-flight decode
            // may still execute against it. Drain first. Rare (encoder
            // reconfiguration); the stall is the trade.
            if self
                .state
                .as_ref()
                .expect("ensure_state built it")
                .session
                .parameters_action(&plan.sps, &plan.pps)
                == ParamsAction::Recreate
            {
                self.drain_gpu()?;
            }
            let state = self.state.as_mut().expect("ensure_state built it");
            // SAFETY: live device (constructor contract); the drain above
            // satisfies ensure_parameters' Recreate contract, and Current/Add
            // touch nothing a submitted decode reads.
            unsafe { state.session.ensure_parameters(&plan.sps, &plan.pps)? };
            match plan_to_vk(plan, &mut state.slots, sps_id) {
                Ok(converted) => {
                    vk_plan = Some(converted);
                    break;
                }
                Err(PlanToVkError::CapacityMismatch { required, capacity }) if attempt == 0 => {
                    debug!(
                        required,
                        capacity, "DPB depth renegotiated — rebuilding session"
                    );
                    self.rebuild_state(plan)?;
                }
                Err(e) => return Err(VkDecodeError::Convert(e)),
            }
        }
        let vk_plan = vk_plan.expect("the rebuilt session matches its own plan");

        // From here to the deferred release is one ledger unit. `plan_to_vk`
        // committed the setup assignment and withheld `release_after_decode`.
        // A `?` in the region leaks a slot per failed AU. Hold the Result so
        // the release runs either way.
        let submitted = (|| -> Result<(), VkDecodeError> {
            let state = self.state.as_mut().expect("ensured above");
            // Session was created with maxActiveReferencePictures; binding more
            // in one op is a silent VUID violation on the drivers that matter.
            let max_active = state.session.config.max_active_references as usize;
            if vk_plan.refs.len() > max_active {
                return Err(VkDecodeError::Unsupported(format!(
                    "AU references {} pictures, session allows {max_active} active references",
                    vk_plan.refs.len()
                )));
            }

            // Coincide: released slots unbind (pictures may still be pending/held).
            // Clear the setup slot's previous binding before it binds fresh.
            let setup = usize::from(vk_plan.setup_slot);
            if state.dpb.is_none() {
                let unbound =
                    sync_slot_bindings(&state.slots, &mut state.slot_image, vk_plan.setup_slot);
                for picture in unbound {
                    state.pool.pictures[picture].bound = false;
                }
            }

            // Free pool picture, never one a consumer holds. Exhaustion means the
            // consumer owes HOLD_HEADROOM releases; no wait frees a picture here.
            let Some(dst) = state.pool.free_index() else {
                debug!(
                    held = state.pool.held_total(),
                    "picture pool exhausted — release_frame owed"
                );
                return Err(VkDecodeError::NoFreeSlot);
            };

            // Cross-queue waits: dst's last timeline (presenter write-back after
            // release), plus — coincide — every referenced image, so reference
            // reads order after a reported layout restore.
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

            let submission = state.submitted;
            let cmd_index = (submission % state.ops.cmds.len() as u64) as usize;
            if let Some((sem, value)) = state.cmd_marks[cmd_index] {
                // SAFETY: live device; the token is a pool picture's semaphore.
                unsafe { wait_timeline(self.dev.ash(), sem, value, "command buffer reuse")? };
            }
            let query_index = (submission % u64::from(state.ops.query_count)) as u32;

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
            // Bitstream is slice NALUs only. A real AU opens with AUD/SEI
            // (and SPS/PPS at IDRs); feeding those to VCN inside the decode
            // range hangs it. `pack_slices` rebases offsets and normalises each
            // Annex-B prefix to three bytes (`crate::ring::three_byte_prefix`).
            let plan_segments: Vec<std::ops::Range<usize>> =
                plan.slices.iter().map(|s| s.data.clone()).collect();
            let Some(packed) = pack_slices(au, &plan_segments) else {
                return Err(VkDecodeError::Unsupported(
                    "packed slice data exceeds the u32 offsets Vulkan submits".into(),
                ));
            };
            let slice_offsets = packed.offsets;
            // SAFETY: live device; the segments are the plan's own in-bounds slice
            // ranges (narrowed by the prefix normalisation, so still in bounds); every
            // pending token is the completion signal of the submission that consumed
            // the slot.
            let upload = unsafe {
                state
                    .ring
                    .upload(&self.dev, au, &packed.segments, &mut poll, &mut wait)?
            };

            // SAFETY: live device; every handle recorded below belongs to this
            // session generation, and the packed slices sit uploaded in the ring slot.
            unsafe {
                record_and_submit(
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
                    references_clean: plan.picture.references_clean,
                },
            );
            Ok(())
        })();

        // Slots the planner retired while this op still bound them
        // (`release_after_decode`). Held through convert/bind/submit; free now
        // so the next AU may take them. Images stay `bound` one frame more.
        // Runs on failure too: the planner already removed them from the DPB.
        if let Some(state) = self.state.as_mut() {
            for &id in &vk_plan.release_after_decode {
                if !state.slots.release(id) {
                    trace!(id, "deferred release of an id the slot map no longer holds");
                }
            }
        }
        submitted?;

        // Outputs become ready frames (pending → held); removed-but-never-output
        // pictures free their images.
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

    /// Hand a delivered frame back. `presenter_signaled` is whether the consumer
    /// sampled the image and enqueued `value + 1` per [`DecodedVkFrame`]. The
    /// decoder then waits that write-back before reuse. Every `decode` /
    /// `take_ready` frame must come back once, including stale-generation ones.
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
        // Retired pool dies on its last token. Presenter fence-waited before
        // the token; decode work drained at retirement.
        if frame.generation != self.generation {
            self.graveyard
                .retain(|r| r.generation != frame.generation || r.pool.held_total() > 0);
        }
        Ok(())
    }

    /// A display-ready frame beyond the one `decode` returned, if any. Non-empty
    /// only around discontinuities/flushes (zero-reorder envelope). Drain after
    /// every decode; leftover frames still occupy pool pictures.
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// Warnings of the most recent successfully planned AU (concealment /
    /// want_keyframe). Cleared by the next `decode`.
    pub fn take_warnings(&mut self) -> Vec<PlanWarning> {
        std::mem::take(&mut self.last_warnings)
    }

    /// Forget the planner's unclean marks after a freeze lift on intra refresh marks
    /// ([`pf_bitstream::clean::CleanLedger::clear`]).
    pub fn forgive_unclean(&mut self) {
        self.planner.forgive_unclean();
    }

    /// Session generation stamped onto newly delivered frames.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Decode-order ordinal of the most recently planned picture. Compare
    /// [`DecodedVkFrame::decode_order`] against this to tell pre-loss from
    /// post-loss. 0 before the first AU plans.
    pub fn decode_order(&self) -> u64 {
        self.decoded
    }

    /// One-line state snapshot for failure paths. Not a stable format.
    pub fn debug_snapshot(&self) -> String {
        match &self.state {
            None => format!("gen={} <no session>", self.generation),
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
                    "gen={} mode={} slots_held={}/{} pool=[{}] pending={} ready={} graveyard={}",
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

    /// Read `frame`'s decode status without waiting.
    ///
    /// [`DecodeStatus::Failed`] covers driver errors and a query slot re-armed
    /// before it was read (unprovable → same conservative verdict).
    ///
    /// Without `queryResultStatusSupport`, `Ok` means the op completed on the
    /// timeline — the same information FFmpeg has on every driver.
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    /// Whether this decode queue family answers per-op `RESULT_STATUS` queries
    /// (`queryResultStatusSupport`).
    ///
    /// When true, [`DecodeStatus::Failed`] is the driver's verdict. When false
    /// (RADV hangs the VCN if a query is recorded), `Ok` means timeline
    /// completion: unmeasured, not clean.
    pub fn status_queries(&self) -> bool {
        self.dev.result_status_queries()
    }

    /// [`Self::poll_status`], but waits for the op first. The only blocking
    /// status read (GPU smoke assertions; integration polls).
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
            // No queries on this driver: verdict degrades to timeline completion.
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

    /// Wait up to `timeout_ns` for [`DecodedVkFrame::semaphore`] to reach
    /// [`DecodedVkFrame::value`]. Measurement only: a timeout degrades the
    /// latency stat; the consumer's GPU wait gates sampling. `frame` must be
    /// unreleased so the semaphore stays alive. Stale-generation declines.
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

    /// Drain the planner (teardown / discontinuity). Buffered pictures become
    /// display-ready via [`Self::take_ready`]; DPB slots free; never-output
    /// pictures free their images.
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
            // A pending picture neither output nor removed should not exist
            // after a flush; free leftovers.
            for (_, entry) in std::mem::take(&mut self.pending) {
                debug!(poc = entry.poc, "pending picture survived a flush — freed");
                state.pool.pictures[entry.image].pending = false;
            }
        } else {
            self.pending.clear();
        }
    }

    /// Clear DPB state a failed AU left so planning resumes at the next IDR.
    ///
    /// After a post-planning failure three ledgers disagree: the planner DPB,
    /// [`SlotMap`], and slot→picture bindings. [`Self::flush`] settles the first
    /// (and still delivers pictures that reached output);
    /// [`reset_slot_bindings`] empties the other two.
    /// Images a consumer holds stay pinned by `held`, as across a rebuild.
    ///
    /// Not a session rebuild: session, pools, and ring are still valid.
    fn recover_dpb(&mut self) {
        debug!("recovering from a failed AU — flushing the H.264 DPB to the next IDR");
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

    /// Session/caps for this plan match its extent + profile, and the stream
    /// sits inside the device's level ceiling. DPB-depth mismatches surface
    /// later as `CapacityMismatch` and take the same rebuild path.
    fn ensure_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        let std_profile = std_profile_for(plan)?;
        if self.caps.as_ref().map(|(p, _)| *p) != Some(std_profile) {
            // SAFETY: live device (constructor contract).
            let raw = unsafe { query_caps(&self.dev, DecodeProfile::H264(std_profile)) }
                .map_err(VkDecodeError::from)?;
            self.caps = Some((std_profile, derive_caps(&raw, NV12)?));
        }
        // A declared level above `maxLevelIdc` is not a refusal — encoders
        // over-claim. Real demands (extent, DPB depth) are checked in
        // `rebuild_state`; parameter sets clamp to `max_level_idc` so the
        // driver never sees a level above caps. Compare within one codec's Std.
        let caps_max_level = self.caps.as_ref().expect("queried above").1.max_level_idc;
        let stream_level = level_to_std(plan.picture.level_idc);
        if stream_level > caps_max_level.code_point() && !self.level_clamp_warned {
            self.level_clamp_warned = true;
            warn!(
                stream_level,
                ceiling = %caps_max_level,
                "stream declares an H.264 level above the device ceiling — the \
                 declared level is advisory (over-declared by some encoders); \
                 proceeding with the parameter sets clamped to the ceiling"
            );
        }
        let coded = vk::Extent2D {
            width: plan.picture.coded_width,
            height: plan.picture.coded_height,
        };
        match &self.state {
            Some(state)
                if state.coded_extent == coded
                    && state.session.config.std_profile_idc == std_profile =>
            {
                Ok(())
            }
            _ => self.rebuild_state(plan),
        }
    }

    /// Tear down the current generation (drain decode work; retire the picture
    /// pool to the graveyard if the consumer still holds images) and build a
    /// fresh one from `plan`. Bumps [`Self::generation`] so old frames route
    /// to the graveyard.
    ///
    /// A pool with holds retires intact until `release_frame` takes its last
    /// token (sent only after the presenter's sampling fence). Tokens carry
    /// generation, so releases cannot alias. Session/ring/ops die here after
    /// [`Self::drain_gpu`]; [`DecodedVkFrame`] borrows pool resources only,
    /// and `poll_status` generation-gates before touching the new query pool.
    fn rebuild_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        self.drain_gpu()?;
        if let Some(state) = self.state.take() {
            debug!("rebuilding decode session (stream renegotiation)");
            // SessionState has no Drop: session/dpb/ring/ops die here (decode
            // drained; presenter never references them). The picture pool may
            // outlive: drop undelivered holds, free pending, graveyard if the
            // consumer still holds delivered images.
            let SessionState { mut pool, .. } = state;
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

        let (std_profile, caps) = self.caps.as_ref().expect("ensure_state queried caps");
        let std_profile = *std_profile;
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
        // Bounds-check the allocation extent (granularity-rounded): that is
        // what images are created at and what maxCodedExtent must cover.
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

        let config = SessionConfig {
            max_coded_extent: image_extent,
            max_dpb_slots: required_slots,
            max_active_references: (required_slots - 1).min(caps.max_active_references),
            std_profile_idc: std_profile,
            max_level_idc: caps.max_level_idc.code_point(),
        };
        let mut pool_plan = plan_pools(caps, required_slots);
        // Test-only: `gpu_parity` copies pictures to the host, and
        // `vkCmdCopyImageToBuffer` needs TRANSFER_SRC — a bit production
        // pools omit. Opt-in via env so no production path grows it.
        if std::env::var("PF_VKD_TEST_READBACK").is_ok_and(|v| v == "1") {
            pool_plan.picture_usage |= vk::ImageUsageFlags::TRANSFER_SRC;
        }
        let decode_profile = DecodeProfile::H264(std_profile);
        // SAFETY: live device per the constructor contract, for every create in
        // this block; each created half is owned by a Drop type the moment it
        // exists, so a mid-build failure unwinds cleanly.
        let state = unsafe {
            let session = VideoSession::create(&self.dev, caps, config)?;
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
            SessionState {
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

impl Drop for VkH264Decoder {
    fn drop(&mut self) {
        // Drain so pool Drop never destroys in-flight decode work; a wedged
        // driver falls through after the bounded timeout. Presenter sampling of
        // held images is the caller's teardown: wait every release token before
        // drop, or remaining graveyard pools are a warned forfeit.
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

/// `STD_VIDEO_H264_PROFILE_IDC_HIGH`: the profile every punktfunk host encodes.
const H264_PROFILE_HIGH: hh::StdVideoH264ProfileIdc = 100;

/// Map `profile_idc` to the Std code point. Identity for the four
/// Vulkan-representable profiles; reject otherwise.
fn std_profile_for(plan: &AuPlan) -> Result<hh::StdVideoH264ProfileIdc, VkDecodeError> {
    match u32::from(plan.picture.profile_idc) {
        p @ (66 | 77 | 100 | 244) => Ok(p),
        _ => Err(VkDecodeError::Params(ParamsError::UnmappableProfileIdc(
            plan.picture.profile_idc,
        ))),
    }
}

impl ScopeRef for crate::pic::VkRef {
    type Std = hh::StdVideoDecodeH264ReferenceInfo;
    fn slot(&self) -> u8 {
        self.slot
    }
    fn std(&self) -> Self::Std {
        self.std
    }
}

/// Picture resource view for DPB `slot`: bound pool picture layer (coincide)
/// or DPB array layer (distinct). `None` when a coincide slot has no binding.
fn slot_view(state: &SessionState, slot: u8) -> Option<vk::ImageView> {
    match &state.dpb {
        Some(dpb) => Some(dpb.dpb_view(slot)),
        None => state.slot_image[usize::from(slot)].map(|p| state.pool.pictures[p].view),
    }
}

/// Record one decode op and submit it under the queue lock: image waits per the
/// pool contract, dst timeline signal at `signal_value`.
///
/// # Safety
///
/// Live device; `state` is the current session generation with `vk_plan` derived
/// against its `SlotMap`, `dst` a free pool picture, the AU resident in `upload`'s
/// ring slot, and the command buffer's previous submission completed (caller
/// waited its mark).
#[allow(clippy::too_many_arguments)]
unsafe fn record_and_submit(
    dev: &DecodeDevice,
    lock: &dyn QueueLock,
    state: &mut SessionState,
    vk_plan: &DecodePlanVk,
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

    let begin_info =
        vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
    // SAFETY: the buffer's previous submission completed (fn contract) and its
    // pool allows per-buffer reset, so begin implicitly resets it.
    unsafe {
        device
            .begin_command_buffer(cmd, &begin_info)
            .map_err(VkDecodeError::from)?
    };

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
    let dst_picture = &state.pool.pictures[dst];
    let dst_image = dst_picture.image;
    let mut image_barriers = Vec::new();
    if coincide {
        // Coincide: dst pool layer is the setup DPB picture.
        image_barriers.push(decode_layer_barrier(
            dst_image,
            dst_picture.layer,
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
            dst_picture.layer,
            vk::ImageLayout::VIDEO_DECODE_DST_KHR,
        ));
    }
    let dependency = vk::DependencyInfo::default()
        .memory_barriers(&memory_barriers)
        .image_memory_barriers(&image_barriers);
    // SAFETY: recording into the begun buffer; synchronization2 is enabled per
    // the DeviceHandles feature contract.
    unsafe { device.cmd_pipeline_barrier2(cmd, &dependency) };

    // Reset the status query before the coding scope. None without
    // queryResultStatusSupport (RADV hangs the VCN if a query is recorded).
    if let Some(query_pool) = state.ops.query_pool {
        // SAFETY: recording; `query_index` is within the pool's count (fn contract).
        unsafe { device.cmd_reset_query_pool(cmd, query_pool, query_index, 1) };
    }

    // Setup/dst: fresh pool picture layer (coincide) or DPB layer (distinct).
    // Resolved before the scope is built — it is the scope's last entry.
    let setup_view = if coincide {
        state.pool.pictures[dst].view
    } else {
        state
            .dpb
            .as_ref()
            .expect("distinct mode")
            .dpb_view(vk_plan.setup_slot)
    };
    // Scope: this AU's references, then other held slots (stay bound so
    // associations persist), then setup as activation (slot index -1 binds
    // without a current association). Shared `build_scope` keeps the
    // fail-closed layout one implementation.
    let held: Vec<u8> = state.slots.held().map(|(slot, _id)| slot).collect();
    let (scope, reference_count) = build_scope(
        &vk_plan.refs,
        held.into_iter(),
        vk_plan.setup_slot,
        setup_view,
        vk_plan.setup_ref,
        &state.slot_refs,
        |slot| slot_view(state, slot),
    )?;

    // resources → std infos → codec slot infos → slot infos. Each vector is
    // finished before the next borrows it, so nothing reallocates under a pointer.
    let resources: Vec<vk::VideoPictureResourceInfoKHR<'_>> = scope
        .iter()
        .map(|e| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded_extent)
                .base_array_layer(0)
                .image_view_binding(e.view)
        })
        .collect();
    let std_refs: Vec<hh::StdVideoDecodeH264ReferenceInfo> = scope.iter().map(|e| e.std).collect();
    let mut dpb_infos: Vec<vk::VideoDecodeH264DpbSlotInfoKHR<'_>> = std_refs
        .iter()
        .map(|std| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(std))
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
    // Decode-op references: exactly this AU's refs (the first `reference_count`
    // scope entries, carrying real slot indices).
    let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
        begin_slots[..reference_count].to_vec();

    // Setup slot as the decode op sees it: real index (the begin list's twin
    // carries -1), same resource, own codec info chain.
    let setup_std = vk_plan.setup_ref;
    let mut setup_dpb = vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_resource = resources[scope.len() - 1];
    let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(i32::from(vk_plan.setup_slot))
        .picture_resource(&setup_resource)
        .push_next(&mut setup_dpb);

    // Decode destination: the setup picture layer (coincide) or pool picture.
    let dst_resource = if coincide {
        setup_resource
    } else {
        vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(coded_extent)
            .base_array_layer(0)
            .image_view_binding(state.pool.pictures[dst].view)
    };

    let std_pic = vk_plan.std_pic;
    // Offsets into the packed slices-only buffer, not the plan's AU-absolute
    // offsets — non-slice NALUs were never uploaded.
    let mut h264_pic = vk::VideoDecodeH264PictureInfoKHR::default()
        .std_picture_info(&std_pic)
        .slice_offsets(slice_offsets);
    let mut decode_info = vk::VideoDecodeInfoKHR::default()
        .src_buffer(state.ring.buffer())
        .src_buffer_offset(upload.offset)
        .src_buffer_range(upload.range)
        .dst_picture_resource(dst_resource)
        .setup_reference_slot(&setup_slot_info)
        .push_next(&mut h264_pic);
    if reference_count > 0 {
        decode_info = decode_info.reference_slots(&decode_refs);
    }

    let begin_coding = vk::VideoBeginCodingInfoKHR::default()
        .video_session(state.session.raw.session())
        .video_session_parameters(state.session.parameters())
        .reference_slots(&begin_slots);
    // One-shot session RESET, consumed here but re-armed on every error path
    // below. A RESET recorded into a buffer that never reaches the queue
    // initialized nothing; the next successful recording must carry it.
    let did_reset = state.session.raw.take_needs_reset();
    // SAFETY: recording into the begun buffer, through end_command_buffer; every
    // pointed-to struct above is a local (or session-state field) that outlives
    // the calls; the session/parameters handles are this generation's own.
    let recorded: Result<(), vk::Result> = unsafe {
        (dev.video_queue().fp().cmd_begin_video_coding_khr)(cmd, &begin_coding);
        if did_reset {
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
            state.session.raw.re_arm_reset();
        }
        return Err(VkDecodeError::from(e));
    }

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
        // Recorded RESET never executed: the next recording must redo it.
        if did_reset {
            state.session.raw.re_arm_reset();
        }
        return Err(VkDecodeError::from(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use ash::vk::Handle as _;

    use super::*;

    /// Fake, never-dereferenced view handle keyed by slot. Lets a scope's
    /// bindings be checked without a device.
    fn fake_view(slot: u8) -> vk::ImageView {
        vk::ImageView::from_raw(u64::from(slot) + 1)
    }

    fn h264_std_ref(frame_num: u16) -> hh::StdVideoDecodeH264ReferenceInfo {
        // SAFETY: StdVideoDecodeH264ReferenceInfo is a plain-C bindgen struct of a
        // bitfield word and integers; all-zero is valid for every field.
        let mut std: hh::StdVideoDecodeH264ReferenceInfo = unsafe { std::mem::zeroed() };
        std.FrameNum = frame_num;
        std
    }

    fn h264_ref(slot: u8, frame_num: u16) -> crate::pic::VkRef {
        crate::pic::VkRef {
            slot,
            std: h264_std_ref(frame_num),
            id: u64::from(slot),
        }
    }

    /// Fail closed: an unbound reference slot is `UnboundReferenceSlot`, not a
    /// skipped entry. Hardware would still decode against the missing picture.
    #[test]
    fn an_h264_reference_slot_without_a_bound_image_fails_the_whole_op() {
        let refs = vec![h264_ref(1, 10), h264_ref(3, 20)];
        let slot_refs = vec![Some(h264_std_ref(0)); 8];
        let err = build_scope(
            &refs,
            [1u8, 3].into_iter(),
            0,
            fake_view(0),
            h264_std_ref(30),
            &slot_refs,
            |slot| (slot != 3).then(|| fake_view(slot)),
        )
        .unwrap_err();
        assert!(
            matches!(err, VkDecodeError::UnboundReferenceSlot { slot: 3 }),
            "{err}"
        );
    }

    /// `reference_count` is this AU's references only. Counting after the
    /// held-slot pass would let the decode op's reference array run into
    /// unrelated held slots — a picture predicted from something never named.
    #[test]
    fn the_h264_reference_count_covers_the_references_and_never_a_held_slot() {
        // Two references (slots 1, 3); slots 5 and 6 are held but not referenced.
        let refs = vec![h264_ref(1, 10), h264_ref(3, 20)];
        let slot_refs = vec![Some(h264_std_ref(77)); 8];
        let (scope, reference_count) = build_scope(
            &refs,
            [1u8, 3, 5, 6].into_iter(),
            0,
            fake_view(0),
            h264_std_ref(30),
            &slot_refs,
            |slot| Some(fake_view(slot)),
        )
        .unwrap();

        assert_eq!(reference_count, 2, "exactly this AU's references");
        assert_eq!(
            scope[..reference_count]
                .iter()
                .map(|e| e.slot_index)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "the decode op's reference prefix is the references, in order"
        );
        assert_eq!(
            scope.iter().map(|e| e.slot_index).collect::<Vec<_>>(),
            vec![1, 3, 5, 6, -1]
        );
    }

    #[test]
    fn std_level_code_points_ascend_so_the_max_level_gate_compares_numerically() {
        use pf_bitstream::h264::Level;
        // Gate is `level_to_std(stream) > caps.max_level_idc.code_point()`.
        // Sound only if Std code points ascend within one codec. Pin the
        // ordering (and the 1b fold onto 1.1).
        let ascending = [
            Level::L1,
            Level::L1_1,
            Level::L2_0,
            Level::L3_1,
            Level::L4,
            Level::L4_2,
            Level::L5_2,
            Level::L6_2,
        ];
        for pair in ascending.windows(2) {
            assert!(
                level_to_std(pair[0]) < level_to_std(pair[1]),
                "{:?} vs {:?}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(level_to_std(Level::L1B), level_to_std(Level::L1_1));

        let max = level_to_std(Level::L4_1);
        assert!(
            level_to_std(Level::L4) <= max,
            "within the ceiling: allowed"
        );
        assert!(
            level_to_std(Level::L4_2) > max,
            "above the ceiling: Unsupported"
        );
    }
}
