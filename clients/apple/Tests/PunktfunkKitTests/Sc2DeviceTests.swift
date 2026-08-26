// The Steam Controller 2 protocol tables: the per-report characteristic map, the stripped-length
// table's parity with the host's id-INCLUDED `pf_driver_proto::triton::out_report_len`, the
// feature-command bytes (hardware-confirmed 2026-06-08), the state parser and the WIRE_MAP —
// all pure statics on `Sc2Device` (the DualSenseHIDTests convention: pin the wire layout
// without a physical pad). Plus the escape-chord invariant mirror (GamepadEscapeChordTests
// pattern): `Sc2Capture` re-declares the chord because the original is main-actor-isolated,
// and the two must not drift.

import XCTest

@testable import PunktfunkKit

final class Sc2DeviceTests: XCTestCase {
    func testOutputCharUUIDIsIdPlus0x35() {
        // Valve routes output report id 0xNN to characteristic 100F6C<NN+0x35> (verified
        // per-actuator on-device 2026-06-09).
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x80), "100f6cb5-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x81), "100f6cb6-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x82), "100f6cb7-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x83), "100f6cb8-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x84), "100f6cb9-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x85), "100f6cba-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x86), "100f6cbb-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x87), "100f6cbc-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x88), "100f6cbd-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x89), "100f6cbe-1735-4313-b402-38567131e5f3")
        // The +0x35 wraps modulo 256 rather than overflowing.
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0xF0), "100f6c25-1735-4313-b402-38567131e5f3")
    }

    func testStrippedLenPlusOneMatchesTheHostTableForEveryKnownId() {
        // The host's id-INCLUDED wire lengths, verbatim from
        // `pf_driver_proto::triton::out_report_len` (crates/pf-driver-proto/src/lib.rs): a
        // Swift-side edit that drifts from the Rust contract fails here, GamepadWireTests-style.
        let hostLen: [UInt8: Int] = [
            0x80: 10, 0x81: 8, 0x82: 4, 0x83: 10, 0x84: 9, 0x85: 4, 0x86: 4,
            0x87: 64, 0x88: 64, 0x89: 64,
        ]
        for (id, host) in hostLen {
            let stripped = Sc2Device.strippedOutputLen(id: id)
            XCTAssertNotNil(stripped, "id 0x\(String(id, radix: 16)) missing from the client table")
            XCTAssertEqual(
                (stripped ?? -999) + 1, host,
                "stripped+1 must equal the host wire length for id 0x\(String(id, radix: 16))")
        }
        // Unknown ids answer nil — clamp to what arrived, never guess a length (the host's
        // default arm is 64 = no trim, the same "never guess" policy from the other side).
        XCTAssertNil(Sc2Device.strippedOutputLen(id: 0x8A))
        XCTAssertNil(Sc2Device.strippedOutputLen(id: 0x42))
        XCTAssertNil(Sc2Device.strippedOutputLen(id: 0x00))
    }

    func testFeatureCommandBytesVerbatim() {
        // DISABLE_LIZARD: [1][0x87 ID_SET_SETTINGS_VALUES][3][9 SETTING_LIZARD_MODE][0 0 u16],
        // zero-padded to the 64-byte feature size (Android sends the identical frame).
        XCTAssertEqual(Sc2Device.disableLizard.count, 64)
        XCTAssertEqual(
            Array(Sc2Device.disableLizard[0 ..< 6]), [0x01, 0x87, 0x03, 0x09, 0x00, 0x00])
        XCTAssertTrue(Sc2Device.disableLizard[6...].allSatisfy { $0 == 0 })
        // The gyro-enable REFERENCE (WRITE_REGISTER reg 0x30 GYRO_MODE val 0x0018) — kept for
        // logging/tests only; nothing in the client may ever send it unprompted.
        XCTAssertEqual(Sc2Device.gyroEnableReference, [0x01, 0x87, 0x03, 0x30, 0x18, 0x00])
        // SDL's lizard-off refresh cadence.
        XCTAssertEqual(Sc2Device.lizardRefreshSeconds, 3.0)
    }

    /// One 46-byte BLE-shaped state report with the client-consumed fields planted.
    private func stateReport(
        id: UInt8 = Sc2Device.idStateBLE, buttons: UInt32 = 0,
        lt: Int16 = 0, rt: Int16 = 0,
        lsX: Int16 = 0, lsY: Int16 = 0, rsX: Int16 = 0, rsY: Int16 = 0
    ) -> [UInt8] {
        var r = [UInt8](repeating: 0, count: 46)
        r[0] = id
        r[1] = 0x42 // seq — parseState must not read it
        func put32(_ v: UInt32, at o: Int) {
            r[o] = UInt8(v & 0xFF)
            r[o + 1] = UInt8((v >> 8) & 0xFF)
            r[o + 2] = UInt8((v >> 16) & 0xFF)
            r[o + 3] = UInt8((v >> 24) & 0xFF)
        }
        func put16(_ v: Int16, at o: Int) {
            let u = UInt16(bitPattern: v)
            r[o] = UInt8(u & 0xFF)
            r[o + 1] = UInt8(u >> 8)
        }
        put32(buttons, at: 2)
        put16(lt, at: 6)
        put16(rt, at: 8)
        put16(lsX, at: 10)
        put16(lsY, at: 12)
        put16(rsX, at: 14)
        put16(rsY, at: 16)
        return r
    }

    func testParseStateTruthTable() {
        var out = Sc2Device.State()
        // Buttons LE u32 @2; triggers i16 @6/@8 clamped to 0...32767 then >>7; sticks i16
        // @10..16 — Android's parseState, byte for byte.
        let report = stateReport(
            buttons: Sc2Device.btnA | Sc2Device.btnSteam | Sc2Device.btnRPadClick,
            lt: 32767, rt: -100, lsX: -32768, lsY: 32767, rsX: 1234, rsY: -1234)
        XCTAssertTrue(Sc2Device.parseState(report, into: &out))
        XCTAssertEqual(out.buttons, Sc2Device.btnA | Sc2Device.btnSteam | Sc2Device.btnRPadClick)
        XCTAssertEqual(out.lt, 255) // 32767 >> 7
        XCTAssertEqual(out.rt, 0) // negative clamps to 0
        XCTAssertEqual(out.lsX, -32768)
        XCTAssertEqual(out.lsY, 32767)
        XCTAssertEqual(out.rsX, 1234)
        XCTAssertEqual(out.rsY, -1234)
        // All three state shapes parse (identical offsets for everything read here)…
        XCTAssertTrue(Sc2Device.parseState(stateReport(id: Sc2Device.idState), into: &out))
        XCTAssertTrue(
            Sc2Device.parseState(stateReport(id: Sc2Device.idStateTimestamp), into: &out))
        // …and non-state / short reports answer false.
        XCTAssertFalse(Sc2Device.parseState([Sc2Device.idBattery, 0, 0], into: &out))
        var short = stateReport()
        short.removeSubrange(17...)
        XCTAssertFalse(Sc2Device.parseState(short, into: &out))
    }

    func testWireMapMatchesAndroidPairForPair() {
        // The full SC2-bit → GamepadWire-bit table (Sc2Device.kt WIRE_MAP): paddles R4/L4/R5/L5
        // = PADDLE1..4, QAM = MISC1, right-pad click = the touchpad wire bit.
        let expected: [(UInt32, UInt32)] = [
            (Sc2Device.btnA, GamepadWire.a),
            (Sc2Device.btnB, GamepadWire.b),
            (Sc2Device.btnX, GamepadWire.x),
            (Sc2Device.btnY, GamepadWire.y),
            (Sc2Device.btnLB, GamepadWire.leftShoulder),
            (Sc2Device.btnRB, GamepadWire.rightShoulder),
            (Sc2Device.btnView, GamepadWire.back),
            (Sc2Device.btnMenu, GamepadWire.start),
            (Sc2Device.btnSteam, GamepadWire.guide),
            (Sc2Device.btnL3, GamepadWire.leftStickClick),
            (Sc2Device.btnR3, GamepadWire.rightStickClick),
            (Sc2Device.btnDpadUp, GamepadWire.dpadUp),
            (Sc2Device.btnDpadDown, GamepadWire.dpadDown),
            (Sc2Device.btnDpadLeft, GamepadWire.dpadLeft),
            (Sc2Device.btnDpadRight, GamepadWire.dpadRight),
            (Sc2Device.btnQAM, GamepadWire.misc1),
            (Sc2Device.btnR4, GamepadWire.paddle1),
            (Sc2Device.btnL4, GamepadWire.paddle2),
            (Sc2Device.btnR5, GamepadWire.paddle3),
            (Sc2Device.btnL5, GamepadWire.paddle4),
            (Sc2Device.btnRPadClick, GamepadWire.touchpadClick),
        ]
        XCTAssertEqual(Sc2Device.wireMap.count, expected.count)
        for (sc2, wire) in expected {
            XCTAssertEqual(Sc2Device.wireButtons(sc2), wire, "sc2 bit 0x\(String(sc2, radix: 16))")
        }
        // Every mapped bit at once, and nothing else.
        let allSc2 = expected.reduce(UInt32(0)) { $0 | $1.0 }
        let allWire = expected.reduce(UInt32(0)) { $0 | $1.1 }
        XCTAssertEqual(Sc2Device.wireButtons(allSc2), allWire)
        // Unmapped SC2 bits translate to nothing.
        XCTAssertEqual(Sc2Device.wireButtons(~allSc2), 0)
        XCTAssertEqual(Sc2Device.wireButtons(0), 0)
    }
}

#if os(iOS) || os(macOS)
/// `Sc2Capture` re-declares the escape chord (the original is `@MainActor`-isolated and the
/// capture reads its mask on the BLE queue) — this pins the two masks and the hold duration
/// together, because the failure of a drift is invisible until someone can't leave a stream
/// with a captured SC2 in their hands (the GamepadEscapeChordTests rationale, one class over).
@MainActor
final class Sc2EscapeChordMirrorTests: XCTestCase {
    func testChordMaskMirrorsGamepadCapture() {
        XCTAssertEqual(Sc2Capture.escapeChord, GamepadCapture.escapeChord)
    }

    func testHoldMirrorsTheCrossClientDisconnectHold() {
        // pf-client-core's DISCONNECT_HOLD — 1.5 s on every client (GamepadCapture's own copy
        // is private; the value is the cross-client contract being pinned).
        XCTAssertEqual(Sc2Capture.disconnectHold, 1.5)
    }
}
#endif
