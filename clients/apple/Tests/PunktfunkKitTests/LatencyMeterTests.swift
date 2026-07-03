// Unit tests for LatencyMeter (one instance per unified-stats stage — see
// design/stats-unification.md): percentiles, the skew-corrected flag, reset-on-drain, the
// absurd-value guard, and the explicit-instant stage form (record(ptsNs:atNs:offsetNs:), used for
// the client-local decode/display stages and the at-present end-to-end stamp). Receipt-path
// latencies are constructed by stamping a pts a known interval in the past, so the result is that
// interval plus the (tiny) clock advance between reads — asserted with tolerance; the explicit
// form is exact.

import Foundation
import XCTest

@testable import PunktfunkKit

final class LatencyMeterTests: XCTestCase {
    private func nowRealtimeNs() -> UInt64 {
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        return UInt64(ts.tv_sec) * 1_000_000_000 + UInt64(ts.tv_nsec)
    }

    func testEmptyDrainIsNil() {
        XCTAssertNil(LatencyMeter().drain())
    }

    func testRecordsPercentilesAndResets() {
        let m = LatencyMeter()
        let now = nowRealtimeNs()
        // Each frame "captured" 5 ms ago, no skew offset → latency ≈ 5 ms.
        for _ in 0..<50 { m.record(ptsNs: now - 5_000_000, offsetNs: 0) }
        guard let s = m.drain() else { return XCTFail("expected samples") }
        XCTAssertEqual(s.count, 50)
        XCTAssertFalse(s.skewCorrected, "offset 0 ⇒ not skew-corrected")
        XCTAssertEqual(s.p50Ms, 5.0, accuracy: 2.0)
        XCTAssertGreaterThanOrEqual(s.p99Ms, s.p50Ms)
        XCTAssertNil(m.drain(), "drain resets the window")
    }

    func testSkewCorrectedFlagSetByNonZeroOffset() {
        let m = LatencyMeter()
        let now = nowRealtimeNs()
        m.record(ptsNs: now - 1_000_000, offsetNs: 250_000) // 1 ms ago, +0.25 ms offset
        XCTAssertEqual(m.drain()?.skewCorrected, true)
    }

    func testExplicitStageRecordIsExact() {
        let m = LatencyMeter()
        // A client-local stage (decode: received→decoded) — start instant as ptsNs, offset 0.
        let receivedNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: UInt64(receivedNs), atNs: receivedNs + 3_000_000, offsetNs: 0)
        guard let s = m.drain() else { return XCTFail("expected a sample") }
        XCTAssertEqual(s.count, 1)
        XCTAssertEqual(s.p50Ms, 3.0, "explicit instants make the sample exact")
        XCTAssertFalse(s.skewCorrected, "local stages record with offset 0")
    }

    func testExplicitStageDropsNonPositiveInterval() {
        let m = LatencyMeter()
        // A stage whose start stamp is missing (0) or after its end must not pollute the window.
        let decodedNs: Int64 = 1_000_000_000_000
        m.record(ptsNs: 0, atNs: decodedNs, offsetNs: 0) // "start unknown" → > 10 s → dropped
        m.record(ptsNs: UInt64(decodedNs + 1), atNs: decodedNs, offsetNs: 0) // negative → dropped
        XCTAssertNil(m.drain())
    }

    func testDropsAbsurdValues() {
        let m = LatencyMeter()
        let now = nowRealtimeNs()
        // pts 1 s in the future → negative latency → dropped.
        m.record(ptsNs: now + 1_000_000_000, offsetNs: 0)
        // pts absurdly far in the past → > 10 s latency → dropped.
        m.record(ptsNs: now - 20_000_000_000, offsetNs: 0)
        XCTAssertNil(m.drain())
    }
}
