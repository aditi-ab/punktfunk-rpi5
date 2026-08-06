// The safe-area stream mode (SafeDisplay), as pure geometry: Moonlight's formula — full native
// height, width reduced by the left+right safe insets — plus the host's dimension rules (even, and
// never under 320×200) and the landscape-inset resolution that makes the row correct even when the
// settings screen it is rendered on is currently portrait.

import XCTest

import PunktfunkShared
@testable import PunktfunkKit

final class SafeDisplayTests: XCTestCase {
    func testLandscapeUsesTheHorizontalInsets() {
        // Landscape: the housing is on a side and iOS symmetrizes the two, so either one is the
        // per-side inset.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 59, right: 59, top: 0, isPhone: true), 59)
        // Asymmetric (or mid-rotation) readings reduce to the larger — never under-inset.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 44, top: 0, isPhone: true), 44)
    }

    func testPortraitFallsBackToTheHousingTopInset() {
        // Portrait on a notched phone: left/right are zero and the housing sits on `top`. Reading
        // the horizontal insets here would compute "no inset" for exactly the devices that need one,
        // so the portrait top inset stands in — it is the same physical intrusion.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 0, top: 59, isPhone: true), 59)
        // A plain status bar is not a housing: an iPad (or a pre-notch iPhone) must not fabricate an
        // inset for a device with nothing to route around.
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 0, top: 24, isPhone: true), 0)
        XCTAssertEqual(
            SafeDisplay.sideInsetPoints(left: 0, right: 0, top: 59, isPhone: false), 0)
    }

    func testModeInsetsWidthOnlyAndKeepsFullHeight() {
        // A Dynamic Island phone: 2556×1179 native, 59 pt per side at nativeScale 3 → 177 px per
        // side, 354 px total. Height is untouched — under aspect-fit only the horizontal axis binds.
        let m = SafeDisplay.mode(
            nativeWidth: 2556, nativeHeight: 1179, sideInsetPoints: 59, scale: 3)
        XCTAssertEqual(m.width, 2202, "2556 − 2×177")
        XCTAssertEqual(m.height, 1178, "odd native heights even-floor")
        // The safe mode must be NARROWER than native, or it would still fill the housing.
        XCTAssertLessThan(m.width, 2556)
    }

    func testNoHousingYieldsTheNativeModeSoTheRowDedups() {
        // Zero inset ⇒ identical to native (bar the even-floor). `resolutionModes` dedups by
        // dimensions, so this is what makes the extra row vanish on a device that has no housing
        // rather than showing a pointless duplicate.
        let m = SafeDisplay.mode(
            nativeWidth: 2360, nativeHeight: 1640, sideInsetPoints: 0, scale: 2)
        XCTAssertEqual(m.width, 2360)
        XCTAssertEqual(m.height, 1640)
    }

    func testResultIsAlwaysHostValid() {
        // Odd widths even-floor: `validate_dimensions` rejects odd outright, and an inset
        // subtraction lands odd about half the time.
        let odd = SafeDisplay.mode(
            nativeWidth: 2001, nativeHeight: 1001, sideInsetPoints: 0, scale: 1)
        XCTAssertEqual(odd.width % 2, 0)
        XCTAssertEqual(odd.height % 2, 0)
        // An absurd inset can't drive the mode under the host's floor.
        let tiny = SafeDisplay.mode(
            nativeWidth: 1280, nativeHeight: 720, sideInsetPoints: 5000, scale: 3)
        XCTAssertEqual(tiny.width, SafeDisplay.minWidth)
        XCTAssertEqual(tiny.height, 720)
        // A negative inset is treated as none rather than widening past the panel.
        let neg = SafeDisplay.mode(
            nativeWidth: 1280, nativeHeight: 720, sideInsetPoints: -40, scale: 3)
        XCTAssertEqual(neg.width, 1280)
    }
}
