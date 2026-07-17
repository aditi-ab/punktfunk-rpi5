import XCTest

import PunktfunkShared
@testable import PunktfunkKit

/// Pins the location-based modifier remap (`InputCapture.applyModifierLayout`) — the wire-boundary
/// swap that relocates the Alt/Super role between the physical ⌥/⌘ keys per side, without touching
/// Control/Shift or the physical detection upstream.
final class ModifierLayoutMappingTests: XCTestCase {
    // The four physical modifier VKs the remap can touch, and everything else it must not.
    private let lWin: UInt32 = 0x5B, rWin: UInt32 = 0x5C
    private let lAlt: UInt32 = 0xA4, rAlt: UInt32 = 0xA5

    func testMacLayoutIsIdentity() {
        for vk: UInt32 in [lWin, rWin, lAlt, rAlt, 0xA0, 0xA2, 0x41, 0x1B] {
            XCTAssertEqual(InputCapture.applyModifierLayout(vk, .mac), vk)
        }
    }

    func testWindowsLayoutSwapsAltAndSuperPerSide() {
        XCTAssertEqual(InputCapture.applyModifierLayout(lWin, .windows), lAlt) // L ⌘ → L Alt
        XCTAssertEqual(InputCapture.applyModifierLayout(rWin, .windows), rAlt) // R ⌘ → R Alt
        XCTAssertEqual(InputCapture.applyModifierLayout(lAlt, .windows), lWin) // L ⌥ → L Win
        XCTAssertEqual(InputCapture.applyModifierLayout(rAlt, .windows), rWin) // R ⌥ → R Win
    }

    func testWindowsLayoutKeepsSideNeverCrossesLeftRight() {
        // A left key never becomes a right VK or vice-versa.
        XCTAssertNotEqual(InputCapture.applyModifierLayout(lWin, .windows), rAlt)
        XCTAssertNotEqual(InputCapture.applyModifierLayout(rWin, .windows), lAlt)
    }

    func testControlShiftAndRegularKeysNeverMove() {
        for vk: UInt32 in [
            0xA0, 0xA1, // L/R Shift
            0xA2, 0xA3, // L/R Control
            0x41, 0x5A, // A, Z
            0x1B, 0x0D, // Esc, Return
        ] {
            XCTAssertEqual(InputCapture.applyModifierLayout(vk, .windows), vk)
        }
    }

    func testWindowsRemapIsItsOwnInverse() {
        // Down-remapped then the same key up-remapped land on the same host VK — no stuck modifier.
        for vk: UInt32 in [lWin, rWin, lAlt, rAlt] {
            let once = InputCapture.applyModifierLayout(vk, .windows)
            XCTAssertEqual(InputCapture.applyModifierLayout(once, .windows), vk)
        }
    }
}
