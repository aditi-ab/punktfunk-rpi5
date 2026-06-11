// The gamepad wire contract: button bit positions (must match
// punktfunk_core::input::gamepad), GC→DualSense touchpad/motion conversions, and the
// player-LED-bits → GCControllerPlayerIndex map. All pure functions.

import GameController
import XCTest

@testable import PunktfunkKit

final class GamepadWireTests: XCTestCase {
    func testButtonBitsMatchTheRustWireContract() {
        // punktfunk_core::input::gamepad constants, spot-checked bit for bit.
        XCTAssertEqual(GamepadWire.dpadUp, 0x0001)
        XCTAssertEqual(GamepadWire.dpadDown, 0x0002)
        XCTAssertEqual(GamepadWire.dpadLeft, 0x0004)
        XCTAssertEqual(GamepadWire.dpadRight, 0x0008)
        XCTAssertEqual(GamepadWire.start, 0x0010)
        XCTAssertEqual(GamepadWire.back, 0x0020)
        XCTAssertEqual(GamepadWire.leftStickClick, 0x0040)
        XCTAssertEqual(GamepadWire.rightStickClick, 0x0080)
        XCTAssertEqual(GamepadWire.leftShoulder, 0x0100)
        XCTAssertEqual(GamepadWire.rightShoulder, 0x0200)
        XCTAssertEqual(GamepadWire.guide, 0x0400)
        XCTAssertEqual(GamepadWire.a, 0x1000)
        XCTAssertEqual(GamepadWire.b, 0x2000)
        XCTAssertEqual(GamepadWire.x, 0x4000)
        XCTAssertEqual(GamepadWire.y, 0x8000)
        XCTAssertEqual(GamepadWire.touchpadClick, 0x10_0000)
        // Every button is enumerated exactly once (releaseAll walks this list).
        let combined: UInt32 = GamepadWire.allButtons.reduce(0) { $0 | $1 }
        XCTAssertEqual(combined, 0x0010_F7FF)
        XCTAssertEqual(GamepadWire.allButtons.count, 16)
        XCTAssertEqual(GamepadWire.allButtons.count, Set(GamepadWire.allButtons).count)
        // Axis ids.
        XCTAssertEqual(GamepadWire.axisLSX, 0)
        XCTAssertEqual(GamepadWire.axisLSY, 1)
        XCTAssertEqual(GamepadWire.axisRSX, 2)
        XCTAssertEqual(GamepadWire.axisRSY, 3)
        XCTAssertEqual(GamepadWire.axisLT, 4)
        XCTAssertEqual(GamepadWire.axisRT, 5)
    }

    func testTouchpadConversionCorners() {
        // GC ±1 with +y up → wire 0...65535 with origin top-left, +y down.
        let topLeft = GamepadWire.touchpad(x: -1, y: 1)
        XCTAssertEqual(topLeft.x, 0)
        XCTAssertEqual(topLeft.y, 0)
        let bottomRight = GamepadWire.touchpad(x: 1, y: -1)
        XCTAssertEqual(bottomRight.x, 65535)
        XCTAssertEqual(bottomRight.y, 65535)
        let center = GamepadWire.touchpad(x: 0, y: 0)
        XCTAssertEqual(Int(center.x), 32768, accuracy: 1)
        XCTAssertEqual(Int(center.y), 32768, accuracy: 1)
        // Out-of-range input clamps instead of wrapping.
        let wild = GamepadWire.touchpad(x: 5, y: -7)
        XCTAssertEqual(wild.x, 65535)
        XCTAssertEqual(wild.y, 65535)
    }

    func testMotionScalingAndClamping() {
        // 20 raw LSB per deg/s — one full revolution per second (360 deg/s = 2π rad/s).
        XCTAssertEqual(GamepadWire.motionRaw(2 * .pi, scale: GamepadWire.gyroLSBPerRadS), 7200)
        // 1 g → 10000 raw.
        XCTAssertEqual(GamepadWire.motionRaw(1, scale: GamepadWire.accelLSBPerG), 10000)
        XCTAssertEqual(GamepadWire.motionRaw(-1, scale: GamepadWire.accelLSBPerG), -10000)
        // Saturation, not overflow.
        XCTAssertEqual(GamepadWire.motionRaw(100, scale: GamepadWire.accelLSBPerG), Int16.max)
        XCTAssertEqual(GamepadWire.motionRaw(-100, scale: GamepadWire.accelLSBPerG), Int16.min)
        XCTAssertEqual(GamepadWire.motionRaw(0, scale: GamepadWire.gyroLSBPerRadS), 0)
    }

    func testPlayerIndexMap() {
        // hid-playstation's DualSense player-LED patterns.
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0), .indexUnset)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b00100), .index1)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b01010), .index2)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b10101), .index3)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b11011), .index4)
        // Unknown patterns: lit-count fallback, clamped to GC's four indices.
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b00001), .index1)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b00011), .index2)
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b11111), .index4)
        // Bits above the 5 LEDs are ignored.
        XCTAssertEqual(GamepadFeedback.playerIndex(forBits: 0b1110_0000), .indexUnset)
    }
}
