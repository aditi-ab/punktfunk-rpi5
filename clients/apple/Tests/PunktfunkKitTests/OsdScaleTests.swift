import XCTest

import PunktfunkShared

/// Pins the overlay-scale arithmetic (`OsdScale`) — the auto sentinel, the clamp, the device-class
/// defaults and the percentage round trip the pickers speak. Twin of `punktfunk-core`'s
/// `osd_scale` tests and the Android client's `OsdScaleTest`.
final class OsdScaleTests: XCTestCase {
    func testAutoIsTheZeroSentinelAndSurvivesSanitize() {
        XCTAssertTrue(OsdScale.isAuto(OsdScale.auto))
        XCTAssertTrue(OsdScale.isAuto(-1))
        XCTAssertTrue(OsdScale.isAuto(.nan))
        XCTAssertFalse(OsdScale.isAuto(1.0))
        XCTAssertEqual(OsdScale.sanitize(OsdScale.auto), OsdScale.auto)
        XCTAssertEqual(OsdScale.sanitize(.nan), OsdScale.auto)
    }

    func testManualValuesClampIntoRange() {
        XCTAssertEqual(OsdScale.sanitize(0.1), OsdScale.range.lowerBound)
        XCTAssertEqual(OsdScale.sanitize(9), OsdScale.range.upperBound)
        XCTAssertEqual(OsdScale.sanitize(1.25), 1.25)
    }

    func testOnlyTvDepartsFromNativeSize() {
        XCTAssertEqual(OsdScale.autoScale(for: .handheld), 1.0)
        XCTAssertEqual(OsdScale.autoScale(for: .tablet), 1.0)
        XCTAssertEqual(OsdScale.autoScale(for: .desktop), 1.0)
        XCTAssertEqual(OsdScale.autoScale(for: .tv), 1.75)
    }

    func testResolvePrefersTheManualValueOverTheClass() {
        XCTAssertEqual(OsdScale.resolve(OsdScale.auto, for: .tv), 1.75)
        XCTAssertEqual(OsdScale.resolve(1.0, for: .tv), 1.0)
        XCTAssertEqual(OsdScale.resolve(2.0, for: .handheld), 2.0)
    }

    func testResolveIsAlwaysDrawable() {
        let classes: [OsdScale.DeviceClass] = [.handheld, .tablet, .desktop, .tv]
        for pref in [OsdScale.auto, .nan, -5, 0.01, 99, 1.5] {
            for deviceClass in classes {
                let scale = OsdScale.resolve(pref, for: deviceClass)
                XCTAssertTrue(scale.isFinite, "\(pref) on \(deviceClass) is not finite")
                XCTAssertTrue(OsdScale.range.contains(scale), "\(pref) on \(deviceClass) → \(scale)")
            }
        }
    }

    func testPercentRoundTrips() {
        for preset in OsdScale.presets {
            XCTAssertEqual(OsdScale.fromPercent(OsdScale.toPercent(preset)), preset)
        }
        XCTAssertEqual(OsdScale.toPercent(1.75), 175)
        XCTAssertEqual(OsdScale.fromPercent(125), 1.25)
    }

    func testTypedPercentClampsButZeroMeansAuto() {
        XCTAssertEqual(OsdScale.fromPercent(0), OsdScale.auto)
        XCTAssertEqual(OsdScale.fromPercent(5), OsdScale.range.lowerBound)
        XCTAssertEqual(OsdScale.fromPercent(500), OsdScale.range.upperBound)
    }

    func testPresetsAreOrdered25ApartAndInRange() {
        for (a, b) in zip(OsdScale.presets, OsdScale.presets.dropFirst()) {
            XCTAssertEqual(OsdScale.toPercent(b) - OsdScale.toPercent(a), 25)
        }
        XCTAssertEqual(OsdScale.presets.first, OsdScale.range.lowerBound)
        XCTAssertTrue(OsdScale.presets.allSatisfy { OsdScale.range.contains($0) })
        XCTAssertTrue(OsdScale.presets.contains(1.0))
    }

    func testCustomTagCannotCollideWithAStoredValue() {
        XCTAssertTrue(OsdScale.isAuto(OsdScale.customTag))
        XCTAssertFalse(OsdScale.presets.contains(OsdScale.customTag))
        XCTAssertNotEqual(OsdScale.customTag, OsdScale.auto)
    }

    func testStepWalksAutomaticAndThePresetsAndWraps() {
        XCTAssertEqual(OsdScale.step(1.0, dir: 1), 1.25)
        XCTAssertEqual(OsdScale.step(1.0, dir: -1), 0.75)
        XCTAssertEqual(OsdScale.step(1.0, dir: 0), 1.0)
        XCTAssertEqual(OsdScale.step(OsdScale.auto, dir: 1), OsdScale.presets.first)
        XCTAssertEqual(OsdScale.step(OsdScale.presets.first!, dir: -1), OsdScale.auto)
        XCTAssertEqual(OsdScale.step(OsdScale.presets.last!, dir: 1), OsdScale.auto)
        // A custom entry has no rung; the first step snaps to Automatic.
        XCTAssertEqual(OsdScale.step(1.6, dir: 1), OsdScale.auto)
        XCTAssertEqual(OsdScale.step(1.6, dir: -1), OsdScale.auto)
    }

    func testLabelsNameTheAutoValue() {
        XCTAssertEqual(OsdScale.label(OsdScale.auto, for: .tv), "Automatic (175%)")
        XCTAssertEqual(OsdScale.label(OsdScale.auto, for: .desktop), "Automatic (100%)")
        XCTAssertEqual(OsdScale.label(1.25, for: .tv), "125%")
    }

    /// The tvOS picker tags options with the percentage as a string; "0" has to survive as
    /// Automatic rather than clamping to the floor.
    func testTvPickerTagRoundTripsAutomatic() {
        XCTAssertEqual(OsdScale.toPercent(OsdScale.auto), 0)
        XCTAssertEqual(OsdScale.fromPercent(OsdScale.toPercent(OsdScale.auto)), OsdScale.auto)
    }
}
