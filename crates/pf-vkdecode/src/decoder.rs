//! [`VkH264Decoder`]: the assembled native decoder — pf-bitstream's planner and
//! WP-A's conversions driving a Vulkan Video session end to end.
//!
//! Per AU: `plan_au` → `plan_to_vk` → AU upload into the bitstream ring → record
//! (barriers, `vkCmdBeginVideoCodingKHR` with every bound DPB slot, the one-time
//! session RESET control, a `RESULT_STATUS_ONLY` query bracketing
//! `vkCmdDecodeVideoKHR`) → submit on the decode queue under the caller's
//! [`QueueLock`] with a per-image timeline signal.
//!
//! **Image model (zero-copy, the FFmpeg pool contract):** decode targets come from
//! a picture pool DECOUPLED from DPB slots ([`crate::images`] module docs) — a
//! slot binds a fresh free image at activation, so a delivered picture is never a
//! decode target while the consumer reads it. Each image's own timeline semaphore
//! carries the AVVkFrame hand-off: the decoder signals `value+1` at decode-write,
//! the presenter waits it, samples, restores the layout and signals `value+1`
//! again in its own submission; [`VkH264Decoder::release_frame`] reports that
//! write-back and the decoder waits it before the image's next use — presenter
//! layout traffic is ordered against decode reads without any copy.
//!
//! The status query is THE point of this program: FFmpeg's `vulkan_decode.c` runs
//! `nb_queries = 0` and therefore architecturally cannot see driver-reported decode
//! corruption (the Xbox Ally X field case). Here every decode op has a query slot,
//! [`VkH264Decoder::poll_status`] reads it WITHOUT waiting, and a non-COMPLETE
//! result is the concealment signal the integration layer wires to
//! `want_keyframe`.
//!
//! Known residual (WP-D on-glass, same class as the shipping AVVkFrame arm): a
//! delivered frame whose picture is STILL a live reference can be sampled by the
//! presenter while a decode references it — reads on both sides, but the
//! presenter's layout round-trip writes metadata. `VK_KHR_unified_image_layouts`
//! (GENERAL everywhere) removes the round-trip entirely and is the documented
//! fast-path TODO once the fleet's drivers carry it.

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

/// One decoded, display-ready picture over a pool image the decoder will not
/// touch again until [`VkH264Decoder::release_frame`] returns it.
///
/// Sync contract (the AVVkFrame shape): pixels are ready when `semaphore`
/// reaches [`Self::value`]. A consumer that SAMPLES the image must, in the same
/// submission that waits `value`, signal `value + 1` after its reads (and layout
/// restore) — and report that via `release_frame(frame, true)`; a consumer that
/// drops the frame unsampled releases with `false`. Handles survive session
/// rebuilds via the graveyard: release every frame exactly once, even
/// stale-generation ones.
#[derive(Debug, Clone)]
pub struct DecodedVkFrame {
    pub image: vk::Image,
    /// The picture format the image was created with, and the format
    /// [`Self::view`] aliases — the caps-resolved `output_format` of the session
    /// that decoded it.
    ///
    /// Read it; do NOT assume it. H.264 in this program is 8-bit 4:2:0 by
    /// envelope, so the format is [`crate::NV12`] on every H.264 frame — but an
    /// H.265 session's picture format is the STREAM's: Main → [`crate::NV12`],
    /// Main 10 → [`crate::P010`], RExt 4:4:4 → [`crate::YUV444_8`] /
    /// [`crate::YUV444_10`], and it can change MID-STREAM when the host
    /// renegotiates (a new generation, a new pool). A consumer that hard-codes
    /// 8-bit 4:2:0 decodes a Main 10 picture correctly and then renders it with
    /// 8-bit transfer/range math, and gives a 4:4:4 picture 4:2:0 UV scaling —
    /// both plausible-looking and wrong, the class this crate exists to refuse.
    /// [`crate::plane_formats`] maps this to the per-plane view formats
    /// [`Self::plane_views`] carry.
    pub format: vk::Format,
    /// Full-picture view of [`Self::format`] (`COLOR` aspect, all planes).
    pub view: vk::ImageView,
    /// Per-plane views for the presenter's sampler path, in the formats
    /// [`crate::plane_formats`] resolves for [`Self::format`]: `R8`/`R8G8` for
    /// the 8-bit families, `R10X6`/`R10X6G10X6` for the 10-bit ones.
    pub plane_views: [vk::ImageView; 2],
    /// Always 0 — pool images are single-layer (kept for the consumer ABI).
    pub layer: u32,
    /// The layout the picture is in when the semaphore signals — and the layout
    /// the consumer must RESTORE after sampling: `VIDEO_DECODE_DPB_KHR`
    /// (coincide) or `VIDEO_DECODE_DST_KHR` (distinct).
    pub layout: vk::ImageLayout,
    /// The ALLOCATED picture extent (`pictureAccessGranularity`-aligned) — what
    /// UV-scale math must divide by (the 1088-row class); the DISPLAY region is
    /// [`Self::crop`].
    pub coded_width: u32,
    pub coded_height: u32,
    /// Conformance-window crop: the region to display.
    pub crop: DisplayCrop,
    /// Colour signalling from the picture's ACTIVE SPS (pf-bitstream applies
    /// E.2.1's "unspecified" inference where the VUI is silent). Per frame, like
    /// [`Self::crop`]: the host switches HDR in-band with a new SPS mid-stream.
    pub colour: ColourDescription,
    /// Timeline pair: pixels ready at `semaphore >= value`; the sampling
    /// consumer signals `value + 1` (see the type docs).
    pub semaphore: vk::Semaphore,
    pub value: u64,
    pub poc: i32,
    pub is_idr: bool,
    /// What the recovery point SEI of this picture's AU (and any outstanding one
    /// before it) is worth — see [`crate::recovery`]. `RecoveryMark::NONE` on every
    /// picture of a stream that carries no recovery point SEI, which is every
    /// punktfunk host today that is not running an NVENC intra-refresh wave.
    ///
    /// It exists because [`Self::is_idr`] cannot answer for an intra-refresh
    /// session: the wave never emits an IDR, so a consumer freezing on loss has no
    /// decoder-visible clean point and holds the last good picture until its
    /// backstop forces the very IDR the wave exists to avoid. The mark is the
    /// stream saying, in-band, where it healed.
    pub recovery: crate::recovery::RecoveryMark,
    /// This picture's position in DECODE order: a strictly increasing per-decoder
    /// ordinal stamped when the AU was planned (1 for the first picture of the
    /// decoder's life; it survives session rebuilds, because it describes the
    /// STREAM, not the Vulkan objects).
    ///
    /// It exists because delivery order is not decode order, and a consumer
    /// pairing [`Self::recovery`] against its own loss needs to know which of the
    /// two a frame belongs to. A post-failure DPB flush hands back every picture
    /// still buffered — pictures decoded BEFORE the loss — and each carries the
    /// recovery marks of the wave it was decoded in. Delivered after the
    /// consumer armed its freeze, those marks read as a heal that happened after
    /// the loss, and lift a freeze on a wave that completed before it. Comparing
    /// this ordinal against the one current at the arm is what tells them apart.
    pub decode_order: u64,
    /// The decode op's slot in the status query pool.
    pub query_slot: u32,
    /// The decode op's submission ordinal (validates the query slot has not been
    /// re-armed since).
    pub submission: u64,
    /// The pool index of the image (release bookkeeping).
    pub picture: u32,
    /// The session generation this frame belongs to (graveyard routing).
    pub generation: u64,
}

/// Everything that can go wrong. Never panics; device loss is first-class so the
/// session layer can tear down and rebuild.
#[derive(Debug)]
pub enum VkDecodeError {
    /// pf-bitstream could not plan the AU at all.
    Plan(PlanError),
    /// [`VkDecodeError::Plan`]'s H.265 counterpart. Note what is NOT here:
    /// `h265::PlanError::RaslSkipped` never becomes an error — the H.265 decoder
    /// answers `Ok(None)` for a RASL picture after an open-GOP join, because it is
    /// undecodable by definition and the next AU plans normally.
    PlanH265(pf_bitstream::h265::PlanError),
    /// A parameter set has no Std representation (stream-integrity failure).
    Params(ParamsError),
    /// An H.265 parameter set has no Std representation, or the stream sits
    /// outside the H.265 decode envelope (chroma format / bit depth / profile) —
    /// a stream-integrity failure, refused rather than half-converted.
    ParamsH265(H265ParamsError),
    /// [`VkDecodeError::Plan`]'s AV1 counterpart.
    PlanAv1(pf_bitstream::av1::PlanError),
    /// [`VkDecodeError::Convert`]'s AV1 counterpart.
    ConvertAv1(PlanToVkAv1Error),
    /// An AV1 sequence header has no Std representation, or the stream sits
    /// outside the AV1 decode envelope (sampling / bit depth / profile).
    ParamsAv1(ParamsAv1Error),
    /// The AV1 access unit's tile groups could not be split into the per-tile
    /// byte ranges `VkVideoDecodeAV1PictureInfoKHR::pTileOffsets` wants — a
    /// malformed or unexpected OBU. Refused rather than submitted with the whole
    /// OBU standing in for its tiles ([`crate::decoder_av1`]).
    TilesAv1(crate::decoder_av1::Av1TileError),
    /// An AV1 frame named a reference slot the planner's store no longer holds.
    ///
    /// Fatal rather than degraded, and for a sharper reason than "the picture
    /// would be wrong": the planner COMPACTS the surviving references into
    /// `AuPlan::refs`, so the seven AV1 reference NAMES stop lining up with that
    /// list the moment one is lost — every later name would resolve to the wrong
    /// picture, which is the plausible-looking corruption this crate refuses to
    /// produce. `ref_index` is the AV1 reference name (`LAST_FRAME` = 0 through
    /// `ALTREF_FRAME` = 6), `slot` the reference slot it pointed at.
    MissingReferenceAv1 { slot: u8, ref_index: u8 },
    /// Every frame of this AV1 temporal unit was SKIPPED because the decoder is
    /// waiting for the next key frame after a failure — nothing decoded, nothing
    /// displayed.
    ///
    /// AV1's answer to [`pf_bitstream::h264::PlanError::AwaitingIdr`], and
    /// deliberately the same KIND of answer: an error, once per access unit, for
    /// as long as the wait lasts. The AV1 planner has no `flush`, so the wait is
    /// held in [`crate::VkAv1Decoder`] rather than in the planner — but a consumer
    /// must not be able to tell the two codecs apart here, because the consumer's
    /// demotion streak is what turns "this rung produces no picture" into "fall
    /// through to the next rung". Answering the wait with a clean `Ok(None)`
    /// instead RESETS that streak once per frame, and a rung whose every key frame
    /// fails (a film-grain sequence on a device without the grain profile, a level
    /// above `maxLevelIdc`, a sequence header disagreeing with the negotiation)
    /// then never demotes at all: one error per key frame, cleared by the inter
    /// frames between them, and a frozen screen for the whole session.
    ///
    /// A key frame ANYWHERE in the unit clears the wait and decodes, so this is
    /// returned only when the unit produced nothing at all.
    AwaitingKeyAv1,
    /// Plan-to-Vulkan conversion failed (caller/session bugs; `CapacityMismatch`
    /// is consumed internally by the rebuild path and only surfaces if the rebuilt
    /// session STILL mismatches).
    Convert(PlanToVkError),
    /// [`VkDecodeError::Convert`]'s H.265 counterpart.
    ConvertH265(PlanToVkH265Error),
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
    /// The picture pool is exhausted: the consumer holds more than
    /// [`HOLD_HEADROOM`] unreleased frames while the stream's whole DPB depth is
    /// live — a real backpressure fault worth surfacing (the pool is sized so a
    /// correct consumer can never hit this). The AU was planned but NOT decoded;
    /// release frames and request a keyframe.
    NoFreeSlot,
    /// A DPB slot this AU references holds no bound image. H.265 only, and fatal
    /// rather than skippable: `StdVideoDecodeH265PictureInfo`'s RPS arrays are
    /// INDICES into `pReferenceSlots`, so dropping one entry would silently
    /// re-point every later index at the wrong picture — the exact class of
    /// plausible-looking corruption this crate refuses to produce. (H.264 carries
    /// no such index arrays and only traces the case.)
    UnboundReferenceSlot { slot: u8 },
    /// The frame belongs to a generation whose retired pool is already gone
    /// (double release, or a frame outliving its graveyard entry).
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

/// Query pool + command pool/buffers. Query slots cycle per SUBMISSION (validated
/// against [`DecodedVkFrame::submission`]); command buffers cycle within the
/// bitstream ring's in-flight bound. Owns and destroys its Vulkan objects.
///
/// `query_pool` is `None` when the decode family lacks `queryResultStatusSupport`
/// (RADV): recording a RESULT_STATUS query there is invalid — on the .25 box it
/// HANGS the VCN ring — so no query objects exist at all and status verdicts fall
/// back to timeline completion.
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
            let profile = chain.wire();
            let mut query_ci = vk::QueryPoolCreateInfo::default()
                .query_type(vk::QueryType::RESULT_STATUS_ONLY_KHR)
                .query_count(query_count);
            // Chained manually: `push_next` would clobber the profile's own
            // `p_next` (its H264 half) — the encoder's exact precedent.
            query_ci.p_next = (profile as *const vk::VideoProfileInfoKHR<'_>).cast();
            // SAFETY: live device; `query_ci` roots the wired chain for the call.
            // The video profile chained in satisfies the "same profile as the
            // session" rule for queries used inside a coding scope.
            Some(unsafe { dev.ash().create_query_pool(&query_ci, None)? })
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

/// A decoded picture awaiting its output verdict: which pool image holds it and
/// everything its eventual [`DecodedVkFrame`] needs. Codec-agnostic — the H.265
/// decoder keeps the same map.
pub(crate) struct PendingPic {
    pub(crate) image: usize,
    pub(crate) submission: u64,
    pub(crate) query_slot: u32,
    /// The image's timeline value the decode signalled (frame readiness).
    pub(crate) timeline_value: u64,
    pub(crate) crop: DisplayCrop,
    pub(crate) colour: ColourDescription,
    pub(crate) poc: i32,
    pub(crate) is_idr: bool,
    /// Folded at PLAN time (the only place the codec's counting unit is known) and
    /// carried here, because a picture's display order is not its decode order —
    /// see [`DecodedVkFrame::recovery`].
    pub(crate) recovery: crate::recovery::RecoveryMark,
    /// See [`DecodedVkFrame::decode_order`].
    pub(crate) decode_order: u64,
}

/// A retired generation's picture pool: images the presenter still holds live
/// here until their release tokens return, then the pool dies.
pub(crate) struct RetiredPool {
    pub(crate) generation: u64,
    pub(crate) pool: PicturePool,
}

/// Everything tied to ONE session generation. A stream renegotiation (extent,
/// DPB depth, profile) retires it and builds fresh.
struct SessionState {
    session: VideoSession,
    slots: SlotMap,
    /// Distinct mode's reference-only DPB backing; `None` in coincide mode (the
    /// picture pool backs the DPB there).
    dpb: Option<DpbPool>,
    pool: PicturePool,
    ring: BitstreamRing,
    ops: OpRing,
    /// Last-known Std reference info per DPB slot — `vkCmdBeginVideoCodingKHR`
    /// wants codec reference info for EVERY bound slot, including ones this AU's
    /// slices do not reference; refreshed from each plan's setup/ref entries so
    /// marking transitions (e.g. MMCO long-term promotion) propagate.
    slot_refs: Vec<Option<hh::StdVideoDecodeH264ReferenceInfo>>,
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

/// The native Vulkan Video H.264 decoder.
pub struct VkH264Decoder {
    dev: DecodeDevice,
    lock: Box<dyn QueueLock>,
    planner: H264Planner,
    /// Caps per Std profile idc, queried once per profile.
    caps: Option<(hh::StdVideoH264ProfileIdc, DecodeCaps)>,
    state: Option<SessionState>,
    /// Decoded pictures awaiting their planner output verdict, keyed by [`PicId`].
    pending: BTreeMap<PicId, PendingPic>,
    /// Display-ready frames not yet handed out (under the zero-reorder punktfunk
    /// envelope at most one per AU; deeper only around discontinuities/flushes).
    ready: VecDeque<DecodedVkFrame>,
    /// Retired generations' pools with consumer-held images (die on their last
    /// release token).
    graveyard: Vec<RetiredPool>,
    /// The most recent plan's warnings ([`Self::take_warnings`]).
    last_warnings: Vec<PlanWarning>,
    /// The outstanding recovery point SEI, if any — see [`crate::recovery`].
    /// Survives session rebuilds on purpose: it is a fact about the STREAM's
    /// prediction structure, not about this decoder's Vulkan objects.
    recovery_watch: crate::recovery::RecoveryWatch,
    /// Pictures planned so far — stamped onto each one as
    /// [`DecodedVkFrame::decode_order`]. Survives session rebuilds for the same
    /// reason the watch does.
    decoded: u64,
    /// Session generation: bumped on every rebuild, stamped into frames.
    generation: u64,
    device_lost: bool,
}

impl VkH264Decoder {
    /// Wrap the borrowed device. Sessions/pools are built lazily from the first
    /// AU's SPS (their shape is the stream's, not the device's).
    ///
    /// # Safety
    ///
    /// The full [`DeviceHandles`] caller contract (liveness, enabled extensions
    /// and features, truthful queue families) — held for this decoder's whole
    /// lifetime, not just this call. The device must additionally have been
    /// created with `VK_KHR_video_decode_h264` enabled; that one part of the
    /// contract is CHECKED below rather than trusted, because getting it wrong is
    /// undefined behaviour at session creation rather than an error.
    pub unsafe fn new(
        handles: &DeviceHandles,
        lock: Box<dyn QueueLock>,
    ) -> Result<Self, VkDecodeError> {
        // SAFETY: forwarded caller contract.
        let dev = unsafe { DecodeDevice::wrap(handles)? };
        // Before anything is queried or created: does this queue family actually
        // run H.264 decode ops? (device.rs — the caps query would answer for the
        // hardware even where the extension was never enabled.)
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
            decoded: 0,
            generation: 0,
            device_lost: false,
        })
    }

    /// Decode one access unit. Returns the next display-ready frame, if the
    /// planner declared one (zero-reorder streams: the AU's own picture).
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
        // `take_warnings` promises "cleared by the next decode", and this IS a
        // decode: clear BEFORE planning, so an AU that fails to plan at all cannot
        // leave the previous AU's warnings behind to be re-read as damage on the
        // next one. That the ledger is drained after every successful decode
        // (`take_warnings` is a `mem::take`) makes the failed-plan case the only
        // one where it could carry over — it is a hole closed by construction, not
        // a fix for anything observed in the field.
        self.last_warnings.clear();
        let plan = self.planner.plan_au(au)?;
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
        // DECODE order — the order the SEI counts in. The mark rides the pending
        // picture to display order, which may differ (crate::recovery).
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
        // whole point of the pool model). Exhaustion means the consumer owes
        // more than HOLD_HEADROOM releases; no wait can free an image here.
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
        // The bitstream buffer carries the SLICE NALUs only, concatenated — a
        // real AU opens with AUD/SEI (and, at IDRs, SPS/PPS) NALUs, and feeding
        // those to the VCN firmware inside the decode range HANGS it (the .25
        // `vcn_unified_0 ring timeout`; FFmpeg feeds slices-only for the same
        // reason). `pack_slices` rebases the offsets into the packed buffer AND
        // normalises each slice's Annex-B prefix to three bytes — the two go
        // together by construction, see `crate::ring::three_byte_prefix`.
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

        // Record + submit, signalling the dst image's next timeline value.
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

    /// A display-ready frame beyond the one `decode` returned, if any (only
    /// non-empty around discontinuities/flushes — the punktfunk envelope is
    /// zero-reorder). Drain after every decode; frames left here still occupy
    /// pool images.
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
    /// tell a frame decoded before a loss from one decoded after it. 0 before the
    /// first AU plans.
    pub fn decode_order(&self) -> u64 {
        self.decoded
    }

    /// One-line state snapshot for failure paths and field logs (not a stable
    /// format).
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

    /// Read `frame`'s decode status WITHOUT waiting.
    ///
    /// [`DecodeStatus::Failed`] covers driver-reported errors AND a query slot
    /// re-armed before it was read (the status is then unprovable — same
    /// conservative verdict).
    ///
    /// On drivers whose decode family lacks `queryResultStatusSupport` (RADV)
    /// there is no per-op verdict to read: `Ok` then means "the decode op
    /// COMPLETED on the timeline" — the same information FFmpeg has on every
    /// driver, no worse; the Ally-X-class detection exists exactly where the
    /// driver can give it (NVIDIA, AMD's Windows driver).
    pub fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        self.read_status(frame, false)
    }

    /// Does this decode queue family answer per-op `RESULT_STATUS` queries at all
    /// (`queryResultStatusSupport`)?
    ///
    /// The single most important thing a support engineer can know about a
    /// session's integrity reporting, and the reason this is exposed rather than
    /// left internal. Where it is TRUE, [`DecodeStatus::Failed`] is the driver's
    /// own verdict on a decode operation — the signal the Xbox Ally X corruption
    /// needed and FFmpeg's query-less Vulkan decoder (`nb_queries = 0`) can never
    /// produce. Where it is FALSE — RADV, whose VCN ring HANGS if a query is
    /// recorded anyway — `Ok` degrades to "the op completed on the timeline", which
    /// is exactly as much as FFmpeg knows on every driver: no worse, but a clean
    /// integrity report from such a session means "nothing was detectable", not
    /// "nothing was wrong". A telemetry surface that cannot say which of those it
    /// is repeats the failure this program exists to end.
    pub fn status_queries(&self) -> bool {
        self.dev.result_status_queries()
    }

    /// [`Self::poll_status`], but WAITs for the op to complete first — the only
    /// place a status read blocks (the GPU smoke test's assertion path; the
    /// integration layer's steady state polls).
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
    /// signal ([`DecodedVkFrame::semaphore`] reaching [`DecodedVkFrame::value`]).
    /// Pure measurement (the integration layer's sampled decode-latency stat):
    /// touches no decoder state, so a timeout or error only degrades the stat —
    /// the consumer's own GPU wait is what gates sampling, never this. `frame`
    /// must be unreleased (`release_frame` still owed), which pins its pool — and
    /// with it the semaphore — alive, graveyarded generations included; a
    /// stale-generation frame declines rather than block on a verdict the
    /// rebuild's drain already implied.
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
        // front (within one codec the Std code points ascend with the level, so
        // the comparison is numeric), never submitted on a hope. The ceiling came
        // from an H.264 caps query, so it is compared against an H.264 code point
        // — the pairing MaxLevelIdc's tag exists to keep honest.
        let caps_max_level = self.caps.as_ref().expect("queried above").1.max_level_idc;
        let stream_level = level_to_std(plan.picture.level_idc);
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
            Some(state)
                if state.coded_extent == coded
                    && state.session.config.std_profile_idc == std_profile =>
            {
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
    /// Why a mid-stream rebuild is safe against presenter-held frames (the
    /// renegotiation-teardown question, settled):
    /// - **Images**: a pool with consumer holds retires to the graveyard INTACT —
    ///   images, views and semaphores stay live until `release_frame` takes its
    ///   last token, and a token is sent only after the presenter's sampling
    ///   submission's fence was waited (its `value+1` write-back included), so no
    ///   pool image is ever destroyed under in-flight GPU reads.
    /// - **Tokens**: every frame and its token carry the generation they were
    ///   born under, and `release_frame` routes strictly by it (current pool vs
    ///   graveyard entry), so releases cannot alias across generations.
    /// - **Session objects**: the session/ring/ops (query pool included) DO die
    ///   right here — but only after [`Self::drain_gpu`], and no consumer-facing
    ///   handle points at them: [`DecodedVkFrame`] borrows pool resources only,
    ///   and `poll_status` generation-gates before it would touch the NEW
    ///   generation's query pool with an old frame's slot.
    fn rebuild_state(&mut self, plan: &AuPlan) -> Result<(), VkDecodeError> {
        self.drain_gpu()?;
        if let Some(state) = self.state.take() {
            debug!("rebuilding decode session (stream renegotiation)");
            // Move the fields out (SessionState has no Drop of its own): the
            // session/dpb/ring/ops die here — decode work was just drained and
            // the presenter never references them. The PICTURE POOL may outlive:
            // undelivered frames drop (their holds cleared), pending pictures
            // free, and if the consumer still holds delivered images the pool
            // retires to the graveyard until its last release token.
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
            max_dpb_slots: required_slots,
            max_active_references: (required_slots - 1).min(caps.max_active_references),
            std_profile_idc: std_profile,
        };
        let mut pool_plan = plan_pools(caps, required_slots);
        // TEST-ONLY readback hook: the GPU parity test (tests/gpu_parity.rs)
        // copies decoded pictures back to the host to hash them against
        // libavcodec's output, and `vkCmdCopyImageToBuffer` requires
        // TRANSFER_SRC on the source image — a bit the zero-copy production
        // pools deliberately do not carry. Opt-in via env so no production path
        // ever grows it. (The fleet's drivers — RADV, NVIDIA, AMD Windows —
        // advertise TRANSFER_SRC on their decode-output formats; it is the same
        // bit FFmpeg's hwdownload path relies on.)
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

impl Drop for VkH264Decoder {
    fn drop(&mut self) {
        // Best-effort decode drain so the pools' Drop impls never destroy
        // in-flight decode work; a wedged driver falls through after the bounded
        // timeout. Presenter-side sampling of graveyarded/held images is the
        // CALLER's teardown contract: the integration layer waits (bounded) for
        // every release token BEFORE dropping this decoder, so remaining
        // graveyard pools here are either token-drained or a warned forfeit.
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

/// Build the delivered frame for one settled pending picture, moving its image
/// pending → held.
///
/// Takes the pool and the two mode facts rather than a `SessionState` so both
/// codecs' decoders share it (their session states differ only in codec-specific
/// fields, and this function reads none of them). The frame's
/// [`DecodedVkFrame::format`] comes off the POOL rather than a caller argument,
/// which is what makes it truthful for both codecs by construction: the pool
/// stamped it from the very `caps.output_format` its images were created with.
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
        query_slot: entry.query_slot,
        submission: entry.submission,
        picture: entry.image as u32,
        generation,
    }
}

/// Split one [`DpbUpdate`]'s verdicts over the pending map: `outputs` (in bump
/// order) become deliverable; `removed` ids that never reached output — an IDR's
/// `no_output_of_prior_pics_flag` discard, or a flush racing a drop — are
/// returned separately so their images are freed instead of leaking. Pure and
/// generic for testability — and codec-agnostic (H.265 plans carry the very same
/// [`DpbUpdate`] type), so both decoders settle through this one function.
pub(crate) fn settle_dpb<F>(pending: &mut BTreeMap<PicId, F>, dpb: &DpbUpdate) -> (Vec<F>, Vec<F>) {
    settle_dpb_ids(pending, &dpb.outputs, &dpb.removed)
}

/// [`settle_dpb`] over the two id lists directly.
///
/// It exists because AV1's planner declares its OWN `DpbUpdate`
/// ([`pf_bitstream::av1::DpbUpdate`]) rather than re-using the H.264 one the way
/// H.265 does — structurally identical, a distinct type. Splitting the settle at
/// the id lists is what lets all three codecs share ONE implementation of the
/// output/free bookkeeping instead of the AV1 rung growing a copy that could drift.
pub(crate) fn settle_dpb_ids<F>(
    pending: &mut BTreeMap<PicId, F>,
    outputs: &[PicId],
    removed: &[PicId],
) -> (Vec<F>, Vec<F>) {
    let mut ready = Vec::new();
    for id in outputs {
        match pending.remove(id) {
            Some(entry) => ready.push(entry),
            // Ids planned before this decoder existed (post-recovery), or
            // dropped across a rebuild: display-order gaps, not errors.
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

/// The picture resource view bound for DPB `slot`: the bound pool image
/// (coincide) or the DPB array layer (distinct). `None` when a coincide slot has
/// no binding (unreachable in practice — every held slot was activated).
fn slot_view(state: &SessionState, slot: u8) -> Option<vk::ImageView> {
    match &state.dpb {
        Some(dpb) => Some(dpb.dpb_view(slot)),
        None => state.slot_image[usize::from(slot)].map(|p| state.pool.pictures[p].view),
    }
}

/// Record one decode op into the chosen command buffer and submit it under the
/// queue lock: image waits per the pool contract, the dst image's timeline
/// signal at `signal_value`.
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
    // there hangs the VCN; OpRing docs).
    if let Some(query_pool) = state.ops.query_pool {
        // SAFETY: recording; `query_index` is within the pool's count (fn contract).
        unsafe { device.cmd_reset_query_pool(cmd, query_pool, query_index, 1) };
    }

    // ---- bound-slot staging ----
    // Scope list: this AU's references first, then every other still-held slot
    // (their resources must stay bound for their associations to persist), then
    // the setup slot as the ACTIVATION entry (slot index -1 binds its resource
    // without a current association; the decode op's setup slot then claims it).
    let mut scope: Vec<(i32, vk::ImageView, hh::StdVideoDecodeH264ReferenceInfo)> = Vec::new();
    for r in &vk_plan.refs {
        match slot_view(state, r.slot) {
            Some(view) => scope.push((i32::from(r.slot), view, r.std)),
            None => trace!(slot = r.slot, "referenced slot without a bound image"),
        }
    }
    for (slot, _id) in state.slots.held() {
        if slot == vk_plan.setup_slot
            || scope
                .iter()
                .any(|&(index, _, _)| index >= 0 && index as u8 == slot)
        {
            continue;
        }
        match (state.slot_refs[usize::from(slot)], slot_view(state, slot)) {
            (Some(std), Some(view)) => scope.push((i32::from(slot), view, std)),
            // Unreachable in practice: every held slot was a setup slot once.
            _ => trace!(
                slot,
                "held slot without reference info/binding — left unbound"
            ),
        }
    }
    let reference_count = vk_plan.refs.len().min(scope.len());
    // The setup/dst resource: the fresh pool image (coincide) or the DPB layer
    // (distinct — the pool image is the separate decode output).
    let setup_view = if coincide {
        state.pool.pictures[dst].view
    } else {
        state
            .dpb
            .as_ref()
            .expect("distinct mode")
            .dpb_view(vk_plan.setup_slot)
    };
    scope.push((-1, setup_view, vk_plan.setup_ref));

    // Staged arrays: resources → std infos → codec slot infos → slot infos. Each
    // vector is fully built before the next borrows it, so nothing reallocates
    // under a stored pointer.
    let resources: Vec<vk::VideoPictureResourceInfoKHR<'_>> = scope
        .iter()
        .map(|&(_, view, _)| {
            vk::VideoPictureResourceInfoKHR::default()
                .coded_extent(coded_extent)
                .base_array_layer(0)
                .image_view_binding(view)
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
    use super::*;

    #[test]
    fn settle_dpb_readies_outputs_in_order_and_returns_never_output_removals() {
        let mut pending: BTreeMap<PicId, u32> = BTreeMap::new();
        pending.insert(1, 100);
        pending.insert(2, 200);
        pending.insert(3, 300);

        // Picture 1 outputs (and is also removed — the normal bump); picture 2
        // is removed WITHOUT ever reaching output (no_output_of_prior_pics):
        // its image must be freed, not leak in the map.
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
        // The gate is `level_to_std(stream) > caps.max_level_idc.code_point()`;
        // that is only sound if the Std code points ascend with the level WITHIN
        // one codec (which is why the ceiling carries its codec — MaxLevelIdc).
        // Pin the ordering across the range (and the 1b fold onto 1.1).
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
