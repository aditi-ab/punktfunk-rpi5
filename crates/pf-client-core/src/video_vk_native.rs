//! Native Vulkan Video decode backend (WP-C of the native-decode program, widened to
//! HEVC by M3 WP-2 and to AV1 by M7): pf-vkdecode's
//! [`VkH264Decoder`]/[`VkH265Decoder`]/[`VkAv1Decoder`] running on the PRESENTER's own
//! VkDevice — the same zero-copy shape the FFmpeg-Vulkan backend had, with no FFmpeg in
//! the path. Auto's TOP rung on both desktop OSes, for ALL THREE codecs — each
//! leg has bit-exact parity against libavcodec (H.264/H.265 on three drivers plus a
//! 92-minute soak, M2/M3; AV1 250/250 on an RTX 5070 Ti, M7 — `video`'s evidence table
//! holds the record) — also pinnable via `PUNKTFUNK_DECODER=native-vulkan`;
//! `video::native_vulkan_gate` is the admission either way, and a failure falls through
//! to the rung below (the platform's own native rung).
//!
//! **Codec dispatch:** the negotiated codec picks the decoder ONCE, at construction
//! ([`Codec`]) — H.264, H.265 or AV1, the three codecs pf-vkdecode speaks. The
//! negotiated picture SHAPE (chroma format + bit depth) is checked there too, against
//! the device: an H.265 or AV1 session this GPU has no decode format for is refused at
//! construction, where the ladder simply walks on to the next rung, rather than at the
//! first AU, where the only exit is an error streak PAST that rung
//! ([`NativeVulkanDecoder::new`]). Nothing below the codec enum is per-codec: the
//! shipped-frame ledger, the release tokens, the
//! decode-status reads, the timeline waits and the teardown drain are shared, because
//! all three decoders deliver the identical [`DecodedVkFrame`] contract (same pool/slot
//! lifecycle, same `value + 1` write-back, same query slots, same generations).
//! Forking that machinery per codec would fork the one part of this backend hardware
//! has already proven.
//!
//! **One AU, several FRAMES (AV1 only).** An AV1 access unit is a TEMPORAL UNIT and may
//! carry more than one frame — the vendored conformance vector puts 274 frames in 250
//! units. [`VkAv1Decoder::decode`] walks them all internally and hands back the first
//! picture the planner declared DISPLAYABLE; the rest come out of `take_ready`, which
//! this backend already drains for H.265's burst output. Two AV1 facts make that walk
//! invisible from here, and both are the decoder's doing rather than this module's:
//! a HIDDEN frame (`show_frame = 0`) is decoded but never declared an output, so it
//! never enters `take_ready` and can never be shipped; and a `show_existing_frame`
//! (`dpb.stored == None`) decodes nothing at all and merely declares an
//! already-decoded picture displayable. So the contract this backend keeps is
//! unchanged — ONE access unit in, at most one displayable frame out.
//!
//! That contract is the SPEC's, not an assumption about punktfunk hosts: AV1 admits
//! exactly one shown frame per temporal unit, and the 24 two-frame units of the
//! vendored vector are a hidden ALTREF plus the frame that shows — one output
//! between them (`pf_bitstream::av1`'s conformance golden pins `shown = 250` across
//! 250 units, with `show_existing = 0`). [`NativeVulkanDecoder::decode`]'s
//! deliverable bound is therefore defence in depth against a stream that is NOT
//! that — a non-conformant encoder, or a scalable stream whose temporal unit carries
//! several operating points — and not the routine case it would be if a
//! `show_existing_frame` could ride alongside a shown frame. It cannot.
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
//! AV1's post-failure wait is NOT that shape, and the difference is deliberate.
//! After a failed frame the decoder empties its own slot ledger and skips every
//! frame until the next key frame (`VkAv1Decoder::awaiting_key`), because the
//! planner's eight-slot store still believes the flushed pictures are resident. But
//! a temporal unit in which every frame was skipped comes back as an ERROR
//! (`VkDecodeError::AwaitingKeyAv1`), once per access unit — exactly what H.264 and
//! H.265 answer for the same wait through their planners' `PlanError::AwaitingIdr`,
//! and for a reason this module owns: an `Ok(None)` with an empty warning ledger is
//! read here as a CLEAN access unit, and a clean AU clears `video.rs`'s demotion
//! streak. A rung whose every key frame fails would then never demote — one error
//! per key frame, zeroed by the skipped frames between them — and the `!delivered`
//! fall-through to the rung below, the documented backstop for a level above
//! `maxLevelIdc`, a sequence header disagreeing with the Welcome and (AV1 only)
//! film grain, would be unreachable. All three codecs demote identically here.
//!
//! **Queue lock:** pf-vkdecode submits on queue 0 of the decode family
//! ([`DECODE_QUEUE_INDEX`] — the presenter creates exactly one queue per family). When
//! the decode family IS the presenter's graphics family, that is the very `VkQueue` the
//! presenter/Skia/overlay submit and present on, so every decode submit must hold the
//! device's shared [`video::QueueLock`] (`vkQueueSubmit` external sync — the 2026-07-09
//! `VK_ERROR_DEVICE_LOST` class). When the families differ, the decode queue has exactly
//! one submitter (this backend, on the pump thread) and locking would serialize decode
//! against present for nothing — [`submit_queues_collide`] is the whole decision. (The
//! FFmpeg path locked on every family only because `lock_queue` was one callback pair for
//! the whole device; the collision the lock exists to prevent is the shared-queue one.)
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
//! `Failed` verdict is driver-reported decode corruption, the class libavcodec's
//! `vulkan_decode.c` (`nb_queries = 0`) architecturally cannot see — the Xbox Ally X
//! field case. It surfaces as an `Err` from the CURRENT `decode_frame` call so the
//! existing streak/reanchor machinery fires exactly as it did for libavcodec errors.
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
//!   diagnose — libavcodec concealed the same event silently and kept its job.
//! - **A driver `Failed` verdict** (and its query-less twin, a decode status that
//!   could not be established at all — [`StatusVerdicts`]). That is a statement about
//!   the DECODER, not the stream, so it stays an `Err`: the same volume libavcodec's
//!   reference-miss errors had, streak-eligible, and a driver making it repeatedly is
//!   precisely what demotion is for.
//! - **A REFUSED AU** — the decoder answering `Err` outright (a plan error, a
//!   Vulkan/session failure). Also an error, also streak-eligible, and counted
//!   separately from concealment in [`DecodeHealth::refused`]: "the stream is
//!   damaged and I coped" and "I could not run" are opposite statements about the
//!   rung, and only the second one means the session is looking at a frozen screen.
//!
//! **AV1 answers a LOST REFERENCE as a refusal, not as concealment**, and that is the
//! codec's doing rather than a policy difference here. AV1's reference array is indexed
//! by reference NAME, so a lost reference leaves a HOLE and there is no legal substitute
//! to write into it — `-1` for a name the frame really references is a spec violation
//! whose firmware behaviour is undefined, so [`VkAv1Decoder::decode`] refuses the AU
//! (`MissingReferenceAv1`) instead of concealing. The refusal counts in
//! [`DecodeHealth::refused`], the `Err` sets `want_keyframe` through `video::Decoder`'s
//! own error arm, and the decoder then skips to the next key frame — answering an
//! `Err` for every access unit of that wait, so the streak keeps ticking until the
//! re-anchor lands (the paragraph above). What must not be done is to launder either
//! answer into a concealment, or into a clean AU: the pictures really were not
//! decoded, and reporting "damaged, coped" — or "nothing to object to" — would put a
//! clean-looking bill of health on a rung that produced no picture.
//!
//! **The invariant all of the above serves: an answer may clear the demotion
//! streak only if it PROVES the rung works.** `video::Decoder::decode_frame` resets
//! the streak on a shipped frame or a CLEAN access unit, and on nothing else — so
//! every state in which this backend produces no picture has to reach it as either
//! a concealment (`Ok(None)` + a recovery request) or an `Err`, never as a clean
//! `Ok(None)`. Concealment is therefore left untouched by the reset (otherwise a
//! driver failing every other AU on a lossy link has its errors zeroed by the
//! concealment between them, and a rung that conceals FOREVER — a host framing
//! regression: every AU damaged, no frame ever shipped — has no escape hatch at
//! all), and the AV1 key-frame wait is an `Err` rather than the clean `Ok(None)` it
//! superficially resembles. The one genuinely clean `Ok(None)` is the decoder that
//! ran and had nothing to object to: it buffered, or it skipped an H.265 RASL
//! picture after an open-GOP join.
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
//! Additional, never a replacement: the wire path is untouched and the other rungs
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
    DecodeStatus, DecodedVkFrame, DeviceHandles, VkAv1Decoder, VkDecodeError, VkH264Decoder,
    VkH265Decoder,
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

/// The queue lock this device's decode lane submits under. One function so the
/// pre-session shape probe ([`hevc_shape_supported`]) and the real decoder cannot pick
/// different serialization for the same device.
fn queue_lock_for(vk: &VulkanDecodeDevice) -> Box<dyn pf_vkdecode::QueueLock> {
    if submit_queues_collide(vk.graphics_qf, vk.decode_qf) {
        Box::new(NativeQueueLock::Shared(vk.queue_lock.clone()))
    } else {
        Box::new(NativeQueueLock::Uncontended)
    }
}

/// The presenter's handles in pf-vkdecode's shape. Same reason as [`queue_lock_for`]:
/// the probe must ask about the DEVICE THE SESSION WOULD USE, not a re-derived one.
fn device_handles(vk: &VulkanDecodeDevice) -> DeviceHandles {
    DeviceHandles {
        get_instance_proc_addr: vk.get_instance_proc_addr,
        instance: vk.instance,
        physical_device: vk.physical_device,
        device: vk.device,
        decode_qf: vk.decode_qf,
        decode_queue_index: DECODE_QUEUE_INDEX,
        graphics_qf: vk.graphics_qf,
    }
}

/// Can this device hardware-decode HEVC at the given picture shape? Asked BEFORE the
/// Hello, so the client never advertises a shape it would have to refuse a session over.
///
/// This is the same question, through the same code, that
/// [`NativeVulkanDecoder::new`]'s H.265 arm asks at construction — `VkH265Decoder::new`
/// then `probe_stream_support` — deliberately, so an advertisement and the rung that has
/// to honour it cannot disagree. It creates and drops a decoder object; that costs a
/// handful of driver capability queries and no session, no images and no submits.
///
/// `false` when the presenter has no Vulkan Video decode at all, which for 4:4:4 is the
/// right answer rather than a missing one — see
/// [`crate::video::hevc_444_hardware_decodable`] for why no other rung can be asked.
pub(crate) fn hevc_shape_supported(
    vk: &VulkanDecodeDevice,
    chroma_format_idc: u8,
    bit_depth_luma_minus8: u8,
) -> bool {
    if !vk.video_decode {
        return false;
    }
    // The device-independent half first: a shape pf-vkdecode has no picture format for
    // needs no driver to refuse it (and `probe_stream_support` would only re-derive it).
    if pf_vkdecode::output_format_for(chroma_format_idc, bit_depth_luma_minus8).is_none() {
        return false;
    }
    // SAFETY: the `DeviceHandles` contract exactly as `NativeVulkanDecoder::new` states
    // it — these are the presenter's live instance/device, which outlive this call by
    // construction (the presenter owns them for the whole process, and this runs on its
    // thread while building the session's Hello). The decoder is dropped before return,
    // so nothing outlives the borrow.
    let dec = unsafe { pf_vkdecode::VkH265Decoder::new(&device_handles(vk), queue_lock_for(vk)) };
    match dec {
        Ok(d) => d
            .probe_stream_support(chroma_format_idc, bit_depth_luma_minus8)
            .is_ok(),
        Err(_) => false,
    }
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
/// this module knowing about the wire's codec bits (and `video::native_vulkan_gate`
/// stays the single admission decision).
///
/// Being IN this enum is not the same as being in `auto`: this list says pf-vkdecode has
/// a decoder, `video::native_vulkan_gate` says whether the automatic ladder may pick it
/// (and the device's own codec-operation caps bit is half of that answer). All three legs
/// are in `auto`, and that is the gate's decision to change, not this list's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeCodec {
    H264,
    H265,
    Av1,
}

/// The decoder this backend drives, chosen ONCE from the negotiated codec.
///
/// Dispatch stops here. Everything the backend does around a decoder — the
/// shipped-frame ledger, release tokens, status-query settling, timeline waits,
/// teardown — is codec-agnostic, because [`VkH264Decoder`], [`VkH265Decoder`] and
/// [`VkAv1Decoder`] expose the same surface over the same [`DecodedVkFrame`] contract
/// (same pool/slot lifecycle, same `value + 1` write-back, same query slots, same
/// generations). The forwarders below are therefore mechanically identical per arm on
/// purpose: the H.264 path is hardware-verified bit-exact, and dispatch must not be
/// able to change its behaviour.
// Unboxed on purpose, against `large_enum_variant`: the arms differ by ~1.7 KB (every
// decoder carries a planner, a slot ledger and pinned Std parameter sets), and exactly
// ONE of these exists per session — inside the `Box<NativeVulkanDecoder>` the backend
// already lives in. So the "waste" is 1.7 KB of slack in a single session-lifetime
// allocation, while boxing would put a second indirection between the pump and the
// decoder on the per-AU path and change how the hardware-verified H.264 decoder is
// reached. Neither trade is worth 1.7 KB.
#[allow(clippy::large_enum_variant)]
enum Codec {
    H264(VkH264Decoder),
    H265(VkH265Decoder),
    Av1(VkAv1Decoder),
}

impl Codec {
    /// Feed one access unit — see [`VkH264Decoder::decode`] /
    /// [`VkH265Decoder::decode`] / [`VkAv1Decoder::decode`]. `Ok(None)` means "no
    /// display-ready picture from this AU", which for H.265 also covers a RASL
    /// picture skipped after an open-GOP join (the module doc's contract: never an
    /// error).
    ///
    /// What `Ok(None)` deliberately does NOT cover on any arm is a decoder waiting
    /// to re-anchor after a failure: H.264/H.265 answer that with their planners'
    /// `PlanError::AwaitingIdr` and AV1 with `VkDecodeError::AwaitingKeyAv1`, one
    /// `Err` per access unit for as long as the wait lasts. A clean `Ok(None)`
    /// would clear the demotion streak once per frame and strand the session on a
    /// rung that produces nothing (module doc).
    ///
    /// AV1 is the one arm where "an access unit" is not "a frame": its AU is a
    /// TEMPORAL UNIT, the decoder walks every frame in it, and what comes back is
    /// the FIRST displayable picture of the walk — the rest, if any, through
    /// [`Self::take_ready`], exactly like H.265's burst output.
    fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedVkFrame>, VkDecodeError> {
        match self {
            Codec::H264(d) => d.decode(au),
            Codec::H265(d) => d.decode(au),
            Codec::Av1(d) => d.decode(au),
        }
    }

    /// Drain the plan warnings of the AU just decoded, TYPED — the three planners
    /// have genuinely different enums ([`pf_vkdecode::PlanWarning`] has
    /// `FrameNumGap`/`Mmco5Rebase`, [`pf_vkdecode::H265PlanWarning`] has
    /// `NonZeroReorder`, [`pf_vkdecode::Av1PlanWarning`] has `MissingShowExisting`,
    /// none a subset of another), so the set is carried as a three-armed value
    /// rather than flattened.
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
            // The AV1 decoder concatenates the WHOLE temporal unit's warnings, in
            // decode order — one unit, one concealment verdict, which is what this
            // backend already assumes for an AU.
            Codec::Av1(d) => PlanWarnings::Av1(d.take_warnings()),
        }
    }

    /// Pull the next already display-ready frame the last AU did not return
    /// directly (H.265 burst output; on AV1 only a non-conformant or
    /// multi-operating-point unit, since the spec admits one shown frame per
    /// temporal unit — [`MAX_DELIVERABLE`]). Drained after EVERY decode, so a
    /// burst can never be stranded inside the decoder.
    fn take_ready(&mut self) -> Option<DecodedVkFrame> {
        match self {
            Codec::H264(d) => d.take_ready(),
            Codec::H265(d) => d.take_ready(),
            Codec::Av1(d) => d.take_ready(),
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
            Codec::Av1(d) => d.release_frame(frame, presented),
        }
    }

    /// The decoder's current session generation (a frame from an older one has an
    /// unknowable status verdict — see [`NativeVulkanDecoder::settle_statuses`]).
    fn generation(&self) -> u64 {
        match self {
            Codec::H264(d) => d.generation(),
            Codec::H265(d) => d.generation(),
            Codec::Av1(d) => d.generation(),
        }
    }

    /// Non-blocking read of a frame's `RESULT_STATUS_ONLY` query.
    fn poll_status(&mut self, frame: &DecodedVkFrame) -> DecodeStatus {
        match self {
            Codec::H264(d) => d.poll_status(frame),
            Codec::H265(d) => d.poll_status(frame),
            Codec::Av1(d) => d.poll_status(frame),
        }
    }

    /// Bounded host wait for a frame's decode-complete timeline signal (the pump's
    /// sampled decode-latency stat).
    fn wait_decoded(&self, frame: &DecodedVkFrame, timeout_ns: u64) -> bool {
        match self {
            Codec::H264(d) => d.wait_decoded(frame, timeout_ns),
            Codec::H265(d) => d.wait_decoded(frame, timeout_ns),
            Codec::Av1(d) => d.wait_decoded(frame, timeout_ns),
        }
    }

    /// Does this device answer per-op decode-status queries at all? A device
    /// fact, not a codec one — forwarded per arm only because the decoders own
    /// the `DecodeDevice`.
    fn status_queries(&self) -> bool {
        match self {
            Codec::H264(d) => d.status_queries(),
            Codec::H265(d) => d.status_queries(),
            Codec::Av1(d) => d.status_queries(),
        }
    }

    /// The newest planned picture's DECODE-order ordinal — the watermark the
    /// pump stamps when it arms a freeze (see [`NativeVkFrame::decode_order`]).
    /// On AV1 a `show_existing_frame` does not advance it, because it decodes
    /// nothing — which is what the watermark is comparing against.
    fn decode_order(&self) -> u64 {
        match self {
            Codec::H264(d) => d.decode_order(),
            Codec::H265(d) => d.decode_order(),
            Codec::Av1(d) => d.decode_order(),
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
/// The split that matters is INTEGRITY vs. spec-legal, not which codec. The
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
///
/// AV1 (M7) has an EMPTY right-hand column: its planner reports nothing that is
/// spec-legal-but-notable, because AV1 has no reorder envelope to announce (no
/// bumping process, no `max_num_reorder_pics`) and no MMCO to rebase. Every AV1
/// warning is damage, and `pf_vkdecode::is_integrity_warning_av1` says so
/// exhaustively so a warning added later cannot default to "clean". The branch
/// below is therefore not dead code on that arm — it is the place a future
/// spec-legal AV1 warning would land without costing a frame.
enum PlanWarnings {
    H264(Vec<pf_vkdecode::PlanWarning>),
    H265(Vec<pf_vkdecode::H265PlanWarning>),
    Av1(Vec<pf_vkdecode::Av1PlanWarning>),
}

impl PlanWarnings {
    fn is_empty(&self) -> bool {
        match self {
            PlanWarnings::H264(w) => w.is_empty(),
            PlanWarnings::H265(w) => w.is_empty(),
            PlanWarnings::Av1(w) => w.is_empty(),
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
            PlanWarnings::Av1(w) => PlanWarnings::Av1(
                w.iter()
                    .filter(|x| pf_vkdecode::is_integrity_warning_av1(x))
                    .cloned()
                    .collect(),
            ),
        }
    }

    fn len(&self) -> usize {
        match self {
            PlanWarnings::H264(w) => w.len(),
            PlanWarnings::H265(w) => w.len(),
            PlanWarnings::Av1(w) => w.len(),
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
            PlanWarnings::Av1(w) => tracing::warn!(
                concealed,
                warnings = ?w,
                "native decode planned with concealment — dropping the frame, \
                 requesting re-anchor"
            ),
        }
    }

    /// The spec-legal log: the planner flagged an envelope fact and planned the AU
    /// in full, so the frame is SHOWN. Rare by construction (SPS activation, MMCO
    /// 5), which is why it is a `warn` and not a per-frame `debug`. Unreachable on
    /// the AV1 arm today — every AV1 warning is damage — and spelled out anyway so
    /// a future spec-legal AV1 warning gets the same treatment rather than the
    /// concealment branch's.
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
            PlanWarnings::Av1(w) => tracing::warn!(
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
        // Whether this picture's own references decoded cleanly — the corroboration
        // the shared gate weighs a host `USER_FLAG_RECOVERY_ANCHOR` against. The
        // planner already knows it (it is the one party that resolved this AU's
        // reference lists), and it is the only thing that can catch the host
        // asserting a re-anchor over a picture THIS decoder had to conceal.
        references_clean: frame.references_clean,
        // Which side of a loss this picture was DECODED on. Carried beside the
        // recovery mark because the mark is worthless without it: a post-failure
        // DPB flush delivers pre-loss pictures after the loss, and their marks
        // describe a wave that completed before it.
        decode_order: frame.decode_order,
        guard,
    }
}

/// The picture format a session of the negotiated shape decodes to, or a named
/// refusal for a shape pf-vkdecode has no output format for at all. `codec` names the
/// codec in the refusal text and nothing else — the map is the CRATE's one
/// (sampling, depth) → format table, shared by every codec it decodes.
///
/// The DEVICE-INDEPENDENT half of [`NativeVulkanDecoder::new`]'s shape check: 4:2:2,
/// monochrome and 12-bit are legal H.265/AV1 that no punktfunk host emits and this
/// client has no plumbing for, so no driver has to be asked about them. Pure, so the
/// refusal is CPU-testable — the device-dependent half (a shape with a format that
/// THIS driver does not advertise) is `probe_stream_support`, covered by
/// pf-vkdecode's `derive_caps_h265`/`derive_caps_av1` refusal tests.
///
/// One function for both codecs because the ENVELOPE is identical: pf-vkdecode's
/// AV1 profile builder admits exactly the four (sampling, depth) pairs
/// [`pf_vkdecode::output_format_for`] maps, and refusing here in different terms
/// than the probe refuses one line later would be two gates to keep in agreement.
fn picture_format(codec: &str, stream: crate::video::StreamFormat) -> Result<vk::Format> {
    let depth = stream.bit_depth_minus8().ok_or_else(|| {
        anyhow!(
            "negotiated {codec} bit depth {} is outside the 8/10-bit decode envelope",
            stream.bit_depth
        )
    })?;
    pf_vkdecode::output_format_for(stream.chroma_format_idc, depth).ok_or_else(|| {
        anyhow!(
            "no native picture format for the negotiated {codec} stream shape \
             (chroma_format_idc={}, {}-bit)",
            stream.chroma_format_idc,
            stream.bit_depth
        )
    })
}

/// The film-grain flag the AV1 construction-time probe asks the device about.
///
/// Grain synthesis is part of the AV1 decode PROFILE — a device that decodes AV1
/// need not offer the grain-enabled one — and the negotiation carries no grain bit,
/// so this is the one probe input that is an ASSUMPTION rather than a negotiated
/// fact. `false` is the right assumption and the safe one:
///
/// * a punktfunk host encodes desktop capture, where film-grain synthesis is off
///   (it exists to re-add grain a denoiser removed from camera footage);
/// * and the failure directions are not symmetric. Probing `false` on a device that
///   only offers the grain profile would REFUSE a session it could have run — but
///   there is no such device (grain support is an added capability, never a
///   replacement). Probing `true` on the far more common device that offers only
///   the grain-LESS profile would refuse every session this rung can actually
///   decode.
///
/// If a grain stream ever does arrive, `ensure_state` re-keys from the SEQUENCE
/// header (never softened to make a query pass) and refuses at the first AU — which
/// lands on the "never delivered a frame" arm in [`crate::video::Decoder`], the same
/// backstop that already covers a level above `maxLevelIdc` and a sequence header
/// disagreeing with the Welcome.
const AV1_PROBE_FILM_GRAIN: bool = false;

/// What the CLIENT PIPELINE itself holds, at its worst moment: how many delivered
/// frames are unreleased between this backend and the screen at once.
///
/// pf-vkdecode's [`pf_vkdecode::HOLD_HEADROOM`] docs enumerate them — two bounded(2)
/// channels, the FrameStore's 1..=3 preroll, the in-flight present, the retired-frame
/// slot — as 4-7 at steady state. Taken at the MAXIMUM, because a bound derived from
/// the average is a bound that fails exactly when it is needed.
const PIPELINE_HOLD: usize = 7;

/// How many display-ready frames the backend will hold back for LATER access units
/// before it starts dropping the oldest (see [`trim_deliverable`]).
///
/// **Derived, not chosen.** [`pf_vkdecode::HOLD_HEADROOM`] is the TOTAL number of
/// delivered-but-unreleased frames the pool is sized for (`picture_count =
/// required_slots + HOLD_HEADROOM`), and a queued frame counts against it exactly
/// like a shipped one: `build_frame` increments the picture's `held` the moment the
/// decoder declares it ready, and it stays held until [`Codec::release_frame`]. So
/// the queue's share of the headroom is whatever the pipeline does not already
/// occupy, and a queue bounded any deeper does not prevent the failure it names —
/// it merely caps the memory while the pool runs out anyway (8 queued + 7 in flight
/// against a headroom of 8 is `NoFreeSlot` on the next AU).
///
/// The other bound it has to stay inside is the STATUS-QUERY ring, which is
/// `picture_count` deep (17 on AV1: nine DPB slots plus the headroom) and is
/// re-armed once per SUBMISSION — up to two per temporal unit. A frame's query is
/// first read the AU after it ships, so a frame that waits `MAX_DELIVERABLE` access
/// units in this queue burns roughly `2 * (MAX_DELIVERABLE + 1)` of those 17 slots
/// before anyone looks at it. Overrun the ring and `read_status` reports the
/// re-armed slot as `Failed`, which [`NativeVulkanDecoder::settle_statuses`]
/// attributes to `driver_failed` — a FABRICATED driver-corruption verdict polluting
/// the one signal [`DecodeHealth::failed`] exists to carry (the Xbox Ally X class).
/// At the derived depth the wait is ~4 of 17 and the question does not arise.
///
/// The queue exists because a decoder can make several pictures display-ready from
/// one AU while the caller takes exactly one per AU: H.265 bumping outputs a burst
/// after a reordering stretch. AV1 cannot — the spec admits exactly one shown frame
/// per temporal unit, and pf-bitstream's conformance golden pins it (250 shown
/// frames across 250 units, no `show_existing_frame` at all) — so on that codec this
/// is defence in depth against a non-conformant or multi-operating-point stream, not
/// a routine case. Either way punktfunk hosts reorder nothing, so on the wire the
/// queue is empty every single AU and the bound never engages.
///
/// It is a bound and not a plain queue because "transient" is an assumption about the
/// HOST, and the failure it fails into is silent: a stream that reliably made two
/// frames displayable per AU would grow this by one per AU until the pool ran out —
/// after which every AU refuses with `NoFreeSlot`, three in a second demote the rung,
/// and nothing in the log would say the cause was a queue that could never drain.
///
/// What the derived depth costs, stated plainly: a stream that really does bump a
/// burst of more than two pictures at once loses the middle of the burst rather than
/// queueing it. That is the right way round. The frames are already several AUs late
/// by the time a burst exists, the stage after this one is newest-wins anyway, and
/// the alternative — a queue deep enough to hold the burst — spends the pool's whole
/// headroom on it and answers `NoFreeSlot` on the next access unit, which is a frozen
/// screen and a demotion rather than a hitch. Reachable only on a reordering stream,
/// which punktfunk hosts do not emit and which the planner already flags
/// (`H265PlanWarning::NonZeroReorder`).
const MAX_DELIVERABLE: usize = pf_vkdecode::HOLD_HEADROOM as usize - PIPELINE_HOLD;

// The derivation must leave the queue able to do its job: carry the one frame a
// two-output access unit strands. A `PIPELINE_HOLD` raised to the headroom (or past
// it) would silently turn every burst into a dropped frame — or underflow the const.
const _: () = assert!(
    MAX_DELIVERABLE >= 1,
    "the deliverable queue must be able to carry at least one frame between AUs"
);

/// One `warn` per this many dropped deliverable frames, after the first. The shape
/// that drops at all drops on EVERY access unit, and a warn per frame at frame rate
/// buries the log it exists to explain — while a single line at the start of a
/// session that then goes quiet reads as a one-off. So: the first drop in full, then
/// a heartbeat with the running total (~every 5 s at 60 fps).
const DROP_WARN_EVERY: u64 = 300;

/// Trim the deliverable queue to `cap` by dropping from the FRONT, returning the
/// dropped frames so the caller can release them unshown.
///
/// Oldest-first, because by the time a queue this deep exists the front frame is
/// several AUs stale and the consumer one stage on is itself newest-wins (the pump's
/// `force_send` overwrites an unconsumed frame). Dropping the NEWEST would keep the
/// stalest picture and present the stream in ever-lagging order; dropping the oldest
/// keeps display order for everything that survives and costs the frames that were
/// already too late to matter.
///
/// ⚠ Called AFTER this AU's own frame has been taken off the front, so `cap` bounds
/// the CARRY-OVER — what is held back for later access units — exactly as
/// [`MAX_DELIVERABLE`] says. Trimming before the take would make an AU that produced
/// two outputs drop the FIRST of them and ship the second, which is display order
/// inverted inside a single access unit.
///
/// Pure over the queue, so the bound is CPU-testable without a GPU.
fn trim_deliverable(
    queue: &mut std::collections::VecDeque<DecodedVkFrame>,
    cap: usize,
) -> Vec<DecodedVkFrame> {
    let mut dropped = Vec::new();
    while queue.len() > cap {
        match queue.pop_front() {
            Some(frame) => dropped.push(frame),
            // Unreachable: `len() > cap >= 0` means the queue is non-empty. Written
            // as a break rather than an `expect` so a bound of 0 on an empty queue
            // could never be a panic in the decode path.
            None => break,
        }
    }
    dropped
}

/// The native backend: the decoder plus the shipped-frame ledger and release channel.
pub(crate) struct NativeVulkanDecoder {
    dec: Codec,
    /// Cloned into every shipped frame's guard. `Option` so teardown can DROP the
    /// backend's own sender: only then does `release_rx` report Disconnected once
    /// the last guard is gone — the teardown short-circuit signal.
    release_tx: Option<mpsc::Sender<NativeReleaseToken>>,
    release_rx: mpsc::Receiver<NativeReleaseToken>,
    /// Display-ready frames not yet handed to the pump (an H.265 burst output, or a
    /// temporal unit that declared more pictures displayable than AV1 permits —
    /// decode delivers one per call; the rest wait here, oldest first, bounded by
    /// [`MAX_DELIVERABLE`]). Every frame in here holds a picture-pool image.
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
    /// formats) and a device that advertises H.265 or AV1 decode need not advertise a
    /// format for every shape of it: 4:4:4 is absent everywhere but NVIDIA. Discovered
    /// lazily, that is a mid-stream ERROR STREAK, and the streak machinery demotes a
    /// Vulkan rung to VAAPI/D3D11VA — which on NVIDIA/Linux (no usable VAAPI) means a
    /// 4K HEVC session lands on SOFTWARE. Refused here it is an ordinary construction
    /// failure, and `video::Decoder::new` simply walks on to the next rung.
    ///
    /// Three legs the probe cannot see, because they are stream facts no negotiation
    /// carries: a level above the device's `maxLevelIdc`, an SPS (or AV1 sequence
    /// header) that disagrees with the Welcome, and — AV1 only — a sequence that
    /// enables FILM GRAIN, which is part of the decode profile and which the probe
    /// therefore has to assume ([`AV1_PROBE_FILM_GRAIN`]). All three still surface at
    /// the first decode, where the demotion walk's next candidate is the rung DIRECTLY
    /// below this one — the property the pre-M10 "never delivered a frame" arm existed
    /// to guarantee, now structural (see [`crate::video::Decoder::decode_frame`]).
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
        let lock = queue_lock_for(vk);
        let handles = device_handles(vk);
        // The `DeviceHandles` caller contract, held for the decoder's whole lifetime
        // and identical for both arms (it is the HANDLES' contract, not the codec's):
        // the handles are the presenter's live instance/device, which outlives every
        // session pump (the run loop tears the pump — and with it this decoder — down
        // first: the exact liveness contract the PyroWave backend also relies on over
        // the same bundle). `video_decode` (checked above) is set only
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
                let wanted = picture_format("HEVC", stream)?;
                // SAFETY: the handle contract stated directly above.
                let d = unsafe { VkH265Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkH265Decoder init: {e}"))?;
                // …and the device-dependent half: does THIS driver advertise that
                // format for a decode session of this profile? Same query and same
                // derivation `ensure_state` would run at the first AU — only the
                // timing differs, and the timing is the whole point.
                let depth = stream
                    .bit_depth_minus8()
                    .expect("picture_format accepted the depth");
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
            NativeCodec::Av1 => {
                // Exactly the H.265 shape check, one codec over — AV1's decode
                // profile is a (seq_profile, sampling, depth, film grain) tuple and
                // a device that advertises the AV1 decode OPERATION need not offer
                // every profile of it. Refused here, the ladder walks to the next
                // rung; discovered at the first AU, the only exit is an error streak
                // PAST that rung.
                let wanted = picture_format("AV1", stream)?;
                // SAFETY: the handle contract stated directly above.
                let d = unsafe { VkAv1Decoder::new(&handles, lock) }
                    .map_err(|e| anyhow!("VkAv1Decoder init: {e}"))?;
                d.probe_stream_support(
                    stream.chroma_format_idc,
                    // AV1's profile key takes the ABSOLUTE bit depth (8/10), not
                    // H.265's `bit_depth_luma_minus8` — the two probes really do
                    // want different numbers, and `picture_format` above is the
                    // one that proved this depth is in the envelope at all.
                    stream.bit_depth,
                    AV1_PROBE_FILM_GRAIN,
                )
                .map_err(|e| {
                    anyhow!(
                        "device cannot decode the negotiated AV1 stream shape \
                         (chroma_format_idc={}, {}-bit, needs {wanted:?}): {e}",
                        stream.chroma_format_idc,
                        stream.bit_depth
                    )
                })?;
                Codec::Av1(d)
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
                 completion — the only signal libavcodec's rungs ever had)"
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
    /// One access unit, at most one DISPLAYABLE frame out. On AV1 the access unit is
    /// a temporal unit and the decoder may decode several frames from it — hidden
    /// frames included, which are never declared displayable and therefore never
    /// reach this ledger at all. Anything a single AU makes displayable beyond the
    /// first waits in [`Self::deliverable`] for the next call, bounded by
    /// [`MAX_DELIVERABLE`].
    ///
    /// `Ok(Some)` = a display-ready picture. `Ok(None)` = no picture this AU, which
    /// covers three unrelated things and the caller treats all three the same
    /// (its no-output/re-anchor machinery): the decoder
    /// buffered without output, an H.265 RASL picture was skipped after an open-GOP
    /// join, or the AU's plan needed CONCEALMENT and its output was released
    /// unshown. `Err` = the DECODER is in trouble — a Vulkan/session error, an AV1
    /// reference the plan could not resolve (which AV1 refuses rather than
    /// conceals — module doc), an AV1 temporal unit skipped in full while the
    /// decoder waits for the next key frame (the H.26x planners' `AwaitingIdr`
    /// under another name), or a driver `RESULT_STATUS` verdict of Failed on a
    /// prior frame — which the caller's streak/demotion machinery is entitled to
    /// act on.
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
                // NOTHING from a refused AU reaches the screen — the same rule the
                // concealment branch below keeps, and it has to be enforced here
                // too because a refusal can STRAND a frame inside the decoder.
                // AV1's `decode_inner` settles each plan of a multi-frame temporal
                // unit in turn, so frame 1 can already sit in `ready` when frame 2
                // fails; left there, the NEXT access unit pops it out of
                // `take_ready` and ships it with an empty warning ledger. That
                // would put a picture from a REFUSED unit on screen, clear the
                // demotion streak with it, and set `video.rs`'s `delivered` —
                // reporting a rung as working on exactly the stream shapes that
                // refuse every AU.
                //
                // On H.264/H.265 this also catches the pictures `recover_dpb`
                // FLUSHES out of the DPB on the AU after a failure (that AU then
                // answers `AwaitingIdr`, so it lands here). Releasing them costs
                // nothing observable: they were decoded BEFORE the loss, they
                // arrive while the pump's freeze is armed, and the freeze gate
                // withholds a non-keyframe there anyway — `session.rs` goes further
                // and discards their recovery marks by `decode_order` for exactly
                // this reason. The pool image comes back an access unit sooner. On
                // a punktfunk stream the set is empty regardless: zero reorder means
                // the DPB buffers no output to flush.
                while let Some(frame) = self.dec.take_ready() {
                    if let Err(e) = self.dec.release_frame(&frame, false) {
                        tracing::debug!(error = %e, "releasing a stranded frame failed");
                    }
                }
                // …and the unit's warnings go with it. They describe a plan whose
                // frames were all released unshown; carried over, the NEXT AU would
                // read them as its own fresh damage.
                let _ = self.dec.take_warnings();
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
            // invisible to libavcodec's query-less decoder — or a status that could
            // not be established at all): this call's output is released unshown
            // either way, because the picture is not fit to present.
            for frame in fresh {
                if let Err(e) = self.dec.release_frame(&frame, false) {
                    tracing::debug!(error = %e, "releasing an unshown frame failed");
                }
            }
            if verdicts.total() > 0 {
                // A verdict about the DECODER rather than the stream: an error,
                // streak-eligible, at the volume libavcodec's reference-miss errors
                // had (never quieter).
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
        // This AU's frame comes off the FRONT first: the bound is on the CARRY-OVER
        // (see [`trim_deliverable`]), so a unit that produced two outputs ships the
        // first and holds the second rather than dropping the first to ship the
        // second.
        let shipped = self.deliverable.pop_front().map(|frame| self.ship(frame));
        // The queue can only ever hand ONE frame per AU to the caller, so anything
        // it cannot drain is a frame holding a pool image forever — see
        // [`MAX_DELIVERABLE`]. Inert on every stream a punktfunk host emits.
        let queued = self.deliverable.len();
        for frame in trim_deliverable(&mut self.deliverable, MAX_DELIVERABLE) {
            self.health.note_dropped();
            // Rate-limited, because the shape this fires on is a stream producing a
            // surplus frame on EVERY access unit: unthrottled that is a warn per
            // frame at frame rate, which buries the log it is supposed to explain.
            // The first one carries the diagnosis; the rest are a running count.
            // `queued` is the PRE-trim depth — the number that says how far past the
            // bound the queue actually got. Read after the trim it would be the
            // constant `MAX_DELIVERABLE` every single time.
            if self.health.dropped == 1 || self.health.dropped % DROP_WARN_EVERY == 0 {
                tracing::warn!(
                    queued,
                    dropped_total = self.health.dropped,
                    poc = frame.poc,
                    "native decode: more display-ready frames than the pump can take — \
                     dropping the oldest so its pool image is not held forever"
                );
            }
            if let Err(e) = self.dec.release_frame(&frame, false) {
                tracing::debug!(error = %e, "releasing an over-queued frame failed");
            }
        }
        Ok(shipped)
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
            // SET, per this fixture's no-boolean-is-false rule — and it earns the
            // rule: a projection that dropped this would default it to `false`,
            // which reads as "this picture's references were concealed" and would
            // make the gate refuse EVERY host recovery anchor on a healthy stream.
            references_clean: true,
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
            references_clean,
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
        assert!(
            references_clean,
            "the reference-cleanliness verdict rides along — without it the gate \
             cannot refute a host recovery anchor that names a picture this decoder \
             had to conceal, which is the grey-with-motion field report"
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
    /// `Decoder::new` can still walk to the next rung — NOT discovered at the first AU,
    /// where the only exit is an error streak that demotes PAST that rung (and on
    /// NVIDIA/Linux, where VAAPI is unusable, straight to software).
    #[test]
    fn a_stream_shape_with_no_native_picture_format_is_refused_at_construction() {
        use crate::video::StreamFormat;
        let f = |chroma, bit_depth| {
            picture_format(
                "HEVC",
                StreamFormat {
                    chroma_format_idc: chroma,
                    bit_depth,
                },
            )
        };
        // What the envelope DOES admit resolves, and to the right format — Main,
        // Main 10 and both RExt 4:4:4 depths.
        assert_eq!(f(1, 8).unwrap(), pf_vkdecode::NV12);
        assert_eq!(f(1, 10).unwrap(), pf_vkdecode::P010);
        assert_eq!(f(3, 8).unwrap(), pf_vkdecode::YUV444_8);
        assert_eq!(f(3, 10).unwrap(), pf_vkdecode::YUV444_10);
        assert_eq!(
            picture_format("HEVC", StreamFormat::SDR_420_8).unwrap(),
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

    /// AV1's construction-time shape gate is the SAME envelope, and it has to stay
    /// that way: [`picture_format`] is what refuses first, and one line later
    /// `VkAv1Decoder::probe_stream_support` builds an `Av1ProfileKey`, which refuses
    /// exactly the same set (monochrome, 4:2:2, the planner's 4:4:0 sentinel, any
    /// depth but 8/10). Two gates that disagreed would mean either a shape refused
    /// here that the device could have decoded, or — worse — a shape admitted here
    /// and then refused mid-stream, where the exit is an error streak past this rung.
    ///
    /// The label is checked too, because it is the only thing a support engineer
    /// reading the refusal has to tell an AV1 session's refusal from an HEVC one.
    #[test]
    fn the_av1_shape_gate_admits_exactly_what_pf_vkdecodes_av1_profile_key_admits() {
        use crate::video::StreamFormat;
        let f = |chroma, bit_depth| {
            picture_format(
                "AV1",
                StreamFormat {
                    chroma_format_idc: chroma,
                    bit_depth,
                },
            )
        };
        // AV1 Main 8-bit / Main 10 / High 4:4:4 at both depths — every combination
        // `Av1ProfileKey::from_negotiated` maps to a profile.
        assert_eq!(f(1, 8).unwrap(), pf_vkdecode::NV12);
        assert_eq!(f(1, 10).unwrap(), pf_vkdecode::P010);
        assert_eq!(f(3, 8).unwrap(), pf_vkdecode::YUV444_8);
        assert_eq!(f(3, 10).unwrap(), pf_vkdecode::YUV444_10);
        // …and the ones it refuses.
        assert!(f(0, 8).is_err(), "monochrome");
        assert!(f(2, 8).is_err(), "4:2:2");
        assert!(
            f(4, 8).is_err(),
            "the planner's 4:4:0 sentinel is not 4:4:4"
        );
        assert!(f(1, 12).is_err(), "12-bit");
        assert!(
            f(1, 0).is_err(),
            "an absurd depth refuses, never underflows"
        );

        // The refusal names the codec — the label is the whole reason this function
        // takes one.
        let err = format!("{:#}", f(2, 8).unwrap_err());
        assert!(err.contains("AV1"), "{err}");
        let hevc = format!(
            "{:#}",
            picture_format(
                "HEVC",
                StreamFormat {
                    chroma_format_idc: 2,
                    bit_depth: 8
                }
            )
            .unwrap_err()
        );
        assert!(hevc.contains("HEVC"), "{hevc}");

        // The probe's OWN gate, asked the same questions through pf-vkdecode's
        // profile key: this is the agreement the comment above claims, asserted
        // rather than assumed.
        for (chroma, depth) in [(1u8, 8u8), (1, 10), (3, 8), (3, 10)] {
            assert!(
                pf_vkdecode::Av1ProfileKey::from_negotiated(chroma, depth, AV1_PROBE_FILM_GRAIN)
                    .is_ok(),
                "{chroma}/{depth} passes here, so it must pass the probe's key too"
            );
        }
        for (chroma, depth) in [(0u8, 8u8), (2, 8), (4, 8), (1, 12), (1, 0)] {
            assert!(
                pf_vkdecode::Av1ProfileKey::from_negotiated(chroma, depth, AV1_PROBE_FILM_GRAIN)
                    .is_err(),
                "{chroma}/{depth} refuses here, so the probe's key must refuse it too"
            );
        }
    }

    /// The deliverable queue can only hand ONE frame per AU to the pump, and every
    /// frame waiting in it pins a picture-pool image. A stream that made two
    /// pictures displayable per access unit would grow it by one per AU until the
    /// pool ran out, after which every AU refuses with `NoFreeSlot`, three in a
    /// second demote the rung, and nothing in the log would name a queue that could
    /// never drain as the cause.
    ///
    /// ⚠ Which stream that is, precisely — the earlier claim here was wrong and the
    /// crate's own golden disproves it. AV1 permits exactly ONE shown frame per
    /// temporal unit, so a `show_existing_frame` can never ride alongside a shown
    /// frame; pf-bitstream's conformance test pins `shown = 250` across 250 units
    /// with `show_existing = 0`, and its 24 two-frame units are a hidden ALTREF plus
    /// the frame that shows it. The real producers are H.265 bumping after a
    /// reordering stretch, and — as defence in depth — a non-conformant or
    /// multi-operating-point AV1 stream. The bound is worth having for those; it is
    /// not the routine case.
    ///
    /// So the bound drops from the FRONT: by the time the queue is this deep the
    /// oldest frame is several AUs stale, and the stage after this one (the pump's
    /// `force_send`) is itself newest-wins. Dropping the newest instead would keep
    /// the stalest picture and present the stream in ever-lagging order.
    ///
    /// ⚠ What this exercises is [`trim_deliverable`] ALONE — the pure half. The
    /// wiring it cannot see is the caller's: that the trim runs AFTER this AU's
    /// frame is taken off the front, that every dropped frame is handed to
    /// `release_frame(.., false)`, and that [`DecodeHealth::dropped`] counts it.
    /// Replace those with `mem::forget` and this test stays green; only a device
    /// (or the `NoFreeSlot` a leaked pool image eventually produces) would notice.
    #[test]
    fn the_deliverable_queue_drops_its_oldest_rather_than_pinning_pool_images_forever() {
        let mut q: std::collections::VecDeque<DecodedVkFrame> = (0..5)
            .map(|i| {
                let mut f = decoded(pf_vkdecode::NV12, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 1);
                // The only field this test reads — distinct per frame so "which
                // ones were dropped" is decidable rather than merely counted.
                f.poc = i;
                f
            })
            .collect();

        let dropped = trim_deliverable(&mut q, 3);
        assert_eq!(
            dropped.iter().map(|f| f.poc).collect::<Vec<_>>(),
            vec![0, 1],
            "the OLDEST two come back for release — not the newest"
        );
        assert_eq!(
            q.iter().map(|f| f.poc).collect::<Vec<_>>(),
            vec![2, 3, 4],
            "…and what survives stays in display order"
        );

        // At or below the bound nothing moves: on every stream a punktfunk host
        // emits this queue is empty, and the bound must be invisible there.
        assert!(trim_deliverable(&mut q, 3).is_empty());
        assert_eq!(q.len(), 3);

        // The bound is DERIVED, and this is the arithmetic it is derived from: a
        // queued frame holds a picture-pool image exactly like a shipped one, and
        // pf-vkdecode sizes that pool at `required_slots + HOLD_HEADROOM`. So the
        // queue at its bound PLUS what the pipeline itself holds must fit inside
        // the headroom — otherwise the pool runs out, every AU refuses with
        // `NoFreeSlot`, and the bound caps memory without preventing the failure it
        // names. Pinned against pf-vkdecode's own constant so a hardcoded depth here
        // (this shipped at 8, against a headroom of 8) fails the build rather than a
        // field session.
        assert!(
            MAX_DELIVERABLE + PIPELINE_HOLD <= pf_vkdecode::HOLD_HEADROOM as usize,
            "a queue of {MAX_DELIVERABLE} on top of the pipeline's {PIPELINE_HOLD} \
             exceeds the {} frames the picture pool is sized for",
            pf_vkdecode::HOLD_HEADROOM
        );

        // The PRODUCTION bound, at the carry-over depth it is derived to: one AU's
        // surplus frame is held (the burst this queue exists for), a second AU's is
        // not. Asserted against `MAX_DELIVERABLE` itself so a change to
        // `PIPELINE_HOLD` lands here rather than in a field log.
        let mut q: std::collections::VecDeque<DecodedVkFrame> = (0..MAX_DELIVERABLE)
            .map(|i| {
                let mut f = decoded(pf_vkdecode::NV12, vk::ImageLayout::VIDEO_DECODE_DST_KHR, 1);
                f.poc = i as i32;
                f
            })
            .collect();
        assert!(
            trim_deliverable(&mut q, MAX_DELIVERABLE).is_empty(),
            "a queue AT the bound is exactly what a two-output AU leaves behind"
        );
        assert_eq!(q.len(), MAX_DELIVERABLE);

        // A zero bound drains rather than looping or panicking. Reachable only
        // through a caller that asks for it — teardown does NOT come through here
        // (`Drop` empties the queue with `mem::take` and releases each frame), so
        // this pins termination and the empty-queue edge, not a production path.
        let drained = q.len();
        assert_eq!(trim_deliverable(&mut q, 0).len(), drained);
        assert!(q.is_empty());
        assert!(trim_deliverable(&mut q, 0).is_empty(), "and it terminates");
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
            references_clean: true,
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

    /// The AV1 arm of the same split (M7). AV1's planner has no spec-legal
    /// companion to `NonZeroReorder`/`Mmco5Rebase` — it announces no reorder
    /// envelope and has no MMCO to rebase — so every warning it emits is damage and
    /// every one of them must conceal.
    ///
    /// Worth asserting despite being "all true", because the failure it catches is
    /// silent: an arm wired to the wrong predicate (or to an empty vector) would
    /// SHOW a picture the stream lost data for and ask for no re-anchor, which is
    /// exactly the invisible-damage shape this program exists to end. The one that
    /// carries most of the weight is `MissingShowExisting` — a frame that decoded
    /// nothing and displayed nothing — because it is the one an author is most
    /// likely to read as harmless.
    #[test]
    fn every_av1_warning_conceals_because_av1_has_no_spec_legal_signal() {
        use pf_vkdecode::Av1PlanWarning as Av1;

        for w in [
            Av1::MissingReference {
                slot: 3,
                ref_index: 1,
            },
            Av1::MissingShowExisting { slot: 5 },
            Av1::TruncatedAu { offset: 900 },
        ] {
            let warnings = PlanWarnings::Av1(vec![w.clone()]);
            assert!(!warnings.is_empty());
            assert_eq!(
                warnings.integrity().len(),
                1,
                "{w:?} means the picture is not fit to present"
            );
        }

        // The whole vocabulary at once — the count the concealment log reports is
        // the damage count, and here it is the full list.
        let all = PlanWarnings::Av1(vec![
            Av1::MissingReference {
                slot: 0,
                ref_index: 0,
            },
            Av1::MissingShowExisting { slot: 1 },
            Av1::TruncatedAu { offset: 4 },
        ]);
        assert_eq!((all.len(), all.integrity().len()), (3, 3));

        // A clean AU is clean: the AV1 arm must not manufacture concealment out of
        // an empty ledger, which is what a stream with no damage produces on every
        // single access unit.
        assert!(PlanWarnings::Av1(Vec::new()).is_empty());
        assert!(PlanWarnings::Av1(Vec::new()).integrity().is_empty());
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
