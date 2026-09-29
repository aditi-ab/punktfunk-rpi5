import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// The phase report the host's lock steers against: arrivals must beat the latch point less
/// decode, not the refresh — else a late-latched client receives frames past its cutoff.
final class PhaseReportTests: XCTestCase {
    private let period: Int64 = 16_666_667
    private let target: Int64 = 10_000_000_000

    /// `count` arrivals at `leadNs` before `ref`, one period apart, each taking `decodeNs`.
    private func report(latchLead: Int64, arrivalsLead leadNs: Int64, before ref: Int64,
                        decodeNs: Int64 = 2_000_000, count: Int = 30) -> PhaseReporter.Report? {
        let arrivals = (0..<count).map { ref - leadNs - Int64($0) * period }
        return PhaseReporter.report(
            targetRealNs: target, latchLeadNs: latchLead, periodNs: period,
            arrivalsNs: arrivals, decodesNs: Array(repeating: decodeNs, count: count))
    }

    func testTheReferenceIsTheLatchPointLessDecode() {
        let latchLead: Int64 = 20_666_667 // one 60 Hz refresh + 4 ms
        let r = report(latchLead: latchLead, arrivalsLead: 2_500_000, before: target - latchLead - 2_000_000)
        XCTAssertEqual(r?.readyByNs, target - latchLead - 2_000_000)
        XCTAssertEqual(Double(r?.leadMeanNs ?? 0), 2_500_000, accuracy: 20_000)
        XCTAssertGreaterThan(r?.coherence ?? 0, 990)
    }

    /// Frames arriving 2.5 ms before the REFRESH — where the old report parked them — land
    /// 3.5 ms past the ready-by point (the latch point sits 4 ms before a refresh, less 2 ms of
    /// decode): the report must read them as a period less 3.5 ms (circularly, 3.5 ms late),
    /// not an on-target 2.5.
    func testFramesAimedAtTheRefreshReadAsLate() {
        let latchLead: Int64 = 20_666_667
        let r = report(latchLead: latchLead, arrivalsLead: 2_500_000, before: target)
        XCTAssertEqual(Double(r?.leadMeanNs ?? 0), Double(period - 3_500_000), accuracy: 20_000)
    }

    /// Without late latch the client renders at the vend (latch lead = the whole lead, two
    /// refreshes): the reference falls on the refresh grid less decode, as before plus decode.
    func testWithoutLateLatchTheReferenceIsTheVendLessDecode() {
        let r = report(latchLead: 2 * period, arrivalsLead: 2_500_000, before: target - 2 * period - 2_000_000)
        XCTAssertEqual(r?.readyByNs, target - 2 * period - 2_000_000)
        XCTAssertEqual(Double(r?.leadMeanNs ?? 0), 2_500_000, accuracy: 20_000)
    }

    func testDecodeAllowanceIsTheWindowsP75() {
        let decodes: [Int64] = [1, 1, 1, 1, 1, 1, 7, 9].map { $0 * 1_000_000 }
        let arrivals = (0..<8).map { target - Int64($0) * period }
        let r = PhaseReporter.report(
            targetRealNs: target, latchLeadNs: 0, periodNs: period, arrivalsNs: arrivals,
            decodesNs: decodes)
        XCTAssertEqual(r?.readyByNs, target - 7_000_000)
    }

    func testUnderEightArrivalsThereIsNoReport() {
        XCTAssertNil(report(latchLead: 0, arrivalsLead: 0, before: target, count: 7))
    }
}
#endif
