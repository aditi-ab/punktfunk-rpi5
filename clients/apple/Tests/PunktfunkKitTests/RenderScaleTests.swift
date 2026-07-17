import XCTest

import PunktfunkShared

/// Pins the render-scale geometry (`RenderScale`) — the multiply → aspect-preserve → even-floor →
/// codec-clamp that turns the `renderScale` setting into a host-valid `Mode`, plus the label/clamp
/// helpers the UI and the connect share.
final class RenderScaleTests: XCTestCase {
    func testSanitizeClampsToRangeAndDefaultsMissing() {
        XCTAssertEqual(RenderScale.sanitize(0), 1.0) // absent / zero → Native
        XCTAssertEqual(RenderScale.sanitize(-2), 1.0) // nonsense → Native
        XCTAssertEqual(RenderScale.sanitize(0.1), 0.5) // below the floor
        XCTAssertEqual(RenderScale.sanitize(9), 4.0) // above the ceiling
        XCTAssertEqual(RenderScale.sanitize(1.5), 1.5) // in range, untouched
    }

    func testMaxDimensionIsCodecAware() {
        XCTAssertEqual(RenderScale.maxDimension(codec: "h264"), 4096)
        XCTAssertEqual(RenderScale.maxDimension(codec: "hevc"), 8192)
        XCTAssertEqual(RenderScale.maxDimension(codec: "av1"), 8192)
        XCTAssertEqual(RenderScale.maxDimension(codec: "auto"), 8192)
    }

    func testNativeScaleIsIdentity() {
        let m = RenderScale.apply(baseWidth: 1920, baseHeight: 1080, scale: 1.0, maxDimension: 8192)
        XCTAssertEqual(m.width, 1920)
        XCTAssertEqual(m.height, 1080)
    }

    func testSupersampleDoubles() {
        let m = RenderScale.apply(baseWidth: 1920, baseHeight: 1080, scale: 2.0, maxDimension: 8192)
        XCTAssertEqual(m.width, 3840)
        XCTAssertEqual(m.height, 2160)
    }

    func testUnderRenderHalves() {
        let m = RenderScale.apply(baseWidth: 1920, baseHeight: 1080, scale: 0.5, maxDimension: 8192)
        XCTAssertEqual(m.width, 960)
        XCTAssertEqual(m.height, 540)
    }

    func testResultsAreAlwaysEven() {
        // 1280×720 × 1.5 = 1920×1080 (already even); 1366×768 × 1.5 = 2049×1152 → 2048×1152.
        let m = RenderScale.apply(baseWidth: 1366, baseHeight: 768, scale: 1.5, maxDimension: 8192)
        XCTAssertEqual(m.width % 2, 0)
        XCTAssertEqual(m.height % 2, 0)
        XCTAssertEqual(m.width, 2048)
        XCTAssertEqual(m.height, 1152)
    }

    func testOverCeilingClampsUniformlyPreservingAspect() {
        // 4K × 4 would be 15360×8640; both axes exceed 8192, so the LARGER (width) lands on the cap
        // and the ratio is kept (16:9 → 8192×4608, even).
        let m = RenderScale.apply(baseWidth: 3840, baseHeight: 2160, scale: 4.0, maxDimension: 8192)
        XCTAssertLessThanOrEqual(m.width, 8192)
        XCTAssertLessThanOrEqual(m.height, 8192)
        XCTAssertEqual(m.width, 8192)
        XCTAssertEqual(m.height, 4608)
        // 16:9 preserved to within the even-floor rounding.
        XCTAssertEqual(Double(m.width) / Double(m.height), 16.0 / 9.0, accuracy: 0.01)
    }

    func testH264CeilingIsTighter() {
        // 1080p × 4 = 7680×4320; under H.264's 4096 wall the width clamps to 4096, aspect kept.
        let m = RenderScale.apply(baseWidth: 1920, baseHeight: 1080, scale: 4.0, maxDimension: 4096)
        XCTAssertEqual(m.width, 4096)
        XCTAssertEqual(m.height, 2304)
    }

    func testMinimumFloorHonoured() {
        // A tiny base under a small scale can't fall below 320×200.
        let m = RenderScale.apply(baseWidth: 400, baseHeight: 300, scale: 0.5, maxDimension: 8192)
        XCTAssertGreaterThanOrEqual(m.width, 320)
        XCTAssertGreaterThanOrEqual(m.height, 200)
    }

    func testLabels() {
        XCTAssertEqual(RenderScale.label(1.0), "Native (1×)")
        XCTAssertEqual(RenderScale.label(2.0), "2× · supersample")
        XCTAssertEqual(RenderScale.label(0.5), "0.5×")
    }
}
