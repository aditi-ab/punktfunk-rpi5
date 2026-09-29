import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// Late latch: the vend during warm-up, then one refresh (half the lead) + 4 ms before the
/// target; a trusted miss backs off 2 ms, clean presents earn it back, never below the base and
/// never outside vend…target.
final class LatchBudgetTests: XCTestCase {
    private let lead = 0.033, targetNs: Int64 = 1_000_000_000

    private func latchAt(_ latch: LatchBudget) -> Double { latch.latchAt(vendAt: 1, target: 1 + lead) }

    /// A present for `targetNs` issued `offsetMs` before it; `lateMs` past it on glass (nil: dropped).
    private func present(
        _ latch: LatchBudget, offsetMs: Double, lateMs: Double? = 0, held: Bool = true, times: Int = 1
    ) {
        for _ in 0..<times {
            latch.observe(
                issuedNs: targetNs - Int64(offsetMs * 1e6),
                presentedNs: lateMs.map { targetNs + Int64($0 * 1e6) }, targetNs: targetNs,
                held: held)
        }
    }

    private func warmedUp() -> LatchBudget {
        let latch = LatchBudget(fixedDelay: nil)
        present(latch, offsetMs: 33, held: false, times: LatchBudget.warmup)
        return latch
    }

    func testRendersAtTheVendUntilWarmThenOneRefreshPlusMarginBeforeTarget() {
        let latch = LatchBudget(fixedDelay: nil)
        XCTAssertEqual(latchAt(latch), 1, accuracy: 1e-9)
        XCTAssertEqual(latch.presentFloor(lead: lead), lead, accuracy: 1e-9)
        present(latch, offsetMs: 33, held: false, times: LatchBudget.warmup)
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0205, accuracy: 1e-9)
        XCTAssertEqual(latch.presentFloor(lead: lead), 0.0205, accuracy: 1e-9)
    }

    func testATrustedMissBacksOffAndAnInFlightOneDoesNot() {
        let latch = warmedUp()
        _ = latchAt(latch) // budget 20.5 ms
        present(latch, offsetMs: 20.5, lateMs: 16.7)
        present(latch, offsetMs: 20.5, lateMs: 16.7) // issued under the old budget: already counted
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0225, accuracy: 1e-9)
        present(latch, offsetMs: 22.5, lateMs: nil) // a drop under the new budget is a miss
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0245, accuracy: 1e-9)
    }

    func testCleanPresentsEarnTheBackOffBackButNeverBelowTheBase() {
        let latch = warmedUp()
        _ = latchAt(latch)
        present(latch, offsetMs: 20.5, lateMs: 16.7)
        _ = latchAt(latch) // 22.5 ms
        present(latch, offsetMs: 22.5, times: LatchBudget.recovery - 1)
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0225, accuracy: 1e-9)
        present(latch, offsetMs: 22.5)
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0215, accuracy: 1e-9)
        present(latch, offsetMs: 21.5, times: 5 * LatchBudget.recovery)
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0205, accuracy: 1e-9)
    }

    func testUnheldMissesNeverBackOff() {
        let latch = warmedUp()
        _ = latchAt(latch)
        present(latch, offsetMs: 20.5, lateMs: 16.7, held: false, times: 10)
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0205, accuracy: 1e-9)
    }

    /// Backed off to the whole lead, nothing is held; on-target presents at the vend still
    /// bring the budget back.
    func testABudgetBackedOffToTheLeadRecoversFromUnheldHits() {
        let latch = warmedUp()
        _ = latchAt(latch)
        for step in 0..<7 { present(latch, offsetMs: 20.5 + 2 * Double(step), lateMs: 16.7) }
        XCTAssertEqual(latchAt(latch), 1, accuracy: 1e-9)
        present(latch, offsetMs: 33, held: false, times: 20 * LatchBudget.recovery)
        XCTAssertEqual(latchAt(latch), 1 + lead - 0.0205, accuracy: 1e-9)
    }

    func testTheLatchStaysBetweenVendAndTarget() {
        let latch = warmedUp()
        _ = latchAt(latch)
        for step in 0..<20 { present(latch, offsetMs: 20.5 + 2 * Double(step), lateMs: 16.7) }
        XCTAssertEqual(latchAt(latch), 1, accuracy: 1e-9) // backed off to the whole lead
        let fixed = LatchBudget(fixedDelay: 0.012)
        XCTAssertEqual(latchAt(fixed), 1.012, accuracy: 1e-9)
        XCTAssertEqual(fixed.latchAt(vendAt: 1, target: 1.008), 1.008, accuracy: 1e-9)
        present(fixed, offsetMs: 21, lateMs: 16.7, times: 5) // fixed only counts
        XCTAssertEqual(latchAt(fixed), 1.012, accuracy: 1e-9)
    }
}
#endif
