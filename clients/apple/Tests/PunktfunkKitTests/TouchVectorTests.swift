// `clients/shared/touch-vectors.json` against `TouchTuning` — the tuning the Android gesture
// layer runs too, so a flick crosses the screen the same distance on both. The table sits
// outside the iOS gate, so this runs under `swift test`.

import XCTest

@testable import PunktfunkKit

final class TouchVectorTests: XCTestCase {
    private func vectors() throws -> [String: Any] {
        try SharedVector.load("touch-vectors.json")
    }

    func testTheTuningMatchesTheVector() throws {
        let t = try XCTUnwrap(try vectors()["tuning"] as? [String: Double])
        func value(_ key: String) -> Double { t[key] ?? .nan }
        XCTAssertEqual(TouchTuning.tapDragWindow * 1000, value("tap_drag_ms"), accuracy: 1e-9)
        XCTAssertEqual(TouchTuning.longPress * 1000, value("long_press_ms"), accuracy: 1e-9)
        XCTAssertEqual(Double(TouchTuning.pointerSens), value("pointer_sens"), accuracy: 1e-6)
        XCTAssertEqual(Double(TouchTuning.accelGain), value("accel_gain"), accuracy: 1e-6)
        XCTAssertEqual(Double(TouchTuning.accelSpeedFloor), value("accel_speed_floor"), accuracy: 1e-6)
        XCTAssertEqual(Double(TouchTuning.accelMax), value("accel_max"), accuracy: 1e-6)
        XCTAssertEqual(
            Double(TouchTuning.keyboardSwipeFraction), value("keyboard_swipe_fraction"), accuracy: 1e-6)
        XCTAssertEqual(Double(TouchTuning.dialArmDeg), value("dial_arm_deg"), accuracy: 1e-6)
        XCTAssertEqual(Double(TouchTuning.dialCommitDeg), value("dial_commit_deg"), accuracy: 1e-6)
        XCTAssertEqual(
            Double(TouchTuning.dialSlop / TouchTuning.tapSlop), value("dial_slop_in_tap_slops"),
            accuracy: 1e-9)
    }

    func testEverySharedGainAgrees() throws {
        let rows = try XCTUnwrap(try vectors()["accel"] as? [[String: Double]])
        XCTAssertGreaterThanOrEqual(rows.count, 8, "the vector file is the contract; keep it rich")
        for row in rows {
            let speed = try XCTUnwrap(row["speed"])
            let want = try XCTUnwrap(row["expect"])
            XCTAssertEqual(Double(TouchTuning.accel(forSpeed: CGFloat(speed))), want, accuracy: 1e-4, "speed \(speed)")
        }
    }
}
