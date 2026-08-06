// The gamepad UI's background palettes. These assertions are the CONTRACT the Rust
// (`pf-console-ui::library::tint`) and Kotlin (`GamepadPalette.tint`) ports have to reproduce —
// the same ids, the same rotation orientation, the same in-gamut results — so one `ui_palette`
// value names the same colour family on every client.

import XCTest
import simd
@testable import PunktfunkShared

final class GamepadPaletteTests: XCTestCase {
    /// The brightest interior pool of the mesh field — the colour a palette is judged by.
    private let violetPool = SIMD3(0.49, 0.39, 0.95)

    /// The brand default must be the IDENTITY transform. Every existing install already sees the
    /// shipped violet backdrop, and a palette table that quietly restyled it would be a
    /// regression dressed as a feature.
    func testVioletIsTheUntouchedShippedField() {
        let violet = GamepadPalette.named("violet")
        XCTAssertEqual(GamepadPalette.all.first?.id, "violet")
        XCTAssertTrue(violet.isIdentity)
        XCTAssertEqual(violet.tint(violetPool), violetPool)
        // An unknown name is a newer client's palette, not an error.
        XCTAssertEqual(GamepadPalette.named("chartreuse").id, "violet")
        XCTAssertEqual(GamepadPalette.named("").id, "violet")
    }

    /// The ids and their order are the cross-client contract (the strip order, and the order
    /// L1/R1 and A cycle through).
    func testTableMatchesTheOtherClients() {
        XCTAssertEqual(
            GamepadPalette.all.map(\.id),
            ["violet", "tide", "forest", "ember", "rose", "graphite"])
        XCTAssertEqual(
            GamepadPalette.all.map(\.name),
            ["Violet", "Tide", "Forest", "Ember", "Rose", "Graphite"])
    }

    /// A rotation moves the hue while roughly holding luminance, and the saturation scale
    /// collapses toward grey — the same four checks the Rust test makes.
    func testTintRotatesHueAndScalesSaturation() {
        XCTAssertTrue(violetPool.z > violetPool.x && violetPool.z > violetPool.y, "blue-dominant")

        // +105° (Ember) turns the blue-dominant pool red-dominant…
        let ember = GamepadPalette.named("ember").tint(violetPool)
        XCTAssertGreaterThan(ember.x, ember.z, "\(ember) should be warm")
        // …−130° (Forest) turns it green-dominant…
        let forest = GamepadPalette.named("forest").tint(violetPool)
        XCTAssertTrue(forest.y > forest.x && forest.y > forest.z, "\(forest)")
        // …and −70° (Tide) lands on a cyan whose green and blue both beat red.
        let tide = GamepadPalette.named("tide").tint(violetPool)
        XCTAssertTrue(tide.y > tide.x && tide.z > tide.x, "\(tide)")

        // Graphite's saturation scale leaves the channels nearly equal…
        let grey = GamepadPalette.named("graphite").tint(violetPool)
        let spread = max(grey.x, grey.y, grey.z) - min(grey.x, grey.y, grey.z)
        XCTAssertLessThan(spread, 0.08, "\(grey)")
        // …at about the source's luminance (it desaturates, it doesn't dim).
        let luma = 0.2126 * violetPool.x + 0.7152 * violetPool.y + 0.0722 * violetPool.z
        XCTAssertEqual(grey.y, luma, accuracy: 0.05)
    }

    /// Every palette stays in gamut on every colour the field is built from — an out-of-range
    /// channel would clamp differently on each platform's rasteriser.
    func testEveryPaletteStaysInGamut() {
        let field: [SIMD3<Double>] = [
            SIMD3(0.075, 0.060, 0.160), SIMD3(0.34, 0.27, 0.72), SIMD3(0.30, 0.26, 0.74),
            SIMD3(0.42, 0.20, 0.54), SIMD3(0.49, 0.39, 0.95), SIMD3(0.28, 0.31, 0.84),
            SIMD3(0.16, 0.26, 0.64), SIMD3(0.45, 0.23, 0.60), SIMD3(0.53, 0.31, 0.75),
            SIMD3(0.35, 0.35, 0.91), SIMD3(0.19, 0.28, 0.70), SIMD3(0.22, 0.18, 0.54),
            SIMD3(0.24, 0.20, 0.58),
        ]
        for palette in GamepadPalette.all {
            for c in field {
                let t = palette.tint(c)
                for v in [t.x, t.y, t.z] {
                    XCTAssertTrue((0...1).contains(v), "\(palette.id) \(c) → \(t)")
                }
            }
        }
    }
}
