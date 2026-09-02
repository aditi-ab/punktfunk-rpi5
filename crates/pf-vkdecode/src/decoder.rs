//! Native Vulkan Video H.264 decoder: planner, session, and decode-queue submit.
//!
//! Each AU is planned, converted, packed into the bitstream ring, recorded
//! (bound DPB slots, one-time session RESET, a `RESULT_STATUS_ONLY` query
//! around `vkCmdDecodeVideoKHR`), then submitted under the caller's
//! [`QueueLock`] with a per-image timeline signal.
//!
//! Decode targets come from a picture pool decoupled from DPB slots
//! ([`crate::images`]). A slot binds a free image at activation so a delivered
//! picture is never a decode target while the consumer reads it. The decoder
//! signals `value+1` at write; the presenter waits, samples, restores layout,
//! and signals `value+1` again; [`VkH264Decoder::release_frame`] reports that
//! write-back before the image's next use.
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
use pf_bitstream::h264::ColourDescription;
use pf_bitstream::h264::DisplayCrop;
use pf_bitstream::h264::DpbUpdate;
use pf_bitstream::h264::H264Planner;
use pf_bitstream::h264::PicId;
use pf_bitstream::h264::PlanError;
use pf_bitstream::h264::PlanWarning;
use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::caps::derive_caps;
use crate::caps::query_h264_caps;
use crate::caps::CapsError;
use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::device::AllocError;
use crate::device::DecodeDevice;
use crate::device::DeviceError;
use crate::device::DeviceHandles;
use crate::device::QueueLock;
use crate::device::QueueSubmitGuard;
use crate::images::plan_pools;
use crate::images::DpbPool;
use crate::images::PicturePool;
use crate::images::HOLD_HEADROOM;
use crate::params::level_to_std;
use crate::params::ParamsError;
use crate::params_av1::ParamsAv1Error;
use crate::params_h265::H265ParamsError;
use crate::pic::plan_to_vk;
use crate::pic::DecodePlanVk;
use crate::pic::PlanToVkError;
use crate::pic_av1::PlanToVkAv1Error;
use crate::pic_h265::PlanToVkH265Error;
use crate::ring::pack_slices;
use crate::ring::BitstreamRing;
use crate::ring::RingLayout;
use crate::ring::UploadedAu;
use crate::ring::INITIAL_SLOT_SIZE;
use crate::ring::RING_SLOTS;
use crate::session::ParamsAction;
use crate::session::SessionConfig;
use crate::session::SessionError;
use crate::session::VideoSession;
use crate::slots::SlotMap;

/// 5 s: longer than a real decode, finite against a wedged driver. Matches the
/// encoder fence budget so session recovery is never parked forever.
const DECODE_TIMEOUT_NS: u64 = 5_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeStatus {
    Pending,
    Ok,
    /// Error, recycled query, or device lost: content is unproven; conceal
    /// (`want_keyframe`).
    Failed,
}

/// Display-ready pool image; the decoder does not touch it until
/// [`VkH264Decoder::release_frame`].
///
/// Pixels ready when `semaphore` reaches [`Self::value`]. A sampler must, in
/// the same submission that waits `value`, signal `value + 1` after reads and
/// layout restore, then `release_frame(frame, true)`. Drop unsampled with
/// `false`. Release every frame once, including stale-generation ones (graveyard).
#[derive(Debug, Clone)]
pub struct DecodedVkFrame {
    pub image: vk::Image,
    /// Caps-resolved `output_format` of the session that decoded this picture;
    /// [`Self::view`] aliases it. Do not assume 8-bit 4:2:0: H.265 Main 10 is
    /// [`crate::P010`], RExt 4:4:4 is [`crate::YUV444_8`] / [`crate::YUV444_10`],
    /// and a renegotiation can change format mid-stream. [`crate::plane_formats`]
    /// maps this to [`Self::plane_views`].
    pub format: vk::Format,
    pub view: vk::ImageView,
    /// Presenter sampler views; formats from [`crate::plane_formats`] on
    /// [`Self::format`].
    pub plane_views: [vk::ImageView; 2],
    /// Always 0 — pool images are single-layer (kept for the consumer ABI).
    pub layer: u32,
    /// Layout at the semaphore signal, and the layout the consumer must restore
    /// after sampling: `VIDEO_DECODE_DPB_KHR` (coincide) or `VIDEO_DECODE_DST_KHR`.
    pub layout: vk::ImageLayout,
    /// Allocated extent (`pictureAccessGranularity`-aligned). UV-scale math
    /// divides by this (1088-row class); display region is [`Self::crop`].
    pub coded_width: u32,
    pub coded_height: u32,
    pub crop: DisplayCrop,
    /// Active-SPS colour; per frame like [`Self::crop`]. A new SPS can switch
    /// HDR mid-stream. Unspecified VUI is inferred by pf-bitstream.
    pub colour: ColourDescription,
    pub semaphore: vk::Semaphore,
    pub value: u64,
    pub poc: i32,
    pub is_idr: bool,
    /// Recovery-point SEI for this AU (and any outstanding one before it); see
    /// [`crate::recovery`]. `NONE` when the stream carries none.
    ///
    /// [`Self::is_idr`] cannot answer for intra-refresh: the wave never emits an
    /// IDR, so a consumer freezing on loss has no decoder-visible clean point.
    pub recovery: crate::recovery::RecoveryMark,
    /// Decode-order ordinal stamped at plan time (1 for the first picture).
    /// Survives session rebuilds: it describes the stream, not Vulkan objects.
    /// Delivery order is not decode order; compare against the ordinal current
    /// at freeze-arm so a flushed pre-loss picture cannot lift the freeze.
    pub decode_order: u64,
    /// Whole prediction chain was fully available
    /// ([`pf_bitstream::h264::PicturePlan::references_clean`]). `true` for
    /// IDR/IRAP/key and any picture whose chain is clean; `false` once this AU
    /// or an ancestor needed concealment.
    ///
    /// Corroborates a host `USER_FLAG_RECOVERY_ANCHOR`: the host infers
    /// known-good from what the client received; this is what actually decoded.
    /// Ignore if you have no such claim to check.
    pub references_clean: bool,
    pub query_slot: u32,
    /// Submission ordinal: the query slot is stale if re-armed since.
    pub submission: u64,
    pub picture: u32,
    /// Session generation; `release_frame` routes by this (current vs graveyard).
    pub generation: u64,
}

/// Decode failure. Never panics; device loss is first-class so the session
/// layer can tear down and rebuild.
#[derive(Debug)]
pub enum VkDecodeError {
    Plan(PlanError),
    /// H.265 plan failure. `h265::PlanError::RaslSkipped` is not this: the
    /// decoder answers `Ok(None)` for a RASL after an open-GOP join.
    PlanH265(pf_bitstream::h265::PlanError),
    /// Parameter set has no Std representation (stream-integrity failure).
    Params(ParamsError),
    /// H.265 parameter set has no Std representation, or the stream sits
    /// outside the envelope (chroma / bit depth / profile). Refused rather
    /// than half-converted.
    ParamsH265(H265ParamsError),
    PlanAv1(pf_bitstream::av1::PlanError),
    ConvertAv1(PlanToVkAv1Error),
    /// AV1 sequence header has no Std representation, or the stream sits
    /// outside the envelope (sampling / bit depth / profile).
    ParamsAv1(ParamsAv1Error),
    /// Tile groups could not be split into `pTileOffsets` ranges. Refused
    /// rather than submitting the whole OBU as tiles ([`crate::decoder_av1`]).
    TilesAv1(crate::decoder_av1::Av1TileError),
    /// Named a reference slot the planner no longer holds. Fatal: the planner
    /// compacting survivors into `AuPlan::refs` would make later names resolve
    /// to the wrong picture. `ref_index` is LAST_FRAME=0 … ALTREF_FRAME=6.
    MissingReferenceAv1 {
        slot: u8,
        ref_index: u8,
    },
    /// Every frame of this temporal unit was skipped while waiting for a key
    /// after a failure. Same kind as [`pf_bitstream::h264::PlanError::AwaitingIdr`]:
    /// an error per AU, so the consumer's demotion streak can fire. `Ok(None)`
    /// would reset that streak once per inter frame and never demote.
    AwaitingKeyAv1,
    /// Plan-to-Vulkan conversion. `CapacityMismatch` is consumed by rebuild and
    /// surfaces only if the rebuilt session still mismatches.
    Convert(PlanToVkError),
    ConvertH265(PlanToVkH265Error),
    /// Device cannot host any session (demote to the next rung).
    Caps(CapsError),
    Device(DeviceError),
    Unsupported(String),
    /// Vulkan call failed (anything but device loss).
    Vk(vk::Result),
    /// `VK_ERROR_DEVICE_LOST`. Later calls fail fast until the owner rebuilds.
    DeviceLost,
    /// Bounded GPU wait expired; fatal for this decoder instance.
    Timeout(&'static str),
    /// Picture pool exhausted: consumer holds more than [`HOLD_HEADROOM`]
    /// unreleased frames while the DPB is full. AU planned but not decoded;
    /// release frames and request a keyframe.
    NoFreeSlot,
    /// A referenced DPB slot binds no image. Fatal on all three codecs.
    ///
    /// H.265/AV1 name slots by index, so dropping one silently re-points later
    /// names. H.264 has no such arrays but hardware still decodes against the
    /// unbound slot. Safe only because [`crate::decoder_h265::RecoveryLatch`]
    /// flushes to the next IRAP/IDR.
    UnboundReferenceSlot {
        slot: u8,
    },
    /// Frame's generation has no retired pool (double release, or outlived the
    /// graveyard entry).
    StaleFrame {
        frame_generation: u64,
        current_generation: u64,
    },
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl std::fmt::Display for VkDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VkDecodeError::Plan(e) => write!(f, "AU planning failed: {e}"),
            VkDecodeError::PlanH265(e) => write!(f, "H.265 AU planning failed: {e}"),
            VkDecodeError::Params(e) => write!(f, "parameter-set conversion failed: {e}"),
            VkDecodeError::ParamsH265(e) => {
                write!(f, "H.265 parameter-set conversion failed: {e}")
            }
            VkDecodeError::PlanAv1(e) => write!(f, "AV1 AU planning failed: {e}"),
            VkDecodeError::ConvertAv1(e) => write!(f, "AV1 plan conversion failed: {e}"),
            VkDecodeError::ParamsAv1(e) => {
                write!(f, "AV1 sequence-header conversion failed: {e}")
            }
            VkDecodeError::TilesAv1(e) => write!(f, "AV1 tile split failed: {e}"),
            VkDecodeError::MissingReferenceAv1 { slot, ref_index } => {
                write!(
                    f,
                    "AV1 reference name {ref_index} points at slot {slot}, which holds \
                     no picture — the surviving references would renumber"
                )
            }
            VkDecodeError::AwaitingKeyAv1 => write!(
                f,
                "every frame of this AV1 temporal unit was skipped — the decoder is \
                 waiting for the next key frame after a failure"
            ),
            VkDecodeError::Convert(e) => write!(f, "plan conversion failed: {e}"),
            VkDecodeError::ConvertH265(e) => write!(f, "H.265 plan conversion failed: {e}"),
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
                    "picture pool exhausted — more than {HOLD_HEADROOM} delivered frames \
                     are unreleased (release_frame owed)"
                )
            }
            VkDecodeError::UnboundReferenceSlot { slot } => {
                write!(
                    f,
                    "DPB slot {slot} is referenced by this AU but binds no image — \
                     the H.265 RPS index arrays would point at the wrong pictures"
                )
            }
            VkDecodeError::StaleFrame {
                frame_generation,
                current_generation,
            } => {
                write!(
                    f,
                    "frame from session generation {frame_generation} (current \
                     {current_generation}) has no retired pool — double release?"
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
            VkDecodeError::PlanH265(e) => Some(e),
            VkDecodeError::Params(e) => Some(e),
            VkDecodeError::ParamsH265(e) => Some(e),
            VkDecodeError::Convert(e) => Some(e),
            VkDecodeError::ConvertH265(e) => Some(e),
            VkDecodeError::PlanAv1(e) => Some(e),
            VkDecodeError::ConvertAv1(e) => Some(e),
            VkDecodeError::ParamsAv1(e) => Some(e),
            VkDecodeError::TilesAv1(e) => Some(e),
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

impl From<H265ParamsError> for VkDecodeError {
    fn from(e: H265ParamsError) -> Self {
        VkDecodeError::ParamsH265(e)
    }
}

impl From<ParamsAv1Error> for VkDecodeError {
    fn from(e: ParamsAv1Error) -> Self {
        VkDecodeError::ParamsAv1(e)
    }
}

impl From<PlanToVkAv1Error> for VkDecodeError {
    fn from(e: PlanToVkAv1Error) -> Self {
        VkDecodeError::ConvertAv1(e)
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
            SessionError::ParamsH265(p) => VkDecodeError::ParamsH265(p),
            SessionError::ParamsAv1(p) => VkDecodeError::ParamsAv1(p),
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

/// Query and command pools. Query slots cycle per submission (checked against
/// [`DecodedVkFrame::submission`]); command buffers cycle within the bitstream
/// ring's in-flight bound. This type owns and destroys the Vulkan objects.
///
/// `query_pool` is `None` without `queryResultStatusSupport`: recording a
/// RESULT_STATUS query is invalid there (RADV hangs the VCN ring). Verdicts
/// then fall back to timeline completion.
pub(crate) struct OpRing {
    device: ash::Device,
    pub(crate) query_pool: Option<vk::QueryPool>,
    pub(crate) query_count: u32,
    cmd_pool: vk::CommandPool,
    pub(crate) cmds: Vec<vk::CommandBuffer>,
}

impl OpRing {
    /// # Safety
    ///
    /// `dev` wraps live handles ([`DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        decode_profile: DecodeProfile,
        query_count: u32,
        cmd_count: u32,
    ) -> Result<Self, vk::Result> {
        let query_pool = if dev.result_status_queries() {
            let mut chain = decode_profile.chain();
            // SAFETY: fn contract. `chain` outlives the call, and the helper's
            // SIGNATURE — not a comment — is what keeps it immobile across it.
            Some(unsafe { Self::create_status_query_pool(dev, chain.wire(), query_count)? })
        } else {
            debug!(
                "decode family lacks queryResultStatusSupport — no per-op status \
                 queries on this driver (verdicts fall back to timeline completion)"
            );
            None
        };

        let destroy_query = |pool: Option<vk::QueryPool>| {
            if let Some(pool) = pool {
                // SAFETY: destroying the just-created query pool (unwind path).
                unsafe { dev.ash().destroy_query_pool(pool, None) };
            }
        };
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .queue_family_index(dev.decode_qf())
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
        // SAFETY: live device; unwind destroys the query pool on failure.
        let cmd_pool = match unsafe { dev.ash().create_command_pool(&pool_ci, None) } {
            Ok(p) => p,
            Err(e) => {
                destroy_query(query_pool);
                return Err(e);
            }
        };
        let alloc = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .command_buffer_count(cmd_count);
        // SAFETY: live device + the pool created above; unwind destroys both pools
        // (destroying the command pool frees any allocated buffers).
        let cmds = match unsafe { dev.ash().allocate_command_buffers(&alloc) } {
            Ok(c) => c,
            Err(e) => {
                // SAFETY: destroying the command pool created above.
                unsafe { dev.ash().destroy_command_pool(cmd_pool, None) };
                destroy_query(query_pool);
                return Err(e);
            }
        };
        Ok(Self {
            device: dev.ash().clone(),
            query_pool,
            query_count,
            cmd_pool,
            cmds,
        })
    }

    /// RESULT_STATUS query pool against `profile`.
    ///
    /// Split out so the profile borrow outlives `vkCreateQueryPool`.
    /// `push_next` would clobber the profile's own `p_next`; the chain is a raw
    /// `*const`, which ends the borrow the moment it is taken. A `&` parameter
    /// holds it for the whole call.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`DeviceHandles`] contract).
    unsafe fn create_status_query_pool(
        dev: &DecodeDevice,
        profile: &vk::VideoProfileInfoKHR<'_>,
        query_count: u32,
    ) -> Result<vk::QueryPool, vk::Result> {
        let mut query_ci = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::RESULT_STATUS_ONLY_KHR)
            .query_count(query_count);
        // Manual chain: `push_next` would clobber the profile's own `p_next`.
        query_ci.p_next = std::ptr::from_ref(profile).cast();
        // SAFETY: fn contract; `query_ci` roots the wired chain for the call, and
        // `profile` is borrowed for the whole of this body so the chain cannot move
        // out from under that pointer. The video profile chained in satisfies the
        // "same profile as the session" rule for queries used inside a coding scope.
        unsafe { dev.ash().create_query_pool(&query_ci, None) }
    }
}

impl Drop for OpRing {
    fn drop(&mut self) {
        // SAFETY: own handles on the contract-live device; the owning decoder
        // drains GPU work before dropping state. Destroying the command pool frees
        // its buffers; both destroys ignore NULL.
        unsafe {
            self.device.destroy_command_pool(self.cmd_pool, None);
            if let Some(pool) = self.query_pool {
                self.device.destroy_query_pool(pool, None);
            }
        }
    }
}

/// Decoded picture waiting for its output verdict, plus the fields its
/// [`DecodedVkFrame`] needs. Codec-agnostic; the H.265 decoder uses the same map.
pub(crate) struct PendingPic {
    pub(crate) image: usize,
    pub(crate) submission: u64,
    pub(crate) query_slot: u32,
    pub(crate) timeline_value: u64,
    pub(crate) crop: DisplayCrop,
    pub(crate) colour: ColourDescription,
    pub(crate) poc: i32,
    pub(crate) is_idr: bool,
    /// Folded at plan time (the codec's counting unit is known only there).
    /// Display order is not decode order; see [`DecodedVkFrame::recovery`].
    pub(crate) recovery: crate::recovery::RecoveryMark,
    /// See [`DecodedVkFrame::decode_order`].
    pub(crate) decode_order: u64,
    /// From the plan at decode time; display order is not decode order. See
    /// [`DecodedVkFrame::references_clean`].
    pub(crate) references_clean: bool,
}

/// Retired generation's picture pool. Lives until release tokens return, then
/// the pool dies.
pub(crate) struct RetiredPool {
    pub(crate) generation: u64,
    pub(crate) pool: PicturePool,
}

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
    /// Coincide: pool image bound to each DPB slot (rebound at activation).
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
    /// Post-failure DPB recovery owed; see [`crate::decoder_h265::RecoveryLatch`].
    recovery: crate::decoder_h265::RecoveryLatch,
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
        Ok(Self {
            dev,
            lock,
            planner: H264Planner::new(),
            caps: None,
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

            // Free pool image, never one a consumer holds. Exhaustion means the
            // consumer owes HOLD_HEADROOM releases; no wait frees an image here.
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
                // SAFETY: live device; the token is a pool image's semaphore.
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
    /// every decode; leftover frames still occupy pool images.
    pub fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        self.ready.pop_front()
    }

    /// Warnings of the most recent successfully planned AU (concealment /
    /// want_keyframe). Cleared by the next `decode`.
    pub fn take_warnings(&mut self) -> Vec<PlanWarning> {
        std::mem::take(&mut self.last_warnings)
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
    /// [`SlotMap`], and slot→image bindings. [`Self::flush`] settles the first
    /// (and still delivers pictures that reached output);
    /// [`crate::decoder_h265::reset_slot_bindings`] empties the other two.
    /// Images a consumer holds stay pinned by `held`, as across a rebuild.
    ///
    /// Not a session rebuild: session, pools, and ring are still valid.
    fn recover_dpb(&mut self) {
        debug!("recovering from a failed AU — flushing the H.264 DPB to the next IDR");
        self.flush();
        if let Some(state) = &mut self.state {
            let unbound = crate::decoder_h265::reset_slot_bindings(
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
            let raw =
                unsafe { query_h264_caps(&self.dev, std_profile) }.map_err(VkDecodeError::from)?;
            self.caps = Some((std_profile, derive_caps(&raw)?));
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

/// Build the delivered frame for one settled pending picture (pending → held).
///
/// Takes the pool and the two mode facts rather than a `SessionState` so both
/// codecs share it. [`DecodedVkFrame::format`] comes off the pool, which
/// stamped it from the `caps.output_format` its images were created with.
pub(crate) fn build_frame(
    pool: &mut PicturePool,
    coincide: bool,
    image_extent: vk::Extent2D,
    entry: &PendingPic,
    generation: u64,
) -> DecodedVkFrame {
    let format = pool.format;
    let picture = &mut pool.pictures[entry.image];
    picture.pending = false;
    picture.held += 1;
    DecodedVkFrame {
        image: picture.image,
        format,
        view: picture.view,
        plane_views: picture.plane_views,
        layer: 0,
        layout: if coincide {
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR
        } else {
            vk::ImageLayout::VIDEO_DECODE_DST_KHR
        },
        coded_width: image_extent.width,
        coded_height: image_extent.height,
        crop: entry.crop,
        colour: entry.colour,
        semaphore: picture.semaphore,
        value: entry.timeline_value,
        poc: entry.poc,
        is_idr: entry.is_idr,
        recovery: entry.recovery,
        decode_order: entry.decode_order,
        references_clean: entry.references_clean,
        query_slot: entry.query_slot,
        submission: entry.submission,
        picture: entry.image as u32,
        generation,
    }
}

/// Split one [`DpbUpdate`]: `outputs` (bump order) become deliverable;
/// `removed` ids that never reached output are returned so their images are
/// freed. Codec-agnostic (H.265 uses the same [`DpbUpdate`]).
pub(crate) fn settle_dpb<F>(pending: &mut BTreeMap<PicId, F>, dpb: &DpbUpdate) -> (Vec<F>, Vec<F>) {
    settle_dpb_ids(pending, &dpb.outputs, &dpb.removed)
}

/// [`settle_dpb`] over the two id lists directly.
///
/// AV1 declares its own [`pf_bitstream::av1::DpbUpdate`] — structurally the
/// same, a distinct type. Settling at the id lists lets all three codecs share
/// one implementation.
pub(crate) fn settle_dpb_ids<F>(
    pending: &mut BTreeMap<PicId, F>,
    outputs: &[PicId],
    removed: &[PicId],
) -> (Vec<F>, Vec<F>) {
    let mut ready = Vec::new();
    for id in outputs {
        match pending.remove(id) {
            Some(entry) => ready.push(entry),
            // Ids planned before this decoder existed, or dropped across a
            // rebuild: display-order gaps, not errors.
            None => trace!(id, "output id without a pending picture"),
        }
    }
    let dropped = removed.iter().filter_map(|id| pending.remove(id)).collect();
    (ready, dropped)
}

/// Bounded timeline wait (no-op for the never-signalled value 0).
///
/// # Safety
///
/// `device` is live and `semaphore` is a live timeline semaphore on it.
pub(crate) unsafe fn wait_timeline(
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

/// Picture resource view for DPB `slot`: bound pool image (coincide) or DPB
/// array layer (distinct). `None` when a coincide slot has no binding.
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
/// against its `SlotMap`, `dst` a free pool image, the AU resident in `upload`'s
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
    let dst_image = state.pool.pictures[dst].image;
    let mut image_barriers = Vec::new();
    if coincide {
        // Coincide: dst pool image is the setup DPB picture.
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

    // Reset the status query before the coding scope. None without
    // queryResultStatusSupport (RADV hangs the VCN if a query is recorded).
    if let Some(query_pool) = state.ops.query_pool {
        // SAFETY: recording; `query_index` is within the pool's count (fn contract).
        unsafe { device.cmd_reset_query_pool(cmd, query_pool, query_index, 1) };
    }

    // Setup/dst: fresh pool image (coincide) or DPB layer (distinct). Resolved
    // before the scope is built — it is the scope's last entry.
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
    // without a current association). Shared with H.265 `build_scope` so the
    // fail-closed layout cannot drift.
    let held: Vec<u8> = state.slots.held().map(|(slot, _id)| slot).collect();
    let (scope, reference_count) = crate::decoder_h265::build_scope(
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

    // Decode destination: the setup picture (coincide) or the pool image.
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
        .video_session(state.session.session())
        .video_session_parameters(state.session.parameters())
        .reference_slots(&begin_slots);
    // One-shot session RESET, consumed here but re-armed on every error path
    // below. A RESET recorded into a buffer that never reaches the queue
    // initialized nothing; the next successful recording must carry it.
    let did_reset = state.session.take_needs_reset();
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
            state.session.re_arm_reset();
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
        let err = crate::decoder_h265::build_scope(
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
        let (scope, reference_count) = crate::decoder_h265::build_scope(
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
    fn settle_dpb_readies_outputs_in_order_and_returns_never_output_removals() {
        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(1, 100);
        pending.insert(2, 200);
        pending.insert(3, 300);

        // 1 outputs and is removed (normal bump). 2 is removed without output
        // (`no_output_of_prior_pics`): free its image, do not leak in the map.
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
