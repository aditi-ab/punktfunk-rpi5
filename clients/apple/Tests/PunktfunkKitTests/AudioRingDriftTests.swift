// The Apple half of the shared de-jitter policy (`punktfunk_core::audio::JitterPolicy`, whose
// constants `AudioRing` mirrors). These pin the two behaviours a listener actually notices, in the
// one client where the policy is hand-written in a second language rather than shared as code — so
// a divergence from the Rust side shows up here rather than as a field report.
//
// The defect being pinned: the ring primed *up* to a target and clamped at a ceiling, with nothing
// walking the depth back *down*. Host-vs-DAC clock skew of a few dozen ppm therefore added latency
// permanently, and the only correction was a `highWater` shed that dropped `2 x prefill` at once —
// its own comment called that "one audible blip".

#if !os(tvOS)
import XCTest

@testable import PunktfunkKit

final class AudioRingDriftTests: XCTestCase {
    private let channels = 2
    private var perMS: Int { 48 * channels }

    /// Run `ms` of audio through the ring at a `quantumMS` device where the producer delivers
    /// `driftPPM` more than the consumer takes. Returns `(final ms, peak ms, silent callbacks)`.
    private func simulate(ms: Int, quantumMS: Int, driftPPM: Int) -> (Int, Int, Int) {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = quantumMS * perMS
        var scratch = [Float](repeating: 0, count: want)
        // Non-zero so a silent callback is distinguishable from real audio.
        let producer = [Float](repeating: 0.25, count: want + 8)
        var carry = 0, peak = 0, final = 0, silent = 0

        for i in 0..<(ms / quantumMS) {
            carry += want * driftPPM
            let extra = carry / 1_000_000
            carry -= extra * 1_000_000
            producer.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: want + extra) }

            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
            // Skip the priming window at the very start.
            if i > 20, scratch.allSatisfy({ $0 == 0 }) { silent += 1 }
            peak = max(peak, ring.bufferedMS)
            final = ring.bufferedMS
        }
        return (final, peak, silent)
    }

    /// THE regression: with the host clock running fast, buffered latency must return to target
    /// instead of climbing to the hard cap and staying pinned there. +200 ppm is deliberately
    /// harsher than real hardware (tens of ppm).
    func testDriftDoesNotRatchetLatencyToTheCeiling() {
        let (final, peak, silent) = simulate(ms: 5 * 60 * 1_000, quantumMS: 5, driftPPM: 200)
        // Must settle inside the headroom band (target 20 + headroom 30), never near the 90 ms cap.
        XCTAssertLessThanOrEqual(final, 50, "settled at \(final) ms — that is the ratchet")
        XCTAssertLessThanOrEqual(peak, 50, "peaked at \(peak) ms")
        XCTAssertEqual(silent, 0, "drift correction must never starve the callback")
    }

    /// The mirror case: a host clock running SLOW is a genuine deficit — no depth is ever deep
    /// enough forever — so the ring must spend it on RARE, clean re-banks (a hollow ring
    /// re-primes on its first click and refills the whole target) rather than riding the knife
    /// edge in permanent sub-frame chatter, which is what "silence-free" used to hide: every
    /// callback a fraction of a frame short, none of them fully silent, all of them audible.
    /// −200 ppm is an exaggeration of real DAC skew (tens of ppm); even so, two minutes may
    /// cost at most a couple of refills' worth of silent callbacks.
    func testNegativeDriftBanksRarelyInsteadOfChattering() {
        let (_, _, silent) = simulate(ms: 2 * 60 * 1_000, quantumMS: 5, driftPPM: -200)
        XCTAssertLessThanOrEqual(
            silent, 24,
            "a draining ring re-banks a few times; a silent-callback stream means it is thrashing")
        XCTAssertGreaterThan(
            silent, 0,
            "a persistent deficit cannot be ridden out silence-free — if this is zero the ring "
                + "is back to sub-frame chatter, which is audible without ever being silent")
    }

    /// A device that pulls a large quantum cannot sustain a target below it — the ring must lift
    /// its target rather than oscillating prime → dropout → re-prime forever.
    func testLargeDeviceQuantumStillPlays() {
        let (_, _, silent) = simulate(ms: 60 * 1_000, quantumMS: 40, driftPPM: 0)
        XCTAssertEqual(silent, 0, "a 40 ms quantum must not starve a 20 ms target")
    }

    /// One transient drain must not manufacture a whole target's worth of fresh silence: the ring
    /// de-primes only after a RUN of short reads.
    func testSingleShortReadDoesNotDeprime() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        // Prime well past target.
        let big = [Float](repeating: 0.5, count: 60 * perMS)
        big.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: big.count) }
        scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        XCTAssertTrue(scratch.contains { $0 != 0 }, "should be playing after priming")

        // Drain it dry at the device's own quantum — an oversized read would count as ITS OWN
        // huge callback and legitimately read as hollow — then starve one callback and feed a
        // normal quantum again. The ring is freshly primed, so its depth average is nowhere near
        // hollow, and one short read must ride on the hysteresis.
        while ring.bufferedMS > 0 {
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        }
        scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        let feed = [Float](repeating: 0.5, count: want)
        feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: want) }
        scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        XCTAssertTrue(
            scratch.contains { $0 != 0 },
            "a single short read must not force a full re-prime")
    }

    /// THE regression that made an iPad crackle where a Mac did not: the de-prime fuse must be the
    /// same SPAN OF TIME whatever the device's IO quantum. It used to be a callback COUNT (4), and
    /// a callback is not a unit of time — the same 4 was ~44 ms on a Mac's ~11 ms quantum and 20 ms
    /// on iOS, whose session asked for a 5 ms IO buffer. A Wi-Fi delivery stall therefore de-primed
    /// this ring on every bunching cycle where the identical policy rode it out elsewhere (measured
    /// on the shared Rust policy: 120 audible gaps per 10 min at a 5 ms quantum against 3 at 8 ms).
    /// Plant the defect by restoring a fixed count and the quanta below stop agreeing.
    ///
    /// Mirrors `deprime_fuse_is_a_duration_not_a_callback_count` in `punktfunk_core::audio`.
    func testDeprimeFuseIsADurationNotACallbackCount() {
        let deprimeMS = 60 // AudioRing.deprimeMS / JitterTuning::COREAUDIO.deprime_ms
        let quanta = [5, 8, 10, 16, 21]
        var deprimedAt: [Int: Int] = [:]
        for quantumMS in quanta {
            let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
            let want = quantumMS * perMS
            var scratch = [Float](repeating: 0, count: want)
            // Prime DEEP: the depth average is seeded with the refill, so `hollow` stays false for
            // the EWMA's whole settling second and the starvation fuse — not the hollow shortcut —
            // is what this measures.
            let big = [Float](repeating: 0.5, count: 80 * perMS)
            big.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: big.count) }
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
            XCTAssertTrue(
                scratch.contains { $0 != 0 }, "q=\(quantumMS)ms: must play after priming")

            // Starve on a trickle far under what the device takes: every read runs short but still
            // carries audio, so an all-zero read can only mean the ring gave up and re-primed.
            let trickle = [Float](repeating: 0.5, count: max(perMS, want / 4))
            var starvedMS = 0
            var deprimedAfterMS: Int?
            for _ in 0..<2_000 {
                trickle.withUnsafeBufferPointer {
                    ring.write($0.baseAddress!, count: trickle.count)
                }
                let short = ring.bufferedSamples < want
                scratch.withUnsafeMutableBufferPointer {
                    ring.read(into: $0.baseAddress!, count: want)
                }
                if scratch.allSatisfy({ $0 == 0 }) {
                    deprimedAfterMS = starvedMS
                    break
                }
                if short { starvedMS += quantumMS }
            }
            guard let deprimedAfterMS else {
                return XCTFail("q=\(quantumMS)ms: never de-primed at all")
            }
            deprimedAt[quantumMS] = deprimedAfterMS
        }

        // Each quantum must give up somewhere around the fuse. The band is wide on purpose: at a
        // short quantum the HOLLOW shortcut legitimately fires a little before the fuse does (the
        // target has grown, the depth was never re-banked, so the click is taken early and spent
        // on a full refill — see `deprimeDebtMS`), and that is the policy working, not drift.
        for (q, ms) in deprimedAt.sorted(by: { $0.key < $1.key }) {
            XCTAssertTrue(
                (deprimeMS - 20...deprimeMS + 25).contains(ms),
                "q=\(q)ms de-primed after \(ms) ms, nowhere near the \(deprimeMS) ms fuse — "
                    + "\(deprimedAt.sorted { $0.key < $1.key })")
        }
        // ...and THE property: the fuse must not SCALE with the quantum. As a callback count these
        // same devices de-primed after 20/32/40/64/84 ms — a 4.2x spread, which is exactly why an
        // iPad crackled where a Mac did not. Measured in time the spread collapses to ~1.3x.
        let spread = Double(deprimedAt.values.max()!) / Double(deprimedAt.values.min()!)
        XCTAssertLessThan(
            spread, 1.6,
            "de-prime time still scales with the IO quantum (\(String(format: "%.2f", spread))x "
                + "across \(deprimedAt.sorted { $0.key < $1.key })) — the fuse is a count again")
    }

    /// Mirror of the Rust `target_grows_on_underruns_and_relaxes_when_quiet`, updated for
    /// near-miss growth: the drain's LAST full read (less than a frame left over) already grows
    /// the floor before anything was audible, clustered genuine underruns raise it further, and
    /// a long — genuinely quiet — spell gives it back, never below the base. The quiet refill
    /// runs DEEP: a knife-edge refill (exactly what each read takes) leaves the ring within a
    /// frame of empty every callback, which now correctly reads as pressure, not quiet.
    func testTargetGrowsOnUnderrunsAndRelaxesWhenQuiet() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        let feed = [Float](repeating: 0.5, count: 60 * perMS)
        func write(ms: Int) {
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: ms * perMS) }
        }
        func read() {
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        }
        XCTAssertEqual(ring.stats.targetMS, 20, "base target must match JitterTuning.COREAUDIO")

        // Prime, then drain: the 5th read is still served in full but leaves nothing over — a
        // near-miss, and the floor grows BEFORE any click.
        write(ms: 25)
        for _ in 0..<5 { read() }
        XCTAssertEqual(ring.stats.targetMS, 30, "a near-miss must grow the floor pre-click")
        XCTAssertEqual(ring.stats.underruns, 0, "nothing was audible yet")

        // Then alternate starve/refill: each dry read is a genuine underrun, each full read in
        // between keeps the de-prime hysteresis from tripping. (The refills land as further
        // near-misses, but growth is one step per window — the cluster is what grows it again.)
        read() // short — underrun 1
        write(ms: 5); read() // full — hysteresis reset
        read() // short — underrun 2
        write(ms: 5); read() // full
        read() // short — underrun 3 → the floor grows one step
        XCTAssertEqual(ring.stats.targetMS, 40, "3 clustered underruns must grow the target 10 ms")
        XCTAssertEqual(ring.stats.underruns, 3)

        // A long clean run at a healthy depth relaxes the growth back to the base…
        write(ms: 60)
        for _ in 0..<(90_000 / 5 + 10) {
            write(ms: 5)
            read()
        }
        XCTAssertEqual(ring.stats.targetMS, 20, "a quiet spell must give the growth back")
        // …and stays there: quiet forever never dips below the base.
        for _ in 0..<(30_000 / 5 + 10) {
            write(ms: 5)
            read()
        }
        XCTAssertEqual(ring.stats.targetMS, 20, "the floor must never go below the base target")
    }

    /// Growth is capped at `maxTargetMS`, exactly like `JitterPolicy` respects
    /// `JitterTuning.max_target_ms`.
    func testTargetGrowthRespectsTheCap() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        let feed = [Float](repeating: 0.5, count: 25 * perMS)
        feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 25 * perMS) }
        // Starve it far past what six growth steps (20 → 70) would need.
        for _ in 0..<40 {
            for _ in 0..<5 {
                scratch.withUnsafeMutableBufferPointer {
                    ring.read(into: $0.baseAddress!, count: want)
                }
            }
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 25 * perMS) }
        }
        XCTAssertLessThanOrEqual(ring.stats.targetMS, 70, "growth must respect maxTargetMS")
    }

    /// THE field scenario: Wi-Fi power-save bunches arrivals — audio is produced steadily but
    /// delivered in bursts, some of them late. A fixed 20 ms target crackles on every late burst
    /// forever; the adaptive floor must deepen until the bunching rides through, and the tail of
    /// the session must be silence-free.
    func testWifiBunchingConvergesToSilenceFree() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        var pending = 0 // ms produced by the host but still "in flight"
        var burst = 0
        var silentTail = 0
        let steps = 4000 // 20 s in 5 ms callbacks
        let feed = [Float](repeating: 0.5, count: 200 * perMS)
        for step in 0..<steps {
            pending += 5 // the host encodes 5 ms per 5 ms of wall clock, stall or not
            // Delivery bunches into ~60 ms bursts; every 4th burst arrives a further 30 ms late.
            if step % 12 == 11 {
                if burst % 4 == 3 {
                    // Hold this burst 30 ms: it is flushed 6 callbacks later instead.
                    burst += 1
                } else {
                    feed.withUnsafeBufferPointer {
                        ring.write($0.baseAddress!, count: pending * perMS)
                    }
                    pending = 0
                    burst += 1
                }
            } else if step % 12 == 5, pending >= 60 {
                // The held burst lands, together with everything produced since.
                feed.withUnsafeBufferPointer {
                    ring.write($0.baseAddress!, count: pending * perMS)
                }
                pending = 0
            }
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
            if step >= steps - 600, scratch.allSatisfy({ $0 == 0 }) { silentTail += 1 }
        }
        XCTAssertGreaterThanOrEqual(
            ring.stats.targetMS, 30,
            "bunched delivery must have grown the target floor")
        XCTAssertEqual(
            silentTail, 0,
            "after adapting, the last 3 s must play through the bunching without a dropout")
    }

    // MARK: - A/V sync (audio latency overhaul, W6)
    //
    // The second half of the same story. Depth alone is not correctness: a ring can be exactly as
    // deep as its link needs and still put audio in the wrong place, because nothing ever compared
    // it to the picture. `AvSync` measures that comparison and asks the ring to move; the ring is
    // free to refuse. These pin both halves — that the loop DOES act (the previous pass in this
    // area shipped a correction that was structurally unreachable and had a green test), and that
    // it can never act far enough to starve the callback.

    /// Build an observation whose measured offset is exactly `offsetMS` (positive = audio late).
    /// Mirrors the Rust `obs` helper: pin now/skew/pts so the only free term is the buffered depth,
    /// then choose the video figure so the difference lands where we want it.
    ///
    /// `rateHz` must match the `AvSync` under test — the depth→ms conversion here has to be the
    /// same one the type does internally, or the observation asks for an offset it isn't building.
    private func obs(offsetMS: Int, depth: Int, rateHz: Int = 48_000) -> AvSync.Observation {
        let bufferedMS = depth / ((rateHz / 1000) * channels)
        let audioE2eMS = bufferedMS + 40 // 40 ms of transport, arbitrary but fixed
        let videoE2eMS = audioE2eMS - offsetMS
        return AvSync.Observation(
            ptsNs: 1_000_000_000,
            nowLocalNs: 1_000_000_000 + 40 * 1_000_000,
            clockOffsetNs: 0,
            bufferedAhead: depth,
            videoE2eNs: Int64(max(0, videoE2eMS)) * 1_000_000)
    }

    /// Fold `n` identical observations in.
    private func settle(
        _ sync: inout AvSync, offsetMS: Int, depth: Int, count: Int = 100, rateHz: Int = 48_000
    ) {
        for _ in 0..<count {
            sync.observe(obs(offsetMS: offsetMS, depth: depth, rateHz: rateHz))
        }
    }

    func testAvSyncNeedsEvidenceBeforeActing() {
        var s = AvSync(channels: channels, rateHz: 48_000)
        // One sample is never enough — the skew estimate and the video figure both settle after
        // connect, and acting on the first would chase the handshake, not the stream.
        XCTAssertNil(s.observe(obs(offsetMS: 50, depth: 30 * perMS)))
        XCTAssertFalse(s.settled)
        XCTAssertNil(s.desiredDepth(currentDepth: 30 * perMS))
        settle(&s, offsetMS: 50, depth: 30 * perMS, count: 99) // 1 + 99 = 100
        XCTAssertTrue(s.settled, "should act once the evidence is in")
    }

    /// No frame on the glass ⇒ no reference ⇒ the loop says nothing, however many observations
    /// arrive. This is the state every session starts in, and the one the stage-1 fallback
    /// presenter stays in for its whole life.
    func testAvSyncWithoutAVideoReferenceNeverActs() {
        var s = AvSync(channels: channels, rateHz: 48_000)
        for _ in 0..<500 {
            s.observe(AvSync.Observation(
                ptsNs: 1_000_000_000, nowLocalNs: 1_040_000_000, clockOffsetNs: 0,
                bufferedAhead: 30 * perMS, videoE2eNs: nil))
        }
        XCTAssertFalse(s.settled)
        XCTAssertNil(s.desiredDepth(currentDepth: 30 * perMS))
    }

    func testAvSyncAimsShallowerWhenAudioIsLate() {
        let depth = 60 * perMS
        var s = AvSync(channels: channels, rateHz: 48_000)
        settle(&s, offsetMS: 40, depth: depth, count: 400)
        guard let want = s.desiredDepth(currentDepth: depth) else {
            return XCTFail("a 40 ms offset is actionable")
        }
        XCTAssertLessThan(want, depth, "audio late must aim shallower")
        // The correction is the offset, not a guess at it.
        let shedMS = (depth - want) / perMS
        XCTAssertTrue((35...45).contains(shedMS), "should aim to shed ~40 ms, got \(shedMS)")
        XCTAssertEqual(s.offsetMS, 40, "and report it, sign and all")
    }

    func testAvSyncAimsDeeperWhenAudioIsEarly() {
        let depth = 20 * perMS
        var s = AvSync(channels: channels, rateHz: 48_000)
        settle(&s, offsetMS: -30, depth: depth, count: 400)
        guard let want = s.desiredDepth(currentDepth: depth) else {
            return XCTFail("a 30 ms offset is actionable")
        }
        XCTAssertGreaterThan(want, depth, "audio early must aim deeper")
        XCTAssertEqual(s.offsetMS, -30)
    }

    func testAvSyncDeadbandsWhatNoOneCanHear() {
        let depth = 30 * perMS
        var s = AvSync(channels: channels, rateHz: 48_000)
        settle(&s, offsetMS: 8, depth: depth, count: 400) // inside the 10 ms deadband
        XCTAssertNil(
            s.desiredDepth(currentDepth: depth),
            "an offset inside the deadband must not provoke a (real, if crossfaded) discontinuity")
        XCTAssertEqual(s.offsetMS, 8, "…but it is still REPORTED — the HUD shows the residual")
    }

    /// A wall-clock step or a stale video figure produces an enormous apparent misalignment.
    /// Clamping it would act on a wrong number as though it were a small real one, so it is
    /// refused outright and the running average is left untouched.
    func testAvSyncRejectsTheImplausibleInsteadOfClampingIt() {
        let depth = 30 * perMS
        var s = AvSync(channels: channels, rateHz: 48_000)
        settle(&s, offsetMS: 30, depth: depth, count: 400)
        let before = s.offsetMS
        // Built directly rather than through `obs`: that helper floors the video figure at zero,
        // which would cap the offset at a merely LARGE value and let this pass without ever
        // exercising the rejection.
        let wild = AvSync.Observation(
            ptsNs: 0, nowLocalNs: 5_000_000_000, clockOffsetNs: 0,
            bufferedAhead: depth, videoE2eNs: 40_000_000)
        XCTAssertNil(s.observe(wild))
        XCTAssertTrue(s.implausible, "a ~5 s offset must be refused, not folded")
        XCTAssertEqual(before, s.offsetMS, "an implausible sample must be discarded, not folded in")
    }

    /// The same refusal for arithmetic that cannot even be CARRIED OUT, which is why the terms are
    /// combined with overflow-reporting operators rather than the wrapping `&-` the latency meters
    /// use.
    ///
    /// This input is not arbitrary. `ptsNs = 1 << 63` reads as `Int64.min` in two's complement, so
    /// the audio leg overflows and the difference lands on EXACTLY `Int64.min` — and `abs()` of
    /// `Int64.min` has no representable result, so in Swift it traps. Check the overflow flags
    /// after the sanity limit instead of before and this observation does not mis-measure the
    /// stream, it aborts the process, from the audio drain thread. The guard's short-circuit
    /// ordering is what makes the sanity check itself safe to run.
    func testAvSyncRefusesAnOffsetItCannotEvenCompute() {
        var s = AvSync(channels: channels, rateHz: 48_000)
        let wild = AvSync.Observation(
            ptsNs: 1 << 63, nowLocalNs: 40_000_000, clockOffsetNs: 0,
            bufferedAhead: 0, videoE2eNs: 40_000_000)
        XCTAssertNil(s.observe(wild))
        XCTAssertTrue(s.implausible)
        XCTAssertFalse(s.settled, "a refused sample is not evidence")
        XCTAssertEqual(s.offsetMS, 0, "and nothing of it was folded in")
    }

    // MARK: - …and what the ring does with the proposal

    /// Drive one read so the ring knows the device quantum (`renderQuantum` seeds the floor).
    private func primeQuantum(_ ring: AudioRing, quantumMS: Int) {
        var scratch = [Float](repeating: 0, count: quantumMS * perMS)
        scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: $0.count) }
    }

    /// The loop is NOT inert: a settled proposal inside the ring's legal band actually moves the
    /// effective target. Without this the whole feature could ship as unreachable code with every
    /// other test still green — which is exactly how the previous drift correction shipped dead.
    func testSyncActuallyMovesTheTarget() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        primeQuantum(ring, quantumMS: 5)
        XCTAssertEqual(ring.stats.targetMS, 20, "base target (JitterTuning.COREAUDIO)")

        // Audio 30 ms EARLY at a 20 ms depth ⇒ aim 50 ms deep: above the floor, under the 90 ms
        // cap, so the ring has no reason to refuse.
        var s = AvSync(channels: channels, rateHz: 48_000)
        settle(&s, offsetMS: -30, depth: 20 * perMS, count: 400)
        ring.setSyncTarget(s.desiredDepth(currentDepth: 20 * perMS))
        XCTAssertEqual(ring.stats.targetMS, 50, "the ring must adopt a legal request")

        // And releasing it returns the ring to exactly where it was.
        ring.setSyncTarget(nil)
        XCTAssertEqual(ring.stats.targetMS, 20)
    }

    /// THE safety invariant: sync only ever proposes. Continuity — the underrun-driven floor —
    /// outranks it, or a lossy link would be "synced" into dropouts. Pinned against a GROWN floor,
    /// not just the base, because the floor sync is most likely to argue with is the one a bad link
    /// earned.
    func testSyncCanNeverStarveTheRing() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        let feed = [Float](repeating: 0.5, count: 25 * perMS)
        func write(ms: Int) {
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: ms * perMS) }
        }
        func read() {
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        }
        // Grow the floor above the base with three clustered genuine underruns (same shape as
        // testTargetGrowsOnUnderrunsAndRelaxesWhenQuiet).
        write(ms: 25)
        for _ in 0..<5 { read() }
        read()
        write(ms: 5); read()
        read()
        write(ms: 5); read()
        read()
        let floor = ring.stats.targetMS
        XCTAssertGreaterThan(floor, 20, "the test needs a GROWN floor to be meaningful")

        // Ask for an absurdly shallow ring — zero.
        ring.setSyncTarget(0)
        XCTAssertEqual(
            ring.stats.targetMS, floor,
            "sync pulled the target below the continuity floor — a link that needs the buffer must "
                + "keep it, and the residual gets reported instead")
        // One frame under the floor is still under the floor.
        ring.setSyncTarget(floor * perMS - perMS)
        XCTAssertEqual(ring.stats.targetMS, floor)
        // And it may not blow past the hard cap either — added latency stays bounded.
        ring.setSyncTarget(Int.max / 2)
        XCTAssertLessThanOrEqual(ring.stats.targetMS, 90, "sync pushed the target past the hard cap")
    }

    /// A device whose callback quantum alone exceeds the hard cap puts the continuity floor ABOVE
    /// the ceiling. The floor must win: clamping naively (`min(max(s, floor), cap)`) would hand
    /// back the cap — quietly below the floor, inverting the whole ordering — on exactly the
    /// awkward hardware this code exists to survive.
    func testAHugeDeviceQuantumDoesNotInvertTheClamp() {
        let ring = AudioRing(seconds: 2, channels: channels, rateHz: 48_000)
        let quantumMS = 500 // absurd, but not a reason to starve the callback
        primeQuantum(ring, quantumMS: quantumMS)
        ring.setSyncTarget(0)
        XCTAssertGreaterThanOrEqual(
            ring.stats.targetMS, quantumMS,
            "the target must still be able to serve one callback")
    }

    /// A ring that ratcheted during a transient must not hold audio late for minutes after the
    /// cause is gone: with sync asking for less, the relax window is the short one.
    func testSyncPressureRelaxesAGrownTargetSoonerThanTimeAlone() {
        let want = 5 * perMS

        func grow(_ ring: AudioRing) {
            var scratch = [Float](repeating: 0, count: want)
            let feed = [Float](repeating: 0.5, count: 25 * perMS)
            func write(ms: Int) {
                feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: ms * perMS) }
            }
            func read() {
                scratch.withUnsafeMutableBufferPointer {
                    ring.read(into: $0.baseAddress!, count: want)
                }
            }
            write(ms: 25)
            for _ in 0..<5 { read() }
            read()
            write(ms: 5); read()
            read()
            write(ms: 5); read()
            read()
        }
        /// Quiet (full) reads needed before the grown target relaxes one step. The ring is
        /// refilled DEEP first: a knife-edge refill (exactly what each read takes) leaves less
        /// than a frame over every callback, which now correctly reads as pressure — near-misses
        /// — and pressure never relaxes anything.
        func quietToRelax(_ ring: AudioRing) -> Int {
            var scratch = [Float](repeating: 0, count: want)
            let feed = [Float](repeating: 0.5, count: 60 * perMS)
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 60 * perMS) }
            let start = ring.stats.targetMS
            var reads = 0
            while ring.stats.targetMS == start, reads < 200_000 {
                feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 5 * perMS) }
                scratch.withUnsafeMutableBufferPointer {
                    ring.read(into: $0.baseAddress!, count: want)
                }
                reads += 1
            }
            return reads
        }

        let slow = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        grow(slow)
        slow.setSyncTarget(nil)
        let slowReads = quietToRelax(slow)

        let fast = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        grow(fast)
        fast.setSyncTarget(perMS) // strictly shallower than the grown target
        let fastReads = quietToRelax(fast)

        XCTAssertLessThan(
            fastReads, slowReads,
            "sync pressure should relax sooner: \(fastReads) vs \(slowReads) quiet reads")
    }

    /// A shrink answered by an underrun or near-miss inside its probe window is undone AT ONCE,
    /// and the sync loop is backed off — mirrors the Rust `a_failed_shrink_probe_is_undone_at_once`
    /// and `a_failed_probe_backs_the_sync_shrink_off`. Before this, the loop re-probed a proven
    /// depth every five quiet seconds and paid an audible starvation event each time it was wrong,
    /// forever — the 0.25.0 MacBook field report.
    func testAFailedShrinkProbeIsUndoneAtOnceAndBacksTheSyncLoopOff() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        let feed = [Float](repeating: 0.5, count: 60 * perMS)
        func write(ms: Int) {
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: ms * perMS) }
        }
        func read() {
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        }
        // Grow the floor (near-miss + a cluster of genuine underruns), as the usual pattern does.
        write(ms: 25)
        for _ in 0..<5 { read() }
        read()
        write(ms: 5); read()
        read()
        write(ms: 5); read()
        read()
        let grown = ring.stats.targetMS
        XCTAssertGreaterThan(grown, 20, "the test needs a GROWN floor to probe")

        // Sync asks for less; a deep, genuinely quiet spell later the shrink probes.
        ring.setSyncTarget(perMS)
        write(ms: 60)
        var reads = 0
        while ring.stats.targetMS == grown, reads < 10_000 {
            write(ms: 5)
            read()
            reads += 1
        }
        XCTAssertEqual(ring.stats.targetMS, grown - 10, "the sync-driven shrink must have probed")

        // Drain to the knife edge: the last full read leaves nothing over — a near-miss, nobody
        // heard anything — and the probe must be undone on the spot.
        while ring.bufferedMS > 5 { read() }
        read()
        XCTAssertEqual(
            ring.stats.targetMS, grown,
            "a failed probe must restore the target on the first near-miss")
        XCTAssertEqual(ring.stats.underruns, 3, "and nothing audible may have paid for it")

        // Backed off: two accelerated windows of clean, deep audio must NOT shrink again…
        write(ms: 60)
        for _ in 0..<(2 * 5_000 / 5) {
            write(ms: 5)
            read()
        }
        XCTAssertEqual(
            ring.stats.targetMS, grown,
            "the five-second cadence must be suspended after a failure")
        // …while the slow, pre-sync window eventually still tests one — backoff is not a freeze.
        for _ in 0..<(2 * 30_000 / 5) {
            write(ms: 5)
            read()
        }
        XCTAssertLessThan(
            ring.stats.targetMS, grown,
            "the slow window must still be allowed to test a shrink")
    }

    /// Growth raises a promise; only a re-prime banks real depth. An underrun while the ring is
    /// HOLLOW — its depth AVERAGE far below the target — re-primes immediately, spending the click
    /// it already cost on the whole refill, instead of riding the knife edge and clicking once per
    /// bunching period indefinitely. The average, not the instant, is what separates a hollow ring
    /// from one late packet (`testSingleShortReadDoesNotDeprime` pins that side).
    func testAHollowRingReprimesOnItsFirstClick() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let want = 5 * perMS
        var scratch = [Float](repeating: 0, count: want)
        let feed = [Float](repeating: 0.5, count: 60 * perMS)
        func write(ms: Int) {
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: ms * perMS) }
        }
        func read() {
            scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        }
        // Grow the floor to 40 the usual way…
        write(ms: 25)
        for _ in 0..<5 { read() }
        read()
        write(ms: 5); read()
        read()
        write(ms: 5); read()
        read()
        XCTAssertEqual(ring.stats.targetMS, 40)
        // …then ride the knife edge for ~2 s of audio, so the depth average genuinely sinks far
        // below the promised 40 ms.
        for _ in 0..<400 {
            write(ms: 5)
            read()
        }
        // One dry read — the click. The ring is hollow, so this single click must re-prime.
        read()
        // A packet arrives, but the ring stays SILENT: it is re-priming toward the full target
        // rather than playing the packet and clicking again at the next bunch.
        write(ms: 10)
        read()
        XCTAssertTrue(
            scratch.allSatisfy { $0 == 0 },
            "a hollow ring must spend its click on the whole refill, not keep limping")
        // And once the refill reaches the target, it plays again.
        write(ms: 40)
        read()
        XCTAssertTrue(scratch.contains { $0 != 0 }, "refilled to target — playback resumes")
    }

    /// The four client rings adopt sync one at a time; an un-wired one must behave exactly as it
    /// did. `nil` is the default, so this pins the initializer too — and every other test in this
    /// file runs without a sync target, which is the real guard that nothing moved underneath them.
    func testNoSyncTargetLeavesTheRingExactlyAsItWas() {
        let a = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        let b = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        b.setSyncTarget(nil)
        let want = 5 * perMS
        var sa = [Float](repeating: 0, count: want)
        var sb = [Float](repeating: 0, count: want)
        let feed = [Float](repeating: 0.5, count: 30 * perMS)
        for step in 0..<4_000 {
            // Uneven delivery so the depth actually moves around and the two rings have something
            // to disagree about.
            if step % 7 == 0 {
                for r in [a, b] {
                    feed.withUnsafeBufferPointer { r.write($0.baseAddress!, count: 30 * perMS) }
                }
            }
            sa.withUnsafeMutableBufferPointer { a.read(into: $0.baseAddress!, count: want) }
            sb.withUnsafeMutableBufferPointer { b.read(into: $0.baseAddress!, count: want) }
            XCTAssertEqual(sa, sb, "step \(step): an explicit nil diverged from the default")
            XCTAssertEqual(a.stats.targetMS, b.stats.targetMS, "step \(step)")
        }
    }

    /// The reporting half of §1.3: the offset must reach the same snapshot the depth does, because
    /// a depth on its own cannot distinguish "deep because the link needs it" from "deep and
    /// therefore late". This is the number the HUD and the 1 Hz log line read.
    func testAvOffsetIsReportedAlongsideTheDepth() {
        let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        XCTAssertEqual(ring.stats.avOffsetMS, 0, "no evidence yet reads as zero, not as noise")
        let feed = [Float](repeating: 0.5, count: 30 * perMS)
        feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 30 * perMS) }

        var s = AvSync(channels: channels, rateHz: 48_000)
        settle(&s, offsetMS: 37, depth: 30 * perMS, count: 400)
        ring.noteAvOffset(s.offsetMS)
        let stats = ring.stats
        XCTAssertEqual(stats.bufferedMS, 30)
        XCTAssertEqual(stats.avOffsetMS, 37, "positive = audio behind the picture")
    }

    // MARK: - Drought concealment (WP-C1)

    /// Concealment is for a ring that is running OUT. A drought a deep ring can cover is
    /// inaudible, and synthesizing over it would insert audio the late packets are about to
    /// duplicate — the stream would then run permanently later and the drift shed would have to
    /// cut it back out, audibly.
    ///
    /// Mirrors `a_drought_is_concealed_only_while_the_ring_is_running_out`.
    func testADroughtIsConcealedOnlyWhileTheRingIsRunningOut() {
        var c = DroughtConceal(maxMS: AudioRing.plcMaxMS)
        let stalledMS = 3 * AudioRing.frameMS
        XCTAssertFalse(
            c.conceal(sinceLastPacketMS: stalledMS, depthMS: 40),
            "a 40 ms ring covers this drought by itself")
        XCTAssertTrue(
            c.conceal(sinceLastPacketMS: stalledMS, depthMS: 0), "an empty ring does not")
        XCTAssertEqual(c.totalMS, AudioRing.frameMS)
    }

    /// Ordinary arrival jitter is not a drought — this policy must be invisible until the wire has
    /// genuinely stopped.
    ///
    /// Mirrors `ordinary_jitter_is_not_a_drought`.
    func testOrdinaryJitterIsNotADrought() {
        var c = DroughtConceal(maxMS: AudioRing.plcMaxMS)
        for _ in 0..<1_000 {
            XCTAssertFalse(c.conceal(sinceLastPacketMS: AudioRing.frameMS, depthMS: 0))
        }
        XCTAssertEqual(c.totalMS, 0)
    }

    /// The window is bounded, and bounded in TIME — the whole reason `deprimeMS` stopped being a
    /// callback count (`testDeprimeFuseIsADurationNotACallbackCount`). Derived from the fuse, so
    /// it cannot drift away from the thing it protects: an edit to one is an edit to both.
    ///
    /// Mirrors `drought_concealment_is_bounded_at_twice_the_deprime_fuse`.
    func testDroughtConcealmentIsBoundedAtTwiceTheDeprimeFuse() {
        let deprimeMS = 60 // AudioRing.deprimeMS / JitterTuning::COREAUDIO.deprime_ms
        XCTAssertEqual(AudioRing.plcMaxMS, 2 * deprimeMS)
        var c = DroughtConceal(maxMS: AudioRing.plcMaxMS)
        var ms = 0
        for _ in 0..<1_000 where c.conceal(sinceLastPacketMS: 2 * AudioRing.frameMS, depthMS: 0) {
            ms += AudioRing.frameMS
        }
        XCTAssertEqual(ms, AudioRing.plcMaxMS, "must use exactly the budget, and stop there")
        XCTAssertEqual(c.totalMS, AudioRing.plcMaxMS, "and report every millisecond of it")
    }

    /// A packet ends the drought and hands back a full budget for the next one — a link that
    /// stalls once a minute must be covered every time, not only the first.
    ///
    /// The other half of the Rust `concealment_already_paid_for_is_not_paid_for_twice` — that
    /// frames a drought already covered are subtracted from the loss concealment the seq path then
    /// asks for — cannot be tested from here: on this leg the gap tracker lives behind the C ABI,
    /// and so does the subtraction (`drought_concealment_is_not_charged_again_by_the_loss_path` in
    /// `punktfunk_core::abi`).
    func testAPacketEndsTheDroughtAndRefreshesTheBudget() {
        var c = DroughtConceal(maxMS: AudioRing.plcMaxMS)
        for _ in 0..<1_000 where c.conceal(sinceLastPacketMS: 2 * AudioRing.frameMS, depthMS: 0) {}
        XCTAssertEqual(c.totalMS, AudioRing.plcMaxMS, "budget spent")
        XCTAssertFalse(c.conceal(sinceLastPacketMS: 2 * AudioRing.frameMS, depthMS: 0))
        c.packet()
        XCTAssertTrue(
            c.conceal(sinceLastPacketMS: 2 * AudioRing.frameMS, depthMS: 0),
            "the next drought must start from a full budget")
        XCTAssertEqual(
            c.totalMS, AudioRing.plcMaxMS + AudioRing.frameMS,
            "the SESSION total keeps counting — it is what the log line reports")
    }

    /// THE field scenario this exists for, played against the real ring: the wire goes quiet for
    /// longer than the de-prime fuse (a Wi-Fi delivery stall, or a host whose capture stalled).
    /// Without concealment the ring drains, starves, and re-primes a whole target's worth of fresh
    /// silence — an artifact far longer than the audio that was missing. Fed one synthesized frame
    /// per drain tick instead, playback continues through the whole budget and nobody hears the
    /// stall at all.
    func testConcealmentRidesOutAStallThatWouldOtherwiseDeprime() {
        /// Prime, then stall the wire for `ms`, ticking the drain thread's 5 ms loop and the
        /// device callback in step. Returns when the first silent callback lands (nil = none).
        func stall(ms: Int, concealing: Bool) -> Int? {
            let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
            let want = 5 * perMS
            var scratch = [Float](repeating: 0, count: want)
            let feed = [Float](repeating: 0.5, count: 25 * perMS)
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 25 * perMS) }
            var drought = DroughtConceal(maxMS: AudioRing.plcMaxMS)
            for tick in 0..<(ms / AudioRing.frameMS) {
                if concealing,
                   drought.conceal(
                    sinceLastPacketMS: tick * AudioRing.frameMS, depthMS: ring.bufferedMS) {
                    feed.withUnsafeBufferPointer {
                        ring.write($0.baseAddress!, count: AudioRing.frameMS * perMS)
                    }
                }
                scratch.withUnsafeMutableBufferPointer {
                    ring.read(into: $0.baseAddress!, count: want)
                }
                if scratch.allSatisfy({ $0 == 0 }) { return tick * AudioRing.frameMS }
            }
            return nil
        }
        // The defect: 25 ms of ring, a 60 ms fuse — the stall is silent well inside the budget.
        guard let deprimedAt = stall(ms: AudioRing.plcMaxMS, concealing: false) else {
            return XCTFail("the unconcealed stall must still de-prime — the ring changed under us")
        }
        XCTAssertLessThan(deprimedAt, AudioRing.plcMaxMS)
        XCTAssertNil(
            stall(ms: AudioRing.plcMaxMS, concealing: true),
            "a stall inside the budget must not reach the listener at all (unconcealed: silent "
                + "after \(deprimedAt) ms)")
    }

    // MARK: - The negotiated rate

    /// The rate REACHES the arithmetic — every ms↔sample conversion in the ring, not just its
    /// capacity. Pinned because the failure mode is silent in both directions: a ring left at 48
    /// while the wire runs at 96 reports (and targets, and sheds at) double the milliseconds it
    /// really holds, and a capacity left as the old `48_000 * channels` literal is half a second of
    /// ring on the one plane that most needs the overflow headroom. Neither throws, warns, or
    /// sounds wrong until a link goes bad.
    func testRateDrivesEveryMsConversionAndTheCapacity() {
        let fast = AudioRing(seconds: 1, channels: channels, rateHz: 96_000)
        // The base target is a TIME (JitterTuning.COREAUDIO's 20 ms) and must read as one at any
        // rate — while costing twice the samples at 96 kHz, which is the whole point.
        XCTAssertEqual(fast.stats.targetMS, 20, "the target is denominated in ms, not samples")

        // 20 ms of 96 kHz audio is 1 920 frames; at 48 kHz the same sample count would read 40 ms.
        let ms20 = 20 * 96 * channels
        let feed = [Float](repeating: 0.5, count: ms20)
        feed.withUnsafeBufferPointer { fast.write($0.baseAddress!, count: ms20) }
        XCTAssertEqual(fast.bufferedMS, 20, "depth must be ms at the NEGOTIATED rate")

        // Capacity is a second of audio at the NEGOTIATED rate, not a second's worth of the old
        // `48_000 * channels` literal. Probed through `write`'s over-capacity guard, which drops a
        // too-large write whole rather than wrapping it: one second exactly must be taken, one
        // sample more must not. On a ring still sized from the 48 000 literal the first of these
        // would be the one silently dropped — which is the half-second-ring defect, expressed as
        // something a test can see.
        let empty = AudioRing(seconds: 1, channels: channels, rateHz: 96_000)
        let overflow = [Float](repeating: 0.5, count: 96_000 * channels + channels)
        overflow.withUnsafeBufferPointer { empty.write($0.baseAddress!, count: overflow.count) }
        XCTAssertEqual(empty.bufferedMS, 0, "an over-capacity write is dropped, not wrapped")
        overflow.withUnsafeBufferPointer {
            empty.write($0.baseAddress!, count: 96_000 * channels)
        }
        XCTAssertGreaterThan(
            empty.bufferedMS, 0,
            "one second of 96 kHz audio must fit — a ring sized from a 48 000 literal holds half, "
                + "and would have dropped this write entirely")

        // And the sync loop agrees about what a millisecond is: a 30 ms correction has to be 30 ms
        // of samples in the ring's own units, or the depth it proposes means something else.
        var s = AvSync(channels: channels, rateHz: 96_000)
        settle(&s, offsetMS: 30, depth: 40 * 96 * channels, count: 400, rateHz: 96_000)
        XCTAssertEqual(
            s.desiredDepth(currentDepth: 40 * 96 * channels), 10 * 96 * channels,
            "audio 30 ms late at a 40 ms depth ⇒ aim 10 ms, in 96 kHz samples")
    }

    // MARK: - The negotiated frame length

    /// The Swift half of core's `the_shed_follows_the_negotiated_frame_length`. Two of this ring's
    /// decisions are denominated in FRAMES, not milliseconds — the smooth shed drops exactly one,
    /// and the effective-target floor is a device quantum plus one — and both were written when
    /// 5 ms was the only frame the protocol had. The lossless plane negotiates 4 ms at 48 kHz/24-bit
    /// and 2 ms at 96 kHz/24-bit, so a ring left on the constant sheds two and a half frames at a
    /// time and fades across an entire one.
    func testFrameGeometryFollowsTheNegotiatedFrameLength() {
        // Default: one 5 ms frame, a 2 ms fade — exactly the pre-hi-res numbers, which is what
        // keeps every Opus session (and the twenty-nine tests above) bit-identical.
        let base = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        XCTAssertEqual(base.frameGeometry.frame, AudioRing.frameMS * perMS)
        XCTAssertEqual(base.frameGeometry.crossfade, 2 * perMS)

        // A 2 ms lossless frame sheds 2 ms, and the fade is capped at HALF of it rather than
        // consuming the whole dropped frame — a fade as long as the material it fades is not a
        // crossfade, it is a ramp replacing the seam.
        let short = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        short.setFrameUs(2_000)
        XCTAssertEqual(short.frameGeometry.frame, 2 * perMS)
        XCTAssertEqual(short.frameGeometry.crossfade, perMS, "fade must be half a 2 ms frame")
        XCTAssertLessThan(
            short.frameGeometry.crossfade, short.frameGeometry.frame,
            "a fade as long as the frame is not a crossfade")

        // Sub-millisecond precision: 2 500 µs at 48 kHz stereo is 240 interleaved samples, and must
        // not truncate to 192 by going through integer milliseconds on the way. This is the whole
        // reason the accessor is denominated in µs.
        let half = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        half.setFrameUs(2_500)
        XCTAssertEqual(half.frameGeometry.frame, 240, "2 500 µs must not truncate to 2 ms")

        // At 96 kHz the same 2 ms frame is twice the samples for the same duration.
        let hires = AudioRing(seconds: 1, channels: channels, rateHz: 96_000)
        hires.setFrameUs(2_000)
        XCTAssertEqual(hires.frameGeometry.frame, 2 * 96 * channels)

        // A degenerate value must not produce a zero-length frame — the shed would become an
        // infinite no-op and the target floor would lose its packet of slack.
        let zero = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
        zero.setFrameUs(0)
        XCTAssertGreaterThanOrEqual(zero.frameGeometry.frame, 1)
    }

    /// The frame reaches the EFFECTIVE TARGET FLOOR, not just a getter. A large-quantum device
    /// cannot sustain a target below its own callback, so the floor is `quantum + one frame` — and
    /// on a 2 ms session that packet of slack should be 2 ms, not the 5 a constant would give.
    func testTargetFloorCarriesOneNegotiatedFrameOverTheDeviceQuantum() {
        func floorMS(frameUs: Int?) -> Int {
            let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
            if let frameUs { ring.setFrameUs(frameUs) }
            // One oversized callback is all it takes: `renderQuantum` is a high-water mark, and
            // 30 ms exceeds the 20 ms base target so the lift is what decides the floor.
            var scratch = [Float](repeating: 0, count: 30 * perMS)
            scratch.withUnsafeMutableBufferPointer {
                ring.read(into: $0.baseAddress!, count: $0.count)
            }
            return ring.stats.targetMS
        }
        XCTAssertEqual(floorMS(frameUs: nil), 35, "30 ms quantum + the default 5 ms frame")
        XCTAssertEqual(floorMS(frameUs: 2_000), 32, "30 ms quantum + a 2 ms lossless frame")
        XCTAssertEqual(floorMS(frameUs: 4_000), 34, "30 ms quantum + a 4 ms lossless frame")
    }

    /// The half-frame cap reaches the SAMPLES. Driven through the hard-cap trim rather than the
    /// slow drift shed because they share `dropFront`, and the trim is the drop that actually fires
    /// in the field (a bunching link trims far more often than it sheds).
    ///
    /// The ring is filled with silence where the trim will cut and full scale after it, so every
    /// blended sample is strictly below full scale and the fade length is simply countable.
    func testTheSeamCrossfadeIsCappedAtHalfTheNegotiatedFrame() {
        func fadeLength(frameUs: Int?) -> Int {
            let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
            if let frameUs { ring.setFrameUs(frameUs) }
            // A fresh ring's cap is target(20) + headroom(30) = 50 ms, so 60 ms of audio trims
            // exactly 10 ms off the front — comfortably more than any fade under test.
            var feed = [Float](repeating: 1, count: 60 * perMS)
            for i in 0..<(10 * perMS) { feed[i] = 0 }
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: feed.count) }

            // Read one 5 ms callback: small enough to stay primed (the floor needs quantum + a
            // frame ≤ the 50 ms banked), large enough to contain any fade under test.
            var out = [Float](repeating: -1, count: 5 * perMS)
            out.withUnsafeMutableBufferPointer {
                ring.read(into: $0.baseAddress!, count: $0.count)
            }
            return out.prefix { $0 < 1 }.count
        }
        // Default: the flat 2 ms fade, well under half of a 5 ms frame.
        XCTAssertEqual(fadeLength(frameUs: nil), 2 * perMS)
        // A 2 ms frame caps the fade at 1 ms — without the cap it would be the whole frame.
        XCTAssertEqual(fadeLength(frameUs: 2_000), perMS, "half of a 2 ms frame")
        // 4 ms leaves the flat 2 ms fade untouched: half of 4 is exactly 2, so the cap binds
        // without shortening it — the boundary worth pinning.
        XCTAssertEqual(fadeLength(frameUs: 4_000), 2 * perMS)
    }

    /// The NEAR-MISS margin follows the frame too — a read that leaves more than one frame in hand
    /// is not a near miss and must not grow the target.
    ///
    /// ⚠ This is the one place the Swift ring deliberately DIVERGES from core, which still measures
    /// the margin against the constant `NEAR_MISS_MARGIN_MS`. The two agree exactly on every Opus
    /// session (both mean 5 ms) and part company only on the lossless plane, where a margin frozen
    /// at 5 ms against a 2 ms frame stops meaning "one packet in hand" and starts meaning "two and
    /// a half" — growing the target on a ring that was never close to starving, which is the
    /// opposite of what the near-miss exists to detect. Pinned here so the divergence stays a
    /// decision rather than becoming drift.
    func testNearMissMarginFollowsTheNegotiatedFrame() {
        func targetAfterLeaving(_ leftover: Int, frameUs: Int?) -> Int {
            let ring = AudioRing(seconds: 1, channels: channels, rateHz: 48_000)
            if let frameUs { ring.setFrameUs(frameUs) }
            // Bank 25 ms — over the 20 ms base target, under the 50 ms hard cap, so nothing trims.
            let feed = [Float](repeating: 0.5, count: 25 * perMS)
            feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: feed.count) }
            // A 5 ms callback primes the ring and leaves 20 ms — nowhere near any margin.
            var prime = [Float](repeating: 0, count: 5 * perMS)
            prime.withUnsafeMutableBufferPointer {
                ring.read(into: $0.baseAddress!, count: $0.count)
            }
            // Then serve one FULL read that leaves exactly `leftover` samples in hand.
            var out = [Float](repeating: 0, count: 20 * perMS - leftover)
            out.withUnsafeMutableBufferPointer {
                ring.read(into: $0.baseAddress!, count: $0.count)
            }
            return ring.stats.targetMS
        }
        // 300 samples ≈ 3.1 ms: MORE than a 2 ms frame, LESS than the 5 ms constant. With the
        // margin tied to the frame this is an ordinary read; tied to the constant it is a near
        // miss and buys a 10 ms growth step.
        XCTAssertEqual(
            targetAfterLeaving(300, frameUs: 2_000), 20,
            "3.1 ms left over is more than a 2 ms frame — not a near miss, no growth")
        // The same read against the DEFAULT 5 ms frame genuinely is a near miss, which is what
        // keeps this test honest: it is not simply asserting that growth never happens.
        XCTAssertEqual(
            targetAfterLeaving(300, frameUs: nil), 30,
            "3.1 ms left over IS inside a 5 ms frame — one growth step")
    }
}
#endif
