#if os(iOS)
import XCTest

@testable import PunktfunkKit

/// Pins the touch-mouse tuning contract (ported 1:1 from the Android client's TouchInput.kt
/// so the two touch clients feel identical) and the mode parsing. The gesture state machine
/// itself needs UITouch instances and is validated on-glass.
final class TouchMouseTests: XCTestCase {
    func testModeParsingDefaultsToTrackpad() {
        XCTAssertEqual(TouchInputMode(rawValue: "trackpad"), .trackpad)
        XCTAssertEqual(TouchInputMode(rawValue: "pointer"), .pointer)
        XCTAssertEqual(TouchInputMode(rawValue: "touch"), .touch)
        // Unknown/unset values must fall back to trackpad — never crash or go touch-silent.
        XCTAssertNil(TouchInputMode(rawValue: "bogus"))
    }

    func testAccelerationCurve() {
        // At or below the speed floor: no acceleration — slow drags stay precise.
        XCTAssertEqual(TouchMouse.Tuning.accel(forSpeed: 0), 1)
        XCTAssertEqual(TouchMouse.Tuning.accel(forSpeed: TouchMouse.Tuning.accelSpeedFloor), 1)
        // Above the floor the gain ramps...
        let mid = TouchMouse.Tuning.accel(forSpeed: 1.0)
        XCTAssertGreaterThan(mid, 1)
        XCTAssertLessThan(mid, TouchMouse.Tuning.accelMax)
        // ...and a flick is capped so it can't fling the cursor uncontrollably.
        XCTAssertEqual(TouchMouse.Tuning.accel(forSpeed: 100), TouchMouse.Tuning.accelMax)
        // Monotonic in between.
        XCTAssertLessThanOrEqual(
            TouchMouse.Tuning.accel(forSpeed: 0.5), TouchMouse.Tuning.accel(forSpeed: 1.5))
    }

    func testTuningRelations() {
        // The tap-drag window must be long enough to hit but short enough not to turn every
        // second tap into a drag.
        XCTAssertGreaterThan(TouchMouse.Tuning.tapDragWindow, 0.1)
        XCTAssertLessThan(TouchMouse.Tuning.tapDragWindow, 0.5)
        // A wheel notch per ~10 pt of two-finger pan (the indirect-trackpad path's feel).
        XCTAssertGreaterThan(TouchMouse.Tuning.scrollNotchPt, 0)
    }
}
#endif
