import AVFoundation
import os

// MARK: - the ms ⇄ interleaved-sample conversion both the ring and the sync loop run on
//
// **Multiply first, divide last.** This is the whole of design/hi-res-audio.md §4.1, and it is the
// Swift half of the same fix core just took (`punktfunk_core::audio::ms_to_samples`). Both types
// below used to precompute `perMS = (rateHz / 1000) * channels` and express every figure they own
// as `ms * perMS`. That division happens FIRST, so 44 100 Hz became 44 samples per millisecond and
// every depth, target, shed threshold, hard cap, de-prime fuse and reported `bufferedMS`/`targetMS`
// came out **2.3 % low** — quietly, permanently, and in the one subsystem two previous programs
// spent their time making trustworthy. 48 000 and 96 000 were exact only because they happen to
// divide.
//
// Keeping `rateHz` and `channels` as the two numbers they are, and dividing last, is exact at every
// rate on the ladder (`pcm::rate_is_supported`: 44 100 / 48 000 / 88 200 / 96 000 / 176 400). It
// costs one integer division per conversion and buys three more rates.
//
// **Why `Int` is enough here, where core needed an explicit `u64`.** Core's conversions run against
// `usize`, which is 32 bits on some embedder targets, so they widen and saturate by hand. This
// package builds for macOS 14 / iOS 17 / tvOS 17 only (Package.swift) — arm64 and x86_64, where
// `Int` is 64-bit — and the largest product any caller can reach is the longest span this file
// names against the top of the ladder: `syncBackoffMaxMS` (480 000 ms) × 176 400 Hz × 8 ch =
// 6.8 × 10¹¹, forty bits, before the divide brings it back to 6.8 × 10⁸. That is five orders of
// magnitude inside `Int.max`. The samples → ms direction is the one that takes a caller-supplied
// count and is guarded, because Swift TRAPS on overflow rather than wrapping and that trap would
// land in a realtime render callback.

/// Interleaved samples per second at a negotiated layout — the denominator both conversions share.
/// `max(1)` on both: a degenerate layout must not divide by zero in a render callback.
func audioInterleavedPerSec(rateHz: Int, channels: Int) -> Int {
    max(rateHz, 1) * max(channels, 1)
}

/// `ms` milliseconds of audio, in interleaved samples. Mirrors `ms_to_samples`.
func audioMsToSamples(rateHz: Int, channels: Int, ms: Int) -> Int {
    ms * audioInterleavedPerSec(rateHz: rateHz, channels: channels) / 1_000
}

/// Interleaved samples back to whole milliseconds — the exact inverse of [`audioMsToSamples`], and
/// the reason `depthMS(target)` round-trips to `targetMS` at every rate on the ladder. §4.1 names
/// that round trip as the tell that this rework is incomplete, so it is also the shape of the test
/// that guards it (`testTheShippingRateLadderRoundTripsMsToSamplesExactly`).
///
/// `samples` arrives from a caller and nothing bounds it — `setSyncTarget(Int.max / 2)` is a real
/// call this file's own tests make — and `samples * 1_000` on that would TRAP, taking the process
/// down from wherever it was asked (the drain thread, or the render callback). Core widens to u128
/// for the same reason; Swift has no u128, so the multiply reports its overflow and saturates.
/// Saturating rather than wrapping, because a wrapped duration is a tiny one: a fuse that blows
/// instantly instead of one that never blows.
func audioSamplesToMs(rateHz: Int, channels: Int, samples: Int) -> Int {
    let (scaled, overflow) = samples.multipliedReportingOverflow(by: 1_000)
    guard !overflow else { return Int.max }
    return scaled / audioInterleavedPerSec(rateHz: rateHz, channels: channels)
}

/// Interleaved samples in one `frameUs` frame — **per channel × channels**. Mirrors
/// `punktfunk_core::audio::pcm::samples_per_frame`, which is the single source of truth for how
/// long a frame is: the host fills a buffer of this size and this ring drains one, so the two agree
/// by construction rather than by both re-deriving `rate × µs` and hoping they round the same way.
///
/// ⚠ **Not the same question as "how many samples is `frameUs` of audio", and at 44.1 kHz not the
/// same answer.** The divide is per channel and FLOORS, because 220.5 samples do not exist: 5 ms of
/// 44.1 kHz stereo audio is 441 interleaved samples, but a 5 ms FRAME of it carries 440. Both the
/// shed size and the near-miss margin mean *exactly one packet*, so computing the first where the
/// wire delivers the second would describe a packet that does not exist. Multiply first here too —
/// `rateHz / 1_000_000` is 0 for every rate below a megahertz.
func audioSamplesPerFrame(rateHz: Int, frameUs: Int, channels: Int) -> Int {
    (max(rateHz, 1) * max(frameUs, 0) / 1_000_000) * max(channels, 1)
}

/// SPSC-ish jitter ring (interleaved float, `channels` per frame), drain thread → render
/// callback. The unfair lock is held for microseconds; fine at render-callback rates. Priming:
/// reads return silence until enough is buffered (at least the target, and at least one
/// packet more than the device's render quantum — large-buffer devices would otherwise
/// chronically out-demand the prefill and oscillate prime → dropout → re-prime).
/// All counts stay whole frames (multiples of `channels`), so the interleave can never slip.
///
/// **Drift correction.** Both ends run at the same nominal rate but on different crystals, so
/// backlog from a network stall or plain host-vs-DAC skew never drains on its own: without
/// correction one 300 ms hiccup leaves audio 300 ms behind video for the rest of the session. This
/// used to be handled by a `highWater` shed that dropped a whole `2 × prefill` at once — its own
/// comment called that "one audible blip". It is now the same two-stage scheme the Rust clients
/// share (`punktfunk_core::audio::JitterPolicy`): a slow depth average that sits above target for a
/// sustained window sheds exactly ONE audio frame with a crossfade — the session's real frame, see
/// `setFrameUs` — and the hard cap is only a backstop.
///
/// **Adaptive depth.** The target is a floor, not a constant: a NEAR-MISS — a read served with
/// less than one frame left over — grows it a step BEFORE anything was audible, repeated genuine
/// underruns grow it too (`noteRead`, mirroring `JitterPolicy::note_read`) up to `maxTargetMS`,
/// and a long quiet spell relaxes it back toward the base — so a session on Wi-Fi that bunches
/// arrivals deepens until it stops crackling, while a clean LAN keeps the tight base latency.
/// Growth only raises a promise; the one thing that re-banks real depth is a re-prime, so an
/// underrun while the ring is HOLLOW (depth average far below the target) re-primes at once,
/// spending the click it already cost on the whole refill. Every shrink is armed as a PROBE:
/// answered by an underrun or near-miss within its window, it is undone on the spot, and a
/// failed sync-driven shrink is not retried for a growing backoff. Keep the constants here in
/// step with `JitterTuning.COREAUDIO`.
///
/// **A/V sync.** On top of all that the depth can be STEERED, by `setSyncTarget` from the drain
/// thread's `AvSync` — because a ring that is the right depth for the link is not thereby the
/// right depth for the picture. Continuity still outranks sync: the request is clamped between
/// the underrun-driven floor above and the hard cap, so the loop can never buy alignment with a
/// dropout. `nil` (the default) is exactly the pre-sync behaviour.
final class AudioRing: @unchecked Sendable {
    /// Mirrors `JitterTuning::COREAUDIO` — see that type for the rationale.
    private static let targetMS = 20
    private static let maxTargetMS = 70
    private static let headroomMS = 30
    private static let hardCapMS = 90
    /// How long the ring may run short before it goes back to priming, in MILLISECONDS of
    /// starvation — not a count of callbacks. As a count (it was 4) the hysteresis meant a
    /// different span of time on every device, because a callback is not a unit of time: 4 of them
    /// is ~44 ms on a Mac's ~11 ms quantum and **20 ms on iOS**, whose session asks for a short IO
    /// buffer. A Wi-Fi delivery stall therefore de-primed this ring on every bunching cycle where
    /// the same policy rode it out elsewhere — measured on the shared Rust policy at 120 audible
    /// gaps per 10 minutes at a 5 ms quantum, against 3 at 8 ms and 1 at 16 ms on an identical
    /// link. Mirrors `JitterTuning::COREAUDIO.deprime_ms`.
    private static let deprimeMS = 60
    /// How long a packet DROUGHT may be concealed (`DroughtConceal`) before this ring is allowed
    /// to underrun and the hysteresis above is allowed to run: twice that window — long enough to
    /// ride out the delivery stalls that de-prime rings today, short enough that a genuinely dead
    /// stream is not papered over. DERIVED from the fuse rather than written out, so it cannot
    /// drift away from the thing it exists to protect. Mirrors `JitterTuning::plc_max_ms`.
    static let plcMaxMS = deprimeMS * 2
    /// Floor in callbacks under `deprimeMS`, so a large-quantum device keeps real hysteresis
    /// instead of de-priming on the first short read. Mirrors `MIN_DEPRIME_CALLBACKS`.
    private static let minDeprimeCallbacks = 2
    /// The protocol's DEFAULT frame — the Opus plane's 5 ms, mirroring `PUNKTFUNK_AUDIO_FRAME_MS`.
    ///
    /// The frame a session actually runs is RESOLVED, not assumed: the lossless plane sizes it to
    /// the path MTU because a 5 ms hi-res frame does not fit one QUIC datagram — 4 ms at
    /// 48 kHz/24-bit stereo, 2 ms at 96 kHz/24-bit stereo under the default ceiling, and shorter
    /// again for surround, whose frame carries three or four times the samples for the same
    /// duration (design/hi-res-audio.md §4.2). `setFrameUs` takes that figure from
    /// `punktfunk_connection_audio_frame_us`; this constant is what a ring keeps until somebody
    /// calls it, which is exactly right for every Opus session and for the tests that pin the
    /// pre-hi-res behaviour.
    ///
    /// Still the right unit for `AvSync`'s EWMA weight, which core also leaves on the constant: it
    /// is a time constant on a loop with a 100-observation settling gate, so a shorter real frame
    /// only makes it settle sooner.
    ///
    /// ⚠ **`DroughtConceal` below is a different story, and it is now BEHIND core.** Core moved its
    /// drought policy onto the resolved frame (`DroughtConceal::new_at_frame_us`) after this file
    /// was last touched: it charges one `frame_us` per concealed frame and triggers at two of them,
    /// where this leg still charges a flat 5 ms. The frame COUNT stays right either way — the drain
    /// thread writes one real frame per `conceal()` — so nothing plays wrong; what is wrong is the
    /// BUDGET and the REPORT. On a 2 ms lossless frame `plcMaxMS` is spent after two fifths of the
    /// wall clock it promises and `plc_ms` over-reports by 2.5×, and a 1 ms surround frame makes
    /// that a factor of five. Fixing it means giving this type the frame the same way the ring gets
    /// it; it is deliberately NOT part of the rate/surround change, and it is the next thing this
    /// file owes core.
    static let frameMS = 5
    /// Depth average must exceed target by this before drift correction fires — the middle of the
    /// headroom band, so the smooth shed always gets its chance BEFORE the hard cap trims.
    private static let shedExcessMS = 15
    /// …and must stay there for this much consumed audio. Long, because a shed is the only thing
    /// here a listener could notice; it must never fire on a transient.
    private static let shedSustainMS = 2_000
    /// The mirror for the sync-driven INSERT: the depth average must sit below the requested
    /// target for this much consumed audio before one frame is duplicated. Equal to the shed's,
    /// so the two corrections are the same instrument in both directions and cannot fight; kept
    /// separate so the insert can be sped up alone if a listen test proves it inaudible. Mirrors
    /// `INSERT_SUSTAIN_MS`.
    private static let insertSustainMS = shedSustainMS
    /// How far below the sync-requested target the depth average must sit before the insert
    /// arms. NOT `shedExcessMS`: the sync loop only asks for more depth once the offset has left
    /// its ±`AvSync.deadbandMS`, so a margin at or above the deadband would leave every request it
    /// is allowed to make permanently unanswered. Half the deadband. Mirrors `INSERT_MARGIN_MS`.
    private static let insertMarginMS = AvSync.deadbandMS / 2
    private static let crossfadeMS = 2
    /// Time constant of the depth average.
    private static let ewmaTauMS = 1_000
    /// Adaptive target floor, mirroring `JitterPolicy::note_read`: this many genuine underruns
    /// inside one window grow the live target a step (up to `maxTargetMS`), and a long quiet
    /// spell relaxes it a step back toward the base — so only the sessions that actually starve
    /// (Wi-Fi power-save bunching is the classic) pay for extra depth, and only while they need
    /// it. All spans are measured in consumed samples, like the Rust policy.
    private static let growUnderruns = 3
    private static let growWindowMS = 5_000
    private static let growStepMS = 10
    private static let shrinkQuietMS = 30_000
    /// The same quiet span, while the A/V sync loop is actively asking to run shallower. A grown
    /// target normally relaxes only after a long spell because, absent other evidence, the only
    /// thing that can justify giving up hard-won slack is time; a sync request IS that evidence —
    /// a measurement saying the extra depth is costing alignment right now — so a smaller target
    /// gets tested sooner. Mirrors `SHRINK_QUIET_SYNC_MS`.
    private static let shrinkQuietSyncMS = 5_000
    /// How long a shrink remains a PROBE, in consumed audio: an underrun or near-miss inside
    /// this window means the shrink was wrong, and the previous target is restored at once.
    /// Mirrors `SHRINK_PROBE_MS`.
    private static let shrinkProbeMS = 5_000
    /// How long a failed probe keeps the sync loop from driving another shrink — without it the
    /// loop pays an audible starvation event every `shrinkQuietSyncMS` on any link whose jitter
    /// genuinely needs the depth, forever. Doubles per consecutive failure, capped; a probe that
    /// survives its window resets it. Mirror `SYNC_BACKOFF_MS` / `SYNC_BACKOFF_MAX_MS`.
    private static let syncBackoffMS = 60_000
    private static let syncBackoffMaxMS = 480_000
    /// A ring is HOLLOW when its depth AVERAGE sits this far below the target: growth only ever
    /// raises the promise, and the one thing that re-banks real depth is a re-prime — so an
    /// underrun in a hollow ring re-primes AT ONCE, spending the click it already cost on the
    /// whole refill instead of riding the knife edge one click per bunching period. Mirrors
    /// `DEPRIME_DEBT_MS`.
    private static let deprimeDebtMS = growStepMS

    private var buf: [Float]
    private var readIdx = 0
    private var writeIdx = 0
    private var primed = false
    private var renderQuantum = 0
    /// Consecutive short reads, and the audio they starved for in interleaved samples. BOTH gate
    /// the de-prime (see `deprimeMS`): the run must be at least that long AND at least
    /// `minDeprimeCallbacks` callbacks, so the fuse is the same span of time whatever the device's
    /// quantum without collapsing to a hair trigger on a large-quantum device.
    private var emptyReads = 0
    private var emptyRun = 0
    private var depthAvg: Double = 0
    private var overRun = 0
    /// The mirror: consumed samples for which the average has sat more than `insertMarginMS`
    /// below the sync-requested target (see the insert branch in `read`).
    private var underRun = 0
    /// The live target in interleaved samples — `targetMS` grown by underrun pressure
    /// (`noteRead`), never below the base. Set in `init` (needs the rate).
    private var targetLive = 0
    /// Underruns seen in the current growth window, and the window's consumed-sample count.
    private var underrunsInWindow = 0
    private var windowRun = 0
    /// Consumed samples since the last underrun (drives the relax-back-down step).
    private var quietRun = 0
    /// Reported, not acted on: short reads that actually starved the callback, and smooth drift
    /// corrections. A rising underrun count means the ring is being starved (network or CPU),
    /// which is a different problem from the depth being wrong.
    private var underrunCount = 0
    private var shedCount = 0
    /// Sync-driven inserts: one duplicated, crossfaded frame each. Concealment in BOTH directions
    /// must be visible — a ring being quietly deepened is a picture moving away from its audio.
    private var insertCount = 0
    /// The depth the A/V sync loop would like, in interleaved samples (`AvSync.desiredDepth`).
    /// `nil` — the default, and what an un-wired session keeps — reproduces the pre-sync
    /// behaviour exactly, so this ring could adopt sync without the other three diverging.
    private var syncTarget: Int?
    /// This read was served with less than one frame left over (set in `read`, consumed by
    /// `noteRead`).
    private var nearMiss = false
    /// A near-miss already grew the target this window — one step per window, so a bunching
    /// episode (a RUN of consecutive near-misses while the ring refills) buys one measured
    /// step, not a sprint to the ceiling.
    private var nearMissGrown = false
    /// The depth average runs a `deprimeDebtMS` debt against the ADAPTIVE target — the one
    /// underrun pressure grew, never the sync-inflated one (set in `read`): an underrun should
    /// re-prime at once instead of waiting out the hysteresis.
    private var hollow = false
    /// Interleaved samples left in the current shrink-probe window (0 = no probe outstanding).
    private var probeRun = 0
    /// The live target before the probed shrink, restored if the probe fails.
    private var probePrevTarget = 0
    /// Interleaved samples before the sync loop may drive another shrink (0 = allowed now).
    private var syncBackoffRun = 0
    /// Length of the NEXT backoff, in ms — doubles per consecutive failed probe, capped.
    private var syncBackoffLenMS = AudioRing.syncBackoffMS
    /// The sync loop's smoothed offset in ms, STORED not computed: the ring owns the depth but has
    /// no timestamps, so the drain thread (which has both a packet's `pts_ns` and the video leg)
    /// hands the number back for reporting. Mirrors `NativeClient::audio_av_offset_ms`.
    private var avOffsetMS = 0
    /// Drought concealment the drain thread has synthesized this session, ms — STORED here for the
    /// same reason `avOffsetMS` is: the ring cannot compute it, but it is where the numbers a
    /// listener's complaint needs can be read under one lock.
    private var plcMS = 0
    /// The negotiated sample rate and interleaved channel count, kept as the two numbers they are
    /// rather than pre-divided into samples-per-millisecond — see the conversion helpers at the top
    /// of this file for why that division WAS the defect. Both are clamped to ≥ 1 on the way in.
    private let channels: Int
    private let rateHz: Int
    /// One protocol audio frame in MICROSECONDS — see `setFrameUs`. Guarded by `lock`, like
    /// everything else the render callback reads.
    private var frameUs = AudioRing.frameMS * 1_000
    private let lock = OSAllocatedUnfairLock()

    /// A ring holding `seconds` of audio at the session's negotiated format. The de-jitter depth
    /// is the ring's own business (`targetMS`), not a caller's prefill.
    ///
    /// Sized in TIME rather than in a sample count, because the sample count is `rateHz × channels`
    /// and the two were indistinguishable while every session ran at 48 kHz: this was
    /// `capacity: 48_000 * channels`, where the literal was silently doing double duty as
    /// samples-per-second. At 96 kHz that same expression is half a second of ring — no error, no
    /// warning, just half the overflow headroom on the plane that needs it most.
    ///
    /// **Every rate on the lossless ladder is exact here** — 44 100 / 48 000 / 88 200 / 96 000 /
    /// 176 400 (`pcm::rate_is_supported`). It was not always: this used to precompute an INTEGER
    /// `perMS = (rateHz / 1000) * channels` and express every figure it owns — target, EWMA depth,
    /// shed threshold, hard cap, de-prime fuse, and the `bufferedMS`/`targetMS` the HUD reports —
    /// as `ms * perMS`. That leading division truncated 44 100 Hz to 44 samples/ms and put all of
    /// them 2.3 % low, which is the sole reason the 44.1 kHz family was deferred rather than
    /// refused (design/hi-res-audio.md §4.1). The conversions now multiply first and divide last,
    /// so there is no rate this ring cannot represent. Mirrors `JitterPolicy::new_at_rate`; keep
    /// the two in step.
    ///
    /// A `rateHz` or `channels` of zero is clamped to 1 rather than rejected: this is built from
    /// wire-supplied values on a path that must not fault in a render callback, and it used to
    /// divide by `perMS` with nothing but a `max(1)` at the use sites.
    init(seconds: Int, channels: Int, rateHz: Int) {
        self.channels = max(channels, 1)
        self.rateHz = max(rateHz, 1)
        buf = [Float](repeating: 0, count: max(seconds, 1) * self.rateHz * self.channels)
        targetLive = msSamples(Self.targetMS)
    }

    /// `ms` of audio in interleaved samples at this session's layout — see `audioMsToSamples`.
    private func msSamples(_ ms: Int) -> Int {
        audioMsToSamples(rateHz: rateHz, channels: channels, ms: ms)
    }

    /// The inverse, for the figures this ring reports — see `audioSamplesToMs`.
    private func samplesMs(_ samples: Int) -> Int {
        audioSamplesToMs(rateHz: rateHz, channels: channels, samples: samples)
    }

    /// Tell the ring how long one audio frame actually is, in microseconds
    /// (`punktfunk_connection_audio_frame_us`). Mirrors `JitterPolicy::set_frame_us`.
    ///
    /// Two of this ring's decisions are denominated in FRAMES rather than milliseconds — the floor
    /// under the effective target (a device quantum plus one frame) and the smooth shed (drop
    /// exactly one frame) — and both were written when `frameMS` was the only frame this protocol
    /// had. The lossless plane negotiates shorter ones: 4 ms at 48 kHz/24-bit, 2 ms at
    /// 96 kHz/24-bit under the default MTU. Left unset, a 96 kHz session would shed two and a half
    /// frames at a time and fade across an entire frame.
    ///
    /// A SETTER rather than an initialiser parameter, exactly as core has it: the default keeps
    /// every Opus session — and every test in `AudioRingDriftTests`, which pins the pre-hi-res
    /// numbers — bit-identical, and a caller that never learned the figure cannot accidentally pass
    /// a wrong one. Idempotent, so the engine-rebuild path that reuses a live ring can simply call
    /// it again.
    ///
    /// Clamped to ≥ 1 µs so a degenerate value can never make `frameSamples` zero and turn the
    /// shed into an infinite no-op.
    func setFrameUs(_ us: Int) {
        lock.lock()
        defer { lock.unlock() }
        frameUs = max(us, 1)
    }

    /// One frame in interleaved samples. Computed in µs so a sub-millisecond frame does not
    /// truncate: 2 500 µs at 48 kHz stereo is 240 samples, not the 192 that routing it through
    /// integer milliseconds first would give. Mirrors `JitterPolicy::frame_samples`; caller holds
    /// the lock.
    ///
    /// Delegated to `audioSamplesPerFrame` — the mirror of core's `pcm::samples_per_frame` — rather
    /// than re-derived from this ring's own ms↔sample conversion, because a second derivation is a
    /// second rounding. The two are only interchangeable when the rate divides the frame: at
    /// 44 100 Hz a 5 ms frame is 220 samples PER CHANNEL, and `msSamples(5)` would say 441 where
    /// the wire delivers 440. The shed and the near-miss margin both mean "exactly one packet", so
    /// a self-derived answer would put them one sample away from the packet they describe.
    private var frameSamples: Int {
        max(audioSamplesPerFrame(rateHz: rateHz, frameUs: frameUs, channels: channels), 1)
    }

    /// The seam crossfade, capped at HALF a frame. `crossfadeMS`'s flat 2 ms is a comfortable slice
    /// of a 5 ms Opus frame and the whole of a 2 ms lossless one — and a fade as long as the
    /// material it is fading is not a crossfade, it is a wholesale replacement of the seam with a
    /// ramp. Mirrors `JitterPolicy::crossfade_samples`; caller holds the lock.
    private var crossfadeSamples: Int { min(msSamples(Self.crossfadeMS), frameSamples / 2) }

    /// The two frame-denominated quantities, taken under the lock — for `AudioRingDriftTests`,
    /// which pins them the same way core's `the_shed_follows_the_negotiated_frame_length` pins its
    /// side. Locked, unlike the computed properties it wraps: those are only ever read from paths
    /// that already hold it.
    var frameGeometry: (frame: Int, crossfade: Int) {
        lock.lock()
        defer { lock.unlock() }
        return (frameSamples, crossfadeSamples)
    }

    /// Effective target depth in interleaved samples: the (adaptively grown) live target, lifted
    /// so it can always serve one device quantum plus a packet (a large-buffer device cannot
    /// sustain a target below its own quantum) — then, if the A/V sync loop has asked for a depth,
    /// its request CLAMPED into that band. Mirrors `JitterPolicy::effective_target`.
    ///
    /// The clamp order is the whole safety argument for steering playback depth off a network
    /// measurement at all: sync may pull the ring shallower to catch the picture up, or push it
    /// deeper when audio runs early, but never below what underrun pressure has proven this link
    /// needs, and never past the hard cap that bounds added latency. A link whose jitter genuinely
    /// demands more buffer than the picture is away keeps its buffer and the residual is REPORTED
    /// (`Stats.avOffsetMS`) rather than taken out of the listener's stream.
    ///
    /// The ceiling is raised to the floor rather than used as-is: a device whose callback quantum
    /// alone exceeds `hardCapMS` makes `floor > cap`, and a plain `min(max(s, floor), cap)` would
    /// then return the CAP — i.e. quietly below the continuity floor, inverting the very ordering
    /// this exists to guarantee, on exactly the awkward hardware it exists to survive. (Rust's
    /// `Ord::clamp` announces the same condition by panicking; Swift would just get it wrong.)
    private var target: Int { target(lift: renderQuantum) }

    /// The effective target with an explicit quantum lift. The property above uses the high-water
    /// `renderQuantum` (priming must survive the biggest callback seen); the hollow check in
    /// `read` passes the CURRENT callback instead, mirroring the Rust side's `want` — a one-off
    /// oversized read would otherwise inflate the debt threshold forever and turn the very next
    /// late packet into a full re-prime.
    private func target(lift quantum: Int) -> Int {
        let floor = adaptiveTarget(lift: quantum)
        guard let want = syncTarget else { return floor }
        let cap = max(msSamples(Self.hardCapMS), floor)
        return min(max(want, floor), cap)
    }

    /// The ADAPTIVE target: the live target underrun pressure has grown, lifted so it can always
    /// serve one quantum plus a packet. The floor the sync request is clamped against, and —
    /// because it is what underrun evidence has PROVEN this link needs — what `hollow` is judged
    /// against. Mirrors `JitterPolicy::adaptive_target`.
    private func adaptiveTarget(lift quantum: Int) -> Int {
        max(targetLive, quantum + frameSamples)
    }

    /// The sync loop is asking to run shallower than the adaptive target has grown to — the
    /// evidence `noteRead` relaxes a grown target on. Compared against the LIVE target, not the
    /// effective one: it is the underrun-driven growth that a sync request is evidence against,
    /// not the device-quantum lift, which no amount of measurement can argue with.
    private var syncWantsLess: Bool {
        guard let want = syncTarget else { return false }
        return want < targetLive
    }

    /// The sync loop is asking to run DEEPER than the adaptive target — audio is early against
    /// the picture. This is what arms the insert in `read`; without a sync request the ring never
    /// adds depth by itself, so an un-wired ring behaves exactly as it did before the insert
    /// existed. Mirrors `JitterPolicy::sync_wants_more`.
    private var syncWantsMore: Bool {
        guard let want = syncTarget else { return false }
        return want > targetLive
    }

    /// Hand the ring the depth the A/V sync loop wants (`AvSync.desiredDepth`), in interleaved
    /// samples, or `nil` to run unsynchronised. Called from the drain thread.
    ///
    /// This is a REQUEST, not a command — see `target` for what happens to it. `nil` is the
    /// default and reproduces the pre-sync behaviour exactly.
    func setSyncTarget(_ samples: Int?) {
        lock.lock()
        defer { lock.unlock() }
        syncTarget = samples
    }

    /// Store the sync loop's smoothed A/V offset for reporting (positive = audio behind the
    /// picture). The ring cannot compute this — it has no timestamps — but it is where the two
    /// numbers a listener's complaint needs, depth and offset, can be read under one lock.
    func noteAvOffset(_ ms: Int) {
        lock.lock()
        defer { lock.unlock() }
        avOffsetMS = ms
    }

    /// Store the drain thread's running drought concealment (`DroughtConceal.totalMS`) for
    /// reporting. Concealment that nobody can see is concealment that hides the bug it is
    /// covering: a healthy `underruns` bought with a climbing `plc_ms` is a link in trouble, not
    /// a link that is fine.
    func notePlcMS(_ ms: Int) {
        lock.lock()
        defer { lock.unlock() }
        plcMS = ms
    }

    /// Buffered depth in interleaved samples — what the sync loop measures against (`bufferedMS`
    /// is the same quantity rounded for humans). Everything queued here must play before the frame
    /// the drain thread is about to write, which is exactly what delays it.
    var bufferedSamples: Int {
        lock.lock()
        defer { lock.unlock() }
        return writeIdx - readIdx
    }

    func write(_ samples: UnsafePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        let capacity = buf.count
        // A single write larger than the whole ring would push readIdx PAST writeIdx below
        // (inverting the valid range — corruption). It never happens (one decoded packet is far
        // under capacity), but guard rather than corrupt.
        guard count <= capacity else { return }
        if writeIdx + count - readIdx > capacity {
            readIdx = writeIdx + count - capacity // overflow: drop oldest
        }
        for i in 0..<count {
            buf[(writeIdx + i) % capacity] = samples[i]
        }
        writeIdx += count
        // Backstop only: the smooth shed in `read` is what normally holds the depth down. The
        // hard cap must always leave room for one device quantum past the target (mirrors the
        // Rust policy's `.max(target + want)`) or a large-quantum device would trim itself into
        // a permanent underrun.
        let cap = max(
            min(target + msSamples(Self.headroomMS), msSamples(Self.hardCapMS)),
            target + renderQuantum)
        if writeIdx - readIdx > cap {
            // Crossfaded, like the smooth shed — see `dropFront`. This is the correction a
            // bunching link actually pays, so it is the one that most needs not to click.
            dropFront(writeIdx - readIdx - cap)
            depthAvg = Double(writeIdx - readIdx)
            overRun = 0
            underRun = 0
        }
    }

    /// Fills `out` completely (silence beyond what's buffered).
    func read(into out: UnsafeMutablePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        renderQuantum = max(renderQuantum, count)
        let available = writeIdx - readIdx

        // Depth average, weighted by the callback size so its time constant is independent of the
        // device quantum.
        let alpha = min(1.0, Double(count) / Double(msSamples(Self.ewmaTauMS)))
        depthAvg += (Double(available) - depthAvg) * alpha

        if !primed {
            if available >= target {
                primed = true
                emptyReads = 0
                emptyRun = 0
                // The refill just banked this much: seed the average with it rather than letting
                // it climb from wherever the drought left it — a freshly-primed ring would
                // otherwise read as hollow for the EWMA's whole settling time, and the FIRST
                // late packet would re-prime a ring that is actually full.
                depthAvg = Double(available)
            } else {
                for i in 0..<count { out[i] = 0 }
                return
            }
        }

        // Hollow: the depth AVERAGE runs a debt against the target — the promise has been raised
        // but the depth was never re-banked (see `deprimeDebtMS`). Judged on the average, not
        // this instant: a single late packet empties the ring for a callback without making it
        // hollow, and must keep the consecutive-empties hysteresis. Lifted by THIS callback's
        // size, not the high-water quantum — see `target(lift:)`.
        //
        // Judged against the ADAPTIVE target, never the sync-inflated one. The debt this exists
        // to call in is GROWTH that was never banked — underrun evidence raised the promise — and
        // only a re-prime cashes that. A sync request is not evidence of starvation; it is a
        // request for alignment, and it has its own gentle instrument (the insert below).
        // Measured against the effective target, a request for ≥ `deprimeDebtMS` more depth made
        // the ring hollow on the very next callback and turned the next single late packet into a
        // full re-prime. The effective target is never below the adaptive one, so this can only be
        // LESS hollow. Mirrors `JitterPolicy::step`.
        hollow = depthAvg + Double(msSamples(Self.deprimeDebtMS)) < Double(adaptiveTarget(lift: count))

        // Drift correction: shed exactly one frame, crossfaded, once the AVERAGE has sat above
        // the threshold for the sustain window. Anything shorter is jitter and must be left alone.
        if depthAvg > Double(target + msSamples(Self.shedExcessMS)) {
            overRun += count
            underRun = 0
            if overRun >= msSamples(Self.shedSustainMS) {
                overRun = 0
                shedOneFrame()
                shedCount += 1
                depthAvg = Double(writeIdx - readIdx)
            }
        } else if syncWantsMore, depthAvg + Double(msSamples(Self.insertMarginMS)) < Double(target) {
            // The mirror of the shed. The sync loop has asked for a DEEPER ring than the adaptive
            // target (audio is early against the picture) and the AVERAGE has sat more than the
            // margin below what it asked for, for the sustain window: duplicate ONE frame at the
            // front, crossfaded. Below-target-only, so it can never fight the trim; sync-only, so
            // an un-wired ring never adds depth by itself and the hollow re-prime keeps its job
            // for growth that was never banked. (Primed-only comes free: an un-primed read
            // returned above.) The ring must hold a whole frame to duplicate — if it does not it
            // is running dry, and the drought path is the tool for that. Mirrors the insert
            // branch in `JitterPolicy::step`.
            overRun = 0
            underRun += count
            if underRun >= msSamples(Self.insertSustainMS), writeIdx - readIdx >= frameSamples {
                underRun = 0
                insertOneFrame()
                insertCount += 1
                // Whatever we duplicated is buffered now — reflect it at once so the next
                // callbacks don't re-fire on a stale average.
                depthAvg += Double(frameSamples)
            }
        } else {
            overRun = 0
            underRun = 0
        }

        let n = min(writeIdx - readIdx, count)
        let capacity = buf.count
        for i in 0..<n {
            out[i] = buf[(readIdx + i) % capacity]
        }
        readIdx += n
        if n < count {
            for i in n..<count { out[i] = 0 }
        }
        // Near-miss: served in full, but with less than one frame left over — the next callback
        // starves unless a packet lands within one frame time.
        //
        // Denominated in the RESOLVED frame, not a fixed 5 ms: against a 2 ms lossless frame a
        // frozen margin stops meaning "one packet in hand" and starts meaning two and a half, so it
        // would grow the target on a ring that was never close to starving — inverting the thing it
        // exists to detect. Identical on every Opus session.
        //
        // (This used to be flagged here as a deliberate divergence, with core still measuring
        // against a `NEAR_MISS_MARGIN_MS` constant and a note that core should follow. It has:
        // `JitterPolicy::step` now compares against `frame_samples()` too, and its own
        // `the_near_miss_margin_is_one_negotiated_frame` pins it. The two policies agree again.)
        //
        // ⚠ `frameSamples` is the WIRE's frame — floored per channel — not `msSamples(frameMS)`.
        // The margin means "exactly one packet", and at 44 100 Hz those two are 440 and 441.
        nearMiss = n == count && writeIdx - readIdx < frameSamples
        noteRead(ranShort: n < count, count: count)
    }

    /// The outcome accounting of one primed read — the Swift mirror of
    /// `JitterPolicy::note_read`. A short read drives both the de-prime hysteresis (a single
    /// transient drain must not manufacture a whole target's worth of fresh silence) and the
    /// adaptive target floor: a device that genuinely keeps starving gets more slack, one step
    /// per window, capped — and gives it back after a long quiet spell, so one bad minute
    /// doesn't cost latency for the rest of the session. Caller holds the lock.
    private func noteRead(ranShort: Bool, count: Int) {
        windowRun += count
        if windowRun >= msSamples(Self.growWindowMS) {
            windowRun = 0
            underrunsInWindow = 0
            nearMissGrown = false
        }
        syncBackoffRun = max(0, syncBackoffRun - count)
        var restored = false
        if probeRun > 0 {
            probeRun = max(0, probeRun - count)
            if ranShort || nearMiss {
                // The probe FAILED: the link answered a shrink with (nearly) starving the ring.
                // Take the depth straight back — re-learning it three audible underruns at a
                // time is what made the sync-vs-growth tug-of-war audible — and keep the sync
                // loop from probing again for a while, doubling per consecutive failure. The
                // residual A/V offset is reported instead; continuity outranks sync. The
                // restore CONSUMES this event as growth evidence: it answered a depth the ring
                // is no longer at, so growing past the proven target on top would overshoot.
                probeRun = 0
                targetLive = max(targetLive, probePrevTarget)
                syncBackoffRun = msSamples(syncBackoffLenMS)
                syncBackoffLenMS = min(syncBackoffLenMS * 2, Self.syncBackoffMaxMS)
                restored = true
            } else if probeRun == 0 {
                // Survived the whole window: the shallower depth is genuinely safe here, so the
                // next probe starts from a clean slate.
                syncBackoffLenMS = Self.syncBackoffMS
            }
        }
        if ranShort {
            quietRun = 0
            emptyReads += 1
            emptyRun += count
            underrunCount += 1
            // Starved for `deprimeMS` of audio, over at least `minDeprimeCallbacks` callbacks.
            // Both, because either alone is wrong at one end of the quantum range: time alone is a
            // hair trigger on a device whose single quantum already exceeds the window, and a
            // callback count alone is the device-dependent fuse this replaced.
            let starved = emptyRun >= msSamples(Self.deprimeMS)
                && emptyReads >= Self.minDeprimeCallbacks
            if starved || hollow {
                // The starvation hysteresis protects a FULL ring from one late packet.
                // A hollow ring is the opposite case: the target has been raised but the depth
                // never re-banked (growth is a promise; only a re-prime cashes it), and riding
                // that out is a click per bunching period, forever. The click just heard has
                // already paid for the refill — take it now.
                primed = false
                emptyReads = 0
                emptyRun = 0
            }
            if !restored {
                underrunsInWindow += 1
            }
            if underrunsInWindow >= Self.growUnderruns {
                underrunsInWindow = 0
                windowRun = 0
                targetLive = min(targetLive + msSamples(Self.growStepMS), msSamples(Self.maxTargetMS))
            }
        } else if nearMiss {
            // Came within one frame of an underrun — the same evidence as one, heard by no one.
            // Growing here, BEFORE the click, is what "no audible jitter" means: waiting for
            // the third audible underrun means the user heard two. One step per window (a
            // bunching episode is a RUN of near-misses while the ring refills, and must buy one
            // measured step, not a sprint to the ceiling); if it worsens into real underruns
            // the path above takes over. A near-miss is pressure, not quiet.
            quietRun = 0
            emptyReads = 0
            emptyRun = 0
            if !nearMissGrown, !restored {
                nearMissGrown = true
                targetLive = min(targetLive + msSamples(Self.growStepMS), msSamples(Self.maxTargetMS))
            }
        } else {
            emptyReads = 0
            emptyRun = 0
            quietRun += count
            // Without a sync request, time is the only evidence that hard-won slack is no longer
            // needed, so a grown target waits out the long window. A request for less IS evidence,
            // and without this branch a ring that ratcheted to the ceiling during a transient would
            // hold audio a ceiling's worth late for minutes after the cause had gone. Every shrink
            // is armed as a PROBE — answered by an underrun or near-miss it is undone at once (see
            // above), and a failed sync-driven guess is not retried for a backoff.
            let syncShrink = syncWantsLess && syncBackoffRun == 0
            let quietNeeded = syncShrink ? Self.shrinkQuietSyncMS : Self.shrinkQuietMS
            if quietRun >= msSamples(quietNeeded) {
                quietRun = 0
                let prev = targetLive
                targetLive = max(targetLive - msSamples(Self.growStepMS), msSamples(Self.targetMS))
                if targetLive < prev {
                    probeRun = msSamples(Self.shrinkProbeMS)
                    probePrevTarget = prev
                }
            }
        }
    }

    /// Drop one audio frame from the front — the smooth drift correction. The session's REAL frame
    /// (`setFrameUs`), so a lossless session sheds its own 2–4 ms rather than two and a half of
    /// them.
    private func shedOneFrame() { dropFront(frameSamples) }

    /// Drop `drop` interleaved samples from the front, linearly crossfading the seam so the
    /// correction is inaudible rather than a click. Mirrors `punktfunk_core::audio::crossfade_drop`;
    /// caller holds the lock.
    ///
    /// Used by BOTH corrections. The hard-cap trim in `write` used to splice raw, on the reasoning
    /// that a ring which blew its ceiling is already a discontinuity — but that describes the
    /// ARRIVALS, not the samples either side of the seam, which are ordinary continuous audio. It
    /// is also the drop that actually fires here: a bunching Wi-Fi link trims far more often than
    /// drift sheds, so the one path left unfaded was the audible one.
    ///
    /// The fade is `crossfadeSamples` — capped at half a frame — then clamped again to what this
    /// particular drop can actually spare on either side of the seam.
    ///
    /// The fade-OUT source is the HEAD of what is discarded — the continuation of the sample the
    /// device just played — blending into the head of what survives, so both ends of the seam are
    /// continuous. (It used to fade out from the discarded region's TAIL, which is adjacent to the
    /// survivors but not to the sample just played, so the seam still opened with a step of
    /// `drop − fade` samples of waveform. Core's `crossfade_drop` had the same defect and the same
    /// fix; `AudioRingDriftTests` now checks the seam against the sample played before it.)
    private func dropFront(_ drop: Int) {
        let available = writeIdx - readIdx
        guard drop > 0, available > drop else { return }
        let fade = min(crossfadeSamples, min(drop, available - drop))
        let capacity = buf.count
        if fade > 0 {
            for i in 0..<fade {
                let old = buf[(readIdx + i) % capacity]
                let new = buf[(readIdx + drop + i) % capacity]
                let t = Float(i + 1) / Float(fade + 1)
                buf[(readIdx + drop + i) % capacity] = old * (1 - t) + new * t
            }
        }
        readIdx += drop
    }

    /// Duplicate one audio frame at the front — the sync-driven deepening, the mirror of
    /// `shedOneFrame`. The session's REAL frame (`setFrameUs`).
    private func insertOneFrame() { insertFront(frameSamples) }

    /// Duplicate the first `insert` interleaved samples at the front — the ring plays them, then
    /// plays them again — linearly crossfading the seam so the correction is continuous rather
    /// than a click. Mirrors `punktfunk_core::audio::crossfade_insert`; caller holds the lock.
    ///
    /// Index-based where core's is a `VecDeque`: the copy lands in the `insert` slots just BEFORE
    /// `readIdx`, which are free exactly when the ring has that much spare capacity (they hold
    /// audio already consumed), and `readIdx` steps back over it. `readIdx`/`writeIdx` are plain
    /// offsets reduced modulo the capacity wherever they touch `buf`, so when `readIdx` is too
    /// small to step back both are shifted forward by one whole capacity first — every position
    /// they name is unchanged, and neither can go negative (which `%` would turn into a negative
    /// index).
    ///
    /// The seam: what would have followed the copy's last sample is the original's `insert`-th
    /// sample onward, so THAT fades out into the original's head, in place. The copy is written
    /// before the seam is blended, so it is verbatim; the fade-out reads sit `insert` past every
    /// write, so one ascending pass is safe.
    private func insertFront(_ insert: Int) {
        let available = writeIdx - readIdx
        let capacity = buf.count
        guard insert > 0, available >= insert, available + insert <= capacity else { return }
        let fade = min(crossfadeSamples, min(insert, available - insert))
        if readIdx < insert {
            readIdx += capacity
            writeIdx += capacity
        }
        for i in 0..<insert {
            buf[(readIdx - insert + i) % capacity] = buf[(readIdx + i) % capacity]
        }
        for i in 0..<fade {
            let old = buf[(readIdx + insert + i) % capacity]
            let new = buf[(readIdx + i) % capacity]
            let t = Float(i + 1) / Float(fade + 1)
            buf[(readIdx + i) % capacity] = old * (1 - t) + new * t
        }
        readIdx -= insert
    }

    /// Current buffered depth in milliseconds — for the stats overlay and the drain thread's
    /// periodic log.
    var bufferedMS: Int {
        lock.lock()
        defer { lock.unlock() }
        return samplesMs(writeIdx - readIdx)
    }

    /// One consistent snapshot of the ring's vitals, taken under a single lock so the numbers in
    /// a log line describe the same instant. Mirrors what the three Rust clients report.
    struct Stats {
        let bufferedMS: Int
        let targetMS: Int
        let underruns: Int
        let sheds: Int
        /// Sync-driven inserts — one duplicated, crossfaded frame each (`insertOneFrame`). Read
        /// next to `sheds`: the same correction, the other direction.
        let inserts: Int
        /// The A/V sync loop's smoothed offset (ms): **positive = audio playing BEHIND the
        /// picture**, negative = ahead of it. `0` before the loop has evidence, or with sync off.
        ///
        /// Reported next to the depth, never instead of it: a deep ring on a jittery link is
        /// CORRECT behaviour, and only the offset separates that from a ring holding audio late.
        let avOffsetMS: Int
        /// Audio synthesized for packet droughts this session (`DroughtConceal`), ms — read next
        /// to `underruns`, which it exists to prevent, because the two only mean something
        /// together.
        let plcMS: Int
    }

    var stats: Stats {
        lock.lock()
        defer { lock.unlock() }
        return Stats(
            bufferedMS: samplesMs(writeIdx - readIdx),
            targetMS: samplesMs(target),
            underruns: underrunCount,
            sheds: shedCount,
            inserts: insertCount,
            avOffsetMS: avOffsetMS,
            plcMS: plcMS)
    }
}

// MARK: - A/V sync

/// The A/V synchronisation controller: turns "when will this audio actually play" and "when did
/// the picture it belongs with reach the glass" into a ring depth `AudioRing` should aim for.
/// The Swift mirror of `punktfunk_core::audio::AvSync` — keep the two in step.
///
/// **The defect it exists to fix.** The host stamps `pts_ns` on every audio datagram and the
/// client decoded it into `AudioPCM` — and then never read it. Video's `pts_ns`, by contrast, is
/// used end to end (`LatencyMeter` computes a true glass-to-glass `displayed + clockOffset − pts`
/// per presented frame). So audio free-ran at whatever depth its jitter ring happened to settle
/// at, video was presented on a wholly independent path, and nothing ever compared them: the A/V
/// offset was an accident of buffer depths. It moved whenever the ring ratcheted under underrun
/// pressure, and — the way this surfaced in the field — it got WORSE every time video got faster,
/// because a quicker decoder lowers the video leg while leaving the audio leg exactly where it was.
///
/// **Video is the master.** In a game streamer the video leg is the input-feel budget and must
/// never be inflated to satisfy the audio clock; audio tolerates small, crossfaded, rate-limited
/// corrections that are inaudible, and `AudioRing.shedOneFrame` already applies them. So audio
/// moves.
///
/// **Continuity outranks sync.** This type only ever PROPOSES a depth. `AudioRing` clamps the
/// proposal to its own underrun-driven floor (see `AudioRing.target`), so a link whose jitter
/// genuinely needs more buffer than the picture is away keeps its buffer and the residual is
/// reported instead of being taken out of the listener's stream.
///
/// Not a class and not locked: it is owned outright by the drain thread that observes packets.
struct AvSync {
    /// Smoothing time constant for the measured offset, in ms of consumed audio. Long enough that
    /// network jitter and a single late datagram do not move it; short enough to track real drift.
    private static let ewmaTauMS = 2_000
    /// Offsets inside this band are left alone. Correcting a few ms costs a (crossfaded, but real)
    /// discontinuity and buys nothing a listener can perceive — detectability for A/V misalignment
    /// sits an order of magnitude above it. The deadband is what keeps the loop from hunting
    /// forever around zero, which would be audible in a way the misalignment it chased was not.
    static let deadbandMS = 10
    /// Observations folded before the first correction is offered. The offset is derived from a
    /// clock skew estimate and a video figure that both need a moment to settle after connect;
    /// acting on the first sample would chase the handshake, not the stream.
    private static let minObservations = 100
    /// An offset larger than this is not believed. A wall-clock step, a paused host, or a stale
    /// video figure can all produce an enormous apparent misalignment, and steering the ring by it
    /// would empty or overfill it outright. Beyond this the loop reports and waits rather than acts.
    private static let saneLimitMS = 1_000
    /// The protocol's frame, in ms — the EWMA is weighted by it so the time constant means the
    /// same thing however often the caller observes. Shares `AudioRing.frameMS`'s caveat: on the
    /// lossless plane the real frame is shorter, so observations arrive more often than this
    /// weight assumes and the average settles proportionally faster. It is a time constant on a
    /// loop with a 100-observation settling gate, so faster is harmless; see that constant.
    private static let frameMS = AudioRing.frameMS

    /// The negotiated layout, in the same two numbers `AudioRing` keeps and for the same reason —
    /// this type's proposal is denominated in the ring's own units, so the two have to agree about
    /// what a millisecond is down to the sample.
    private let rateHz: Int
    private let channels: Int
    /// EWMA of the measured offset in ns. Positive = audio is scheduled to play LATE relative to
    /// the picture it belongs with.
    private var offsetAvgNs: Double = 0
    private var observations = 0
    /// Set once an observation lands outside `saneLimitMS`, for reporting.
    private(set) var implausible = false

    /// `channels` is the negotiated interleaved channel count (2/6/8), `rateHz` the negotiated
    /// sample rate — every rate on the lossless ladder, exactly, for the reason `AudioRing.init`
    /// gives: the ms ⇄ sample conversion multiplies before it divides, so the 44.1 kHz family is
    /// representable here too and the depth this type proposes lands in the units the ring measures
    /// itself in. Mirrors `AvSync::new_at_rate`.
    init(channels: Int, rateHz: Int) {
        self.rateHz = max(rateHz, 1)
        self.channels = max(channels, 1)
    }

    /// Interleaved samples to whole milliseconds — see `audioSamplesToMs`.
    private func samplesMs(_ samples: Int) -> Int {
        audioSamplesToMs(rateHz: rateHz, channels: channels, samples: samples)
    }

    /// One measurement handed to `observe`. Every field is in the units its source already
    /// produces, so no caller has to do clock arithmetic to use it correctly.
    struct Observation {
        /// The host capture timestamp carried by the audio frame being queued (host clock).
        let ptsNs: UInt64
        /// Local `CLOCK_REALTIME` now — the same basis `LatencyMeter` stamps video in.
        let nowLocalNs: Int64
        /// Host clock minus client clock, from the skew handshake (`clockOffsetNs`).
        ///
        /// It very nearly CANCELS: the video figure this is differenced against was computed with
        /// the same offset and the same sign, so as long as both terms use one value the skew
        /// drops out of the result entirely. That is what makes the connect-time offset good
        /// enough here even though the absolute legs would prefer a re-synced one.
        let clockOffsetNs: Int64
        /// How much audio is already queued AHEAD of this frame, in interleaved samples —
        /// everything that must play before it does.
        let bufferedAhead: Int
        /// The video plane's current end-to-end figure in ns: `displayed + clockOffset − pts`, as
        /// `LatencyMeter` already computes it per presented frame. `nil` while nothing has reached
        /// the glass recently — no reference, no correction.
        let videoE2eNs: Int64?
    }

    /// Fold one measurement. Returns the smoothed offset in ns once there is enough evidence to
    /// believe it (positive = audio late), or `nil` while still settling.
    ///
    /// Rejecting the implausible rather than clamping it is deliberate: a wall-clock step or a
    /// stale video figure produces a huge apparent offset, and a clamped-but-wrong value would be
    /// acted on as though it were a small real one.
    @discardableResult
    mutating func observe(_ o: Observation) -> Int64? {
        // No frame on the glass yet ⇒ no reference to align against, so nothing to say.
        guard let videoE2eNs = o.videoE2eNs else { return nil }
        // When this frame's samples will actually reach the speaker, expressed in the host's
        // capture clock — the same clock, and the same shape, as the video figure it is compared
        // against.
        // Deliberately still rounded to whole MILLISECONDS rather than converted straight to ns: it
        // keeps every 48/96 kHz session bit-identical to the shipped behaviour, and the ≤ 1 ms it
        // discards is an order of magnitude inside `deadbandMS`, which is the resolution this loop
        // acts on at all. The conversion itself is now exact at every rate.
        let bufferedNs = Int64(samplesMs(o.bufferedAhead)) * 1_000_000
        // Overflow-reporting arithmetic, NOT the wrapping `&+`/`&-` the meters use. Every term is
        // a nanosecond count on the same epoch (~1.8e18), so the DIFFERENCE is tiny while the
        // operands sit within a factor of five of `Int64.max` — and a garbage `pts_ns` would wrap
        // a nonsense value round into a small, plausible-looking offset. This loop's entire
        // defence is that it can tell nonsense from a real misalignment, so an overflow takes the
        // same exit the sanity limit does rather than being silently believed.
        let (playAtLocal, o1) = o.nowLocalNs.addingReportingOverflow(bufferedNs)
        let (playAtHost, o2) = playAtLocal.addingReportingOverflow(o.clockOffsetNs)
        let (audioE2eNs, o3) = playAtHost.subtractingReportingOverflow(Int64(bitPattern: o.ptsNs))
        let (offsetNs, o4) = audioE2eNs.subtractingReportingOverflow(videoE2eNs)
        guard !o1, !o2, !o3, !o4, abs(offsetNs) <= Int64(Self.saneLimitMS) * 1_000_000 else {
            implausible = true
            return nil
        }
        implausible = false

        let alpha = min(1.0, Double(Self.frameMS) / Double(Self.ewmaTauMS))
        if observations == 0 {
            offsetAvgNs = Double(offsetNs)
        } else {
            offsetAvgNs += (Double(offsetNs) - offsetAvgNs) * alpha
        }
        observations += 1
        return settled ? Int64(offsetAvgNs) : nil
    }

    /// Enough evidence folded to act on.
    var settled: Bool { observations >= Self.minObservations }

    /// The smoothed offset in ms (positive = audio late), for the HUD. Reported as soon as it is
    /// measured, including while still settling — a number the operator can watch converge is more
    /// useful than a blank that hides whether the loop is working at all.
    var offsetMS: Int { Int(offsetAvgNs / 1_000_000) }

    /// The ring depth that would place audio with the picture, given where the ring is now.
    /// `nil` while unsettled or inside the deadband — the caller then leaves the ring alone.
    ///
    /// Audio late (offset > 0) means there is too much queued: aim shallower. Audio early means
    /// aim deeper.
    func desiredDepth(currentDepth: Int) -> Int? {
        guard settled else { return nil }
        let offsetMs = offsetAvgNs / 1_000_000
        guard abs(offsetMs) >= Double(Self.deadbandMS) else { return nil }
        // One millisecond of samples as a float, so a fractional offset scales smoothly. The
        // division is done on the CONSTANT, not on the product: `x * 96000.0 / 1000.0` rounds twice
        // and can land one ulp — and so one sample — away from the `x * 96.0` every shipped 48 kHz
        // session computes today. This way it is exactly 96.0 / 192.0 there, and correctly 88.2 at
        // 44 100 Hz stereo, where the old integer `perMS` said 88 and steered every correction
        // 0.23 % short. Mirrors `AvSync::desired_depth`.
        let perMs = Double(audioInterleavedPerSec(rateHz: rateHz, channels: channels)) / 1_000
        let delta = Int(offsetMs * perMs)
        return max(0, currentDepth - delta)
    }
}

// MARK: - Drought concealment

/// Bounded concealment of a packet DROUGHT — the Apple leg of the policy the three Rust clients
/// share (`punktfunk_core::audio::DroughtConceal`; design/host-source-stutter-fixes.md, WP-C1).
///
/// The decode path already conceals a SEQ GAP: core's in-ABI decoder synthesizes the packets the
/// sequence says went missing before the one that arrived (`nextAudioPcm`). But that only fires
/// when a LATER packet arrives to reveal the gap. When the wire simply goes quiet — a delivery
/// stall on a bunching Wi-Fi link, or a host whose capture stalled — nothing arrives to reveal
/// anything: `AudioRing` drains to empty, the render callback runs short, and `noteRead` de-primes
/// and then re-primes a whole target's worth of fresh silence. The artifact is far longer than the
/// audio actually missing, and this is the shape the 2026-08-15 field session spent 3–16 % of its
/// wall-clock in.
///
/// So a drought that is draining the ring gets concealed too, from the same decoder state
/// (`PunktfunkConnection.audioPlc`), for a bounded time. The BOUND is denominated in TIME, never in
/// frames or callbacks: that is the recorded lesson from the very fuse this protects, where a count
/// gave an iPad a third of a Mac's slack (`AudioRing.deprimeMS`, and
/// `testDeprimeFuseIsADurationNotACallbackCount`). What it COUNTS is frames — one per synthesized
/// packet, which is what the drain thread actually produces — and the resolved frame length is what
/// converts between the two. Those are the same discipline, not opposite ones: the policy is stated
/// in time and the conversion is exact, instead of a frame being assumed to be 5 ms.
///
/// Time is passed IN, so the policy stays as deterministic as the ring's own.
struct DroughtConceal {
    /// Frames concealed since the last real packet. Counted in FRAMES rather than milliseconds
    /// because that is what the drain thread actually does — one `audioPlc()` frame per `conceal()`
    /// that says yes — and because the frame is no longer a fixed 5 ms; see `init(maxMS:frameUs:)`.
    private var concealed = 0
    private let maxMS: Int
    /// One frame, in MICROSECONDS. Everything time-denominated here derives from it.
    private let frameUs: Int
    /// Concealed over the session, in FRAMES.
    private var total = 0

    /// At the protocol's default frame (`AudioRing.frameMS`) — every Opus session, and every test
    /// that pins the pre-hi-res numbers.
    init(maxMS: Int) {
        self.init(maxMS: maxMS, frameUs: AudioRing.frameMS * 1_000)
    }

    /// At an explicitly negotiated frame length (`punktfunk_connection_audio_frame_us`).
    ///
    /// This type charges one frame per concealed frame and bounds itself in WALL-CLOCK
    /// milliseconds, so the two have to agree about how long a frame is. They did not: the frame was
    /// assumed to be 5 ms, and on a 2 ms lossless frame that made the `maxMS` budget run out after
    /// two fifths of the time it is meant to buy, with the reported `plc_ms` two and a half times
    /// too high — and on the 1 ms frame a 5.1 session negotiates, a fifth and five times. The frame
    /// COUNT was always right (it charged 5 and divided by 5), which is exactly why this went
    /// unnoticed: the load-bearing number was fine and only the two human-facing ones were wrong.
    /// Mirrors `DroughtConceal::new_at_frame_us`.
    init(maxMS: Int, frameUs: Int) {
        self.maxMS = maxMS
        self.frameUs = max(frameUs, 1)
    }

    /// How long a drought must last before it is concealed at all — TWO FRAMES, so an ordinary
    /// inter-packet gap is never mistaken for a stall. It was a fixed `2 × frameMS`, which on a 2 ms
    /// lossless frame waits five frames instead of two before conceding there is a stall.
    ///
    /// ⚠ In whole milliseconds, because that is the granularity the caller measures the quiet wire
    /// at (core compares `Duration`s in µs). Every rung on the ladder is a multiple of 500 µs, so
    /// `2 × frameUs` is always a whole number of ms and nothing truncates; the floor of 1 exists
    /// only so a degenerate `frameUs` cannot produce a zero-length tolerance, which would conceal
    /// ordinary jitter as though it were a stall.
    private var afterMS: Int { max(2 * frameUs / 1_000, 1) }

    /// Ring depth below which a drought is worth concealing, in ms — also two frames. A drought a
    /// deep ring can cover is not audible, and concealing it would synthesize audio the late packets
    /// are about to duplicate, pushing the whole stream later and handing the drift shed a mess to
    /// clean up audibly. Rounds UP, like core's `div_ceil`, so the floor is never *less* than the
    /// two frames it promises.
    private var floorMS: Int { (2 * frameUs + 999) / 1_000 }

    /// Concealed since the last real packet, in ms — the figure the `maxMS` budget bounds.
    private var concealedMS: Int { concealed * frameUs / 1_000 }

    /// Concealment over the session, ms — what the 10 s `plc_ms=` line reports. Concealment must be
    /// visible: a policy that quietly papers over a failing link is a policy that hides the bug.
    var totalMS: Int { total * frameUs / 1_000 }

    /// A packet arrived, ending any drought — the next one starts from a full budget.
    ///
    /// Nothing to divide: the run is a frame count, so ending it is one assignment. The Rust twin
    /// hands that count BACK, for its caller to subtract from the loss concealment the seq path is
    /// about to ask for. Here that subtraction is core's, on the far side of the ABI, because that
    /// is where the gap tracker lives (see `punktfunk_connection_audio_plc`) — a packet genuinely
    /// lost inside a covered drought must not be concealed twice either way.
    mutating func packet() {
        concealed = 0
    }

    /// Should one more frame be concealed? `depthMS` is the playout ring as the render callback
    /// last left it.
    mutating func conceal(sinceLastPacketMS: Int, depthMS: Int) -> Bool {
        if sinceLastPacketMS < afterMS || depthMS > floorMS || concealedMS >= maxMS {
            return false
        }
        concealed += 1
        total += 1
        return true
    }
}

/// CoreAudio channel layout for the canonical wire order FL FR FC LFE RL RR [SL SR]. nil for
/// stereo (the standard layout is correct). For 5.1/7.1 we list explicit channel labels via
/// `kAudioChannelLayoutTag_UseChannelDescriptions` — preset tags (DTS_5_1 etc.) don't reliably
/// match Moonlight's order. NB the 7.1 mapping (verified against the WASAPI 0x63F + SPA orderings):
/// wire idx 4-5 = RL/RR = the WAVE *back* pair → LeftSurround/RightSurround; idx 6-7 = SL/SR = the
/// WAVE *side* pair → LeftSurroundDirect/RightSurroundDirect. (Using RearSurround* for 6-7 would
/// swap side/back vs the Windows/Linux clients.)
func wireChannelLayout(channels: Int) -> AVAudioChannelLayout? {
    let labels: [AudioChannelLabel]
    switch channels {
    case 6:
        labels = [
            kAudioChannelLabel_Left, kAudioChannelLabel_Right, kAudioChannelLabel_Center,
            kAudioChannelLabel_LFEScreen, kAudioChannelLabel_LeftSurround,
            kAudioChannelLabel_RightSurround,
        ]
    case 8:
        labels = [
            kAudioChannelLabel_Left, kAudioChannelLabel_Right, kAudioChannelLabel_Center,
            kAudioChannelLabel_LFEScreen,
            kAudioChannelLabel_LeftSurround, kAudioChannelLabel_RightSurround, // wire RL/RR (back)
            kAudioChannelLabel_LeftSurroundDirect, kAudioChannelLabel_RightSurroundDirect, // wire SL/SR (side)
        ]
    default:
        return nil
    }
    let size = MemoryLayout<AudioChannelLayout>.size
        + (labels.count - 1) * MemoryLayout<AudioChannelDescription>.stride
    let raw = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 16)
    defer { raw.deallocate() }
    let layout = raw.bindMemory(to: AudioChannelLayout.self, capacity: 1)
    layout.pointee.mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions
    layout.pointee.mChannelBitmap = AudioChannelBitmap(rawValue: 0)
    layout.pointee.mNumberChannelDescriptions = UInt32(labels.count)
    // `mChannelDescriptions` is the C variable-length tail array (declared `[1]`, over-allocated
    // above). Scope the pointer with `withUnsafeMutablePointer` — taking `&…mChannelDescriptions`
    // inline yields a pointer valid only for that expression, so building a buffer from it that
    // outlives the call is a dangling-pointer bug. Inside the closure it stays valid while we fill it.
    withUnsafeMutablePointer(to: &layout.pointee.mChannelDescriptions) { tail in
        let descs = UnsafeMutableBufferPointer(start: tail, count: labels.count)
        for (i, lbl) in labels.enumerated() {
            descs[i] = AudioChannelDescription(
                mChannelLabel: lbl, mChannelFlags: AudioChannelFlags(rawValue: 0),
                mCoordinates: (0, 0, 0))
        }
    }
    return AVAudioChannelLayout(layout: layout)
}
