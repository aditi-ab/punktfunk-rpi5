import Foundation
import XCTest
import simd

@testable import PunktfunkShared

/// The console UI's cross-client contract, against `clients/shared/console-vectors.json`.
///
/// The background palette table exists in three hand-written copies — this client,
/// `pf-console-ui`'s `library.rs`, and the Android client's `GamepadPalette.kt` — and until this
/// file the only thing holding them together was a comment in each asking the next person to keep
/// them in step. This is the sibling of `SharedFoundationTests.testDeepLinkSharedVectors`, read
/// the same way and for the same reason.
///
/// What it pins beyond the definitions is the DERIVED table: the 16 mesh cells each palette
/// produces, which is what actually reaches the gradient. `GamepadPaletteTests` already asserts
/// the invariants (hue spread, gamut, lightness honesty); this asserts the values.
///
/// The tab names and the shell motion are pinned here too, since `GpSettingsTab` and
/// `ConsoleMotion` moved into `PunktfunkShared` (`ConsoleContract.swift`) — where `GamepadPalette`
/// already sat, and for exactly this reason. This client implements the version-2 `motion_spring`
/// block; the deprecated v1 `motion` block is Android's until it migrates.
final class ConsoleVectorsTests: XCTestCase {
    /// Read from the repo, not from a bundle resource: a copy would be a second file, and a
    /// second file drifts. Four `deletingLastPathComponent()` calls walk
    /// `Tests/PunktfunkKitTests/` → `Tests/` → `apple/` → `clients/`.
    private static var vectorFileURL: URL {
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("shared/console-vectors.json")
    }

    private struct VectorFile: Decodable {
        let cellRamp: [Double]
        let meshInterior: [[Double]]
        let palettes: [Palette]
        let tabs: [Tab]
        let motionSpring: MotionSpring

        // swiftlint:disable:next nesting
        struct Tab: Decodable {
            let name: String
            let desktopOnly: Bool?
            enum CodingKeys: String, CodingKey {
                case name
                case desktopOnly = "desktop_only"
            }
        }

        // swiftlint:disable:next nesting
        struct MotionSpring: Decodable {
            let response: Double
            let damping: Double
            let pushSlideDp: Double
            let enterScale: Double
            let exitScale: Double
            let revealAlpha: Double
            let interruptible: Bool
            enum CodingKeys: String, CodingKey {
                case response, damping, interruptible
                case pushSlideDp = "push_slide_dp"
                case enterScale = "enter_scale"
                case exitScale = "exit_scale"
                case revealAlpha = "reveal_alpha"
            }
        }

        // swiftlint:disable:next nesting
        struct Palette: Decodable {
            let id: String
            let name: String
            let light: Bool
            let stops: [[Double]]
            let ground: [Double]
            let accent: [Double]
            let mesh: [[Double]]
            let blobs: [[Double]]
        }

        // swiftlint:disable:next identifier_name
        enum CodingKeys: String, CodingKey {
            case cellRamp = "cell_ramp"
            case meshInterior = "mesh_interior"
            case palettes, tabs
            case motionSpring = "motion_spring"
        }
    }

    /// The section names, against the shared vectors — a setting is found under the same word on
    /// every client. The desktop's `Input` tab is `desktop_only` (touch mode, mouse, invert-scroll
    /// and shortcuts have nothing to set on a phone or a TV); this client's trailing `About` is its
    /// own and not in the shared list (`GpSettingsTab.shared` drops it).
    func testTabNamesMatchTheSharedVectors() throws {
        let file = try JSONDecoder().decode(VectorFile.self, from: Data(contentsOf: Self.vectorFileURL))
        let want = file.tabs.filter { $0.desktopOnly != true }.map(\.name)
        XCTAssertEqual(GpSettingsTab.shared.map(\.rawValue), want, "console settings tabs")
        XCTAssertEqual(GpSettingsTab.allCases.last, .about, "About ends the strip")
    }

    /// The screen transition — the version-2 `motion_spring` block. Parameters, not samples:
    /// springs are integrator-dependent, and two implementations honouring response/damping agree
    /// to the eye. The geometry (slide, scales, reveal alpha) is unchanged from v1.
    func testMotionMatchesTheSharedVectors() throws {
        let file = try JSONDecoder().decode(VectorFile.self, from: Data(contentsOf: Self.vectorFileURL))
        let m = file.motionSpring
        assertClose(ConsoleMotion.response, m.response, "response")
        assertClose(ConsoleMotion.damping, m.damping, "damping")
        assertClose(ConsoleMotion.pushSlideDp, m.pushSlideDp, "push_slide_dp")
        assertClose(ConsoleMotion.enterScale, m.enterScale, "enter_scale")
        assertClose(ConsoleMotion.exitScale, m.exitScale, "exit_scale")
        assertClose(ConsoleMotion.revealAlpha, m.revealAlpha, "reveal_alpha")
        XCTAssertEqual(ConsoleMotion.interruptible, m.interruptible, "interruptible")
    }

    private func assertClose(
        _ got: Double, _ want: Double, _ what: String, tolerance: Double = 1e-6,
        file: StaticString = #filePath, line: UInt = #line
    ) {
        XCTAssertEqual(
            got, want, accuracy: tolerance,
            "\(what): vectors say \(want), this client computes \(got)", file: file, line: line)
    }

    func testPaletteTableMatchesTheSharedVectors() throws {
        let url = Self.vectorFileURL
        XCTAssertTrue(
            FileManager.default.fileExists(atPath: url.path),
            "the shared vector file must be reachable at \(url.path)")
        let file = try JSONDecoder().decode(VectorFile.self, from: Data(contentsOf: url))

        XCTAssertEqual(file.cellRamp, GamepadPalette.cellRamp, "cellRamp")
        XCTAssertEqual(file.palettes.count, GamepadPalette.all.count, "palette count")

        for (want, p) in zip(file.palettes, GamepadPalette.all) {
            XCTAssertEqual(want.id, p.id, "palette order")
            XCTAssertEqual(want.name, p.name, "\(p.id) name")
            XCTAssertEqual(want.light, p.light, "\(p.id) light")

            XCTAssertEqual(want.stops.count, p.stops.count, "\(p.id) stop count")
            for (i, (ws, s)) in zip(want.stops, p.stops).enumerated() {
                assertClose(s.x, ws[0], "\(p.id) stops[\(i)].r")
                assertClose(s.y, ws[1], "\(p.id) stops[\(i)].g")
                assertClose(s.z, ws[2], "\(p.id) stops[\(i)].b")
            }
            assertClose(p.ground.x, want.ground[0], "\(p.id) ground.r")
            assertClose(p.ground.y, want.ground[1], "\(p.id) ground.g")
            assertClose(p.ground.z, want.ground[2], "\(p.id) ground.b")
            assertClose(p.accent.x, want.accent[0], "\(p.id) accent.r")
            assertClose(p.accent.y, want.accent[1], "\(p.id) accent.g")
            assertClose(p.accent.z, want.accent[2], "\(p.id) accent.b")

            let mesh = p.meshColors
            XCTAssertEqual(want.mesh.count, mesh.count, "\(p.id) mesh cells")
            for (i, (wc, c)) in zip(want.mesh, mesh).enumerated() {
                assertClose(c.x, wc[0], "\(p.id) mesh[\(i)].r")
                assertClose(c.y, wc[1], "\(p.id) mesh[\(i)].g")
                assertClose(c.z, wc[2], "\(p.id) mesh[\(i)].b")
            }
            let blobs = p.blobColors
            XCTAssertEqual(want.blobs.count, blobs.count, "\(p.id) blob count")
            for (i, (wc, c)) in zip(want.blobs, blobs).enumerated() {
                assertClose(c.x, wc[0], "\(p.id) blob[\(i)].r")
                assertClose(c.y, wc[1], "\(p.id) blob[\(i)].g")
                assertClose(c.z, wc[2], "\(p.id) blob[\(i)].b")
            }
        }
    }

    /// The four wandering mesh control points. This client keeps them as literal arguments to a
    /// nested `wob(...)` inside `GamepadChrome.meshPoints(at:)` rather than as a named table, so
    /// the values are checked here against the vectors and the shape is pinned by the count.
    func testMeshInteriorIsFourPoints() throws {
        let file = try JSONDecoder().decode(
            VectorFile.self, from: Data(contentsOf: Self.vectorFileURL))
        XCTAssertEqual(file.meshInterior.count, 4, "the mesh has four interior control points")
        for p in file.meshInterior {
            XCTAssertEqual(p.count, 6, "each point is (x, y, amp, sx, sy, phase)")
        }
    }
}
