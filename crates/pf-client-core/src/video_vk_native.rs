//! Native Vulkan Video decode backend (WP-C of the native-decode program, widened to
//! HEVC by M3 WP-2): pf-vkdecode's [`VkH264Decoder`]/[`VkH265Decoder`] running on the
//! PRESENTER's own VkDevice — the same zero-copy shape as the FFmpeg-Vulkan backend,
//! with no FFmpeg in the path. Auto's rung immediately ABOVE FFmpeg-Vulkan since the
//! 2026-08-05 ladder decision (WP-D closed bit-exact — the program is dropping FFmpeg
//! from the client), also pinnable via `PUNKTFUNK_DECODER=native-vulkan`;
//! `video::native_vulkan_gate` is the admission either way, and a failure falls
//! through to the FFmpeg-Vulkan rung.
//!
//! **Codec dispatch:** the negotiated codec picks the decoder ONCE, at construction
//! ([`Codec`]) — H.264 or H.265, the two codecs pf-vkdecode speaks. The negotiated
//! picture SHAPE (chroma format + bit depth) is checked there too, against the
//! device: an H.265 session this GPU has no decode format for is refused at
//! construction, where the ladder answers with FFmpeg-Vulkan, rather than at the
//! first AU, where the only exit is an error streak PAST that rung
//! ([`NativeVulkanDecoder::new`]). Nothing below the codec enum is per-codec: the
//! shipped-frame ledger, the release tokens, the
//! decode-status reads, the timeline waits and the teardown drain are shared, because
//! both decoders deliver the identical [`DecodedVkFrame`] contract (same pool/slot
//! lifecycle, same `value + 1` write-back, same query slots, same generations).
//! Forking that machinery per codec would fork the one part of this backend hardware
//! has already proven.
//!
//! **A skipped RASL picture is NOT a decode error.** An HEVC stream joined at a CRA
//! carries leading pictures whose references precede the join; the spec's own answer
//! (8.1.3 NOTE) is to decode and output nothing for them. [`VkH265Decoder::decode`]
//! implements exactly that: `h265::PlanError::RaslSkipped` never becomes a
//! `VkDecodeError`, so the AU comes back as `Ok` with whatever was ALREADY
//! display-ready (usually `None`) and with the warning ledger cleared. This backend
//! must therefore treat `Ok(None)` as "no picture this AU" and nothing more — no
//! release-unshown, no re-anchor request, no error. Mapping it to an error would make
//! every open-GOP join beg the host for a keyframe it has no reason to send. (Dead in
//! the field today — punktfunk hosts emit IDR-only re-entry points — but it is the
//! contract pf-bitstream's `h265` module docs record for this wiring.)
//!
//! **Queue lock:** pf-vkdecode submits on queue 0 of the decode family
//! ([`DECODE_QUEUE_INDEX`] — the presenter creates exactly one queue per family). When
//! the decode family IS the presenter's graphics family, that is the very `VkQueue` the
//! presenter/Skia/overlay submit and present on, so every decode submit must hold the
//! device's shared [`video::QueueLock`] (`vkQueueSubmit` external sync — the 2026-07-09
//! `VK_ERROR_DEVICE_LOST` class). When the families differ, the decode queue has exactly
//! one submitter (this backend, on the pump thread) and locking would serialize decode
//! against present for nothing — [`submit_queues_collide`] is the whole decision. (The
//! FFmpeg path locks on every family only because `lock_queue` is one callback pair for
//! the whole device; the collision it exists to prevent is the shared-queue one.)
//!
//! **Release lifecycle** (decode → present → retire → release): each delivered frame
//! ships as a [`NativeVkFrame`] whose [`NativeReleaseGuard`] sends a token (seq +
//! generation) into this backend's channel on drop. The presenter drops the frame only
//! after the sampling submission's fence has been waited (its retired-frame slot), so a
//! returned token proves the GPU is done with the image; a frame dropped UNPRESENTED
//! (newest-wins displacement, post-demotion drain) releases through the same drop. The
//! backend drains the channel at every `decode` entry and calls
//! [`Codec::release_frame`] — but only once the frame's decode-status query has
//! also been read (the slot stays pinned meanwhile, which is what makes re-polling the
//! query safe: an unreleased slot can never be recycled under the poll).
//!
//! **Status queries:** every decode op carries a `RESULT_STATUS_ONLY` query —
//! [`Codec::poll_status`], read non-blockingly here at each decode entry. A
//! `Failed` verdict is driver-reported decode corruption, the class FFmpeg's
//! `vulkan_decode.c` (`nb_queries = 0`) architecturally cannot see — the Xbox Ally X
//! field case. It surfaces as an `Err` from the CURRENT `decode_frame` call so the
//! existing streak/reanchor machinery fires exactly as it does for FFmpeg errors.
//!
//! **The recovery policy** (M4) — what a damaged stream ASKS for, and why it cannot
//! storm. There are two kinds of damage and they are answered differently:
//!
//! - **Concealment** (the plan needed a substitute for something lost: an integrity
//!   warning). The AU's output is released UNSHOWN, [`DecodeHealth`] records it, and
//!   `decode` answers `Ok(None)` with [`NativeVulkanDecoder::take_recovery_request`]
//!   raised. `video::Decoder` turns that into its ordinary `want_keyframe`, which the
//!   pump drains, arms the freeze on, and asks through the ONE ~100 ms recovery
//!   throttle every other ask already shares (`session.rs`'s `last_kf_req`: frame-gap
//!   RFI, dropped-climb, no-output streak, overdue backstop, decoder recovery). It is
//!   deliberately NOT an `Err`: an error ticks the demotion streak, and three of them
//!   in a second would demote the native rung on exactly the lossy links it exists to
//!   diagnose — an FFmpeg rung conceals the same event silently and keeps its job.
//! - **A driver `Failed` verdict** (and its query-less twin, a decode status that
//!   could not be established at all — [`StatusVerdicts`]). That is a statement about
//!   the DECODER, not the stream, so it stays an `Err`: same volume as an FFmpeg
//!   reference-miss error, streak-eligible, and a driver making it repeatedly is
//!   precisely what demotion is for.
//! - **A REFUSED AU** — the decoder answering `Err` outright (a plan error, a
//!   Vulkan/session failure). Also an error, also streak-eligible, and counted
//!   separately from concealment in [`DecodeHealth::refused`]: "the stream is
//!   damaged and I coped" and "I could not run" are opposite statements about the
//!   rung, and only the second one means the session is looking at a frozen screen.
//!
//! Because concealment is not an error, it must not clear the demotion streak
//! either — `video::Decoder::decode_frame` leaves the streak untouched on a
//! concealed `Ok(None)` and resets it only on a shipped frame or a clean AU.
//! Otherwise a driver failing every other AU on a lossy link has its errors zeroed
//! by the concealment between them, and a rung that conceals FOREVER (a host
//! framing regression: every AU damaged, no frame ever shipped) has no escape
//! hatch at all.
//!
//! Neither can storm, for two independent reasons. The ask is throttled to one per
//! 100 ms per session whatever the damage rate; and once the freeze is armed the gate
//! lifts only on a proven re-anchor, so a run of damaged AUs refreshes an existing
//! freeze rather than compounding into more requests. A stream that never recovers
//! therefore costs one keyframe ask per 100 ms, not one per AU.
//!
//! **Recovery-point SEI** (M4): pf-vkdecode's `RecoveryWatch` folds the parsed SEI
//! into a per-picture mark that rides the frame ([`NativeVkFrame::recovery`]) into
//! the shared gate's `on_local_recovery`. It is the only way a client can see an
//! intra-refresh session heal on the two backends that run a wave WITHOUT setting the
//! wire mark (Windows AMF and QSV — only Linux libav-NVENC sets it): the wave emits
//! no IDR and libavcodec flags none, so without this such a session freezes for the
//! full 500 ms backstop and then forces the very IDR the wave exists to avoid.
//! Additional, never a replacement: the wire path is untouched and the FFmpeg rungs
//! keep exactly the behaviour they had.
//!
//! **Teardown:** dropping this backend (demotion, session end) waits — bounded — for
//! every shipped frame's token before dropping the decoder, because the decoder's Drop
//! destroys the pool images and its own drain only covers DECODE work, not the
//! presenter's in-flight sampling. Tokens arrive as the presenter's fence waits/drops
//! displace the frames; a presenter wedged past [`TEARDOWN_BUDGET`] forfeits (warned).

use crate::video::{
    ColorDesc, DecodeHealth, NativeReleaseGuard, NativeReleaseToken, NativeVkFrame, NativeVkLayout,
    VulkanDecodeDevice,
};
use anyhow::{anyhow, bail, Result};
use pf_vkdecode::ash::vk;
use pf_vkdecode::ash::vk::Handle as _;
use pf_vkdecode::{
    DecodeStatus, DecodedVkFrame, DeviceHandles, VkDecodeError, VkH264Decoder, VkH265Decoder,
};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The queue index this backend submits on within the decode family: the presenter
/// creates exactly ONE queue (index 0) per family it enables (`vk/setup.rs` — one
/// `VkDeviceQueueCreateInfo` per family, `queue_count = 1`), so 0 is the only queue
/// that exists.
const DECODE_QUEUE_INDEX: u32 = 0;

/// Teardown budget for the presenter to hand back every outstanding frame token (its
/// next present's fence wait, typically one frame). Generous against a paused stream,
/// finite against a wedged presenter — after this the pools are destroyed anyway
/// (warned; the realistic residue is a logically-held frame, not in-flight GPU work).
const TEARDOWN_BUDGET: Duration = Duration::from_millis(500);

/// Query-poll belt: a frame whose token has returned had its decode op complete on the
/// GPU (the presenter's submit waited the decode timeline), so its status query MUST be
/// readable — if it still reads Pending after this many polls, give the slot back
/// anyway rather than strand it (debug-logged; the status is then simply unknown).
const MAX_POLLS_AFTER_RELEASE: u32 = 3;

/// Do the presenter's and the decoder's submit queues collide? Both sides use queue
/// index 0 of their family by construction (the presenter's graphics queue is
/// `get_device_queue(qfi, 0)`, the decoder's is [`DECODE_QUEUE_INDEX`] of `decode_qf`),
/// so the collision test is family equality. Pure — the queue-lock decision is
/// CPU-testable.
fn submit_queues_collide(graphics_qf: u32, decode_qf: u32) -> bool {
    graphics_qf == decode_qf
}

/// [`pf_vkdecode::QueueLock`] over the device's shared [`crate::video::QueueLock`] —
/// or over nothing, when the decode queue provably has no other submitter (see the
/// module doc's queue-lock section).
enum NativeQueueLock {
    /// Decode shares the presenter's graphics queue: serialize with everyone.
    Shared(std::sync::Arc<crate::video::QueueLock>),
    /// A separate decode family/queue: this backend is its only submitter.
    Uncontended,
}

impl pf_vkdecode::QueueLock for NativeQueueLock {
    fn lock(&self) {
        if let NativeQueueLock::Shared(l) = self {
            l.lock();
        }
    }
    fn unlock(&self) {
        if let NativeQueueLock::Shared(l) = self {
            l.unlock();
        }
    }
}

/// The codecs pf-vkdecode has a decoder for — the native rung's whole vocabulary,
/// named ash-free so `video.rs` can pick one from the negotiated wire codec without
/// this module knowing about FFmpeg's codec ids (and `video::native_vulkan_gate`
/// stays the single admission decision). AV1 is deliberately absent: the Vulkan
/// decode op exists and real hardware advertises it, but there is no AV1 decoder in
/// pf-vkdecode, so those sessions must keep falling through to the FFmpeg rungs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeCodec {
    H264,
    H265,
}

/// The decoder this backend drives, chosen ONCE from the negotiated codec.
///
/// Dispatch stops here. Everything the backend does around a decoder — the
/// shipped-frame ledger, release tokens, status-query settling, timeline waits,
/// teardown — is codec-agnostic, because [`VkH264Decoder`] and [`VkH265Decoder`]
/// expose the same surface over the same [`DecodedVkFrame`] contract (same pool/slot
/// lifecycle, same `value + 1` write-back, same query slots, same generations). The
/// forwarders below are therefore mechanically identical per arm on purpose: the
/// H.264 path is hardware-verified bit-exact, and dispatch must not be able to change
/// its behaviour.
// Unboxed on purpose, against `large_enum_variant`: the arms differ by ~1.7 KB (both
// decoders carry a planner, a slot ledger and pinned Std parameter sets), and exactly
// ONE of these exists per session — inside the `Box<NativeVulkanDecoder>` the backend
// already lives in. So the "waste" is 1.7 KB of slack in a single session-lifetime
// allocation, while boxing would put a second indirection between the pump and the
// decoder on the per-AU path and change how the hardware-verified H.264 decoder is
// reached. Neither trade is worth 1.7 KB.
#[allow(clippy::large_enum_variant)]
enum Codec {
    H264(VkH264Decoder),
    H265(VkH265Decoder),
}

impl Codec {
    /// Feed one access unit — see [`VkH264Decoder::decode`] /
    /// [`VkH265Decoder::decode`]. `Ok(None)` means "no display-ready picture from
    /// this AU", which for H.265 also covers a RASL picture skipped after an
    /// open-GOP join (the module doc's contract: never an error).
    fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        match self {
            Codec::H264(d) => d.decode(au),
            Codec::H265(d) => d.decode(au),
        }
    }

    /// Drain the plan warnings of the AU just decoded, TYPED — the two planners
    /// have genuinely different enums ([`pf_vkdecode::PlanWarning`] has
    /// `FrameNumGap`/`Mmco5Rebase`, [`pf_vkdecode::H265PlanWarning`] has
    /// `NonZeroReorder`, neither a subset of the other), so the pair is carried as
    /// a two-armed value rather than flattened.
    ///
    /// Typed and not rendered because the backend must BRANCH on them: only some
    /// warnings mean the picture is damaged ([`PlanWarnings::integrity`]), and
    /// dropping a frame for the others costs a visible hitch on a stream the
    /// planner says it planned correctly. Strings would make that a substring
    /// match on `Debug` output.
    fn take_warnings(&mut self) -> PlanWarnings {
        match self {
            Codec::H264(d) => PlanWarnings::H264(d.take_warnings()),
            Codec::H265(d) => PlanWarnings::H265(d.take_warnings()),
        }
    }

    /// Pull the next already display-ready frame the last AU did not return
    /// directly (burst output).
    fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        match self {
            Codec::H264(d) => d.take_ready(),
            Codec::H265(d) => d.take_ready(),
        }
    }

    /// Hand a delivered frame back to its pool; `presented` reports whether the
    /// consumer enqueued the frame's `value + 1` timeline signal.
    fn release_frame(
        &mut self,
        frame: &DecodedVkFrame,
        presented: bool,
    ) -> Result<(), VkDecodeError> {
        match self {
            Codec::H264(d) => d.release_frame(frame, presented),
            Codec::H265(d) => d.release_frame(frame, presented),
        }
    }

    /// The decoder's current session generation (a frame from an older one has an
    /// unknowable status verdict — see [`NativeVulkanDecoder::settle_statuses`]).
    fn generation(&self) -> u64 {
        match self {
            Codec::H264(d) => d.generation(),
            Codec::H265(d) => d.generation(),
        }
    }

    /// Non-blocking read of a frame's `RESULT_STATUS_ONLY` query.
    fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        match self {
            Codec::H264(d) => d.poll_status(frame),
            Codec::H265(d) => d.poll_status(frame),
        }
    }

    /// Bounded host wait for a frame's decode-complete timeline signal (the pump's
    /// sampled decode-latency stat).
    fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        match self {
            Codec::H264(d) => d.wait_decoded(frame, timeout_ns),
            Codec::H265(d) => d.wait_decoded(frame, timeout_ns),
        }
    }

    /// Does this device answer per-op decode-status queries at all? A device
    /// fact, not a codec one — forwarded per arm only because the decoders own
    /// the `DecodeDevice`.
    fn status_queries(&self) -> bool {
        match self {
            Codec::H264(d) => d.status_queries(),
            Codec::H265(d) => d.status_queries(),
        }
    }

    /// The newest planned picture's DECODE-order ordinal — the watermark the
    /// pump stamps when it arms a freeze (see [`NativeVkFrame::decode_order`]).
    fn decode_order(&self) -> u64 {
        match self {
            Codec::H264(d) => d.decode_order(),
            Codec::H265(d) => d.decode_order(),
        }
    }
}

/// What one pass of [`NativeVulkanDecoder::settle_statuses`] learned about the
/// decode status of previously shipped frames.
///
/// Two numbers, not one, because the SAME `DecodeStatus::Failed` means two
/// different things depending on the device. Where the decode family answers
/// `RESULT_STATUS` queries it is the driver's own verdict on its own decode — the
/// Xbox Ally X signal, and the count `DecodeHealth::failed` reports. Where it does
/// NOT (RADV, whose VCN ring hangs if a query is recorded anyway), `poll_status`
/// degrades to reading the decode timeline, and a `Failed` there means the session
/// generation is gone, the device was lost, or the semaphore could not be read —
/// none of which the driver ever said anything about. Reporting those as driver
/// verdicts renders `integrity: driver-failed 1 · no driver status`, which is
/// self-contradictory and points a support engineer at hardware that never spoke.
///
/// Both cost the picture, so both release their frame unshown, both surface as an
/// error, and both extend the concealed run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct StatusVerdicts {
    /// Frames the DRIVER reported corrupt. Only ever non-zero on a device that
    /// answers status queries.
    driver_failed: u32,
    /// Frames whose status could not be established on a device with no query
    /// support — the degraded timeline path.
    unreadable: u32,
}

impl StatusVerdicts {
    fn total(&self) -> u32 {
        self.driver_failed + self.unreadable
    }
}

/// The plan warnings one AU produced, still in their codec's own enum.
///
/// The split that matters is INTEGRITY vs. spec-legal, not H.264 vs. H.265. Both
/// planners emit two kinds of warning through one channel:
///
/// - **Integrity** — a reference the DPB does not hold, a `frame_num` gap, an AU
///   whose NALU walk stopped early. The plan was completed with a SUBSTITUTE in
///   place of something lost: the picture is damaged, so its output is released
///   unshown and a re-anchor is requested.
/// - **Spec-legal envelope signals** — h265's `NonZeroReorder` (the activated SPS
///   sets `sps_max_num_reorder_pics > 0`) and h264's `Mmco5Rebase`. pf-bitstream
///   documents both as "spec-legal and fully planned"; they exist as the field
///   signal that a punktfunk-host assumption broke, not as damage. `NonZeroReorder`
///   in particular fires on the AU that ACTIVATES an SPS — the opening IDR, and the
///   fresh IDR at every ABR resolution change — so treating it as concealment costs
///   a released-unshown frame plus a keyframe round trip at every renegotiation, on
///   a stream the planner planned correctly. pf-bitstream's own conformance harness
///   excludes `NonZeroReorder` from its integrity set for exactly this reason.
///
/// Everything is logged either way; only integrity warnings drop the frame.
enum PlanWarnings {
    H264(Vec<pf_vkdecode::PlanWarning>),
    H265(Vec<pf_vkdecode::H265PlanWarning>),
}

impl PlanWarnings {
    fn is_empty(&self) -> bool {
        match self {
            PlanWarnings::H264(w) => w.is_empty(),
            PlanWarnings::H265(w) => w.is_empty(),
        }
    }

    /// Just the warnings that mean the picture is damaged — the concealment set.
    /// Allocates, but only off the clean path: [`Self::is_empty`] is true for every
    /// AU of a healthy stream.
    ///
    /// The predicate itself lives in pf-vkdecode
    /// ([`pf_vkdecode::is_integrity_warning`]) rather than here, so the
    /// fault-injection harness asserts detection against the SAME list this
    /// conceals on. Two copies would let a test prove a detection production does
    /// not actually perform.
    fn integrity(&self) -> PlanWarnings {
        match self {
            PlanWarnings::H264(w) => PlanWarnings::H264(
                w.iter()
                    .filter(|x| pf_vkdecode::is_integrity_warning(x))
                    .cloned()
                    .collect(),
            ),
            PlanWarnings::H265(w) => PlanWarnings::H265(
                w.iter()
                    .filter(|x| pf_vkdecode::is_integrity_warning_h265(x))
                    .cloned()
                    .collect(),
            ),
        }
    }

    fn len(&self) -> usize {
        match self {
            PlanWarnings::H264(w) => w.len(),
            PlanWarnings::H265(w) => w.len(),
        }
    }

    /// The concealment log. Per arm so the rendering is the codec's OWN enum —
    /// `warnings=[FrameNumGap { .. }]`, exactly what the hardware-verified H.264
    /// path emitted before dispatch existed (a `Vec<String>` renders
    /// `["FrameNumGap { .. }"]`, and a wrapper enum would prefix the arm).
    ///
    /// `concealed` is how many of the rendered warnings are INTEGRITY warnings —
    /// the count the frame was actually dropped for. Both numbers are carried
    /// because they can differ: the list is every warning of the AU (a spec-legal
    /// companion is context worth having), while the count is the damage. On H.264
    /// the two coincide for every warning a punktfunk host can produce.
    fn warn_concealment(&self, concealed: usize) {
        // Spelled out per arm rather than shared through a `const`: this is the
        // H.264 path's PRODUCTION log line, and a literal is what keeps it a static
        // tracing message rather than a formatted one.
        match self {
            PlanWarnings::H264(w) => tracing::warn!(
                concealed,
                warnings = ?w,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            ),
            PlanWarnings::H265(w) => tracing::warn!(
                concealed,
                warnings = ?w,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            ),
        }
    }

    /// The spec-legal log: the planner flagged an envelope fact and planned the AU
    /// in full, so the frame is SHOWN. Rare by construction (SPS activation, MMCO
    /// 5), which is why it is a `warn` and not a per-frame `debug`.
    fn warn_planned_in_full(&self) {
        match self {
            PlanWarnings::H264(w) => tracing::warn!(
                warnings = ?w,
                "native decode: spec-legal envelope signal — the AU was planned in \
                 full and the frame is kept"
            ),
            PlanWarnings::H265(w) => tracing::warn!(
                warnings = ?w,
                "native decode: spec-legal envelope signal — the AU was planned in \
                 full and the frame is kept"
            ),
        }
    }
}

/// One frame shipped to the presenter and not yet fully settled: settled = its release
/// token came back (GPU reads proven done) AND its status query was read.
struct Shipped {
    seq: u64,
    frame: DecodedVkFrame,
    /// The presenter (or a drop on the way there) returned the token.
    released: bool,
    /// The token said the sampling submission (with its `value + 1` timeline
    /// signal) was enqueued — forwarded to `release_frame` so the decoder waits
    /// the write-back before reusing the image.
    presented: bool,
    /// The status query read a conclusive verdict (or the poll belt expired).
    resolved: bool,
    /// Polls attempted after the token returned — see [`MAX_POLLS_AFTER_RELEASE`].
    polls_after_release: u32,
}

/// Mark the shipped entry a token names as released. Returns false when nothing
/// matches (a late token from before a demotion drain — benign). Pure bookkeeping,
/// split out so the channel-drain behavior is CPU-testable.
fn note_token(outstanding: &mut [Shipped], token: NativeReleaseToken) -> bool {
    match outstanding.iter_mut().find(|s| s.seq == token.seq) {
        Some(s) => {
            debug_assert_eq!(
                s.frame.generation, token.generation,
                "a token's generation always matches the frame it rode on"
            );
            s.released = true;
            s.presented = token.presented;
            true
        }
        None => false,
    }
}

/// Flatten a delivered [`DecodedVkFrame`] into the ash-free [`NativeVkFrame`] the
/// presenter consumes. Pure over the frame (the guard is the caller's), so the
/// projection — every fact the presenter can no longer look up for itself — is
/// CPU-testable.
///
/// The one that is easy to get wrong is [`NativeVkFrame::vk_format`]: the picture
/// format is the STREAM's, not the codec's. H.264 in this program is always the 8-bit
/// 4:2:0 envelope (NV12), but an H.265 session decodes Main to NV12, Main 10 to P010
/// and RExt 4:4:4 to the two-plane 4:4:4 formats — and can change format mid-stream
/// when the host renegotiates. A consumer that assumes 8-bit 4:2:0 renders a Main 10
/// picture with 8-bit transfer/range math: plausible-looking and wrong. So the format
/// is carried, never inferred, all the way to the presenter's CSC pass.
fn project_frame(frame: &DecodedVkFrame, guard: NativeReleaseGuard) -> NativeVkFrame {
    NativeVkFrame {
        image: frame.image.as_raw(),
        vk_format: crate::video::RawVkFormat(frame.format.as_raw()),
        plane_views: [frame.plane_views[0].as_raw(), frame.plane_views[1].as_raw()],
        layer: frame.layer,
        layout: if frame.layout == vk::ImageLayout::VIDEO_DECODE_DPB_KHR {
            NativeVkLayout::DecodeDpb
        } else {
            NativeVkLayout::DecodeDst
        },
        semaphore: frame.semaphore.as_raw(),
        semaphore_value: frame.value,
        generation: frame.generation,
        width: frame.crop.width,
        height: frame.crop.height,
        coded_width: frame.coded_width,
        coded_height: frame.coded_height,
        crop_x: frame.crop.x,
        crop_y: frame.crop.y,
        // H.273 code points straight off the picture's ACTIVE SPS/VUI — per frame,
        // never latched, because the Windows host switches an HDR desktop to
        // PQ/BT.2020 IN-BAND (the Welcome still says SDR). pf-bitstream applies
        // E.2.1's "unspecified" inference (2/2/2, limited) where the VUI is
        // silent, and `csc_rows` resolves "unspecified" to its BT.709-limited
        // SDR default — same verdicts libavcodec's CICP passthrough produced.
        color: ColorDesc {
            primaries: frame.colour.colour_primaries,
            transfer: frame.colour.transfer_characteristics,
            matrix: frame.colour.matrix_coefficients,
            full_range: frame.colour.video_full_range,
        },
        keyframe: frame.is_idr,
        poc: frame.poc,
        // The recovery point SEI's verdict for THIS picture, folded by
        // pf-vkdecode's `RecoveryWatch` at plan time and translated here into the
        // shared gate's vocabulary. The two structs are deliberately separate
        // types with the same shape: pf-vkdecode must not depend on punktfunk-core
        // to describe a bitstream fact, and punktfunk-core must not depend on
        // pf-vkdecode to accept one.
        recovery: punktfunk_core::reanchor::LocalRecovery {
            sei_here: frame.recovery.sei_here,
            is_recovery_point: frame.recovery.is_recovery_point,
        },
        // Which side of a loss this picture was DECODED on. Carried beside the
        // recovery mark because the mark is worthless without it: a post-failure
        // DPB flush delivers pre-loss pictures after the loss, and their marks
        // describe a wave that completed before it.
        decode_order: frame.decode_order,
        guard,
    }
}

/// The picture format an H.265 session of the negotiated shape decodes to, or a named
/// refusal for a shape pf-vkdecode has no output format for at all.
///
/// The DEVICE-INDEPENDENT half of [`NativeVulkanDecoder::new`]'s shape check: 4:2:2
/// and 12-bit are legal H.265 that no punktfunk host emits and this client has no
/// plumbing for, so no driver has to be asked about them. Pure, so the refusal is
/// CPU-testable — the device-dependent half (a shape with a format that THIS driver
/// does not advertise) is [`VkH265Decoder::probe_stream_support`], covered by
/// pf-vkdecode's `derive_caps_h265` refusal tests.
fn h265_picture_format(stream: crate::video::StreamFormat) -> Result<vk::Format> {
    let depth = stream.bit_depth_minus8().ok_or_else(|| {
        anyhow!(
            "negotiated HEVC bit depth {} is outside the 8/10-bit decode envelope",
            stream.bit_depth
        )
    })?;
    pf_vkdecode::output_format_for(stream.chroma_format_idc, depth).ok_or_else(|| {
        anyhow!(
            "no native picture format for the negotiated HEVC stream shape \
             (chroma_format_idc={}, {}-bit)",
            stream.chroma_format_idc,
            stream.bit_depth
        )
    })
}

/// The native backend: the decoder plus the shipped-frame ledger and release channel.
pub(crate) struct NativeVulkanDecoder {
    dec: Codec,
    /// Cloned into every shipped frame's guard. `Option` so teardown can DROP the
    /// backend's own sender: only then does `release_rx` report Disconnected once
    /// the last guard is gone — the teardown short-circuit signal.
    release_tx: Option<mpsc::Sender<NativeReleaseToken>>,
    release_rx: mpsc::Receiver<NativeReleaseToken>,
    /// Display-ready frames not yet handed to the pump (burst outputs — decode
    /// delivers one per call; the rest wait here, oldest first).
    deliverable: std::collections::VecDeque<DecodedVkFrame>,
    outstanding: Vec<Shipped>,
    next_seq: u64,
    /// The session's integrity counters (M4). Plain adds on the decode path, read
    /// once per stats window — no allocation, no per-frame work.
    health: DecodeHealth,
    /// Stream damage happened and the host should be asked for a re-anchor.
    /// Drained by `video::Decoder::decode_frame`, which routes it into the same
    /// `want_keyframe` every other recovery ask uses (module doc's policy).
    want_recovery: bool,
    /// Corrupt the AU on its way into the decoder — `PUNKTFUNK_AU_FAULT`,
    /// `None` unless armed. Lives at THIS boundary rather than in
    /// `video::Decoder::decode_frame` on purpose: this is the lane whose detectors
    /// the injector exists to fire, and putting the knob here means a faulted AU
    /// is byte-identical to what the decoder would have been handed by a lossy
    /// network — no other backend's behaviour can be perturbed by a typo'd
    /// variable.
    fault: Option<pf_vkdecode::AuFault>,
}

// SAFETY: the decoder is used strictly serially through `&mut self` from whichever
// single thread owns the enclosing `Decoder` (the session pump) — `Send` only moves
// that ownership. The `Rc`s inside pf-vkdecode's planners (H.264 and H.265 alike)
// never escape them, so they all move together; every queue submission runs under the
// collision-aware queue lock; the mpsc endpoints are `Send`. Same contract, same shape
// as the `VulkanDecoder` and `PyroWaveDecoder` impls above/beside it. Deliberately NOT
// `Sync`.
unsafe impl Send for NativeVulkanDecoder {}

impl NativeVulkanDecoder {
    /// Build the backend over the presenter's device for `codec` — the codec the
    /// session negotiated, already admitted by `video::native_vulkan_gate` (which
    /// checked that the decode family advertises this codec's decode op; the
    /// decoders re-check it themselves rather than trust the caller, because
    /// creating a video session for a codec operation the family cannot run is
    /// undefined behaviour rather than an error).
    ///
    /// Sessions and pools are built lazily from the first AU's parameter sets, so
    /// nothing BELOW this constructor depends on the stream's shape — which is why
    /// the shape is checked HERE, against `stream` (the host's resolved Welcome
    /// facts), rather than being discovered at the first decode.
    ///
    /// The difference is which rung a refusal lands on. pf-vkdecode's picture format
    /// is the STREAM's (Main → NV12, Main 10 → P010, RExt 4:4:4 → the two-plane 4:4:4
    /// formats) and a device that advertises H.265 decode need not advertise a format
    /// for every shape of it: 4:4:4 is absent everywhere but NVIDIA. Discovered
    /// lazily, that is a mid-stream ERROR STREAK, and the streak machinery demotes a
    /// Vulkan rung to VAAPI/D3D11VA — PAST FFmpeg-Vulkan, which on NVIDIA/Linux (no
    /// usable VAAPI) means a 4K HEVC session lands on SOFTWARE. Refused here it is an
    /// ordinary construction failure, and `video::Decoder::new` falls through to
    /// FFmpeg-Vulkan — the rung that session ran on before this backend existed.
    ///
    /// Two legs the probe cannot see, because they are stream facts no negotiation
    /// carries: a level above the device's `maxLevelIdc`, and an SPS that disagrees
    /// with the Welcome. Those still surface at the first decode — and are caught by
    /// the "never delivered a frame" arm in [`crate::video::Decoder::decode_frame`],
    /// which routes exactly that state to FFmpeg-Vulkan instead of past it.
    ///
    /// H.264 is deliberately NOT probed: its envelope is fixed at 8-bit 4:2:0, so the
    /// only fact a probe could add is a profile idc guess — on the one path in this
    /// program that is hardware-verified bit-exact against libavcodec. It keeps the
    /// never-delivered arm as its backstop.
    pub(crate) fn new(
        vk: &VulkanDecodeDevice,
        codec: NativeCodec,
        stream: crate::video::StreamFormat,
    ) -> Result<NativeVulkanDecoder> {
        if !vk.video_decode {
            bail!("presenter device lacks Vulkan Video decode");
        }
        let lock: Box<dyn pf_vkdecode::QueueLock> =
            if submit_queues_collide(vk.graphics_qf, vk.decode_qf) {
                Box::new(NativeQueueLock::Shared(vk.queue_lock.clone()))
            } else {
                Box::new(NativeQueueLock::Uncontended)
            };
        let handles = DeviceHandles {
            get_instance_proc_addr: vk.get_instance_proc_addr,
            instance: vk.instance,
            physical_device: vk.physical_device,
            device: vk.device,
            decode_qf: vk.decode_qf,
            decode_queue_index: DECODE_QUEUE_INDEX,
            graphics_qf: vk.graphics_qf,
        };
        // The `DeviceHandles` caller contract, held for the decoder's whole lifetime
        // and identical for both arms (it is the HANDLES' contract, not the codec's):
        // the handles are the presenter's live instance/device, which outlives every
        // session pump (the run loop tears the pump — and with it this decoder — down
        // first: the exact liveness contract the FFmpeg and PyroWave backends already
        // rely on over the same bundle). `video_decode` (checked above) is set only
        // when the presenter enabled the Vulkan Video decode extension stack +
        // synchronization2/timelineSemaphore at device creation — including the
        // per-codec `VK_KHR_video_decode_h264`/`_h265`/`_av1` extensions, one for
        // every codec operation the decode family advertises (`vk/setup.rs` enables
        // exactly those it finds). What the decoders then re-check for themselves is
        // the QUEUE FAMILY's advertised `videoCodecOperations` — the device's own
        // claim about the family, which is what `native_vulkan_gate` reads too. That
        // is not a proof the extension was enabled at `vkCreateDevice`; it is the
        // same fact `vk/setup.rs` derived its enable list FROM, so the two agree by
        // construction here and the check catches a caller that got the family wrong.
        // `decode_qf`/`graphics_qf` mirror the families the presenter created queues
        // for (one queue, index 0, each).
        let dec = match codec {
            NativeCodec::H264 => {
                // SAFETY: the handle contract stated directly above.
                let d = unsafe { VkH264Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkH264Decoder init: {e}"))?;
                Codec::H264(d)
            }
            NativeCodec::H265 => {
                // The device-independent half of the shape check, first: a stream
                // shape pf-vkdecode has NO picture format for (4:2:2, 12-bit) needs
                // no driver to refuse it.
                let wanted = h265_picture_format(stream)?;
                // SAFETY: the handle contract stated directly above.
                let d = unsafe { VkH265Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkH265Decoder init: {e}"))?;
                // …and the device-dependent half: does THIS driver advertise that
                // format for a decode session of this profile? Same query and same
                // derivation `ensure_state` would run at the first AU — only the
                // timing differs, and the timing is the whole point.
                let depth = stream
                    .bit_depth_minus8()
                    .expect("h265_picture_format accepted the depth");
                d.probe_stream_support(stream.chroma_format_idc, depth)
                    .map_err(|e| {
                        anyhow!(
                            "device cannot decode the negotiated HEVC stream shape \
                             (chroma_format_idc={}, {}-bit, needs {wanted:?}): {e}",
                            stream.chroma_format_idc,
                            stream.bit_depth
                        )
                    })?;
                Codec::H265(d)
            }
        };
        let (release_tx, release_rx) = mpsc::channel();
        let status_queries = dec.status_queries();
        if !status_queries {
            // Said once, loudly, at construction rather than only in the stats
            // line: on this device a clean integrity report means "nothing was
            // detectable", not "nothing was wrong" — and a support engineer
            // reading a log after the fact has no stats window to consult.
            tracing::warn!(
                "native decode: this device's decode queue family does not support \
                 RESULT_STATUS queries — driver-reported corruption is not \
                 observable on this session (decode status degrades to timeline \
                 completion, FFmpeg parity)"
            );
        }
        // `PUNKTFUNK_AU_FAULT=<mode>[:<period>]` — the deliberate-corruption knob
        // (pf_vkdecode::fault). Unset is the only normal state; a spec that does
        // not parse leaves the injector disarmed and says so rather than half
        // arming.
        let fault = std::env::var("PUNKTFUNK_AU_FAULT").ok().and_then(|spec| {
            match pf_vkdecode::AuFault::from_spec(&spec) {
                Some(f) => {
                    tracing::warn!(
                        mode = ?f.mode(),
                        period = f.period(),
                        "PUNKTFUNK_AU_FAULT: deliberately corrupting decoder input"
                    );
                    Some(f)
                }
                None => {
                    tracing::warn!(
                        value = %spec,
                        "PUNKTFUNK_AU_FAULT not understood (want drop|truncate|flip[:period]) \
                         — ignored"
                    );
                    None
                }
            }
        });
        Ok(NativeVulkanDecoder {
            dec,
            release_tx: Some(release_tx),
            release_rx,
            deliverable: std::collections::VecDeque::new(),
            outstanding: Vec::new(),
            next_seq: 0,
            health: DecodeHealth {
                status_queries,
                ..DecodeHealth::default()
            },
            want_recovery: false,
            fault,
        })
    }

    /// This session's integrity counters — see [`DecodeHealth`].
    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    /// The newest planned picture's DECODE-order ordinal — see
    /// [`NativeVkFrame::decode_order`].
    pub(crate) fn decode_order(&self) -> u64 {
        self.dec.decode_order()
    }

    /// Drain the "the stream was damaged; please ask the host to re-anchor" flag.
    /// Deliberately separate from an `Err` return — see the module doc's recovery
    /// policy: concealment is a fact about the STREAM and must not tick the
    /// decoder-demotion streak.
    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.want_recovery)
    }

    /// Feed one complete access unit.
    ///
    /// `Ok(Some)` = a display-ready picture. `Ok(None)` = no picture this AU, which
    /// covers three unrelated things and the caller treats all three the same
    /// (its no-output/re-anchor machinery, exactly as for FFmpeg): the decoder
    /// buffered without output, an H.265 RASL picture was skipped after an open-GOP
    /// join, or the AU's plan needed CONCEALMENT and its output was released
    /// unshown. `Err` = the DECODER is in trouble — a Vulkan/session error, or a
    /// driver `RESULT_STATUS` verdict of Failed on a prior frame — which the
    /// caller's streak/demotion machinery is entitled to act on.
    ///
    /// That split is the M4 recovery policy and it is deliberate (module doc):
    /// concealment says the STREAM lost data, not that this decoder is failing, so
    /// it raises [`Self::take_recovery_request`] instead of an error. The ask
    /// reaches the host at the same moment and through the same 100 ms throttle it
    /// always did; what it no longer does is spend a life on the demotion streak
    /// and cost the session its hardware rung on a lossy link.
    ///
    /// A skipped RASL picture is not trouble at all: the decoder never turns
    /// `h265::PlanError::RaslSkipped` into a `VkDecodeError` and clears the warning
    /// ledger on its way out, so the concealment branch cannot fire on it either.
    /// Nothing is released unshown and no re-anchor is asked for (module doc;
    /// pf-bitstream `h265`).
    ///
    /// Ordering: the CURRENT AU decodes FIRST — the planner's reference state must
    /// advance even when a PRIOR frame's status turns out Failed, or the recovery
    /// IDR would land on a decoder that skipped an AU and reports a phantom
    /// reference gap. The prior-frame verdicts are checked after; a corrupt verdict
    /// costs exactly this one AU's output (released unshown), never parser state.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<NativeVkFrame>> {
        self.drain_releases();

        // Fault injection, at the last possible moment before the decoder: a
        // faulted AU is byte-for-byte what a lossy network would have delivered,
        // so every detector below sees the real thing rather than a special case.
        // Inert (and free — no branch cost worth naming, no copy) unless armed.
        let faulted;
        let au = match self.fault.as_mut().map(|f| f.apply(au)) {
            None | Some(pf_vkdecode::FaultAction::Pass) => au,
            Some(pf_vkdecode::FaultAction::Drop) => {
                tracing::warn!(len = au.len(), "PUNKTFUNK_AU_FAULT: dropping this AU");
                // Never fed, so nothing decodes and nothing is display-ready —
                // the same observable state a lost AU produces. The NEXT AU is
                // where detection happens.
                //
                // Prior frames' verdicts still have to SETTLE here, though, or a
                // fault run defers every one of them by an AU and the query slots
                // sit unread meanwhile. What is deliberately NOT folded is a
                // clean verdict: the health ledger holds one entry per AU the
                // decoder was FED, and an AU that never reached it is no evidence
                // that anything is healthy — folding `note(false, false, 0)` here
                // would reset the concealed run on the very AU that was lost.
                let verdicts = self.settle_statuses();
                if verdicts.total() > 0 {
                    self.health.note(false, false, verdicts.total());
                    return Err(self.status_error(verdicts));
                }
                return Ok(None);
            }
            Some(pf_vkdecode::FaultAction::Corrupt(bytes)) => {
                tracing::warn!(
                    len = au.len(),
                    corrupted_len = bytes.len(),
                    "PUNKTFUNK_AU_FAULT: corrupting this AU"
                );
                faulted = bytes;
                &faulted[..]
            }
        };

        // A REFUSAL is the loudest thing this lane can say, and it has to reach
        // the health ledger before it reaches the caller. Folded here rather than
        // after the `?` because there is no after: a `PlanError`
        // (`Parse`/`OutsideEnvelope`/`AwaitingIdr`/`NoActiveParamSet`) or a
        // Vulkan/session failure returns straight out, and until M4's review this
        // path incremented nothing at all — so a rung refusing EVERY AU (a host
        // renegotiating outside the envelope: a frozen screen) reported
        // `damaged 0 · failed 0 · run 0` and printed no integrity line whatsoever.
        // A clean bill of health on a decoder that decoded nothing is the exact
        // failure this program exists to end.
        //
        // Prior frames' status verdicts settle first, for the same reason the
        // clean path settles before it folds: the refusal costs this AU, and the
        // frames already shipped still owe their verdicts.
        let delivered = match self.dec.decode(au) {
            Ok(delivered) => delivered,
            Err(e) => {
                let verdicts = self.settle_statuses();
                self.health.note(false, true, verdicts.total());
                tracing::warn!(
                    error = %e,
                    driver_failed = verdicts.driver_failed,
                    "native decode refused the access unit"
                );
                return Err(anyhow!("decode: {e}"));
            }
        };
        let warnings = self.dec.take_warnings();
        // Everything this AU made display-ready, oldest first (`take_ready` drained
        // so burst outputs are never stranded inside the decoder).
        let mut fresh: Vec<DecodedVkFrame> = Vec::new();
        if let Some(frame) = delivered {
            fresh.push(frame);
        }
        while let Some(frame) = self.dec.take_ready() {
            fresh.push(frame);
        }

        let verdicts = self.settle_statuses();
        // ONLY integrity warnings are concealment (see [`PlanWarnings`]): a
        // spec-legal envelope signal — h265's `NonZeroReorder` on every SPS
        // activation, h264's `Mmco5Rebase` — is an AU the planner planned in FULL,
        // and dropping its frame would hitch the picture at every renegotiation.
        let integrity = warnings.integrity();
        let concealed = !integrity.is_empty();
        // One fold per AU, whatever the verdict: a clean AU is what ENDS a run,
        // and a counter that only ever counts damage cannot tell a lossy link
        // apart from a stream that never came back.
        self.health.note(concealed, false, verdicts.total());
        if concealed || verdicts.total() > 0 {
            // Concealment planned into THIS AU, or a bad status verdict on a
            // PRIOR frame (driver-reported corruption — the Ally X class,
            // invisible to FFmpeg's query-less decoder — or a status that could
            // not be established at all): this call's output is released unshown
            // either way, because the picture is not fit to present.
            for frame in fresh {
                if let Err(e) = self.dec.release_frame(&frame, false) {
                    tracing::debug!(error = %e, "releasing an unshown frame failed");
                }
            }
            if verdicts.total() > 0 {
                // A verdict about the DECODER rather than the stream: an error,
                // streak-eligible, same volume as an FFmpeg reference-miss error
                // (never quieter).
                return Err(self.status_error(verdicts));
            }
            warnings.warn_concealment(integrity.len());
            self.want_recovery = true;
            return Ok(None);
        }
        if !warnings.is_empty() {
            warnings.warn_planned_in_full();
        }

        self.deliverable.extend(fresh);
        Ok(self.deliverable.pop_front().map(|frame| self.ship(frame)))
    }

    /// Wrap a delivered [`DecodedVkFrame`] for the presenter and enter it into the
    /// shipped ledger (the original stays here — release/poll need it).
    fn ship(&mut self, frame: DecodedVkFrame) -> NativeVkFrame {
        let seq = self.next_seq;
        self.next_seq += 1;
        let token = NativeReleaseToken {
            seq,
            generation: frame.generation,
            presented: false,
        };
        let native = project_frame(
            &frame,
            NativeReleaseGuard::new(
                self.release_tx
                    .as_ref()
                    .expect("release_tx lives until Drop")
                    .clone(),
                token,
            ),
        );
        self.outstanding.push(Shipped {
            seq,
            frame,
            released: false,
            presented: false,
            resolved: false,
            polls_after_release: 0,
        });
        native
    }

    /// Bounded wait for a shipped frame's decode-complete signal — the pump's
    /// sampled decode-latency stat (`Decoder::wait_hw_decoded`), one frame per
    /// stats window. The raw pair names a frame still in the shipped ledger (the
    /// pump waits on the same thread that just shipped it, before any settle
    /// could retire it); the ledger lookup is the liveness proof — an unreleased
    /// frame pins its pool, so a pair matching nothing (already settled, or a
    /// stray) just declines the sample instead of touching unknown handles.
    pub(crate) fn wait_timeline(&self, sem: u64, value: u64, timeout_ns: u64) -> bool {
        self.outstanding
            .iter()
            .find(|s| s.frame.semaphore.as_raw() == sem && s.frame.value == value)
            .is_some_and(|s| self.dec.wait_decoded(&s.frame, timeout_ns))
    }

    /// Drain the release channel, marking returned frames (release itself waits for
    /// the status read — see [`Self::settle_statuses`]).
    fn drain_releases(&mut self) {
        while let Ok(token) = self.release_rx.try_recv() {
            if !note_token(&mut self.outstanding, token) {
                tracing::debug!(
                    seq = token.seq,
                    generation = token.generation,
                    "release token without an outstanding frame"
                );
            }
        }
    }

    /// The error a bad status verdict surfaces as, worded for the device it came
    /// from: a driver that reported corruption is named as such, a device that
    /// cannot report one is not blamed for a verdict it never gave.
    fn status_error(&self, verdicts: StatusVerdicts) -> anyhow::Error {
        if verdicts.driver_failed > 0 {
            anyhow!(
                "driver reported decode corruption on {} prior frame(s) \
                 (RESULT_STATUS_ONLY query) — re-anchor needed",
                verdicts.driver_failed
            )
        } else {
            anyhow!(
                "decode status unreadable on {} prior frame(s) (this device answers \
                 no RESULT_STATUS queries — the verdict degraded to the decode \
                 timeline) — re-anchor needed",
                verdicts.unreadable
            )
        }
    }

    /// Poll the status query of every unresolved shipped frame (non-blocking) and
    /// release the ones that are both status-settled and token-returned. Returns the
    /// frames that NEWLY read `Failed`, split by whether this device can produce a
    /// driver verdict at all — see [`StatusVerdicts`].
    ///
    /// Polling an unreleased frame is always sound: its slot is pinned until
    /// `release_frame`, so the query slot it names cannot have been recycled under it
    /// (the false-`Failed` a recycled slot would read).
    fn settle_statuses(&mut self) -> StatusVerdicts {
        let mut verdicts = StatusVerdicts::default();
        // A device fact, read once: it decides which KIND of verdict a `Failed`
        // read below is (`StatusVerdicts`), never whether the frame is dropped.
        let status_queries = self.dec.status_queries();
        let Self {
            dec, outstanding, ..
        } = self;
        for s in outstanding.iter_mut() {
            if s.resolved {
                continue;
            }
            // A session rebuild (stream renegotiation) already made this frame stale:
            // its SESSION objects (query pool included) are gone — the picture pool
            // lives on in the decoder's graveyard while we hold the image, but the
            // query verdict is unknowable and poll_status would report the
            // conservative Failed — which is NOT driver corruption. Resolve it
            // quietly; the rebuild rode an IDR, so the stream has its re-anchor
            // already.
            if s.frame.generation != dec.generation() {
                tracing::debug!(
                    poc = s.frame.poc,
                    frame_generation = s.frame.generation,
                    "outstanding frame outlived its session generation — status unknowable"
                );
                s.resolved = true;
                continue;
            }
            match dec.poll_status(&s.frame) {
                DecodeStatus::Ok => s.resolved = true,
                DecodeStatus::Failed => {
                    s.resolved = true;
                    if status_queries {
                        verdicts.driver_failed += 1;
                        tracing::warn!(
                            poc = s.frame.poc,
                            slot = s.frame.query_slot,
                            "decode status query: Failed (driver-reported corruption)"
                        );
                    } else {
                        // No query pool on this device, so nothing here is the
                        // driver's opinion of the decode: `poll_status` degraded
                        // to reading the decode timeline and could not establish
                        // completion (a lost device, an unreadable semaphore).
                        // The picture is dropped exactly the same way — it is the
                        // ATTRIBUTION that must not be invented (`StatusVerdicts`).
                        verdicts.unreadable += 1;
                        tracing::warn!(
                            poc = s.frame.poc,
                            "decode status unreadable — this device answers no \
                             RESULT_STATUS queries, so this is a timeline failure, \
                             not a driver verdict"
                        );
                    }
                }
                DecodeStatus::Pending => {
                    if s.released {
                        // Token back ⇒ the decode op completed before the presenter's
                        // sampling ⇒ the query should be readable. Belt, not a path.
                        s.polls_after_release += 1;
                        if s.polls_after_release >= MAX_POLLS_AFTER_RELEASE {
                            tracing::debug!(
                                poc = s.frame.poc,
                                "status query still pending after release — giving \
                                 the slot back with an unknown verdict"
                            );
                            s.resolved = true;
                        }
                    }
                }
            }
        }
        outstanding.retain(|s| {
            if !(s.released && s.resolved) {
                return true;
            }
            match dec.release_frame(&s.frame, s.presented) {
                Ok(()) => {}
                // Not a best-effort no-op: stale-generation frames release into the
                // decoder's graveyard (a rebuild retires a still-held pool INTACT,
                // and this very call is what lets it die on its last token). An Err
                // is therefore a bookkeeping ghost — a double release — never a
                // held image left dangling.
                Err(e) => tracing::debug!(error = %e, "release_frame: {e}"),
            }
            false
        });
        verdicts
    }
}

impl Drop for NativeVulkanDecoder {
    fn drop(&mut self) {
        // Ordering contract: the run loop drops the PRESENTER's frame (its retired
        // slot, fence-waited) before joining the pump that owns this backend — so
        // by the time this Drop runs, outstanding tokens are either already in the
        // channel or arrive imminently; the bounded wait below is for that hand-off,
        // not for future GPU work.
        //
        // Frames never handed to the pump release directly (unsampled).
        for frame in std::mem::take(&mut self.deliverable) {
            if let Err(e) = self.dec.release_frame(&frame, false) {
                tracing::debug!(error = %e, "releasing an undelivered frame failed");
            }
        }
        // Drop our own sender FIRST: once every shipped guard is gone too, the
        // channel reports Disconnected — the "presenter can no longer produce
        // tokens" signal that short-circuits the wait instead of burning the full
        // budget against a presenter that is already gone.
        drop(self.release_tx.take());
        // Wait (bounded) for the presenter to hand back every shipped frame before
        // the decoder's Drop destroys the pool images: a returned token proves the
        // sampling submission's fence was waited, i.e. no GPU work of the
        // presenter's still reads the pools (the decoder's own drain covers only
        // decode work). Graveyarded pools ride the same token contract — a
        // mid-stream renegotiation retires a still-held pool INTACT, and the
        // release calls below route stale-generation frames into the graveyard,
        // so those pools too die only once their last presenter fence was waited.
        let deadline = Instant::now() + TEARDOWN_BUDGET;
        loop {
            self.drain_releases();
            let Self {
                dec, outstanding, ..
            } = self;
            outstanding.retain(|s| {
                if !s.released {
                    return true;
                }
                if let Err(e) = dec.release_frame(&s.frame, s.presented) {
                    tracing::debug!(error = %e, "teardown release_frame: {e}");
                }
                false
            });
            if self.outstanding.is_empty() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                tracing::warn!(
                    outstanding = self.outstanding.len(),
                    "native decode teardown: presenter still holds frames past the \
                     budget — destroying the pools anyway"
                );
                break;
            }
            match self
                .release_rx
                .recv_timeout((deadline - now).min(Duration::from_millis(50)))
            {
                Ok(token) => {
                    note_token(&mut self.outstanding, token);
                }
                // Every sender is gone (ours dropped above, every guard dropped):
                // no more tokens can EVER arrive — anything still outstanding is a
                // bookkeeping ghost, not a held frame. Stop waiting.
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if !self.outstanding.is_empty() {
                        tracing::debug!(
                            outstanding = self.outstanding.len(),
                            "release channel disconnected with entries outstanding — \
                             no tokens can arrive; proceeding with teardown"
                        );
                    }
                    break;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }
        // `self.dec` drops after this body: it drains its own decode-side GPU work
        // and destroys any remaining graveyard pools (warned — a forfeit here means
        // the presenter kept frames past the budget).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A delivered frame whose every field carries a DISTINCT non-zero value.
    ///
    /// Deliberately not "inert handles, zeros elsewhere": [`project_frame`] is a
    /// 20-field struct literal lifted out of `ship`, and the bugs it can hide are
    /// field SWAPS and DROPS — `crop_x: frame.crop.y`, `semaphore_value: frame.poc
    /// as u64`, a `keyframe` that stopped being carried. Against zeros every one of
    /// those passes. So: no two numbers here are equal, no boolean is false, and
    /// each CICP code point differs from the others.
    fn decoded(format: vk::Format, layout: vk::ImageLayout, generation: u64) -> DecodedVkFrame {
        DecodedVkFrame {
            image: vk::Image::from_raw(0x1001),
            format,
            view: vk::ImageView::from_raw(0x2001),
            plane_views: [
                vk::ImageView::from_raw(0x2002),
                vk::ImageView::from_raw(0x2003),
            ],
            layer: 3,
            layout,
            coded_width: 1920,
            coded_height: 1088,
            // A non-origin crop: punktfunk hosts emit origin crops only, but x != y
            // here is what makes an x/y swap in the projection visible.
            crop: pf_vkdecode::DisplayCrop {
                x: 8,
                y: 4,
                width: 1904,
                height: 1072,
            },
            // BT.2020 primaries / PQ transfer / a third code point for the matrix,
            // so no two CICP fields share a value.
            colour: pf_vkdecode::ColourDescription {
                colour_primaries: 9,
                transfer_characteristics: 16,
                matrix_coefficients: 10,
                video_full_range: true,
            },
            semaphore: vk::Semaphore::from_raw(0x3001),
            value: 7,
            poc: 5,
            is_idr: true,
            // Both facts SET and distinct from the defaults, for the same reason
            // every other field here is: a projection that dropped the recovery
            // mark would silently reinstate the 500 ms freeze on every
            // intra-refresh session, and against `false` that passes.
            recovery: pf_vkdecode::RecoveryMark {
                sei_here: true,
                is_recovery_point: true,
            },
            // Distinct from every other number here for the same reason: a
            // projection that dropped the decode ordinal would make every frame
            // look pre-loss (0) and silently disable the local-recovery path.
            decode_order: 17,
            query_slot: 2,
            submission: 11,
            picture: 6,
            generation,
        }
    }

    /// A shipped-ledger entry with inert handles — the bookkeeping under test is pure.
    fn shipped(seq: u64, generation: u64) -> Shipped {
        Shipped {
            seq,
            // The ledger under test never reads the picture format; NV12 is what an
            // H.264 session always delivers.
            frame: decoded(
                pf_vkdecode::NV12,
                vk::ImageLayout::VIDEO_DECODE_DST_KHR,
                generation,
            ),
            released: false,
            presented: false,
            resolved: false,
            polls_after_release: 0,
        }
    }

    /// Project one frame with a throwaway guard (the channel is the caller's).
    fn project(frame: &DecodedVkFrame) -> NativeVkFrame {
        let (tx, _rx) = mpsc::channel();
        project_frame(
            frame,
            NativeReleaseGuard::new(
                tx,
                NativeReleaseToken {
                    seq: 0,
                    generation: frame.generation,
                    presented: false,
                },
            ),
        )
    }

    /// The picture format is the STREAM's, and it must reach the presenter intact:
    /// H.264 and H.265 Main deliver NV12, Main 10 delivers P010, RExt 4:4:4 delivers
    /// the two-plane 4:4:4 formats. The presenter picks bit depth, MSB packing and
    /// chroma siting from exactly this number, so a projection that dropped or
    /// defaulted it would render a Main 10 picture with 8-bit math — decoded
    /// correctly, displayed wrong, and nothing would flag it.
    #[test]
    fn the_projection_carries_the_pictures_own_format_whatever_the_codec() {
        for format in [
            pf_vkdecode::NV12,
            pf_vkdecode::P010,
            pf_vkdecode::YUV444_8,
            pf_vkdecode::YUV444_10,
        ] {
            let frame = decoded(format, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 1);
            assert_eq!(
                project(&frame).vk_format,
                crate::video::RawVkFormat(format.as_raw()),
                "the presenter reads the format off the frame, never off the codec"
            );
        }
    }

    /// EVERY field of the projection, against a frame whose values are all distinct
    /// (see [`decoded`]): the display crop is what the presenter shows, the coded
    /// extent is what it must divide by (the 1088-row lesson), the crop ORIGIN is
    /// what its UV-scale path assumes is (0,0), the timeline pair is what it waits,
    /// the CICP quadruple is what it does colour maths with, and the decode layout is
    /// what it has to restore after sampling. A swap or a drop among any of them is a
    /// silently wrong picture, so the list here is deliberately exhaustive — if
    /// `NativeVkFrame` grows a field, this test should stop compiling before it can
    /// go unchecked.
    #[test]
    fn the_projection_carries_every_field_the_presenter_can_no_longer_look_up() {
        let frame = decoded(pf_vkdecode::P010, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 4);
        let p = project(&frame);
        // Destructured, not field-accessed: a NEW field on NativeVkFrame breaks this
        // pattern and lands the author right here.
        let NativeVkFrame {
            image,
            vk_format,
            plane_views,
            layer,
            layout,
            semaphore,
            semaphore_value,
            generation,
            width,
            height,
            coded_width,
            coded_height,
            crop_x,
            crop_y,
            color,
            keyframe,
            poc,
            recovery,
            decode_order,
            guard: _,
        } = p;
        assert_eq!(image, 0x1001);
        assert_eq!(
            vk_format,
            crate::video::RawVkFormat(pf_vkdecode::P010.as_raw())
        );
        assert_eq!(
            plane_views,
            [0x2002, 0x2003],
            "the plane views, in order — NOT the whole-image view (0x2001)"
        );
        assert_eq!(layer, 3, "the picture's array layer, not slot 0");
        assert_eq!(layout, NativeVkLayout::DecodeDst);
        assert_eq!(semaphore, 0x3001);
        assert_eq!(
            semaphore_value, 7,
            "the frame's timeline value — not its POC (5)"
        );
        assert_eq!(generation, 4);
        assert_eq!((width, height), (1904, 1072), "the display crop's SIZE");
        assert_eq!(
            (coded_width, coded_height),
            (1920, 1088),
            "the allocated surface — the UV-scale denominator"
        );
        assert_eq!((crop_x, crop_y), (8, 4), "the crop ORIGIN, x then y");
        assert_eq!(color.primaries, 9);
        assert_eq!(color.transfer, 16);
        assert_eq!(color.matrix, 10);
        assert!(color.full_range);
        assert!(
            keyframe,
            "is_idr rides through as the pump's re-anchor signal"
        );
        assert_eq!(poc, 5);
        assert_eq!(
            recovery,
            punktfunk_core::reanchor::LocalRecovery {
                sei_here: true,
                is_recovery_point: true,
            },
            "the recovery point SEI's verdict reaches the gate — it is the ONLY \
             clean point an intra-refresh session has"
        );
        assert_eq!(
            decode_order, 17,
            "the decode ordinal rides along — without it the pump cannot tell a \
             frame decoded before a loss from one decoded after it"
        );

        // Coincide mode: the picture IS a DPB slot, so the presenter must put the
        // layer back in DPB layout after sampling.
        let dpb = project(&decoded(
            pf_vkdecode::NV12,
            vk::ImageLayout::VIDEO_DECODE_DPB_KHR,
            4,
        ));
        assert_eq!(dpb.layout, NativeVkLayout::DecodeDpb);
    }

    /// The construction-time shape refusal, device-independent half. A negotiated
    /// shape pf-vkdecode has no picture format for must be refused where
    /// `Decoder::new` still has FFmpeg-Vulkan to fall through to — NOT discovered at
    /// the first AU, where the only exit is an error streak that demotes PAST that
    /// rung to VAAPI/D3D11VA (and on NVIDIA/Linux, straight to software).
    #[test]
    fn a_stream_shape_with_no_native_picture_format_is_refused_at_construction() {
        use crate::video::StreamFormat;
        let f = |chroma, bit_depth| {
            h265_picture_format(StreamFormat {
                chroma_format_idc: chroma,
                bit_depth,
            })
        };
        // What the envelope DOES admit resolves, and to the right format — Main,
        // Main 10 and both RExt 4:4:4 depths.
        assert_eq!(f(1, 8).unwrap(), pf_vkdecode::NV12);
        assert_eq!(f(1, 10).unwrap(), pf_vkdecode::P010);
        assert_eq!(f(3, 8).unwrap(), pf_vkdecode::YUV444_8);
        assert_eq!(f(3, 10).unwrap(), pf_vkdecode::YUV444_10);
        assert_eq!(
            h265_picture_format(StreamFormat::SDR_420_8).unwrap(),
            pf_vkdecode::NV12,
            "the default/older-host shape is the ordinary one"
        );
        // 4:2:2 and monochrome are legal H.265 with no output plumbing here.
        assert!(f(2, 8).is_err(), "4:2:2");
        assert!(f(0, 8).is_err(), "monochrome");
        // 12-bit has no output format either, and a depth BELOW 8 must not wrap
        // around into a plausible `bit_depth_luma_minus8`.
        assert!(f(1, 12).is_err(), "12-bit");
        assert!(
            f(1, 0).is_err(),
            "an absurd depth refuses, never underflows"
        );
        assert!(f(3, 6).is_err());
    }

    #[test]
    fn release_tokens_mark_their_frame_and_tolerate_strays() {
        let mut outstanding = vec![shipped(0, 1), shipped(1, 1)];
        assert!(note_token(
            &mut outstanding,
            NativeReleaseToken {
                seq: 1,
                generation: 1,
                presented: true,
            }
        ));
        assert!(!outstanding[0].released);
        assert!(outstanding[1].released);
        assert!(
            outstanding[1].presented,
            "the token's presented flag rides into the ledger (the decoder waits \
             the presenter's value+1 write-back only when it was really enqueued)"
        );
        // A stray token (frame already settled away — e.g. a post-demotion drain)
        // matches nothing and must not panic or mis-mark.
        assert!(!note_token(
            &mut outstanding,
            NativeReleaseToken {
                seq: 7,
                generation: 1,
                presented: false,
            }
        ));
        assert!(!outstanding[0].released);
    }

    #[test]
    fn the_guard_sends_its_token_exactly_once_on_drop() {
        let (tx, rx) = mpsc::channel();
        let token = NativeReleaseToken {
            seq: 42,
            generation: 3,
            presented: false,
        };
        let guard = NativeReleaseGuard::new(tx, token);
        assert!(
            rx.try_recv().is_err(),
            "nothing is sent while the frame lives"
        );
        drop(guard);
        assert_eq!(rx.try_recv().ok(), Some(token), "drop sends the token");
        assert!(rx.try_recv().is_err(), "exactly once");
    }

    #[test]
    fn a_dropped_unpresented_frame_still_releases_through_the_same_guard() {
        // The newest-wins channel/store displacement path: the frame never reaches a
        // present, but dropping it must still return its slot.
        let (tx, rx) = mpsc::channel();
        let frame = NativeVkFrame {
            image: 0,
            vk_format: crate::video::RawVkFormat(pf_vkdecode::NV12.as_raw()),
            plane_views: [0; 2],
            layer: 0,
            layout: NativeVkLayout::DecodeDst,
            semaphore: 0,
            semaphore_value: 0,
            generation: 5,
            width: 1920,
            height: 1080,
            coded_width: 1920,
            coded_height: 1088,
            crop_x: 0,
            crop_y: 0,
            color: ColorDesc {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: false,
            },
            keyframe: true,
            poc: 0,
            recovery: punktfunk_core::reanchor::LocalRecovery::NONE,
            decode_order: 1,
            guard: NativeReleaseGuard::new(
                tx,
                NativeReleaseToken {
                    seq: 9,
                    generation: 5,
                    presented: false,
                },
            ),
        };
        drop(frame);
        assert_eq!(
            rx.try_recv().ok(),
            Some(NativeReleaseToken {
                seq: 9,
                generation: 5,
                presented: false,
            }),
            "an unpresented drop reports presented=false — the decoder must not \
             wait a value+1 write-back that was never enqueued"
        );
    }

    #[test]
    fn a_dead_channel_is_ignored_not_fatal() {
        // Demotion mid-stream: the backend (and its Receiver) are gone while the
        // presenter still holds a frame — its drop must be a no-op, not a panic.
        let (tx, rx) = mpsc::channel();
        drop(rx);
        let guard = NativeReleaseGuard::new(
            tx,
            NativeReleaseToken {
                seq: 1,
                generation: 1,
                presented: false,
            },
        );
        drop(guard); // must not panic
    }

    /// Concealment is the INTEGRITY warnings, not "any warning at all".
    ///
    /// h265's `NonZeroReorder` is emitted on the AU that ACTIVATES an SPS with
    /// `sps_max_num_reorder_pics > 0` — the opening IDR, and the fresh IDR at every
    /// ABR resolution change. pf-bitstream documents it as spec-legal and fully
    /// planned (C.5.2 bumping honours the reordering) and excludes it from its own
    /// integrity set. Treating it as concealment releases that IDR UNSHOWN, errors,
    /// and begs the host for a keyframe: a visible hitch at every renegotiation, on
    /// a stream the planner says it planned correctly.
    #[test]
    fn a_spec_legal_envelope_warning_is_not_concealment() {
        use pf_vkdecode::H265PlanWarning as H265;
        use pf_vkdecode::PlanWarning as H264;

        // The case from the field: an SPS activation, nothing else.
        let reorder = PlanWarnings::H265(vec![H265::NonZeroReorder {
            max_num_reorder_pics: 1,
        }]);
        assert!(!reorder.is_empty(), "it IS a warning and IS logged");
        assert!(
            reorder.integrity().is_empty(),
            "…but it is not concealment: the frame must be shown, not dropped"
        );

        // h264's twin: an MMCO 5 was planned in full too (the plan carries the
        // pre-rebase 8.2.1 values).
        let mmco5 = PlanWarnings::H264(vec![H264::Mmco5Rebase]);
        assert!(!mmco5.is_empty());
        assert!(mmco5.integrity().is_empty());

        // Everything that means a reference or a slice was LOST still is — this is
        // the H.264 behaviour the hardware-verified path shipped with.
        for w in [
            H264::FrameNumGap {
                expected: 4,
                got: 7,
            },
            H264::MissingReference {
                context: "list0",
                detail: "poc 12".into(),
            },
            H264::TruncatedAu { offset: 900 },
        ] {
            let warnings = PlanWarnings::H264(vec![w]);
            assert_eq!(warnings.integrity().len(), 1, "damage is concealment");
        }
        for w in [
            H265::MissingReference {
                context: "StCurrBefore",
                detail: "poc 12".into(),
            },
            H265::TruncatedAu { offset: 900 },
        ] {
            let warnings = PlanWarnings::H265(vec![w]);
            assert_eq!(warnings.integrity().len(), 1);
        }

        // Mixed AU: the damage decides, and the count the error reports is the
        // damage count — the spec-legal companion rides along in the log only.
        let mixed = PlanWarnings::H265(vec![
            H265::NonZeroReorder {
                max_num_reorder_pics: 2,
            },
            H265::TruncatedAu { offset: 12 },
        ]);
        assert_eq!(mixed.len(), 2);
        assert_eq!(mixed.integrity().len(), 1);
    }

    /// The counter a support engineer reads first. A total alone cannot tell a
    /// lossy link that keeps recovering apart from a stream that went down and
    /// stayed down — `damaged 40 · run 0` and `damaged 40 · run 40` are the same
    /// number and completely different problems. So the run must climb only while
    /// damage is CONSECUTIVE, and the worst run must survive the recovery that
    /// clears it (a once-per-second sample of `run` misses the bad moment almost
    /// every time).
    #[test]
    fn the_concealed_run_separates_a_lossy_link_from_a_stream_that_never_came_back() {
        let mut h = DecodeHealth::default();
        // A lossy link: single damaged AUs with clean stretches between.
        for _ in 0..3 {
            h.note(true, false, 0);
            h.note(false, false, 0);
            h.note(false, false, 0);
        }
        assert_eq!(h.damaged, 3);
        assert_eq!(h.run, 0, "the last AU was clean");
        assert_eq!(h.worst_run, 1, "…and no two damaged AUs were adjacent");

        // A stream that stopped recovering.
        let mut h = DecodeHealth::default();
        for _ in 0..7 {
            h.note(true, false, 0);
        }
        assert_eq!((h.damaged, h.run, h.worst_run), (7, 7, 7));
        // One clean AU ends the run but never the record.
        h.note(false, false, 0);
        assert_eq!((h.damaged, h.run, h.worst_run), (7, 0, 7));
    }

    /// A REFUSED AU — the decoder answering `Err` rather than concealing — has to
    /// reach the ledger, and has to be told apart from concealment.
    ///
    /// This is the shape the M4 review found reporting a clean bill of health: a
    /// host renegotiating outside the decode envelope makes every `plan_au` fail,
    /// the picture freezes, and before this counter existed the stats surface read
    /// `damaged 0 · failed 0 · run 0` and printed no integrity line at all. The
    /// two counts must stay separate because they say opposite things about the
    /// RUNG: concealment means the decoder coped with a damaged stream, refusal
    /// means it could not run.
    #[test]
    fn a_rung_refusing_every_au_cannot_report_a_clean_bill_of_health() {
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        for _ in 0..5 {
            h.note(false, true, 0);
        }
        assert_eq!(h.refused, 5, "every refusal is counted");
        assert_eq!(h.damaged, 0, "and none of them is concealment");
        assert_eq!(h.failed, 0, "nor a driver verdict — the driver never ran");
        assert_eq!(
            (h.run, h.worst_run),
            (5, 5),
            "a refused AU is as absent from the screen as a concealed one"
        );
        // A single good AU ends the run; the totals stand.
        h.note(false, false, 0);
        assert_eq!((h.refused, h.run, h.worst_run), (5, 0, 5));
    }

    /// The three verdicts count apart and share one run — because "the bitstream
    /// arrived incomplete", "the decoder refused it" and "the hardware failed the
    /// decode" have three different causes and three different fixes, while "did
    /// the picture ever come back" has one answer.
    #[test]
    fn concealment_refusal_and_driver_failure_are_three_separate_counts() {
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        h.note(true, false, 0);
        h.note(false, true, 0);
        h.note(false, false, 2);
        assert_eq!((h.damaged, h.refused, h.failed), (1, 1, 2));
        assert_eq!((h.run, h.worst_run), (3, 3), "one unbroken run of three");
    }

    /// A driver `Failed` verdict counts apart from concealment and extends the same
    /// run. Apart, because "the bitstream arrived incomplete" and "the hardware
    /// could not decode what arrived" have different causes and different fixes,
    /// and collapsing them is how "the stream is fine, it's your GPU" arguments
    /// start. Same run, because a frame the driver failed is as absent from the
    /// screen as a concealed one — and "did the picture ever come back" is what the
    /// run answers.
    #[test]
    fn driver_failures_count_separately_but_share_the_run() {
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        h.note(false, false, 2); // two prior frames reported corrupt at once
        assert_eq!((h.damaged, h.failed, h.run), (0, 2, 1));
        h.note(true, false, 1); // and an AU that ALSO needed concealment
        assert_eq!((h.damaged, h.failed, h.run), (1, 3, 2));
        h.note(false, false, 0);
        assert_eq!((h.run, h.worst_run), (0, 2));
    }

    /// `status_queries` is set once from the device and never touched by the
    /// per-AU fold — a counter update must not be able to turn "this driver cannot
    /// report corruption" into "it reported none".
    ///
    /// And, the invariant the doc contracts on both sides of this boundary state:
    /// where the device answers no status queries, `failed` can only ever read 0.
    /// It is not a hypothetical. `read_status` returns `Failed` on such a device
    /// for a lost device, a retired session generation or an unreadable semaphore,
    /// and counting those would render `integrity: driver-failed 1 · no driver
    /// status` — one line contradicting itself, pointing a support engineer at a
    /// verdict the hardware cannot give. So this feeds `note` a real failure and
    /// pins the zero; a test that only ever passed `0` would assert nothing.
    #[test]
    fn the_status_query_capability_survives_every_fold() {
        let mut h = DecodeHealth {
            status_queries: false,
            ..DecodeHealth::default()
        };
        h.note(true, false, 0);
        h.note(false, false, 0);
        assert!(!h.status_queries);
        h.note(false, false, 1);
        assert_eq!(
            h.failed, 0,
            "a device that answers no status queries can produce no driver \
             verdict — `failed` must stay 0 whatever `read_status` returned"
        );
        assert_eq!(
            h.run, 1,
            "…but the frame was still dropped, so the run still counts it: the \
             ATTRIBUTION is what must not be invented, not the damage"
        );
        assert_eq!(h.worst_run, 1);

        // The same fold on a device that CAN answer does count it.
        let mut h = DecodeHealth {
            status_queries: true,
            ..DecodeHealth::default()
        };
        h.note(false, false, 1);
        assert_eq!((h.failed, h.run), (1, 1));
    }

    #[test]
    fn the_queue_lock_is_shared_only_when_the_families_collide() {
        // Same family ⇒ same VkQueue (both sides use index 0) ⇒ shared lock.
        assert!(submit_queues_collide(0, 0));
        assert!(submit_queues_collide(2, 2));
        // A separate decode family has exactly one submitter — no lock.
        assert!(!submit_queues_collide(0, 3));
    }
}
