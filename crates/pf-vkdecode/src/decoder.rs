//! [`VkH264Decoder`]: the assembled native decoder — pf-bitstream's planner and
//! WP-A's conversions driving a Vulkan Video session end to end.
//!
//! Per AU: `plan_au` → `plan_to_vk` → AU upload into the bitstream ring → record
//! (barriers, `vkCmdBeginVideoCodingKHR` with every bound DPB slot, the one-time
//! session RESET control, a `RESULT_STATUS_ONLY` query bracketing
//! `vkCmdDecodeVideoKHR`) → submit on the decode queue under the caller's
//! [`QueueLock`] with a per-output-slot timeline signal.
//!
//! The status query is THE point of this program: FFmpeg's `vulkan_decode.c` runs
//! `nb_queries = 0` and therefore architecturally cannot see driver-reported decode
//! corruption (the Xbox Ally X field case). Here every decode op has a query slot,
//! [`VkH264Decoder::poll_status`] reads it WITHOUT waiting, and a non-COMPLETE
//! result is the concealment signal WP-C wires to `want_keyframe`.
//!
//! What stays for WP-C (the integration layer): feeding `PlanWarning`s and Failed
//! statuses into the recovery machinery, presenting frames (including the
//! coincide-mode layout dance — see [`DecodedVkFrame::layout`]), and throttling so
//! output slots are consumed before their ring position recycles.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use ash::vk;
use ash::vk::native as hh;
use pf_bitstream::h264::AuPlan;
use pf_bitstream::h264::DisplayCrop;
use pf_bitstream::h264::DpbUpdate;
use pf_bitstream::h264::H264Planner;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::PlanError;
use pf_bitstream::h264::PlanWarning;
use tracing::debug;
use tracing::trace;

use crate::caps::derive_caps;
use crate::caps::query_h264_caps;
use crate::caps::CapsError;
use crate::caps::DecodeCaps;
use crate::caps::H264ProfileChain;
use crate::device::AllocError;
use crate::device::DecodeDevice;
use crate::device::DeviceError;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::device::QueueSubmitGuard;
use crate::images::plan_pools;
use crate::images::ImagePool;
use crate::images::OUTPUT_RING;
use crate::params::level_to_std;
use crate::params::ParamsError;
use crate::pic::plan_to_vk;
use crate::pic::DecodePlanVk;
use crate::pic::PlanToVkError;
use crate::ring::BitstreamRing;
use crate::ring::RingLayout;
use crate::ring::UploadedAu;
use crate::ring::INITIAL_SLOT_SIZE;
use crate::ring::RING_SLOTS;
use crate::session::ParamsAction;
use crate::session::SessionConfig;
use crate::session::SessionError;
use crate::session::VideoSession;
use crate::slots::SlotError;
use crate::slots::SlotMap;

/// Ceiling on any blocking GPU wait on the decode thread (5 s) — generous against
/// a real decode, finite against a wedged driver, matching the encoder's fence
/// budget so the session layer's recovery path is never parked forever.
const DECODE_TIMEOUT_NS: u64 = 5_000_000_000;

/// Result of one decode op's `RESULT_STATUS_ONLY` query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeStatus {
    /// The op has not completed (or its status is not yet readable).
    Pending,
    /// The driver reports the op COMPLETE.
    Ok,
    /// The driver reports an error status, the query slot was recycled before it
    /// was read, or the device is lost — in every case the frame's content is
    /// unproven and the caller should treat it as concealed (want_keyframe).
    Failed,
}

/// One decoded, display-ready picture. Handles are BORROWED from the decoder's
/// pools: valid until the decoder rebuilds its session (stream renegotiation —
/// detectable via [`Self::generation`]). The consumer waits `semaphore >= value`
/// before reading pixels and MUST hand every delivered frame back through
/// [`VkH264Decoder::release_frame`] once done — the frame's slot is excluded from
/// every reuse path (setup assignment, output-ring recycling) until then, which
/// is what makes a delivered image safe to read while decoding continues.
#[derive(Debug, Clone)]
pub struct DecodedVkFrame {
    pub image: vk::Image,
    /// Full-picture NV12 view (what decode wrote through).
    pub view: vk::ImageView,
    /// `R8`/`R8G8` per-plane views for the presenter's sampler path.
    pub plane_views: [vk::ImageView; 2],
    /// The image array layer the picture occupies (the views already select it).
    pub layer: u32,
    /// The layout the picture is in when the semaphore signals:
    /// `VIDEO_DECODE_DST_KHR` (distinct mode) or `VIDEO_DECODE_DPB_KHR` (coincide
    /// mode — the picture may still be a live reference, so a consumer that
    /// transitions it for sampling MUST transition it back before the next decode
    /// references the slot; WP-C owns that dance).
    pub layout: vk::ImageLayout,
    pub coded_width: u32,
    pub coded_height: u32,
    /// Conformance-window crop: the region to display. First-class here so no
    /// consumer ever derives geometry from the (padded) pool shape again.
    pub crop: DisplayCrop,
    /// Timeline pair: the picture's pixels are ready when `semaphore` reaches
    /// `value`.
    pub semaphore: vk::Semaphore,
    pub value: u64,
    pub poc: i32,
    pub is_idr: bool,
    /// The decode op's slot in the status query pool (for [`VkH264Decoder::poll_status`]).
    pub query_slot: u32,
    /// The session generation this frame's handles belong to. Bumped on every
    /// session rebuild; a frame from an older generation points into destroyed
    /// pools, so every decoder entry point taking a frame checks this FIRST and
    /// reports the frame stale rather than touching the new pools.
    pub generation: u64,
}

/// Everything that can go wrong. Never panics; device loss is first-class so the
/// session layer can tear down and rebuild.
#[derive(Debug)]
pub enum VkDecodeError {
    /// pf-bitstream could not plan the AU at all.
    Plan(PlanError),
    /// A parameter set has no Std representation (stream-integrity failure).
    Params(ParamsError),
    /// Plan-to-Vulkan conversion failed (caller/session bugs; `CapacityMismatch`
    /// is consumed internally by the rebuild path and only surfaces if the rebuilt
    /// session STILL mismatches).
    Convert(PlanToVkError),
    /// The device's capabilities cannot host any session (demote to the next rung).
    Caps(CapsError),
    /// The handle bundle was rejected.
    Device(DeviceError),
    /// The stream asks for more than this device's caps allow.
    Unsupported(String),
    /// A Vulkan call failed (anything but device loss).
    Vk(vk::Result),
    /// `VK_ERROR_DEVICE_LOST` — every later call fails fast with this until the
    /// owner rebuilds on fresh handles.
    DeviceLost,
    /// A bounded GPU wait expired: the driver is wedged; treat as fatal for this
    /// decoder instance.
    Timeout(&'static str),
    /// Every slot that could host this decode still backs an unreleased frame:
    /// the consumer owes [`VkH264Decoder::release_frame`] calls. Unreachable
    /// under WP-C's one-in/one-out loop (each delivered frame is released before
    /// the next `decode`); reaching it means the AU was planned but NOT decoded —
    /// the caller should release frames and request a keyframe.
    NoFreeSlot,
    /// The frame belongs to an older session generation (its handles point into
    /// destroyed pools). Delivered frames do not survive a stream renegotiation.
    StaleFrame {
        frame_generation: u64,
        current_generation: u64,
    },
    /// No device memory type satisfies an allocation's requirements.
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl std::fmt::Display for VkDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VkDecodeError::Plan(e) => write!(f, "AU planning failed: {e}"),
            VkDecodeError::Params(e) => write!(f, "parameter-set conversion failed: {e}"),
            VkDecodeError::Convert(e) => write!(f, "plan conversion failed: {e}"),
            VkDecodeError::Caps(e) => write!(f, "decode capabilities unusable: {e}"),
            VkDecodeError::Device(e) => write!(f, "device handles rejected: {e}"),
            VkDecodeError::Unsupported(what) => write!(f, "outside device caps: {what}"),
            VkDecodeError::Vk(r) => write!(f, "Vulkan call failed: {r:?}"),
            VkDecodeError::DeviceLost => write!(f, "VK_ERROR_DEVICE_LOST"),
            VkDecodeError::Timeout(what) => {
                write!(f, "GPU wait expired after {DECODE_TIMEOUT_NS} ns: {what}")
            }
            VkDecodeError::NoFreeSlot => {
                write!(
                    f,
                    "every candidate slot backs an unreleased frame — release_frame owed"
                )
            }
            VkDecodeError::StaleFrame {
                frame_generation,
                current_generation,
            } => {
                write!(
                    f,
                    "frame from session generation {frame_generation}, current is \
                     {current_generation} — its handles are gone"
                )
            }
            VkDecodeError::NoMemoryType { type_bits, flags } => {
                write!(
                    f,
                    "no memory type satisfies bits {type_bits:#x} with {flags:?}"
                )
            }
        }
    }
}

impl std::error::Error for VkDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VkDecodeError::Plan(e) => Some(e),
            VkDecodeError::Params(e) => Some(e),
            VkDecodeError::Convert(e) => Some(e),
            VkDecodeError::Caps(e) => Some(e),
            VkDecodeError::Device(e) => Some(e),
            _ => None,
        }
    }
}

impl From<vk::Result> for VkDecodeError {
    fn from(r: vk::Result) -> Self {
        if r == vk::Result::ERROR_DEVICE_LOST {
            VkDecodeError::DeviceLost
        } else {
            VkDecodeError::Vk(r)
        }
    }
}

impl From<PlanError> for VkDecodeError {
    fn from(e: PlanError) -> Self {
        VkDecodeError::Plan(e)
    }
}

impl From<ParamsError> for VkDecodeError {
    fn from(e: ParamsError) -> Self {
        VkDecodeError::Params(e)
    }
}

impl From<CapsError> for VkDecodeError {
    fn from(e: CapsError) -> Self {
        VkDecodeError::Caps(e)
    }
}

impl From<DeviceError> for VkDecodeError {
    fn from(e: DeviceError) -> Self {
        VkDecodeError::Device(e)
    }
}

impl From<SessionError> for VkDecodeError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Vk(r) => VkDecodeError::from(r),
            SessionError::Params(p) => VkDecodeError::Params(p),
            SessionError::NoMemoryType { type_bits, flags } => {
                VkDecodeError::NoMemoryType { type_bits, flags }
            }
        }
    }
}

impl From<AllocError> for VkDecodeError {
    fn from(e: AllocError) -> Self {
        match e {
            AllocError::Vk(r) => VkDecodeError::from(r),
            AllocError::NoMemoryType { type_bits, flags } => {
                VkDecodeError::NoMemoryType { type_bits, flags }
            }
        }
    }
}

/// Query pool + command pool/buffers, one op slot per output slot. Owns and
/// destroys its Vulkan objects.
struct OpRing {
    device: ash::Device,
    query_pool: vk::QueryPool,
    cmd_pool: vk::CommandPool,
    cmds: Vec<vk::CommandBuffer>,
}

impl OpRing {
    /// # Safety
    ///
    /// `dev` wraps live handles ([`DeviceHandles`] contract).
    unsafe fn create(
        dev: &DecodeDevice,
        std_profile_idc: hh::StdVideoH264ProfileIdc,
        op_slots: u32,
    ) -> Result<Self, vk::Result> {
        let mut chain = H264ProfileChain::new(std_profile_idc);
        let profile = chain.wire();
        let mut query_ci = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::RESULT_STATUS_ONLY_KHR)
            .query_count(op_slots);
        // Chained manually: `push_next` would clobber the profile's own `p_next`
        // (its H264 half) — the encoder's exact precedent for this trap.
        query_ci.p_next = (profile as *const vk::VideoProfileInfoKHR<'_>).cast();
        // SAFETY: live device; `query_ci` roots the wired chain for the call. The
        // video profile chained in satisfies the "same profile as the session"
        // rule for queries used inside a coding scope.
        let query_pool = unsafe { dev.ash().create_query_pool(&query_ci, None)? };

        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(dev.decode_qf())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: live device; unwind destroys the query pool on failure.
        let cmd_pool = match unsafe { dev.ash().create_command_pool(&pool_ci, None) } {
            Ok(p) => p,
            Err(e) => {
                // SAFETY: destroying the just-created query pool.
                unsafe { dev.ash().destroy_query_pool(query_pool, None) };
                return Err(e);
            }
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(op_slots);
        // SAFETY: live device + the pool created above; unwind destroys both pools
        // (destroying the command pool frees any allocated buffers).
        let cmds = match unsafe { dev.ash().allocate_command_buffers(&alloc) } {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: destroying the two pools created above.
                unsafe {
                    dev.ash().destroy_command_pool(cmd_pool, None);
                    dev.ash().destroy_query_pool(query_pool, None);
                }
                return Err(e);
            }
        };
        Ok(Self {
            device: dev.ash().clone(),
            query_pool,
            cmd_pool,
            cmds,
        })
    }
}

impl Drop for OpRing {
    fn drop(&mut self) {
        // SAFETY: own handles on the contract-live device; the owning decoder
        // drains GPU work before dropping state. Destroying the command pool frees
        // its buffers; both destroys ignore NULL.
        unsafe {
            self.device.destroy_command_pool(self.cmd_pool, None);
            self.device.destroy_query_pool(self.query_pool, None);
        }
    }
}

/// Everything tied to ONE session generation. A stream renegotiation (extent, DPB
/// depth, profile) drops and rebuilds the whole struct.
struct SessionState {
    session: VideoSession,
    slots: SlotMap,
    pool: ImagePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Last-known Std reference info per DPB slot — `vkCmdBeginVideoCodingKHR`
    /// wants codec reference info for EVERY bound slot, including ones this AU's
    /// slices do not reference; refreshed from each plan's setup/ref entries so
    /// marking transitions (e.g. MMCO long-term promotion) propagate.
    slot_refs: Vec<Option<hh::StdVideoDecodeH264ReferenceInfo>>,
    /// Distinct-mode output ring cursor (unused in coincide mode).
    out_cursor: usize,
    /// Live-frame counts per OUTPUT slot: every [`DecodedVkFrame`] built over the
    /// slot (pending, ready, or delivered-and-unreleased) counts one; the slot is
    /// not reusable while nonzero. The coincide twin of this gate additionally
    /// pins the [`SlotMap`] so `plan_to_vk`'s setup assignment skips the slot.
    live_frames: Vec<u32>,
}

impl SessionState {
    /// Count a new frame over `out_slot` (and pin its DPB slot in coincide mode,
    /// where output slot == DPB slot).
    fn note_frame_live(&mut self, out_slot: usize) {
        self.live_frames[out_slot] += 1;
        if self.pool.coincide {
            // Output slots mirror DPB slots one-to-one in coincide mode; the
            // envelope-gated capacity (<= 17) keeps the index within u8.
            self.slots.pin(out_slot as u8);
        }
    }

    /// Un-count a frame over `out_slot` (release or internal drop).
    fn note_frame_dead(&mut self, out_slot: usize) {
        match self.live_frames[out_slot].checked_sub(1) {
            Some(remaining) => self.live_frames[out_slot] = remaining,
            None => {
                debug!(out_slot, "frame released more often than counted");
                return;
            }
        }
        if self.pool.coincide && !self.slots.unpin(out_slot as u8) {
            debug!(out_slot, "coincide slot unpinned without a pin");
        }
    }
}

/// The native Vulkan Video H.264 decoder.
pub struct VkH264Decoder {
    dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    planner: H264Planner,
    /// Caps per Std profile idc, queried once per profile.
    caps: Option<(hh::StdVideoH264ProfileIdc, DecodeCaps)>,
    state: Option<SessionState>,
    /// Decoded pictures awaiting their planner output verdict, keyed by [`PicId`].
    pending_frames: BTreeMap<PicId, DecodedVkFrame>,
    /// Display-ready frames not yet handed out (under the zero-reorder punktfunk
    /// envelope at most one per AU; deeper only around discontinuities/flushes).
    ready: VecDeque<DecodedVkFrame>,
    /// Session generation: bumped on every rebuild, stamped into frames so stale
    /// ones are detectable ([`DecodedVkFrame::generation`]).
    generation: u64,
    device_lost: bool,
    /// The last `decode` call's plan warnings (concealment signals), held for
    /// [`Self::take_warnings`] — the integration layer's recovery hook. Cleared at
    /// every `decode` entry so a warning is never attributed to the wrong AU.
    last_warnings: Vec<PlanWarning>,
}

impl VkH264Decoder {
    /// Wrap the borrowed device. Sessions/pools are built lazily from the first
    /// AU's SPS (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The full [`DeviceHandles`] caller contract (liveness, enabled extensions
    /// and features, truthful queue families) — held for this decoder's whole
    /// lifetime, not just this call.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        Ok(Self {
            dev,
            lock,
            planner: H264Planner::new(),
            caps: None,
            state: None,
            pending_frames: BTreeMap::new(),
            ready: VecDeque::new(),
            generation: 0,
            device_lost: false,
            last_warnings: Vec::new(),
        })
    }

    /// Decode one access unit. Returns the next display-ready frame, if the
    /// planner declared one (zero-reorder streams: the AU's own picture).
    ///
    /// Never panics. `VkDecodeError::DeviceLost` latches: every later call fails
    /// fast until the owner rebuilds the decoder on fresh handles.
    pub fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        self.last_warnings.clear();
        if self.device_lost {
            return Err(VkDecodeError::DeviceLost);
        }
        let result = self.decode_inner(au);
        if matches!(result, Err(VkDecodeError::DeviceLost)) {
            self.device_lost = true;
        }
        result
    }

    /// The plan warnings (concealment signals) of the most recent [`Self::decode`]
    /// call, taken. Non-empty means the AU was planned around missing/damaged
    /// references — the picture decodes but its content is concealed, and the caller
    /// should request a re-anchor (the recovery wiring the module doc reserves for
    /// the integration layer).
    pub fn take_warnings(&mut self) -> Vec<PlanWarning> {
        std::mem::take(&mut self.last_warnings)
    }

    /// The current session generation ([`DecodedVkFrame::generation`]'s counterpart):
    /// lets a caller holding delivered frames tell a STALE frame (its session was
    /// rebuilt — every decoder entry point would report it so) apart from a live one,
    /// without tripping the conservative `Failed` a stale status poll returns.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        let plan = self.planner.plan_au(au)?;
        for warning in &plan.warnings {
            // The recovery verdict is the integration layer's ([`Self::take_warnings`]);
            // never silent here though.
            trace!(?warning, "plan warning");
        }
        self.last_warnings = plan.warnings.clone();

        self.ensure_state(&plan)?;
        let sps_id = plan.sps.seq_parameter_set_id;

        // Convert, with ONE rebuild retry on CapacityMismatch — the designed
        // trigger for a DPB-depth renegotiation (pic.rs docs).
        let mut vk_plan: Option<DecodePlanVk> = None;
        for attempt in 0..2 {
            // A parameters RECREATE destroys the old object, which an in-flight
            // decode may still be executing against: drain first. Recreate is a
            // parameter-set content change under a stable id — rare enough (an
            // encoder reconfiguration) that the stall is the right trade.
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
            match plan_to_vk(&plan, &mut state.slots, sps_id) {
                Ok(converted) => {
                    vk_plan = Some(converted);
                    break;
                }
                Err(PlanToVkError::CapacityMismatch { required, capacity }) if attempt == 0 => {
                    debug!(
                        required,
                        capacity, "DPB depth renegotiated — rebuilding session"
                    );
                    self.rebuild_state(&plan)?;
                }
                Err(PlanToVkError::Slot(SlotError::AllPinned { free })) => {
                    // Every free slot backs an unreleased frame. A GPU drain
                    // first (bounded — it costs nothing on this error path and
                    // rules out any in-flight hold), but consumer pins clear
                    // ONLY via release_frame, so the verdict stands: explicit
                    // backpressure. The AU was planned but not decoded; the
                    // caller releases frames and requests recovery. Unreachable
                    // under the one-in/one-out release loop.
                    debug!(free, "no unpinned DPB slot — release_frame owed");
                    self.drain_gpu()?;
                    return Err(VkDecodeError::NoFreeSlot);
                }
                Err(e) => return Err(VkDecodeError::Convert(e)),
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

        // Output slot: the setup slot itself (coincide — output IS the DPB
        // picture, and the pin layer above already guaranteed it backs no live
        // frame) or the next FREE ring slot (distinct — slots with live frames
        // are skipped; all-busy is the same backpressure verdict as AllPinned).
        let out_slot = if state.pool.coincide {
            usize::from(vk_plan.setup_slot)
        } else {
            let ring_len = state.pool.outputs.len();
            let mut chosen = None;
            for _ in 0..ring_len {
                let candidate = state.out_cursor;
                state.out_cursor = (state.out_cursor + 1) % ring_len;
                if state.live_frames[candidate] == 0 {
                    chosen = Some(candidate);
                    break;
                }
            }
            match chosen {
                Some(slot) => slot,
                None => {
                    debug!("every output-ring slot backs an unreleased frame");
                    self.drain_gpu()?;
                    return Err(VkDecodeError::NoFreeSlot);
                }
            }
        };
        let state = self.state.as_mut().expect("ensured above");
        // The op slot's command buffer + query must not still be in flight, and
        // (distinct mode) neither may the output image.
        let prev = (
            state.pool.outputs[out_slot].semaphore,
            state.pool.outputs[out_slot].value,
        );
        // SAFETY: live device; the semaphore is the pool's own.
        unsafe { wait_timeline(self.dev.ash(), prev.0, prev.1, "output slot reuse")? };

        // Upload the AU (recycles/grows against the same timeline facts).
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
        // SAFETY: live device; the pending tokens cover their slots' GPU reads by
        // construction (every submit signals its output slot's semaphore and marks
        // its bitstream slot with that pair).
        let upload = unsafe { state.ring.upload(&self.dev, au, &mut poll, &mut wait)? };

        // Record + submit, signalling the output slot's next timeline value.
        let signal_value = state.pool.outputs[out_slot].value + 1;
        // SAFETY: live device; every handle recorded below belongs to this
        // session generation, and the AU sits uploaded in the ring slot.
        unsafe {
            record_and_submit(
                &self.dev,
                &*self.lock,
                state,
                &vk_plan,
                &upload,
                out_slot,
                signal_value,
            )?;
        }
        state.pool.outputs[out_slot].value = signal_value;
        state.ring.pending.set_pending(
            upload.slot,
            (state.pool.outputs[out_slot].semaphore, signal_value),
        );

        // Refresh the per-slot reference cache from this AU's facts.
        state.slot_refs[usize::from(vk_plan.setup_slot)] = Some(vk_plan.setup_ref);
        for r in &vk_plan.refs {
            state.slot_refs[usize::from(r.slot)] = Some(r.std);
        }

        // Frame bookkeeping: the decoded picture waits for its output verdict,
        // and counts as LIVE over its slot from this moment (two-phase release:
        // the slot is reusable only after the DPB removed the picture AND
        // release_frame ran / the frame was dropped internally).
        let out = &state.pool.outputs[out_slot];
        let frame = DecodedVkFrame {
            image: out.image,
            view: out.view,
            plane_views: out.plane_views,
            layer: out.layer,
            layout: if state.pool.coincide {
                vk::ImageLayout::VIDEO_DECODE_DPB_KHR
            } else {
                vk::ImageLayout::VIDEO_DECODE_DST_KHR
            },
            coded_width: plan.picture.coded_width,
            coded_height: plan.picture.coded_height,
            crop: plan.picture.display_crop,
            semaphore: out.semaphore,
            value: signal_value,
            poc: plan.picture.pic_order_cnt,
            is_idr: plan.picture.is_idr,
            query_slot: out_slot as u32,
            generation: self.generation,
        };
        state.note_frame_live(out_slot);
        self.pending_frames.insert(vk_plan.setup_id, frame);

        // The plan's DPB verdicts over the pending map: outputs become ready,
        // removed-but-never-output ids (no_output_of_prior_pics_flag discards)
        // are DROPPED — releasing their slots, not leaking their frames.
        let (ready, dropped) = settle_dpb(&mut self.pending_frames, &plan.dpb);
        for frame in ready {
            self.ready.push_back(frame);
        }
        for frame in dropped {
            debug!(
                poc = frame.poc,
                slot = frame.query_slot,
                "picture removed without output — dropping its frame"
            );
            state.note_frame_dead(frame.query_slot as usize);
        }
        Ok(self.ready.pop_front())
    }

    /// Hand a delivered frame back: its slot becomes reusable (once the planner's
    /// DPB has also removed the picture). Every frame `decode`/`take_ready`
    /// returns MUST come back through here exactly once — until then its image is
    /// protected from every decode target and the pipeline eventually reports
    /// [`VkDecodeError::NoFreeSlot`] instead of overwriting it.
    pub fn release_frame(&mut self, frame: &DecodedVkFrame) -> Result<(), VkDecodeError> {
        if frame.generation != self.generation {
            // The pools this frame indexed are gone; there is nothing to release.
            return Err(VkDecodeError::StaleFrame {
                frame_generation: frame.generation,
                current_generation: self.generation,
            });
        }
        let Some(state) = &mut self.state else {
            return Err(VkDecodeError::StaleFrame {
                frame_generation: frame.generation,
                current_generation: self.generation,
            });
        };
        let slot = frame.query_slot as usize;
        if slot >= state.live_frames.len() {
            return Err(VkDecodeError::StaleFrame {
                frame_generation: frame.generation,
                current_generation: self.generation,
            });
        }
        state.note_frame_dead(slot);
        Ok(())
    }

    /// A display-ready frame beyond the one `decode` returned, if any (only
    /// non-empty around discontinuities/flushes — the punktfunk envelope is
    /// zero-reorder).
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// Read `frame`'s decode status WITHOUT waiting.
    ///
    /// [`DecodeStatus::Failed`] covers driver-reported errors AND a query slot
    /// recycled before it was read (the status is then unprovable — same
    /// conservative verdict), so poll before the pipeline wraps a ring.
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    /// [`Self::poll_status`], but WAITs for the op to complete first — the only
    /// place a status read blocks (the GPU smoke test's assertion path; WP-C's
    /// steady state polls).
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
        let slot = frame.query_slot as usize;
        if slot >= state.pool.outputs.len() || state.pool.outputs[slot].value != frame.value {
            trace!(
                slot,
                "status query slot recycled before it was read — unprovable, reported Failed"
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
        // output ring it is sized to).
        let result = unsafe {
            self.dev.ash().get_query_pool_results(
                state.ops.query_pool,
                frame.query_slot,
                &mut status,
                flags,
            )
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

    /// Drain the planner (teardown / stream discontinuity): every buffered
    /// picture becomes display-ready via [`Self::take_ready`] (those frames stay
    /// live until released), all DPB slots free, and any picture removed without
    /// ever reaching output has its frame dropped and its slot un-counted.
    pub fn flush(&mut self) {
        let update = self.planner.flush();
        let (ready, dropped) = settle_dpb(&mut self.pending_frames, &update);
        if let Some(state) = &mut self.state {
            state.slots.apply(&update);
            for frame in &dropped {
                state.note_frame_dead(frame.query_slot as usize);
            }
            // Defensive: a pending frame neither output nor removed should not
            // exist (flush drains everything); un-count any leftover.
            for (_, frame) in std::mem::take(&mut self.pending_frames) {
                debug!(poc = frame.poc, "pending frame survived a flush — dropped");
                state.note_frame_dead(frame.query_slot as usize);
            }
        } else {
            self.pending_frames.clear();
        }
        for frame in ready {
            self.ready.push_back(frame);
        }
    }

    /// Session/caps for THIS plan exist and match its extent + profile, and the
    /// stream sits inside the device's level ceiling. DPB-depth mismatches
    /// surface later as `plan_to_vk`'s `CapacityMismatch` (the designed trigger)
    /// and take the same rebuild path.
    fn ensure_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        let std_profile = std_profile_for(plan)?;
        if self.caps.as_ref().map(|(p, _)| *p) != Some(std_profile) {
            // SAFETY: live device (constructor contract).
            let raw =
                unsafe { query_h264_caps(&self.dev, std_profile) }.map_err(VkDecodeError::from)?;
            self.caps = Some((std_profile, derive_caps(&raw)?));
        }
        // The level gate: a stream above the device's maxLevelIdc is refused up
        // front (Std code points ascend with the level, so the comparison is
        // numeric), never submitted on a hope.
        let caps_max_level = self.caps.as_ref().expect("queried above").1.max_level_idc;
        let stream_level = level_to_std(plan.picture.level_idc);
        if stream_level > caps_max_level {
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
            // Compared against the STREAM's coded extent (the pool tracks it
            // beside its granularity-rounded image extent).
            Some(state)
                if state.pool.coded_extent == coded
                    && state.session.config.std_profile_idc == std_profile =>
            {
                Ok(())
            }
            _ => self.rebuild_state(plan),
        }
    }

    /// Tear down the current session generation (draining its GPU work) and build
    /// a fresh one shaped by `plan`, bumping [`Self::generation`] so frames of the
    /// old one are detectably stale.
    fn rebuild_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        self.drain_gpu()?;
        if self.state.is_some() {
            debug!("rebuilding decode session (stream renegotiation)");
        }
        // Frames referencing the old pools die with them (their generation stamp
        // makes any copy the consumer still holds report stale, never read).
        if !self.pending_frames.is_empty() || !self.ready.is_empty() {
            debug!(
                pending = self.pending_frames.len(),
                ready = self.ready.len(),
                "dropping undelivered frames across a session rebuild"
            );
            self.pending_frames.clear();
            self.ready.clear();
        }
        self.state = None;
        self.generation += 1;

        let (std_profile, caps) = self.caps.as_ref().expect("ensure_state queried caps");
        let std_profile = *std_profile;
        let dpb_slots = plan.picture.max_dpb_frames as u32 + 1;
        if dpb_slots > caps.max_dpb_slots {
            return Err(VkDecodeError::Unsupported(format!(
                "stream needs {dpb_slots} DPB slots, device caps at {}",
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

        let config = SessionConfig {
            max_coded_extent: image_extent,
            max_dpb_slots: dpb_slots,
            max_active_references: (dpb_slots - 1).min(caps.max_active_references),
            std_profile_idc: std_profile,
        };
        let pool_plan = plan_pools(caps, dpb_slots, OUTPUT_RING);
        // SAFETY: live device per the constructor contract, for every create in
        // this block; each created half is owned by a Drop type the moment it
        // exists, so a mid-build failure unwinds cleanly.
        let state = unsafe {
            let session = VideoSession::create(&self.dev, caps, config)?;
            let pool = ImagePool::create(&self.dev, caps, &pool_plan, coded, std_profile)
                .map_err(VkDecodeError::from)?;
            let ring = BitstreamRing::create(
                &self.dev,
                RingLayout::new(
                    INITIAL_SLOT_SIZE,
                    RING_SLOTS,
                    caps.min_bitstream_offset_alignment,
                    caps.min_bitstream_size_alignment,
                ),
                std_profile,
            )
            .map_err(VkDecodeError::from)?;
            let ops = OpRing::create(&self.dev, std_profile, pool_plan.output_slots)
                .map_err(VkDecodeError::from)?;
            SessionState {
                session,
                slots: SlotMap::new(plan.picture.max_dpb_frames),
                slot_refs: vec![None; dpb_slots as usize],
                live_frames: vec![0; pool_plan.output_slots as usize],
                pool,
                ring,
                ops,
                out_cursor: 0,
            }
        };
        self.state = Some(state);
        Ok(())
    }

    /// Wait out every in-flight decode of the current session generation.
    fn drain_gpu(&mut self) -> Result<(), VkDecodeError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        for out in &state.pool.outputs {
            // SAFETY: live device; pool-owned semaphore.
            unsafe { wait_timeline(self.dev.ash(), out.semaphore, out.value, "session drain")? };
        }
        Ok(())
    }
}

impl Drop for VkH264Decoder {
    fn drop(&mut self) {
        // Best-effort drain so the pools' Drop impls never destroy in-flight
        // objects; a wedged driver falls through after the bounded timeout (the
        // destroys then race the GPU, but the alternative is hanging teardown
        // forever — same trade the encoder's fence budget makes).
        if let Err(e) = self.drain_gpu() {
            debug!(error = %e, "drain on drop failed; tearing down anyway");
        }
    }
}

/// Map the plan's `profile_idc` to the Std code point (identity for the four
/// Vulkan-representable profiles, reject otherwise — WP-A's exact rule).
fn std_profile_for(plan: &AuPlan) -> Result<hh::StdVideoH264ProfileIdc, VkDecodeError> {
    match u32::from(plan.picture.profile_idc) {
        p @ (66 | 77 | 100 | 244) => Ok(p),
        _ => Err(VkDecodeError::Params(ParamsError::UnmappableProfileIdc(
            plan.picture.profile_idc,
        ))),
    }
}

/// Split one [`DpbUpdate`]'s verdicts over the pending-frame map: `outputs` (in
/// bump order) become ready; `removed` ids that never reached output — an IDR's
/// `no_output_of_prior_pics_flag` discard, or a flush racing a drop — are
/// returned separately so the caller releases their slots instead of leaking
/// them in the map forever. Pure and generic for testability.
fn settle_dpb<F>(pending: &mut BTreeMap<PicId, F>, dpb: &DpbUpdate) -> (Vec<F>, Vec<F>) {
    let mut ready = Vec::new();
    for id in &dpb.outputs {
        match pending.remove(id) {
            Some(frame) => ready.push(frame),
            // Ids planned before this decoder existed (post-recovery), or
            // dropped across a rebuild: display-order gaps, not errors.
            None => trace!(id, "output id without a pending frame"),
        }
    }
    let dropped = dpb
        .removed
        .iter()
        .filter_map(|id| pending.remove(id))
        .collect();
    (ready, dropped)
}

/// Bounded timeline wait (no-op for the never-signalled value 0).
///
/// # Safety
///
/// `device` is live and `semaphore` is a live timeline semaphore on it.
unsafe fn wait_timeline(
    device: &ash::Device,
    semaphore: vk::Semaphore,
    value: u64,
    what: &'static str,
) -> Result<(), VkDecodeError> {
    if value == 0 {
        return Ok(());
    }
    let semaphores = [semaphore];
    let values = [value];
    let info = vk::SemaphoreWaitInfo::default()
        .semaphores(&semaphores)
        .values(&values);
    // SAFETY: fn contract; the info arrays are locals outliving the call.
    match unsafe { device.wait_semaphores(&info, DECODE_TIMEOUT_NS) } {
        Ok(()) => Ok(()),
        Err(vk::Result::TIMEOUT) => Err(VkDecodeError::Timeout(what)),
        Err(e) => Err(VkDecodeError::from(e)),
    }
}

/// Record one decode op into the out-slot's command buffer and submit it under
/// the queue lock with the timeline signal.
///
/// # Safety
///
/// Live device; `state` is the current session generation with `vk_plan` derived
/// against its `SlotMap` and the AU resident in `upload`'s ring slot; the out
/// slot's previous use has completed (caller waited its timeline value).
#[allow(clippy::too_many_arguments)]
unsafe fn record_and_submit(
    dev: &DecodeDevice,
    lock: &dyn QueueLock,
    state: &mut SessionState,
    vk_plan: &DecodePlanVk,
    upload: &UploadedAu,
    out_slot: usize,
    signal_value: u64,
) -> Result<(), VkDecodeError> {
    let device = dev.ash();
    let cmd = state.ops.cmds[out_slot];
    let coded_extent = state.pool.coded_extent;

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
    // The setup target layer is fully overwritten: discard via UNDEFINED, with an
    // execution+memory dependency on earlier ops that touched the layer.
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
    let (setup_image, setup_layer) = state.pool.dpb_target(vk_plan.setup_slot);
    let mut image_barriers = vec![decode_layer_barrier(
        setup_image,
        setup_layer,
        vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
    )];
    if !state.pool.coincide {
        let out = &state.pool.outputs[out_slot];
        image_barriers.push(decode_layer_barrier(
            out.image,
            out.layer,
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
    // SAFETY: recording; the pool is sized to the output ring (fn contract).
    unsafe { device.cmd_reset_query_pool(cmd, state.ops.query_pool, out_slot as u32, 1) };

    // ---- bound-slot staging ----
    // Scope list: this AU's references first, then every other still-held slot
    // (their resources must stay bound for their associations to persist), then
    // the setup slot as the ACTIVATION entry (slot index -1 binds its resource
    // without a current association; the decode op's setup slot then claims it).
    let mut scope: Vec<(i32, u8, hh::StdVideoDecodeH264ReferenceInfo)> = Vec::new();
    for r in &vk_plan.refs {
        scope.push((i32::from(r.slot), r.slot, r.std));
    }
    for (slot, _id) in state.slots.held() {
        if slot == vk_plan.setup_slot || scope.iter().any(|(_, s, _)| *s == slot) {
            continue;
        }
        match state.slot_refs[usize::from(slot)] {
            Some(std) => scope.push((i32::from(slot), slot, std)),
            // Unreachable in practice: every held slot was a setup slot once.
            None => trace!(
                slot,
                "held slot without cached reference info — left unbound"
            ),
        }
    }
    let reference_count = vk_plan.refs.len();
    scope.push((-1, vk_plan.setup_slot, vk_plan.setup_ref));

    // Staged arrays: resources → std infos → codec slot infos → slot infos. Each
    // vector is fully built before the next borrows it, so nothing reallocates
    // under a stored pointer.
    let resources: Vec<vk::VideoPictureResourceInfoKHR<'_>> = scope
        .iter()
        .map(|&(_, slot, _)| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded_extent)
                .base_array_layer(0)
                .image_view_binding(state.pool.dpb_view(slot))
        })
        .collect();
    let std_refs: Vec<hh::StdVideoDecodeH264ReferenceInfo> =
        scope.iter().map(|&(_, _, std)| std).collect();
    let mut dpb_infos: Vec<vk::VideoDecodeH264DpbSlotInfoKHR<'_>> = std_refs
        .iter()
        .map(|std| vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(std))
        .collect();
    let mut begin_slots: Vec<vk::VideoReferenceSlotInfoKHR<'_>> = Vec::with_capacity(scope.len());
    for (index, &(slot_index, _, _)) in scope.iter().enumerate() {
        begin_slots.push(
            vk::VideoReferenceSlotInfoKHR::default()
                .slot_index(slot_index)
                .picture_resource(&resources[index]),
        );
    }
    for (slot_info, dpb_info) in begin_slots.iter_mut().zip(dpb_infos.iter_mut()) {
        *slot_info = (*slot_info).push_next(dpb_info);
    }
    // The decode op's reference list: exactly this AU's references (the first
    // `reference_count` scope entries, which carry their real slot indices).
    let decode_refs: Vec<vk::VideoReferenceSlotInfoKHR<'_>> =
        begin_slots[..reference_count].to_vec();

    // The setup slot as the decode op sees it: its REAL index (the begin list's
    // twin entry carries -1), same resource, its own codec info chain.
    let setup_std = vk_plan.setup_ref;
    let mut setup_dpb = vk::VideoDecodeH264DpbSlotInfoKHR::default().std_reference_info(&setup_std);
    let setup_resource = resources[scope.len() - 1];
    let setup_slot_info = vk::VideoReferenceSlotInfoKHR::default()
        .slot_index(i32::from(vk_plan.setup_slot))
        .picture_resource(&setup_resource)
        .push_next(&mut setup_dpb);

    // Decode destination: the setup picture itself (coincide) or the output image.
    let dst_resource = if state.pool.coincide {
        setup_resource
    } else {
        vk::VideoPictureResourceInfoKHR::default()
            .coded_extent(coded_extent)
            .base_array_layer(0)
            .image_view_binding(state.pool.outputs[out_slot].view)
    };

    let std_pic = vk_plan.std_pic;
    let mut h264_pic = vk::VideoDecodeH264PictureInfoKHR::default()
        .std_picture_info(&std_pic)
        .slice_offsets(&vk_plan.slice_offsets);
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
        device.cmd_begin_query(
            cmd,
            state.ops.query_pool,
            out_slot as u32,
            vk::QueryControlFlags::empty(),
        );
        (dev.video_decode_queue().fp().cmd_decode_video_khr)(cmd, &decode_info);
        device.cmd_end_query(cmd, state.ops.query_pool, out_slot as u32);
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
    let signals = [vk::SemaphoreSubmitInfo::default()
        .semaphore(state.pool.outputs[out_slot].semaphore)
        .value(signal_value)
        .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
    let submits = [vk::SubmitInfo2::default()
        .command_buffer_infos(&cmd_infos)
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
    use super::*;

    #[test]
    fn settle_dpb_readies_outputs_in_order_and_returns_never_output_removals() {
        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(1, 100);
        pending.insert(2, 200);
        pending.insert(3, 300);

        // Picture 1 outputs (and is also removed — the normal bump); picture 2
        // is removed WITHOUT ever reaching output (no_output_of_prior_pics):
        // its frame must come back as dropped, not leak in the map.
        let update = DpbUpdate {
            stored: Some(3),
            outputs: vec![1],
            removed: vec![1, 2],
        };
        let (ready, dropped) = settle_dpb(&mut pending, &update);
        assert_eq!(ready, vec![100]);
        assert_eq!(dropped, vec![200]);
        assert_eq!(
            pending.keys().copied().collect::<Vec<_>>(),
            vec![3],
            "the still-buffered picture stays pending"
        );

        // Output order is bump order, and unknown ids are tolerated.
        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(5, 500);
        pending.insert(4, 400);
        let update = DpbUpdate {
            stored: None,
            outputs: vec![5, 99, 4],
            removed: vec![],
        };
        let (ready, dropped) = settle_dpb(&mut pending, &update);
        assert_eq!(ready, vec![500, 400], "bump order, not id order");
        assert!(dropped.is_empty());
    }

    #[test]
    fn std_level_code_points_ascend_so_the_max_level_gate_compares_numerically() {
        use pf_bitstream::h264::Level;
        // The gate is `level_to_std(stream) > caps.max_level_idc`; that is only
        // sound if the Std code points ascend with the level. Pin the ordering
        // across the range (and the 1b fold onto 1.1).
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

        // The gate itself, on both sides of a ceiling.
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
