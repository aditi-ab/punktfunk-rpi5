// Unit tests for LatencyMeter: percentiles, the skew-corrected flag, reset-on-drain, and the
// absurd-value guard. Latencies are constructed by stamping a pts a known interval in the past, so
// the result is that interval plus the (tiny) clock advance between reads — asserted with tolerance.

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
