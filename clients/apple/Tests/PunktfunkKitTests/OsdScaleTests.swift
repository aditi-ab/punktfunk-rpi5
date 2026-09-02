import XCTest

@testable import PunktfunkShared

/// The overlay scale is derived from the platform, not stored, so the only thing with a way to
/// drift is the multiplier itself — and it has a twin in `OsdScaleUi.kt`'s `TV_OSD_SCALE`. A client
/// that enlarges the chrome by a different amount from the other is the bug this pins; the literal
/// here is deliberately spelled out rather than read from the constant it checks.
final class OsdScaleTests: XCTestCase {
    func testTheTvMultiplierMatchesTheAndroidTwin() {
        XCTAssertEqual(OsdScale.tv, 1.75, accuracy: 1e-6)
    }

    /// Everything held or sat in front of draws at design size; pt already fits there. The tests
    /// run on macOS, so this is the desktop arm of the `#if`.
    func testANearFieldPlatformDrawsAtDesignSize() {
        XCTAssertEqual(OsdScale.current, 1, accuracy: 1e-6)
    }
}
