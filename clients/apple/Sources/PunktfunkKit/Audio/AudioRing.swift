import AVFoundation
import os

/// SPSC-ish jitter ring (interleaved float, `channels` per frame), drain thread → render
/// callback. The unfair lock is held for microseconds; fine at render-callback rates. Priming:
/// reads return silence until enough is buffered (at least the target, and at least one
/// packet more than the device's render quantum — large-buffer devices would otherwise
/// chronically out-demand the prefill and oscillate prime → dropout → re-prime).
/// All counts stay whole frames (multiples of `channels`), so the interleave can never slip.
///
/// **Drift correction.** Both ends run at 48 kHz but on different crystals, so backlog from a
/// network stall or plain host-vs-DAC skew never drains on its own: without correction one 300 ms
/// hiccup leaves audio 300 ms behind video for the rest of the session. This used to be handled by
/// a `highWater` shed that dropped a whole `2 × prefill` at once — its own comment called that "one
/// audible blip". It is now the same two-stage scheme the Rust clients share
/// (`punktfunk_core::audio::JitterPolicy`): a slow depth average that sits above target for a
/// sustained window sheds ONE 5 ms frame with a crossfade, and the hard cap is only a backstop.
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
    private static let deprimeAfter = 4
    /// The protocol's frame: the shed unit, and the slack added over a large device quantum.
    private static let frameMS = 5
    /// Depth average must exceed target by this before drift correction fires — the middle of the
    /// headroom band, so the smooth shed always gets its chance BEFORE the hard cap trims.
    private static let shedExcessMS = 15
    /// …and must stay there for this much consumed audio. Long, because a shed is the only thing
    /// here a listener could notice; it must never fire on a transient.
    private static let shedSustainMS = 2_000
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
    /// Post-read depth below which a served callback counts as a NEAR-MISS: the device got its
    /// samples, but with less than one protocol frame left in hand — the same evidence as an
    /// underrun, except nobody heard it yet, so the target grows BEFORE the click instead of
    /// after the third one. Mirrors `NEAR_MISS_MARGIN_MS`.
    private static let nearMissMarginMS = frameMS
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
    private var emptyReads = 0
    private var depthAvg: Double = 0
    private var overRun = 0
    /// The live target in interleaved samples — `targetMS` grown by underrun pressure
    /// (`noteRead`), never below the base. Set in `init` (needs `perMS`).
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
    /// The depth the A/V sync loop would like, in interleaved samples (`AvSync.desiredDepth`).
    /// `nil` — the default, and what an un-wired session keeps — reproduces the pre-sync
    /// behaviour exactly, so this ring could adopt sync without the other three diverging.
    private var syncTarget: Int?
    /// This read was served with less than `nearMissMarginMS` left over (set in `read`,
    /// consumed by `noteRead`).
    private var nearMiss = false
    /// A near-miss already grew the target this window — one step per window, so a bunching
    /// episode (a RUN of consecutive near-misses while the ring refills) buys one measured
    /// step, not a sprint to the ceiling.
    private var nearMissGrown = false
    /// The depth average runs a `deprimeDebtMS` debt against the target (set in `read`): an
    /// underrun should re-prime at once instead of waiting out the hysteresis.
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
    private let channels: Int
    private let perMS: Int
    private let lock = OSAllocatedUnfairLock()

    /// `capacity` in samples (interleaved — `channels` per frame, a whole number of frames).
    /// The de-jitter depth is the ring's own business (`targetMS`), not a caller's prefill.
    init(capacity: Int, channels: Int) {
        buf = [Float](repeating: 0, count: capacity)
        self.channels = channels
        perMS = 48 * channels
        targetLive = Self.targetMS * perMS
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
        let floor = max(targetLive, quantum + Self.frameMS * perMS)
        guard let want = syncTarget else { return floor }
        let cap = max(Self.hardCapMS * perMS, floor)
        return min(max(want, floor), cap)
    }

    /// The sync loop is asking to run shallower than the adaptive target has grown to — the
    /// evidence `noteRead` relaxes a grown target on. Compared against the LIVE target, not the
    /// effective one: it is the underrun-driven growth that a sync request is evidence against,
    /// not the device-quantum lift, which no amount of measurement can argue with.
    private var syncWantsLess: Bool {
        guard let want = syncTarget else { return false }
        return want < targetLive
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
            min(target + Self.headroomMS * perMS, Self.hardCapMS * perMS),
            target + renderQuantum)
        if writeIdx - readIdx > cap {
            readIdx = writeIdx - cap
            depthAvg = Double(cap)
            overRun = 0
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
        let alpha = min(1.0, Double(count) / Double(Self.ewmaTauMS * perMS))
        depthAvg += (Double(available) - depthAvg) * alpha

        if !primed {
            if available >= target {
                primed = true
                emptyReads = 0
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
        hollow = depthAvg + Double(Self.deprimeDebtMS * perMS) < Double(target(lift: count))

        // Drift correction: shed exactly one frame, crossfaded, once the AVERAGE has sat above
        // the threshold for the sustain window. Anything shorter is jitter and must be left alone.
        if depthAvg > Double(target + Self.shedExcessMS * perMS) {
            overRun += count
            if overRun >= Self.shedSustainMS * perMS {
                overRun = 0
                shedOneFrame()
                shedCount += 1
                depthAvg = Double(writeIdx - readIdx)
            }
        } else {
            overRun = 0
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
        nearMiss = n == count && writeIdx - readIdx < Self.nearMissMarginMS * perMS
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
        if windowRun >= Self.growWindowMS * perMS {
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
                syncBackoffRun = syncBackoffLenMS * perMS
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
            underrunCount += 1
            if emptyReads >= Self.deprimeAfter || hollow {
                // The consecutive-empties hysteresis protects a FULL ring from one late packet.
                // A hollow ring is the opposite case: the target has been raised but the depth
                // never re-banked (growth is a promise; only a re-prime cashes it), and riding
                // that out is a click per bunching period, forever. The click just heard has
                // already paid for the refill — take it now.
                primed = false
                emptyReads = 0
            }
            if !restored {
                underrunsInWindow += 1
            }
            if underrunsInWindow >= Self.growUnderruns {
                underrunsInWindow = 0
                windowRun = 0
                targetLive = min(targetLive + Self.growStepMS * perMS, Self.maxTargetMS * perMS)
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
            if !nearMissGrown, !restored {
                nearMissGrown = true
                targetLive = min(targetLive + Self.growStepMS * perMS, Self.maxTargetMS * perMS)
            }
        } else {
            emptyReads = 0
            quietRun += count
            // Without a sync request, time is the only evidence that hard-won slack is no longer
            // needed, so a grown target waits out the long window. A request for less IS evidence,
            // and without this branch a ring that ratcheted to the ceiling during a transient would
            // hold audio a ceiling's worth late for minutes after the cause had gone. Every shrink
            // is armed as a PROBE — answered by an underrun or near-miss it is undone at once (see
            // above), and a failed sync-driven guess is not retried for a backoff.
            let syncShrink = syncWantsLess && syncBackoffRun == 0
            let quietNeeded = syncShrink ? Self.shrinkQuietSyncMS : Self.shrinkQuietMS
            if quietRun >= quietNeeded * perMS {
                quietRun = 0
                let prev = targetLive
                targetLive = max(targetLive - Self.growStepMS * perMS, Self.targetMS * perMS)
                if targetLive < prev {
                    probeRun = Self.shrinkProbeMS * perMS
                    probePrevTarget = prev
                }
            }
        }
    }

    /// Drop one protocol frame from the front, linearly crossfading the seam so the correction is
    /// inaudible rather than a click. Mirrors `punktfunk_core::audio::crossfade_drop`; caller holds
    /// the lock.
    private func shedOneFrame() {
        let drop = Self.frameMS * perMS
        let available = writeIdx - readIdx
        guard available > drop else { return }
        let fade = min(Self.crossfadeMS * perMS, min(drop, available - drop))
        let capacity = buf.count
        if fade > 0 {
            // The tail of what we discard fades out into the head of what survives.
            for i in 0..<fade {
                let old = buf[(readIdx + drop - fade + i) % capacity]
                let new = buf[(readIdx + drop + i) % capacity]
                let t = Float(i + 1) / Float(fade + 1)
                buf[(readIdx + drop + i) % capacity] = old * (1 - t) + new * t
            }
        }
        readIdx += drop
    }

    /// Current buffered depth in milliseconds — for the stats overlay and the drain thread's
    /// periodic log.
    var bufferedMS: Int {
        lock.lock()
        defer { lock.unlock() }
        return (writeIdx - readIdx) / max(perMS, 1)
    }

    /// One consistent snapshot of the ring's vitals, taken under a single lock so the numbers in
    /// a log line describe the same instant. Mirrors what the three Rust clients report.
    struct Stats {
        let bufferedMS: Int
        let targetMS: Int
        let underruns: Int
        let sheds: Int
        /// The A/V sync loop's smoothed offset (ms): **positive = audio playing BEHIND the
        /// picture**, negative = ahead of it. `0` before the loop has evidence, or with sync off.
        ///
        /// Reported next to the depth, never instead of it: a deep ring on a jittery link is
        /// CORRECT behaviour, and only the offset separates that from a ring holding audio late.
        let avOffsetMS: Int
    }

    var stats: Stats {
        lock.lock()
        defer { lock.unlock() }
        return Stats(
            bufferedMS: (writeIdx - readIdx) / max(perMS, 1),
            targetMS: target / max(perMS, 1),
            underruns: underrunCount,
            sheds: shedCount,
            avOffsetMS: avOffsetMS)
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
    private static let deadbandMS = 10
    /// Observations folded before the first correction is offered. The offset is derived from a
    /// clock skew estimate and a video figure that both need a moment to settle after connect;
    /// acting on the first sample would chase the handshake, not the stream.
    private static let minObservations = 100
    /// An offset larger than this is not believed. A wall-clock step, a paused host, or a stale
    /// video figure can all produce an enormous apparent misalignment, and steering the ring by it
    /// would empty or overfill it outright. Beyond this the loop reports and waits rather than acts.
    private static let saneLimitMS = 1_000
    /// The protocol's frame, in ms — the EWMA is weighted by it so the time constant means the
    /// same thing however often the caller observes.
    private static let frameMS = 5

    /// Interleaved samples per millisecond at the negotiated layout (48 × channels).
    private let perMS: Int
    /// EWMA of the measured offset in ns. Positive = audio is scheduled to play LATE relative to
    /// the picture it belongs with.
    private var offsetAvgNs: Double = 0
    private var observations = 0
    /// Set once an observation lands outside `saneLimitMS`, for reporting.
    private(set) var implausible = false

    /// `channels` is the negotiated interleaved channel count (2/6/8).
    init(channels: Int) {
        perMS = 48 * max(channels, 1)
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
        let bufferedNs = Int64(o.bufferedAhead / max(perMS, 1)) * 1_000_000
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
        let delta = Int(offsetMs * Double(perMS))
        return max(0, currentDepth - delta)
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
