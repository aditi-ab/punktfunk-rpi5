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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
            let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
    private func obs(offsetMS: Int, depth: Int) -> AvSync.Observation {
        let bufferedMS = depth / perMS
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
    private func settle(_ sync: inout AvSync, offsetMS: Int, depth: Int, count: Int = 100) {
        for _ in 0..<count { sync.observe(obs(offsetMS: offsetMS, depth: depth)) }
    }

    func testAvSyncNeedsEvidenceBeforeActing() {
        var s = AvSync(channels: channels)
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
        var s = AvSync(channels: channels)
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
        var s = AvSync(channels: channels)
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
        var s = AvSync(channels: channels)
        settle(&s, offsetMS: -30, depth: depth, count: 400)
        guard let want = s.desiredDepth(currentDepth: depth) else {
            return XCTFail("a 30 ms offset is actionable")
        }
        XCTAssertGreaterThan(want, depth, "audio early must aim deeper")
        XCTAssertEqual(s.offsetMS, -30)
    }

    func testAvSyncDeadbandsWhatNoOneCanHear() {
        let depth = 30 * perMS
        var s = AvSync(channels: channels)
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
        var s = AvSync(channels: channels)
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
        var s = AvSync(channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
        primeQuantum(ring, quantumMS: 5)
        XCTAssertEqual(ring.stats.targetMS, 20, "base target (JitterTuning.COREAUDIO)")

        // Audio 30 ms EARLY at a 20 ms depth ⇒ aim 50 ms deep: above the floor, under the 90 ms
        // cap, so the ring has no reason to refuse.
        var s = AvSync(channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels * 2, channels: channels)
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

        let slow = AudioRing(capacity: 48_000 * channels, channels: channels)
        grow(slow)
        slow.setSyncTarget(nil)
        let slowReads = quietToRelax(slow)

        let fast = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let a = AudioRing(capacity: 48_000 * channels, channels: channels)
        let b = AudioRing(capacity: 48_000 * channels, channels: channels)
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
        let ring = AudioRing(capacity: 48_000 * channels, channels: channels)
        XCTAssertEqual(ring.stats.avOffsetMS, 0, "no evidence yet reads as zero, not as noise")
        let feed = [Float](repeating: 0.5, count: 30 * perMS)
        feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: 30 * perMS) }

        var s = AvSync(channels: channels)
        settle(&s, offsetMS: 37, depth: 30 * perMS, count: 400)
        ring.noteAvOffset(s.offsetMS)
        let stats = ring.stats
        XCTAssertEqual(stats.bufferedMS, 30)
        XCTAssertEqual(stats.avOffsetMS, 37, "positive = audio behind the picture")
    }
}
#endif
