import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// Source-timestamp playout: the Swift `CadenceClock` against the SAME synthetic inputs its Rust
/// original runs (`punktfunk_core::phase::tests`) — one test per Rust test, matching names, the
/// same constants, the same deterministic LCG. The port is hand-written because this pipeline does
/// not link the Rust type, so agreement on these vectors is the whole lockstep contract: a
/// constant or a rounding rule that drifts on either side fails here, not on a user's screen.
final class CadenceClockTests: XCTestCase {
    /// 120 Hz in ns.
    private static let p: Int64 = 8_333_333

    /// A source stamping realtime, played out by a client whose present clock is monotonic and
    /// therefore a whole different era. The loop must never need to be told about this.
    private static let pts0: UInt64 = 1_786_000_000_000_000_000
    private static let domain: Int64 = -1_785_000_000_000_000_000
    /// Transport + decode: what `ready − pts` sits at once the domain is taken out.
    private static let delay: Int64 = 12_000_000

    /// Deterministic LCG in ±spread around zero — no OS randomness in tests. The multiplier,
    /// increment and the `>> 33` fold are the Rust harness's, so both sides replay the identical
    /// jitter sequence for a given seed.
    private struct Lcg {
        private var state: UInt64
        init(_ seed: UInt64) { state = seed }
        mutating func noise(_ spreadNs: Int64) -> Int64 {
            state = state &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
            guard spreadNs != 0 else { return 0 }
            return Int64(state >> 33) % (2 * spreadNs) - spreadNs
        }
    }

    private static func ptsAt(_ k: Int64) -> UInt64 {
        UInt64(bitPattern: Int64(pts0) + k * p)
    }

    /// Run `n` frames of a well-behaved 120 Hz source and hand back the clock.
    private static func settled(_ n: Int64, spread: Int64) -> CadenceClock {
        let c = CadenceClock(tuning: .snapping())
        var rng = Lcg(7)
        for k in 0..<n {
            let ready = Int64(bitPattern: ptsAt(k)) + domain + delay + rng.noise(spread)
            _ = c.dueNs(srcPtsNs: ptsAt(k), readyNs: ready, frameIntervalNs: p)
        }
        return c
    }

    func testSettlesFromCold() {
        let c = Self.settled(400, spread: 1_000_000)
        let err = c.health().offsetNs - (Self.domain + Self.delay)
        XCTAssertLessThan(
            abs(err), 500_000,
            "offset must converge on the true transport delay, off by \(err) ns")
        XCTAssertEqual(c.health().reanchors, 1, "only the cold start anchors")
    }

    /// The type-2 property, and the reason the loop carries a rate term at all: two free-running
    /// crystals produce a RAMP, and a proportional-only loop lags a ramp forever. Asserted against
    /// its own type-1 twin so the difference is the measurement, not a threshold anyone chose.
    func testTracksAClockRamp() {
        let ramp: Int64 = 400 // ns per frame ≈ 48 ppm, an ordinary crystal pair
        func run(_ tuning: CadenceTuning) -> Int64 {
            let c = CadenceClock(tuning: tuning)
            var lastErr: Int64 = 0
            for k in Int64(0)..<4_000 {
                let pts = Int64(bitPattern: Self.ptsAt(k))
                let ready = pts + Self.domain + Self.delay + k * ramp
                _ = c.dueNs(srcPtsNs: Self.ptsAt(k), readyNs: ready, frameIntervalNs: Self.p)
                lastErr = (ready - pts) - c.health().offsetNs
            }
            return abs(lastErr)
        }
        let type2 = run(.snapping())
        // The same loop with its integral gain switched off: a shift this large truncates every
        // residual to zero, which is exactly "proportional only".
        var type1Tuning = CadenceTuning.snapping()
        type1Tuning.skewShift = 63
        let type1 = run(type1Tuning)
        XCTAssertLessThan(
            type2 * 4, type1,
            "a rate term must beat proportional-only on a ramp: \(type2) ns vs \(type1) ns")
        XCTAssertLessThan(type2, 3_000, "steady-state ramp error \(type2) ns")
    }

    func testRejectsASingleOutlier() {
        let c = Self.settled(400, spread: 200_000)
        let before = c.health().offsetNs
        // One frame arrives half a second late — a stall, not a new operating point.
        let k: Int64 = 400
        _ = c.dueNs(
            srcPtsNs: Self.ptsAt(k),
            readyNs: Int64(bitPattern: Self.ptsAt(k)) + Self.domain + Self.delay + 500_000_000,
            frameIntervalNs: Self.p)
        let moved = abs(c.health().offsetNs - before)
        // The clamped correction, plus the one frame of rate the loop advances by regardless —
        // that advance is the estimate doing its job, not the outlier moving it.
        let t = CadenceTuning.snapping()
        let bound = (t.errorClampNs >> t.offsetShift) + abs(c.health().skewNs)
        XCTAssertLessThanOrEqual(
            moved, bound, "one outlier moved the estimate \(moved) ns, past the clamp's \(bound)")
    }

    func testReanchorsOnAGap() {
        let c = Self.settled(400, spread: 200_000)
        let anchors = c.health().reanchors
        // The stream was paused for two seconds; the estimate cannot have tracked across that.
        let far = Self.ptsAt(400) + 2_000_000_000
        let ready = Int64(bitPattern: far) + Self.domain + Self.delay + 4_000_000
        _ = c.dueNs(srcPtsNs: far, readyNs: ready, frameIntervalNs: Self.p)
        XCTAssertEqual(c.health().reanchors, anchors + 1)
        XCTAssertEqual(
            c.health().offsetNs, ready - Int64(bitPattern: far),
            "a re-anchor adopts the new sample outright rather than slewing to it")
    }

    func testReanchorsOnRegression() {
        let c = Self.settled(400, spread: 200_000)
        let anchors = c.health().reanchors
        let back = Self.ptsAt(200) // source timestamps went backwards
        _ = c.dueNs(
            srcPtsNs: back, readyNs: Int64(bitPattern: back) + Self.domain + Self.delay,
            frameIntervalNs: Self.p)
        XCTAssertEqual(c.health().reanchors, anchors + 1)
    }

    /// A due time in the past is returned AS IS. Clamping it to `readyNs` would quietly turn every
    /// late frame into a fresh anchor, which is how an arrival-driven presenter behaves — the
    /// thing this clock exists to stop being.
    func testLateFrameReturnsPastDue() {
        let c = Self.settled(400, spread: 200_000)
        let k: Int64 = 400
        let ready = Int64(bitPattern: Self.ptsAt(k)) + Self.domain + Self.delay + 30_000_000
        let due = c.dueNs(srcPtsNs: Self.ptsAt(k), readyNs: ready, frameIntervalNs: Self.p)
        XCTAssertLessThan(due, ready, "a frame that arrived 30 ms late must read as already due")
        XCTAssertEqual(c.health().late, 1)
    }

    func testOffCadenceDoesNotMoveTheLoop() {
        let c = Self.settled(400, spread: 500_000)
        let before = c.health()
        let due = c.noteOffCadence(readyNs: 1_000_000, frameIntervalNs: Self.p)
        let after = c.health()
        XCTAssertEqual(before.offsetNs, after.offsetNs)
        XCTAssertEqual(before.skewNs, after.skewNs)
        XCTAssertEqual(before.jitterNs, after.jitterNs)
        XCTAssertEqual(before.frames, after.frames, "and it is not a cadence sample")
        XCTAssertEqual(due, 1_000_000 + c.cushionNs())
    }

    /// One domain in, same domain out: shifting the whole present-side trace by an arbitrary
    /// constant must change every due time by exactly that constant and nothing else. This is what
    /// lets each client feed its own clock without a conversion in the path — and on Apple it is
    /// what makes `mediaTimeNs(forRealtimeNs:)` the ONE conversion in the whole loop.
    func testDomainOffsetIsAbsorbed() {
        let shift: Int64 = 987_654_321_000
        func run(_ extra: Int64) -> [Int64] {
            let c = CadenceClock(tuning: .snapping())
            var rng = Lcg(11)
            return (Int64(0)..<300).map { k in
                let ready =
                    Int64(bitPattern: Self.ptsAt(k)) + Self.domain + Self.delay + extra
                    + rng.noise(2_000_000)
                return c.dueNs(srcPtsNs: Self.ptsAt(k), readyNs: ready, frameIntervalNs: Self.p)
            }
        }
        let a = run(0)
        let b = run(shift)
        for (i, pair) in zip(a, b).enumerated() {
            XCTAssertEqual(
                pair.1 - pair.0, shift, "frame \(i) shifted by \(pair.1 - pair.0) not \(shift)")
        }
    }

    /// The invariant that separates this from a metronome: a source that genuinely runs at an
    /// irregular rate is REPRODUCED, not evened out. Anything that made these due spacings more
    /// uniform than the source's own would be a bug.
    func testPreservesSourceCadence() {
        let c = CadenceClock(tuning: .snapping())
        // A deliberately lumpy source: alternating short and long frames.
        let spacings = (0..<300).map { $0 % 2 == 0 ? Self.p / 2 : Self.p * 3 / 2 }
        var pts = Self.pts0
        var dues: [Int64] = []
        var ptss: [Int64] = []
        var rng = Lcg(13)
        for s in spacings {
            pts = UInt64(bitPattern: Int64(bitPattern: pts) + s)
            let ready = Int64(bitPattern: pts) + Self.domain + Self.delay + rng.noise(500_000)
            ptss.append(Int64(bitPattern: pts))
            dues.append(c.dueNs(srcPtsNs: pts, readyNs: ready, frameIntervalNs: Self.p))
        }
        // Compare the back half, once the loop has settled.
        for i in 200..<dues.count {
            let dDue = dues[i] - dues[i - 1]
            let dPts = ptss[i] - ptss[i - 1]
            XCTAssertLessThan(
                abs(dDue - dPts), 200_000, "due spacing \(dDue) must follow the source's \(dPts)")
        }
    }

    func testCushionRespectsCeiling() {
        let c = CadenceClock(tuning: .freeRunning())
        var rng = Lcg(17)
        // Jitter far wider than a frame — the cushion must still never exceed one interval.
        for k in Int64(0)..<500 {
            let ready =
                Int64(bitPattern: Self.ptsAt(k)) + Self.domain + Self.delay + rng.noise(40_000_000)
            _ = c.dueNs(srcPtsNs: Self.ptsAt(k), readyNs: ready, frameIntervalNs: Self.p)
            XCTAssertLessThanOrEqual(
                c.cushionNs(), Self.p, "cushion \(c.cushionNs()) ns exceeded the frame interval")
        }
        XCTAssertGreaterThan(c.jitterNs(), Self.p, "the harness must actually have stressed it")
    }
}
#endif
