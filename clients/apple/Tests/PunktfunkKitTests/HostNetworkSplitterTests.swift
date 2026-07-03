// Unit tests for HostNetworkSplitter (the host/network split of the unified stats model's
// host+network stage — design/stats-unification.md Phase 2): pts matching, the per-frame
// tiling arithmetic (network = combined − host, floored at 0), drain/reset semantics, the
// bounded pending ring, and the absurd-receipt clamp. All samples use explicit instants, so
// the expectations are exact.

import Foundation
import XCTest

@testable import PunktfunkKit

final class HostNetworkSplitterTests: XCTestCase {
    /// An arbitrary host-capture pts (ns) far from zero, like a real CLOCK_REALTIME stamp.
    private let basePts: UInt64 = 1_000_000_000_000

    private func receipt(_ s: HostNetworkSplitter, pts: UInt64, combinedMs: Int64,
                         offsetNs: Int64 = 0) {
        s.recordReceipt(
            ptsNs: pts, receivedNs: Int64(pts) + combinedMs * 1_000_000 - offsetNs,
            offsetNs: offsetNs)
    }

    func testEmptyDrainIsNil() {
        XCTAssertNil(HostNetworkSplitter().drain())
    }

    func testMatchSplitsCombinedIntoHostAndNetwork() {
        let s = HostNetworkSplitter()
        receipt(s, pts: basePts, combinedMs: 8) // capture→received 8 ms
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000) // host says 3 ms of it was its own
        guard let split = s.drain() else { return XCTFail("expected a matched sample") }
        XCTAssertEqual(split.count, 1)
        XCTAssertEqual(split.hostP50Ms, 3.0)
        XCTAssertEqual(split.networkP50Ms, 5.0, "the two terms tile the combined interval")
        XCTAssertNil(s.drain(), "drain resets the window")
    }

    func testSkewOffsetAppliesToTheCombinedInterval() {
        let s = HostNetworkSplitter()
        // Client clock 2 ms behind the host: the raw difference alone would read 6 ms.
        receipt(s, pts: basePts, combinedMs: 8, offsetNs: 2_000_000)
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000)
        XCTAssertEqual(s.drain()?.networkP50Ms, 5.0)
    }

    func testUnmatchedTimingIsSkipped() {
        let s = HostNetworkSplitter()
        receipt(s, pts: basePts, combinedMs: 8)
        // A timing for an AU we never received (FEC-dropped) must not fabricate a sample.
        s.noteHostTiming(ptsNs: basePts + 1, hostUs: 3_000)
        XCTAssertNil(s.drain())
    }

    func testReceiptSurvivesADrainUntilItsTimingArrives() {
        let s = HostNetworkSplitter()
        receipt(s, pts: basePts, combinedMs: 8)
        XCTAssertNil(s.drain(), "no timing matched yet")
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000) // arrives one tick late — still matches
        XCTAssertEqual(s.drain()?.hostP50Ms, 3.0)
    }

    func testEachReceiptMatchesOnce() {
        let s = HostNetworkSplitter()
        receipt(s, pts: basePts, combinedMs: 8)
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000)
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000) // duplicate 0xCF — no second sample
        XCTAssertEqual(s.drain()?.count, 1)
    }

    func testNetworkFlooredAtZero() {
        let s = HostNetworkSplitter()
        // A slightly-off skew offset can make host_us exceed the combined interval.
        receipt(s, pts: basePts, combinedMs: 2)
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000)
        guard let split = s.drain() else { return XCTFail("expected a sample") }
        XCTAssertEqual(split.hostP50Ms, 3.0)
        XCTAssertEqual(split.networkP50Ms, 0.0)
    }

    func testPendingRingDropsOldest() {
        let s = HostNetworkSplitter()
        for i in 0..<300 { // cap is 256 — the first receipts fall out
            receipt(s, pts: basePts + UInt64(i), combinedMs: 8)
        }
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000) // evicted — no match
        XCTAssertNil(s.drain())
        s.noteHostTiming(ptsNs: basePts + 299, hostUs: 3_000) // newest — still pending
        XCTAssertEqual(s.drain()?.count, 1)
    }

    func testAbsurdReceiptsAreDropped() {
        let s = HostNetworkSplitter()
        receipt(s, pts: basePts, combinedMs: -1) // received before capture — clock step
        receipt(s, pts: basePts + 1, combinedMs: 20_000) // > 10 s — garbage pts/offset
        s.noteHostTiming(ptsNs: basePts, hostUs: 1_000)
        s.noteHostTiming(ptsNs: basePts + 1, hostUs: 1_000)
        XCTAssertNil(s.drain())
    }

    func testResetForgetsPendingReceipts() {
        let s = HostNetworkSplitter()
        receipt(s, pts: basePts, combinedMs: 8)
        s.reset()
        s.noteHostTiming(ptsNs: basePts, hostUs: 3_000)
        XCTAssertNil(s.drain(), "a fresh session must not match a previous session's receipts")
    }
}
