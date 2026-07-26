// The gamepad wire contract: button bit positions (must match
// punktfunk_core::input::gamepad), GC→DualSense touchpad/motion conversions, and the
// player-LED-bits → GCControllerPlayerIndex map. All pure functions.

import GameController
import PunktfunkCore
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
        XCTAssertEqual(GamepadWire.misc1, 0x0020_0000)
        // Every button is enumerated exactly once (releaseAll walks this list).
        let combined: UInt32 = GamepadWire.allButtons.reduce(0) { $0 | $1 }
        XCTAssertEqual(combined, 0x0030_F7FF)
        XCTAssertEqual(GamepadWire.allButtons.count, 17)
        XCTAssertEqual(GamepadWire.allButtons.count, Set(GamepadWire.allButtons).count)
        // Paddles are defined but not yet forwarded, so they stay out of allButtons for now.
        for paddle in [GamepadWire.paddle1, GamepadWire.paddle2, GamepadWire.paddle3, GamepadWire.paddle4] {
            XCTAssertFalse(GamepadWire.allButtons.contains(paddle))
        }
        // Axis ids.
        XCTAssertEqual(GamepadWire.axisLSX, 0)
        XCTAssertEqual(GamepadWire.axisLSY, 1)
        XCTAssertEqual(GamepadWire.axisRSX, 2)
        XCTAssertEqual(GamepadWire.axisRSY, 3)
        XCTAssertEqual(GamepadWire.axisLT, 4)
        XCTAssertEqual(GamepadWire.axisRT, 5)
    }

    func testButtonBitsMatchTheCABIVerbatim() {
        // Assert EVERY wire constant against the generated C ABI header (punktfunk_core.h, the same
        // source `punktfunk_core::input::gamepad` emits), so a Swift-side edit that drifts from the
        // Rust contract fails CI — not just the handful spot-checked above. (Cross-cutting review
        // finding G15: the button values were re-declared per client with only a 3-of-19 check.)
        XCTAssertEqual(GamepadWire.dpadUp, UInt32(PUNKTFUNK_BTN_DPAD_UP))
        XCTAssertEqual(GamepadWire.dpadDown, UInt32(PUNKTFUNK_BTN_DPAD_DOWN))
        XCTAssertEqual(GamepadWire.dpadLeft, UInt32(PUNKTFUNK_BTN_DPAD_LEFT))
        XCTAssertEqual(GamepadWire.dpadRight, UInt32(PUNKTFUNK_BTN_DPAD_RIGHT))
        XCTAssertEqual(GamepadWire.start, UInt32(PUNKTFUNK_BTN_START))
        XCTAssertEqual(GamepadWire.back, UInt32(PUNKTFUNK_BTN_BACK))
        XCTAssertEqual(GamepadWire.leftStickClick, UInt32(PUNKTFUNK_BTN_LS_CLICK))
        XCTAssertEqual(GamepadWire.rightStickClick, UInt32(PUNKTFUNK_BTN_RS_CLICK))
        XCTAssertEqual(GamepadWire.leftShoulder, UInt32(PUNKTFUNK_BTN_LB))
        XCTAssertEqual(GamepadWire.rightShoulder, UInt32(PUNKTFUNK_BTN_RB))
        XCTAssertEqual(GamepadWire.guide, UInt32(PUNKTFUNK_BTN_GUIDE))
        XCTAssertEqual(GamepadWire.a, UInt32(PUNKTFUNK_BTN_A))
        XCTAssertEqual(GamepadWire.b, UInt32(PUNKTFUNK_BTN_B))
        XCTAssertEqual(GamepadWire.x, UInt32(PUNKTFUNK_BTN_X))
        XCTAssertEqual(GamepadWire.y, UInt32(PUNKTFUNK_BTN_Y))
        XCTAssertEqual(GamepadWire.touchpadClick, UInt32(PUNKTFUNK_BTN_TOUCHPAD))
        XCTAssertEqual(GamepadWire.misc1, UInt32(PUNKTFUNK_GAMEPAD_BTN_MISC1))
        XCTAssertEqual(GamepadWire.paddle1, UInt32(PUNKTFUNK_GAMEPAD_BTN_PADDLE1))
        XCTAssertEqual(GamepadWire.paddle2, UInt32(PUNKTFUNK_GAMEPAD_BTN_PADDLE2))
        XCTAssertEqual(GamepadWire.paddle3, UInt32(PUNKTFUNK_GAMEPAD_BTN_PADDLE3))
        XCTAssertEqual(GamepadWire.paddle4, UInt32(PUNKTFUNK_GAMEPAD_BTN_PADDLE4))
        // Axis ids and pad count share the same header.
        XCTAssertEqual(GamepadWire.axisLSX, UInt32(PUNKTFUNK_AXIS_LS_X))
        XCTAssertEqual(GamepadWire.axisLSY, UInt32(PUNKTFUNK_AXIS_LS_Y))
        XCTAssertEqual(GamepadWire.axisRSX, UInt32(PUNKTFUNK_AXIS_RS_X))
        XCTAssertEqual(GamepadWire.axisRSY, UInt32(PUNKTFUNK_AXIS_RS_Y))
        XCTAssertEqual(GamepadWire.axisLT, UInt32(PUNKTFUNK_AXIS_LT))
        XCTAssertEqual(GamepadWire.axisRT, UInt32(PUNKTFUNK_AXIS_RT))
        XCTAssertEqual(GamepadWire.maxPads, Int(MAX_PADS))
    }

    func testPadIndexRidesFlagsOnEveryPerPadEvent() {
        // The wire pad index is the low byte of `flags` (punktfunk_core::input) on button + axis.
        let btn = PunktfunkInputEvent.gamepadButton(GamepadWire.a, down: true, pad: 3)
        XCTAssertEqual(btn.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_BUTTON.rawValue))
        XCTAssertEqual(btn.code, GamepadWire.a)
        XCTAssertEqual(btn.x, 1)
        XCTAssertEqual(btn.flags, 3)
        let axis = PunktfunkInputEvent.gamepadAxis(GamepadWire.axisRT, value: 200, pad: 5)
        XCTAssertEqual(axis.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_AXIS.rawValue))
        XCTAssertEqual(axis.code, GamepadWire.axisRT)
        XCTAssertEqual(axis.x, 200)
        XCTAssertEqual(axis.flags, 5)
        // Single-controller path stays byte-identical: pad 0 ⇒ flags 0, exactly as before.
        XCTAssertEqual(PunktfunkInputEvent.gamepadButton(GamepadWire.a, down: false, pad: 0).flags, 0)
        XCTAssertEqual(PunktfunkInputEvent.gamepadAxis(GamepadWire.axisLSX, value: 0, pad: 0).flags, 0)
    }

    func testArrivalAndRemoveWireLayout() {
        // GamepadArrival (kind 14): code = the GamepadType wire byte, flags = pad index.
        let arrival = PunktfunkInputEvent.gamepadArrival(
            pref: PunktfunkConnection.GamepadType.dualSense.rawValue, pad: 2)
        XCTAssertEqual(arrival.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_ARRIVAL.rawValue))
        XCTAssertEqual(arrival.code, PunktfunkConnection.GamepadType.dualSense.rawValue) // 2
        XCTAssertEqual(arrival.flags, 2)
        // The GamepadType raw values ARE the GamepadPref wire bytes the host resolves.
        XCTAssertEqual(PunktfunkConnection.GamepadType.xbox360.rawValue, 1)
        XCTAssertEqual(PunktfunkConnection.GamepadType.dualSense.rawValue, 2)
        XCTAssertEqual(PunktfunkConnection.GamepadType.xboxOne.rawValue, 3)
        XCTAssertEqual(PunktfunkConnection.GamepadType.dualShock4.rawValue, 4)
        // GamepadRemove (kind 13): flags = pad index (the core stamps the per-pad seq).
        let remove = PunktfunkInputEvent.gamepadRemove(pad: 7)
        XCTAssertEqual(remove.kind, UInt8(PUNKTFUNK_INPUT_KIND_GAMEPAD_REMOVE.rawValue))
        XCTAssertEqual(remove.flags, 7)
        // 16 addressable pads (punktfunk_core::input::MAX_PADS).
        XCTAssertEqual(GamepadWire.maxPads, 16)
    }

    func testAnExplicitControllerTypeIsWhatEveryPadDeclares() {
        // The regression this pins: the "Controller type" setting reached the Hello only, and each
        // pad's arrival then re-declared the DETECTED kind — which the host honors over the
        // session default, so "emulate my DualSense as a DualShock 4" produced a DualSense.
        // (pf-client-core's `declared_kind` test is the same table.)
        XCTAssertEqual(
            GamepadManager.declaredKind(setting: .dualShock4, detected: .dualSense), .dualShock4)
        // Every physical pad in a mixed session follows the one explicit choice.
        for detected: PunktfunkConnection.GamepadType in [
            .dualSense, .xbox360, .switchPro, .dualSenseEdge,
        ] {
            XCTAssertEqual(
                GamepadManager.declaredKind(setting: .xbox360, detected: detected), .xbox360)
        }
        // Automatic keeps per-pad detection — otherwise a mixed session collapses to one type.
        XCTAssertEqual(GamepadManager.declaredKind(setting: .auto, detected: .dualSense), .dualSense)
        XCTAssertEqual(GamepadManager.declaredKind(setting: .auto, detected: .switchPro), .switchPro)
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
