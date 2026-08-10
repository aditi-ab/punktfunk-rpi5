// The remembered-controller glyph table.
//
// The load-bearing assertion here is that every SF Symbol name RESOLVES. `Image(systemName:)`
// renders a name the OS doesn't know as NOTHING at all — no crash, no log, no red build — so a
// typo in the table would silently blank a legend cell on real hardware and be invisible until
// someone looked at a device. This test is the only thing standing between that and a release.

import GameController
import XCTest
@testable import PunktfunkKit

#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

final class GamepadGlyphTests: XCTestCase {
    private let roles: [GamepadButtonRole] = [.a, .b, .x, .y, .leftShoulder, .rightShoulder]

    /// Does the running OS actually have this symbol?
    private func symbolExists(_ name: String) -> Bool {
        #if canImport(UIKit)
        return UIImage(systemName: name) != nil
        #elseif canImport(AppKit)
        return NSImage(systemSymbolName: name, accessibilityDescription: nil) != nil
        #else
        return true
        #endif
    }

    func testEveryGlyphNameResolvesOnThisOS() {
        for kind in PunktfunkConnection.GamepadType.allCases {
            for role in roles {
                let name = GamepadGlyphs.symbol(role, for: kind)
                XCTAssertTrue(
                    symbolExists(name),
                    "SF Symbol \"\(name)\" (\(role), \(kind)) does not resolve — the legend cell "
                        + "would render blank on device")
            }
        }
    }

    /// ✕ is the BOTTOM button on a PlayStation pad, which is `GCExtendedGamepad.buttonA` — the
    /// whole point of the table being positional. Getting this backwards would print ◯ where the
    /// user has to press ✕.
    func testPlayStationFaceButtonsAreShapesInPositionalOrder() {
        for kind in [PunktfunkConnection.GamepadType.dualSense, .dualSenseEdge, .dualShock4] {
            XCTAssertEqual(GamepadGlyphs.symbol(.a, for: kind), "xmark.circle")
            XCTAssertEqual(GamepadGlyphs.symbol(.b, for: kind), "circle.circle")
            XCTAssertEqual(GamepadGlyphs.symbol(.x, for: kind), "square.circle")
            XCTAssertEqual(GamepadGlyphs.symbol(.y, for: kind), "triangle.circle")
        }
    }

    /// Nintendo's labels sit transposed on the same physical positions: the bottom button (role
    /// `.a`) is labelled B, and the right one (role `.b`) is labelled A.
    func testSwitchFaceButtonsAreTransposed() {
        XCTAssertEqual(GamepadGlyphs.symbol(.a, for: .switchPro), "b.circle")
        XCTAssertEqual(GamepadGlyphs.symbol(.b, for: .switchPro), "a.circle")
        XCTAssertEqual(GamepadGlyphs.symbol(.x, for: .switchPro), "y.circle")
        XCTAssertEqual(GamepadGlyphs.symbol(.y, for: .switchPro), "x.circle")
    }

    /// `.auto` is what a device that has never seen a controller reports, and Xbox letters are the
    /// neutral default — they are also the positional names `GCExtendedGamepad` itself uses.
    func testUnknownAndXboxFamiliesUseLetters() {
        for kind in [PunktfunkConnection.GamepadType.auto, .xbox360, .xboxOne, .steamDeck] {
            XCTAssertEqual(GamepadGlyphs.symbol(.a, for: kind), "a.circle")
            XCTAssertEqual(GamepadGlyphs.symbol(.b, for: kind), "b.circle")
            XCTAssertEqual(GamepadGlyphs.symbol(.x, for: kind), "x.circle")
            XCTAssertEqual(GamepadGlyphs.symbol(.y, for: kind), "y.circle")
        }
    }

    /// The key-path bridge the legends reach this table through (`buttonGlyph` spells its buttons
    /// as key paths). A wrong mapping here would print the wrong button on every family at once.
    func testRolesResolveFromExtendedGamepadKeyPaths() {
        XCTAssertEqual(GamepadButtonRole(keyPath: \.buttonA), .a)
        XCTAssertEqual(GamepadButtonRole(keyPath: \.buttonB), .b)
        XCTAssertEqual(GamepadButtonRole(keyPath: \.buttonX), .x)
        XCTAssertEqual(GamepadButtonRole(keyPath: \.buttonY), .y)
        XCTAssertEqual(GamepadButtonRole(keyPath: \.leftShoulder), .leftShoulder)
        XCTAssertEqual(GamepadButtonRole(keyPath: \.rightShoulder), .rightShoulder)
        // A button outside the six the legends name has no honest glyph on every family, so it
        // falls through to the caller's own fallback rather than guessing.
        XCTAssertNil(GamepadButtonRole(keyPath: \.leftTrigger))
    }
}
