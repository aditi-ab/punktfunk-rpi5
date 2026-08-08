// The gamepad UI's background palettes. These assertions are the CONTRACT the Rust
// (`pf-console-ui::library`) and Kotlin (`GamepadPalette.kt`) ports reproduce — the same ids in
// the same order, the same light/dark split, the same ramp — so one `ui_palette` value is one look
// on every client.

import XCTest
import simd
@testable import PunktfunkShared

final class GamepadPaletteTests: XCTestCase {
    private func luma(_ c: SIMD3<Double>) -> Double {
        0.2126 * c.x + 0.7152 * c.y + 0.0722 * c.z
    }

    /// Hue angle in degrees, or nil for something too grey to have one.
    private func hue(_ c: SIMD3<Double>) -> Double? {
        let maxV = max(c.x, c.y, c.z)
        let minV = min(c.x, c.y, c.z)
        let d = maxV - minV
        guard d >= 0.04 else { return nil }
        let h: Double
        if maxV == c.x {
            h = 60 * (((c.y - c.z) / d).truncatingRemainder(dividingBy: 6))
        } else if maxV == c.y {
            h = 60 * ((c.z - c.x) / d + 2)
        } else {
            h = 60 * ((c.x - c.y) / d + 4)
        }
        return (h + 360).truncatingRemainder(dividingBy: 360)
    }

    /// The brand default must still be the SHIPPED field, colour for colour. Every install
    /// already sees it, and a palette table that quietly restyled the default would be a
    /// regression dressed as a feature.
    func testVioletIsTheUntouchedShippedField() {
        let violet = GamepadPalette.named("violet")
        XCTAssertEqual(GamepadPalette.all.first?.id, "violet")
        XCTAssertTrue(violet.stops.isEmpty, "the default is the explicit grid")
        XCTAssertEqual(violet.meshColors, GamepadPalette.violetMesh)
        // An unknown name is a newer client's palette, not an error.
        XCTAssertEqual(GamepadPalette.named("chartreuse").id, "violet")
        XCTAssertEqual(GamepadPalette.named("").id, "violet")
    }

    /// Ids, order and the light/dark split are the cross-client contract.
    func testTableMatchesTheOtherClients() {
        XCTAssertEqual(
            GamepadPalette.all.map(\.id),
            ["violet", "oled", "nebula", "abyss", "ember", "moss", "graphite",
             "holo", "sunset", "bloom", "dawn", "mint", "opal"])
        // Dark fields lead, pale ones follow, so stepping the row walks one direction.
        let firstLight = GamepadPalette.all.firstIndex { $0.light }
        XCTAssertEqual(firstLight, 7)
        XCTAssertTrue(GamepadPalette.all.dropFirst(7).allSatisfy(\.light))
    }

    /// OLED is the one palette whose selling point is measurable: it has to be genuinely black,
    /// not merely the darkest of the dark fields.
    func testOLEDIsActuallyBlack() {
        let oled = GamepadPalette.named("oled")
        XCTAssertEqual(oled.ground, SIMD3(0, 0, 0), "the calm lift must be nothing")
        let cells = oled.meshColors
        XCTAssertGreaterThanOrEqual(
            cells.filter { luma($0) == 0 }.count, 3,
            "the shaded corner has to be switched off, not dimmed")
        let mean = cells.map(luma).reduce(0, +) / Double(cells.count)
        let darkestOther = GamepadPalette.all
            .filter { $0.id != "oled" }
            .map { p in p.meshColors.map(luma).reduce(0, +) / Double(p.meshColors.count) }
            .min() ?? 0
        XCTAssertLessThan(mean, darkestOther / 2, "oled is barely darker than \(darkestOther)")
    }

    /// A palette must read as SEVERAL hues, not one hue at several brightnesses — that was
    /// exactly the complaint about the hue-rotation model this replaced.
    func testEveryPaletteIsMultiTone() {
        for p in GamepadPalette.all {
            let hues = p.meshColors.compactMap(hue)
            XCTAssertGreaterThanOrEqual(hues.count, 8, "\(p.id): too few coloured cells")
            var spread = 0.0
            for a in hues {
                for b in hues {
                    let d = abs(a - b).truncatingRemainder(dividingBy: 360)
                    spread = max(spread, min(d, 360 - d))
                }
            }
            // Graphite and Opal are deliberately near-neutral; the rest must travel.
            let floor = (p.id == "graphite" || p.id == "opal") ? 20.0 : 45.0
            XCTAssertGreaterThanOrEqual(spread, floor, "\(p.id) spans only \(spread)° of hue")
        }
    }

    /// Every colour stays in gamut, and a pale palette really is pale — its ink flips, so a
    /// mislabelled one would put dark text on a dark field.
    func testPalettesAreInGamutAndHonestAboutLightness() {
        for p in GamepadPalette.all {
            for c in p.meshColors + p.blobColors {
                for v in [c.x, c.y, c.z] {
                    XCTAssertTrue((0...1).contains(v), "\(p.id) \(c)")
                }
            }
            let mean = p.meshColors.map(luma).reduce(0, +) / Double(p.meshColors.count)
            if p.light {
                XCTAssertGreaterThan(mean, 0.5, "\(p.id) is flagged light")
                XCTAssertGreaterThan(luma(p.ground), 0.6, "\(p.id)'s ground is dark")
                XCTAssertLessThan(luma(p.accent), 0.45, "\(p.id)'s accent is too pale")
            } else {
                XCTAssertLessThan(mean, 0.45, "\(p.id) is flagged dark")
                XCTAssertLessThan(luma(p.ground), 0.2, "\(p.id)'s ground is light")
                XCTAssertGreaterThan(luma(p.accent), 0.25, "\(p.id)'s accent is too dark")
            }
        }
    }

    /// The ramp is the shared sampling rule the Rust and Kotlin ports reproduce.
    func testRampInterpolatesBetweenStops() {
        let stops = [SIMD3(0.0, 0.0, 0.0), SIMD3(1.0, 0.0, 0.0), SIMD3(1.0, 1.0, 1.0)]
        XCTAssertEqual(GamepadPalette.ramp(stops, 0), SIMD3(0.0, 0.0, 0.0))
        XCTAssertEqual(GamepadPalette.ramp(stops, 1), SIMD3(1.0, 1.0, 1.0))
        XCTAssertEqual(GamepadPalette.ramp(stops, 0.5), SIMD3(1.0, 0.0, 0.0))
        XCTAssertEqual(GamepadPalette.ramp(stops, 0.25).x, 0.5, accuracy: 1e-9)
        // Out of range clamps rather than trapping.
        XCTAssertEqual(GamepadPalette.ramp(stops, -3), SIMD3(0.0, 0.0, 0.0))
        XCTAssertEqual(GamepadPalette.ramp(stops, 9), SIMD3(1.0, 1.0, 1.0))
        XCTAssertEqual(GamepadPalette.ramp([], 0.5), SIMD3(0.0, 0.0, 0.0))
    }
}
