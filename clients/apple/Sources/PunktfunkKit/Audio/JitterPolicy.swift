// The playback de-jitter policy: the Swift twin of `punktfunk_core::audio::JitterPolicy` under
// `JitterTuning::COREAUDIO`. `clients/shared/jitter-vectors.json` holds the two together: core
// writes it and `JitterVectorTests` replays it here, so a change lands on both sides or fails.
//
// Pure arithmetic over interleaved sample counts, driven by audio consumed rather than wall clock.
// `AudioRing` owns the samples and applies what `step` returns. The depth average is a `Float`,
// like core's `f32`, so every threshold it crosses is crossed on the same callback.

/// Priming, drift correction, adaptive depth and A/V-sync steering for one playback ring.
///
/// **Priming.** Silence until the target is banked, lifted to one callback plus one frame so a
/// large-buffer device does not oscillate prime → dropout → re-prime.
///
/// **Drift.** A depth average above target for a sustained window sheds one crossfaded frame.
/// The headroom line trims only a sustained excess of that average; only the hard cap trims on
/// sight, so a delivery clump the ring drains before the next one is never cut.
///
/// **Adaptive depth.** Near-misses and repeated underruns grow the target a step up to
/// `maxTargetMS`; a quiet spell relaxes it, and every shrink is a probe undone on its first
/// underrun. A hollow ring (average far below target) re-primes on its first underrun.
///
/// **A/V sync.** `setSyncTarget` steers the depth, clamped between the adaptive floor and the
/// hard cap: continuity outranks sync.
struct JitterPolicy {
    /// What one callback should do (core's `JitterStep`).
    struct Step: Equatable {
        var dropFront = 0
        /// Never set in the same step as `dropFront`.
        var insertFront = 0
        /// Crossfade across the drop or insert seam.
        var crossfade = 0
        /// `dropFront` is the cap trim, not the smooth shed.
        var hardTrim = false
        /// Play silence: priming, or re-priming after a sustained drain.
        var silence = false
    }

    // `JitterTuning::COREAUDIO`, in ms.
    static let baseTargetMS = 20
    static let maxTargetMS = 70
    static let headroomMS = 30
    static let hardCapMS = 180
    static let deprimeMS = 60
    /// Longest drought `DroughtConceal` covers: twice the de-prime fuse (`plc_max_ms`).
    static let plcMaxMS = deprimeMS * 2
    /// The protocol's default frame (`FRAME_MS`); `setFrameUs` takes the negotiated one.
    static let frameMS = 5
    /// Middle of the headroom band, never under two frames (`shed_excess_ms`).
    static let shedExcessMS = max(headroomMS / 2, 2 * frameMS)

    private static let ewmaTauMS = 1_000
    private static let shedSustainMS = 2_000
    private static let insertSustainMS = shedSustainMS
    /// Half the A/V deadband, so every request the sync loop may make can be answered.
    private static let insertMarginMS = AvSync.deadbandMS / 2
    private static let shedCrossfadeMS = 2
    private static let growUnderruns = 3
    private static let growWindowMS = 5_000
    private static let growStepMS = 10
    private static let shrinkQuietMS = 30_000
    private static let shrinkQuietSyncMS = 5_000
    private static let shrinkProbeMS = 5_000
    private static let deprimeDebtMS = growStepMS
    private static let minDeprimeCallbacks = 2
    private static let syncBackoffMS = 60_000
    private static let syncBackoffMaxMS = 480_000

    private let rateHz: Int
    private let channels: Int
    private var frameUs = frameMS * 1_000
    /// The live target, grown by underrun pressure, never below the base.
    private var target: Int
    private(set) var isPrimed = false
    /// Consecutive short reads and the audio they starved; both gate the de-prime.
    private var empties = 0
    private var emptiesRun = 0
    private var depthAvg: Float = 0
    private var overRun = 0
    private var underRun = 0
    private var underruns = 0
    private var windowRun = 0
    private var quietRun = 0
    /// `want` from the last `step`, for `noteRead`'s sample-denominated timers.
    private var lastWant = 0
    private var syncTarget: Int?
    private var nearMiss = false
    private var nearMissGrown = false
    private var hollow = false
    private var probeRun = 0
    private var probePrevTarget = 0
    private var syncBackoffRun = 0
    private var syncBackoffLenMS = syncBackoffMS

    /// Zero rates and channel counts clamp to 1: this is built from wire values on a path that
    /// must not fault in a render callback.
    init(channels: Int, rateHz: Int) {
        self.rateHz = max(rateHz, 1)
        self.channels = max(channels, 1)
        target = audioMsToSamples(rateHz: self.rateHz, channels: self.channels, ms: Self.baseTargetMS)
    }

    /// The negotiated frame length, µs; clamped to ≥ 1 so a frame is never zero samples.
    mutating func setFrameUs(_ us: Int) {
        frameUs = max(us, 1)
    }

    /// The depth `AvSync` wants, or nil to run unsynchronised. A request: clamped between the
    /// adaptive floor and the hard cap.
    mutating func setSyncTarget(_ samples: Int?) {
        syncTarget = samples
    }

    var targetMS: Int { samplesMs(target) }
    /// The depth aimed for at the last callback's size, sync request and quantum lift included.
    var effectiveTargetMS: Int { samplesMs(effectiveTarget(want: lastWant)) }
    var avgDepthMS: Int { samplesMs(Int(max(depthAvg, 0))) }

    /// One frame in interleaved samples: the wire's frame, floored per channel.
    var frameSamples: Int {
        max(audioSamplesPerFrame(rateHz: rateHz, frameUs: frameUs, channels: channels), 1)
    }

    /// The seam crossfade, capped at half a frame.
    var crossfadeSamples: Int { min(msSamples(Self.shedCrossfadeMS), frameSamples / 2) }

    private func msSamples(_ ms: Int) -> Int {
        audioMsToSamples(rateHz: rateHz, channels: channels, ms: ms)
    }

    private func samplesMs(_ samples: Int) -> Int {
        audioSamplesToMs(rateHz: rateHz, channels: channels, samples: samples)
    }

    private var syncWantsLess: Bool { syncTarget.map { $0 < target } ?? false }
    private var syncWantsMore: Bool { syncTarget.map { $0 > target } ?? false }

    /// The live target lifted to serve one callback plus one frame.
    private func adaptiveTarget(want: Int) -> Int { max(target, want + frameSamples) }

    /// The sync request clamped into `[adaptive, hardCap]`. The ceiling is raised to the floor
    /// first: a callback past the hard cap must not invert the clamp.
    private func effectiveTarget(want: Int) -> Int {
        let floor = adaptiveTarget(want: want)
        guard let s = syncTarget else { return floor }
        let cap = max(msSamples(Self.hardCapMS), floor)
        return min(max(s, floor), cap)
    }

    /// Decide this callback, before reading: `depth` is what the ring holds, `want` what the
    /// device asks for, both in interleaved samples.
    mutating func step(depth: Int, want: Int) -> Step {
        lastWant = want
        let target = effectiveTarget(want: want)
        // Weighted by `want`, so the time constant holds whatever the device quantum.
        let alpha = min(max(Float(want) / Float(msSamples(Self.ewmaTauMS)), 0), 1)
        depthAvg += (Float(depth) - depthAvg) * alpha

        // Both caps leave room for this callback, or a large quantum trims into an underrun.
        let cap = max(min(target + msSamples(Self.headroomMS), msSamples(Self.hardCapMS)), target + want)
        let hard = max(msSamples(Self.hardCapMS), cap)

        var out = Step()
        // The headroom line is judged on the average; only the hard cap trims on sight.
        let line: Int?
        if depth > hard {
            line = hard
        } else if depth > cap, depthAvg > Float(cap) {
            line = cap
        } else {
            line = nil
        }
        if let line {
            // Whole frames, restarting the drift clock from what is left.
            let keep = line - line % channels
            out.dropFront = depth - keep
            out.hardTrim = true
            out.crossfade = min(crossfadeSamples, keep)
            depthAvg = Float(keep)
            overRun = 0
            underRun = 0
        } else if depthAvg > Float(target + msSamples(Self.shedExcessMS)) {
            overRun += want
            underRun = 0
            if overRun >= msSamples(Self.shedSustainMS) {
                out.dropFront = min(frameSamples, depth)
                out.crossfade = min(crossfadeSamples, max(depth - out.dropFront, 0))
                overRun = 0
            }
        } else if isPrimed, syncWantsMore,
            Int(depthAvg) + msSamples(Self.insertMarginMS) < target
        {
            // The shed's mirror: sync asked deeper and the average sat below it long enough.
            overRun = 0
            underRun += want
            if underRun >= msSamples(Self.insertSustainMS), depth >= frameSamples {
                out.insertFront = frameSamples
                out.crossfade = crossfadeSamples
                underRun = 0
            }
        } else {
            overRun = 0
            underRun = 0
        }
        if !out.hardTrim {
            depthAvg = max(depthAvg - Float(out.dropFront) + Float(out.insertFront), 0)
        }

        let kept = max(depth - out.dropFront, 0)
        if !isPrimed, kept >= target {
            isPrimed = true
            empties = 0
            emptiesRun = 0
            // Seeded with the refill, or the first late packet re-primes a full ring.
            depthAvg = Float(kept)
        }
        out.silence = !isPrimed
        let after = kept + out.insertFront
        // Served, with less than one frame left.
        nearMiss = isPrimed && after >= want && after - want < frameSamples
        // A debt against the adaptive target, never the sync-inflated one.
        hollow = isPrimed
            && Int(depthAvg) + msSamples(Self.deprimeDebtMS) < adaptiveTarget(want: want)
        return out
    }

    /// The outcome of the read `step` allowed. `ranShort` is a genuine underrun; an unprimed
    /// read is priming silence and changes nothing.
    mutating func noteRead(ranShort: Bool) {
        guard isPrimed else { return }
        let want = max(lastWant, 1)
        let nearMiss = self.nearMiss
        self.nearMiss = false
        windowRun += want
        if windowRun >= msSamples(Self.growWindowMS) {
            windowRun = 0
            underruns = 0
            nearMissGrown = false
        }
        syncBackoffRun = max(syncBackoffRun - want, 0)
        var restored = false
        if probeRun > 0 {
            probeRun = max(probeRun - want, 0)
            if ranShort || nearMiss {
                // The shrink was wrong: restore the proven depth and back the sync loop off,
                // doubling per failure. Consumed as growth evidence.
                probeRun = 0
                target = max(target, probePrevTarget)
                syncBackoffRun = msSamples(syncBackoffLenMS)
                syncBackoffLenMS = min(syncBackoffLenMS * 2, Self.syncBackoffMaxMS)
                restored = true
            } else if probeRun == 0 {
                syncBackoffLenMS = Self.syncBackoffMS
            }
        }
        if ranShort {
            quietRun = 0
            empties += 1
            emptiesRun += want
            let starved = emptiesRun >= msSamples(Self.deprimeMS)
                && empties >= Self.minDeprimeCallbacks
            if starved || hollow {
                isPrimed = false
                empties = 0
                emptiesRun = 0
            }
            if !restored {
                underruns += 1
            }
            if underruns >= Self.growUnderruns {
                underruns = 0
                windowRun = 0
                target = min(target + msSamples(Self.growStepMS), msSamples(Self.maxTargetMS))
            }
        } else if nearMiss {
            // The same evidence as an underrun, heard by no one: grow once per window.
            quietRun = 0
            empties = 0
            emptiesRun = 0
            if !nearMissGrown, !restored {
                nearMissGrown = true
                target = min(target + msSamples(Self.growStepMS), msSamples(Self.maxTargetMS))
            }
        } else {
            empties = 0
            emptiesRun = 0
            quietRun += want
            // A sync request for less is evidence the depth costs alignment: test sooner.
            let syncShrink = syncWantsLess && syncBackoffRun == 0
            let quietNeeded = syncShrink ? Self.shrinkQuietSyncMS : Self.shrinkQuietMS
            if quietRun >= msSamples(quietNeeded) {
                quietRun = 0
                let prev = target
                target = max(target - msSamples(Self.growStepMS), msSamples(Self.baseTargetMS))
                if target < prev {
                    probeRun = msSamples(Self.shrinkProbeMS)
                    probePrevTarget = prev
                }
            }
        }
    }
}
