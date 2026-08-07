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

    /// The mirror case: a host clock running SLOW must keep audio flowing rather than being
    /// "corrected" into a stutter.
    func testNegativeDriftKeepsPlaying() {
        let (_, _, silent) = simulate(ms: 2 * 60 * 1_000, quantumMS: 5, driftPPM: -200)
        XCTAssertEqual(silent, 0, "a draining ring must re-prime, not chatter")
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

        // Drain it dry with one oversized read, then feed a normal quantum again. The length comes
        // off the buffer pointer, not off `huge`: touching the array inside the closure that is
        // already holding it exclusively is an exclusivity violation.
        var huge = [Float](repeating: 0, count: 200 * perMS)
        huge.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: $0.count) }
        let feed = [Float](repeating: 0.5, count: want)
        feed.withUnsafeBufferPointer { ring.write($0.baseAddress!, count: want) }
        scratch.withUnsafeMutableBufferPointer { ring.read(into: $0.baseAddress!, count: want) }
        XCTAssertTrue(
            scratch.contains { $0 != 0 },
            "a single short read must not force a full re-prime")
    }

    /// Mirror of the Rust `target_grows_on_underruns_and_relaxes_when_quiet`: clustered genuine
    /// underruns raise the target floor (that session needs the slack), a long quiet spell gives
    /// it back — and the floor never dips below the base.
    func testTargetGrowsOnUnderrunsAndRelaxesWhenQuiet() {
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
        XCTAssertEqual(ring.stats.targetMS, 20, "base target must match JitterTuning.COREAUDIO")

        // Prime, drain dry, then alternate starve/refill: each dry read is a genuine underrun,
        // each full read in between keeps the de-prime hysteresis from tripping.
        write(ms: 25)
        for _ in 0..<5 { read() } // drains to zero
        read() // short — underrun 1
        write(ms: 5); read() // full — hysteresis reset
        read() // short — underrun 2
        write(ms: 5); read() // full
        read() // short — underrun 3 → the floor grows one step
        XCTAssertEqual(ring.stats.targetMS, 30, "3 clustered underruns must grow the target 10 ms")
        XCTAssertEqual(ring.stats.underruns, 3)

        // A long clean run (30 s of consumed audio) relaxes the growth back to the base…
        for _ in 0..<(30_000 / 5 + 10) {
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
}
#endif
