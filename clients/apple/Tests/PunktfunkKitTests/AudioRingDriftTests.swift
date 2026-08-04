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
}
#endif
