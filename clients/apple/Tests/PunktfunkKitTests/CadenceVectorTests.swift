// `clients/shared/cadence-vectors.json` against the Swift `CadenceClock`: scripted runs that
// punktfunk-core's `phase::vectors` wrote from the Rust loop this one is a hand-written port of.
// Every due time and the final health must match to the nanosecond; a constant or a rounding
// rule that drifts on either side fails here, not on a user's screen.

import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

final class CadenceVectorTests: XCTestCase {
    private func tuning(_ name: String) -> CadenceTuning? {
        switch name {
        case "snapping": return .snapping()
        case "free_running": return .freeRunning()
        default: return nil
        }
    }

    /// JSONSerialization reads an integer into an `NSNumber` that holds it exactly.
    private func ints(_ value: Any?) throws -> [NSNumber] {
        try XCTUnwrap(value as? [NSNumber])
    }

    func testEverySharedRunAgrees() throws {
        let root = try SharedVector.load("cadence-vectors.json")
        let cases = try XCTUnwrap(root["cases"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 5, "the vector file is the contract; keep it rich")
        for c in cases {
            let name = try XCTUnwrap(c["name"] as? String)
            let tuningName = try XCTUnwrap(c["tuning"] as? String)
            let clock = CadenceClock(
                tuning: try XCTUnwrap(tuning(tuningName), "a tuning this test does not know: \(tuningName)"))
            let steps = try XCTUnwrap(c["steps"] as? [[String: Any]])
            for (i, step) in steps.enumerated() {
                let at = "\(name) step \(i)"
                if step["reset"] != nil {
                    clock.reset()
                } else if let due = step["due"] {
                    let a = try ints(due)
                    let got = clock.dueNs(
                        srcPtsNs: a[0].uint64Value, readyNs: a[1].int64Value,
                        frameIntervalNs: a[2].int64Value)
                    XCTAssertEqual(got, (step["expect"] as? NSNumber)?.int64Value, at)
                } else if let off = step["off_cadence"] {
                    let a = try ints(off)
                    let got = clock.noteOffCadence(
                        readyNs: a[0].int64Value, frameIntervalNs: a[1].int64Value)
                    XCTAssertEqual(got, (step["expect"] as? NSNumber)?.int64Value, at)
                } else {
                    XCTFail("\(at): a step this test does not know")
                }
            }
            let want = try XCTUnwrap(c["health"] as? [String: NSNumber])
            let h = clock.health()
            XCTAssertEqual(Int64(h.frames), want["frames"]?.int64Value, "\(name) frames")
            XCTAssertEqual(Int64(h.late), want["late"]?.int64Value, "\(name) late")
            XCTAssertEqual(Int64(h.reanchors), want["reanchors"]?.int64Value, "\(name) reanchors")
            XCTAssertEqual(h.offsetNs, want["offset_ns"]?.int64Value, "\(name) offset")
            XCTAssertEqual(h.skewNs, want["skew_ns"]?.int64Value, "\(name) skew")
            XCTAssertEqual(h.jitterNs, want["jitter_ns"]?.int64Value, "\(name) jitter")
            XCTAssertEqual(h.cushionNs, want["cushion_ns"]?.int64Value, "\(name) cushion")
        }
    }
}
#endif
