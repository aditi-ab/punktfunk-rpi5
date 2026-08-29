// Steam Controller 2 (2026, Valve "Ibex" / SDL "Triton", wired 28DE:1302) protocol constants +
// every piece of pure SC2 logic the client needs — framing, the per-report characteristic map,
// the feature commands, and the light state parser. Cross-client parity: this file is the Apple
// sibling of Android's `Sc2Device.kt`, and the transport facts are the ones proven against
// real hardware on the bench (on-glass 2026-06-08/09: live input end-to-end
// after the report-id prepend fix, gyro-enable forwarded over GATT, and full multi-actuator
// haptics via the per-report characteristic map).
//
// The GATT service is Valve's CUSTOM vendor service, NOT standard HID-over-GATT (0x1812) —
// which is exactly why a third-party app may read it raw: iOS keeps its own 0x1812 binding (the
// lizard-mode keyboard/mouse) while we open a second handle to the vendor service, the same
// coexistence Steam Link relies on. The full report rides the punktfunk wire verbatim
// (`PunktfunkConnection.sendHidReport` → the host's as-is virtual 28DE:1302 pad); the parser
// here extracts only what the client itself consumes — the button word for the typed mirror +
// exit chord, and sticks/triggers for the degrade path.
//
// Everything in this file is device-free by design (no CoreBluetooth import), so the tables and
// framing rules are pinned by unit tests that run on any Mac — `Sc2DeviceTests` /
// `Sc2FramingTests` — the same convention as `DualSenseHID`'s report builders.

import Foundation

enum Sc2Device {
    // MARK: - GATT topology (Valve vendor service; cf. Android's `Sc2BleLink.kt`)

    /// The custom Valve service every SC2 exposes over BLE.
    static let serviceUUID = "100F6C32-1735-4313-B402-38567131E5F3"
    /// Input characteristic (notify): report 0x45, a bare 45-byte state payload — the
    /// characteristic VALUE carries NO report-id byte (the id is implied by the UUID), which is
    /// the framing root cause debugged on-glass 2026-06-09: without re-prepending 0x45 an
    /// id-keyed consumer drops ~every frame. See `frameIncoming`.
    static let inputCharUUID = "100F6C7A-1735-4313-B402-38567131E5F3"
    /// Timestamp characteristic (notify): report 0x47. Deliberately NOT subscribed — the bench
    /// stack subscribed it and then ignored every 0x47, Android never subscribes it, and the gyro is
    /// hardware-proven NOT to ride it (the IMU streams inside the SAME 0x45 report once enabled,
    /// bench 2026-06-08). The constant stays for the connect-time characteristic census log.
    static let timestampCharUUID = "100F6C7C-1735-4313-B402-38567131E5F3"
    /// Report/feature characteristic (write/read; notify on some firmware): where FEATURE
    /// commands land — the hardware-proven gyro-enable/lizard-off path. Props 0x0a on-device.
    static let reportCharUUID = "100F6C34-1735-4313-B402-38567131E5F3"

    /// Per-OUTPUT-report characteristic: Valve routes each output report id 0xNN to its OWN
    /// characteristic `100F6C<NN+0x35>` (the id only SELECTS the characteristic and is stripped
    /// from the written payload). Lowercase, the form CoreBluetooth's `CBUUID.uuidString` is
    /// compared against case-insensitively. Verified per-actuator on-device 2026-06-09 — the
    /// 0x82 test buzz failed while mis-routed to the 0x80 characteristic.
    static func outputCharUUID(id: UInt8) -> String {
        String(format: "100f6c%02x-1735-4313-b402-38567131e5f3", (Int(id) + 0x35) & 0xFF)
    }

    /// Declared STRIPPED payload length per output report id (wire length = stripped + 1). The
    /// hardware-verified table; its id-INCLUDED mirror is the host's
    /// `pf_driver_proto::triton::out_report_len`.
    ///
    /// ⚠ The two tables are HAND-MIRRORED, not generated. `Sc2DeviceTests` pins `strippedLen + 1`
    /// against a Swift *transcription* of the host values, so it catches a drift made HERE — it
    /// cannot see a change made on the Rust side, which would leave this table silently stale and
    /// the GATT write trimmed to the wrong length. Editing either table means editing both (the
    /// Rust doc carries the same warning). `nil` = unknown id: clamp to what arrived minus the id
    /// byte, never guess-trim beyond that.
    static func strippedOutputLen(id: UInt8) -> Int? {
        switch id {
        case 0x80: return 9 // grip rumble    → 100F6CB5 (left/right motor fields)
        case 0x81: return 7 // trackpad pulse → 100F6CB6 (side byte 01=L 02=R 03=both; one char)
        case 0x82: return 3 // haptic command → 100F6CB7 (Steam's ping/test buzz)
        case 0x83: return 9 // LFO tone       → 100F6CB8
        case 0x84: return 8 // log sweep      → 100F6CB9
        case 0x85: return 3 // script         → 100F6CBA
        case 0x86: return 3 // vendor         → 100F6CBB
        case 0x87, 0x88, 0x89: return 63 // vendor big → 100F6CBC/BD/BE
        default: return nil
        }
    }

    // MARK: - Input report ids (`ETritonReportIDTypes`)

    static let idState: UInt8 = 0x42
    static let idBattery: UInt8 = 0x43
    static let idStateBLE: UInt8 = 0x45
    static let idWirelessX: UInt8 = 0x46
    static let idStateTimestamp: UInt8 = 0x47
    static let idWireless: UInt8 = 0x79

    // MARK: - Feature commands (Sc2Device.kt; hardware-confirmed 2026-06-08)

    /// The feature report that turns lizard mode (built-in keyboard/mouse emulation) off:
    /// `[report id 1][ID_SET_SETTINGS_VALUES 0x87][length 3][SETTING_LIZARD_MODE 9]
    /// [LIZARD_MODE_OFF u16]`, zero-padded to the 64-byte feature size (the firmware accepts the
    /// padded form — it is exactly what a Windows host's hidclass sends). The firmware watchdog
    /// re-enables lizard mode after a few seconds of silence, so this is re-sent every
    /// `lizardRefreshSeconds` (SDL's cadence) — and the host's Steam sends its own through the
    /// raw plane once it grabs the virtual pad, which lands on the same characteristic.
    static let disableLizard: [UInt8] = {
        var b = [UInt8](repeating: 0, count: 64)
        b[0] = 0x01 // feature report id
        b[1] = 0x87 // ID_SET_SETTINGS_VALUES
        b[2] = 3 // one ControllerSetting {u8 num, u16 value}
        b[3] = 9 // SETTING_LIZARD_MODE
        // [4..6] = LIZARD_MODE_OFF (0) — already zero
        return b
    }()

    /// The gyro-enable Steam itself sends — WRITE_REGISTER, reg 0x30 (GYRO_MODE), value 0x0018
    /// (raw accel | raw gyro); confirmed both ways on real hardware 2026-06-08. Kept ONLY for
    /// logging and tests: the client must NEVER self-enable the gyro (a permanent enable re-flies
    /// the desktop cursor) — Steam's own forwarded write is what opens `Sc2ImuGate`.
    static let gyroEnableReference: [UInt8] = [0x01, 0x87, 0x03, 0x30, 0x18, 0x00]

    /// SDL's lizard-off refresh cadence.
    static let lizardRefreshSeconds: TimeInterval = 3.0

    // MARK: - Button bits in the state report's u32 (SDL `TritonButtons`)

    static let btnA: UInt32 = 0x0000_0001
    static let btnB: UInt32 = 0x0000_0002
    static let btnX: UInt32 = 0x0000_0004
    static let btnY: UInt32 = 0x0000_0008
    static let btnQAM: UInt32 = 0x0000_0010
    static let btnR3: UInt32 = 0x0000_0020
    static let btnView: UInt32 = 0x0000_0040
    static let btnR4: UInt32 = 0x0000_0080
    static let btnR5: UInt32 = 0x0000_0100
    static let btnRB: UInt32 = 0x0000_0200
    static let btnDpadDown: UInt32 = 0x0000_0400
    static let btnDpadRight: UInt32 = 0x0000_0800
    static let btnDpadLeft: UInt32 = 0x0000_1000
    static let btnDpadUp: UInt32 = 0x0000_2000
    static let btnMenu: UInt32 = 0x0000_4000
    static let btnL3: UInt32 = 0x0000_8000
    static let btnSteam: UInt32 = 0x0001_0000
    static let btnL4: UInt32 = 0x0002_0000
    static let btnL5: UInt32 = 0x0004_0000
    static let btnLB: UInt32 = 0x0008_0000
    static let btnRPadClick: UInt32 = 0x0040_0000

    /// Wire mapping: SC2 button bit → punktfunk `GamepadWire` bit, the inverse of the host's
    /// typed-fallback mapping (`triton_proto::from_gamepad`): paddles R4/L4/R5/L5 =
    /// PADDLE1/2/3/4, QAM = MISC1, right-pad click = the touchpad wire bit. Same pairs, same
    /// order, as Android's `Sc2Device.WIRE_MAP`.
    static let wireMap: [(sc2: UInt32, wire: UInt32)] = [
        (btnA, GamepadWire.a),
        (btnB, GamepadWire.b),
        (btnX, GamepadWire.x),
        (btnY, GamepadWire.y),
        (btnLB, GamepadWire.leftShoulder),
        (btnRB, GamepadWire.rightShoulder),
        (btnView, GamepadWire.back),
        (btnMenu, GamepadWire.start),
        (btnSteam, GamepadWire.guide),
        (btnL3, GamepadWire.leftStickClick),
        (btnR3, GamepadWire.rightStickClick),
        (btnDpadUp, GamepadWire.dpadUp),
        (btnDpadDown, GamepadWire.dpadDown),
        (btnDpadLeft, GamepadWire.dpadLeft),
        (btnDpadRight, GamepadWire.dpadRight),
        (btnQAM, GamepadWire.misc1),
        (btnR4, GamepadWire.paddle1),
        (btnL4, GamepadWire.paddle2),
        (btnR5, GamepadWire.paddle3),
        (btnL5, GamepadWire.paddle4),
        (btnRPadClick, GamepadWire.touchpadClick),
    ]

    /// Translate an SC2 button word into the wire `GamepadWire` bitmask.
    static func wireButtons(_ sc2: UInt32) -> UInt32 {
        var out: UInt32 = 0
        for (bit, wire) in wireMap where sc2 & bit != 0 {
            out |= wire
        }
        return out
    }

    // MARK: - State parser (typed mirror + exit chord only; the raw report is the product)

    /// The typed-mirror fields of one state report (buttons/sticks/triggers only).
    struct State: Equatable {
        var buttons: UInt32 = 0 // SC2 bit layout
        var lsX: Int32 = 0 // i16, +y = up (device convention = wire convention)
        var lsY: Int32 = 0
        var rsX: Int32 = 0
        var rsY: Int32 = 0
        var lt: Int32 = 0 // 0...255 (device 0...32767 scaled down)
        var rt: Int32 = 0
    }

    /// Parse the client-consumed fields out of a state report (`0x42`/`0x45`/`0x47` — identical
    /// offsets for everything read here) into `out`. Returns false for non-state/short reports.
    /// Offsets are id-first wire offsets: buttons u32 @2, triggers i16 @6/@8 (`>>7` → 0...255),
    /// sticks i16 @10/@12/@14/@16 — Android's `parseState`, byte for byte.
    static func parseState(_ report: [UInt8], into out: inout State) -> Bool {
        guard report.count >= 18 else { return false }
        switch report[0] {
        case idState, idStateBLE, idStateTimestamp: break
        default: return false
        }
        func i16(_ o: Int) -> Int32 {
            Int32(Int16(bitPattern: UInt16(report[o]) | (UInt16(report[o + 1]) << 8)))
        }
        out.buttons = UInt32(report[2]) | (UInt32(report[3]) << 8)
            | (UInt32(report[4]) << 16) | (UInt32(report[5]) << 24)
        out.lt = min(max(i16(6), 0), 32767) >> 7
        out.rt = min(max(i16(8), 0), 32767) >> 7
        out.lsX = i16(10)
        out.lsY = i16(12)
        out.rsX = i16(14)
        out.rsY = i16(16)
        return true
    }

    // MARK: - Framing (pure; the BLE shim calls these — see Sc2FramingTests)

    /// Incoming (up-path): a GATT characteristic VALUE is the raw payload with NO HID report-id
    /// byte, so re-prepend `0x45` for state-sized (≥ 40 B) payloads — the wire then carries the
    /// same id-first framing as USB, which is punktfunk's contract (the host's virtual pad does
    /// the rest; no 0x45→0x42 rewrite and no 54-byte zero-pad — those belong to a
    /// synthetic-USB queue contract, not ours). Short payloads (battery/status) pass through
    /// unmodified. Observed live rate ~66 Hz, len 45.
    static func frameIncoming(_ payload: [UInt8]) -> [UInt8] {
        guard payload.count >= 40 else { return payload }
        return [idStateBLE] + payload
    }

    /// One resolved OUTPUT write: which per-report characteristic, and the bare payload to put
    /// on it.
    struct OutputWrite: Equatable {
        /// Lowercase characteristic UUID (`outputCharUUID`).
        let charUUID: String
        let payload: [UInt8]
    }

    /// Outgoing OUTPUT (`kind == 0`, HID_RAW_OUTPUT): the frame arrives id-first
    /// `[0xNN][payload…]`; the id SELECTS the per-report characteristic and is STRIPPED, and the
    /// payload is trimmed to the declared stripped length, clamped to what arrived. The clamp is
    /// redundant-but-kept: current Windows hosts already trim each drained OUTPUT frame to
    /// `out_report_len(id)` before the HidRaw push, but older hosts pad to 64 B — and the GATT
    /// write must carry exactly the declared length either way. Unknown id: the whole id-stripped
    /// payload (never guess-trim). `nil` for a frame too short to carry a payload.
    static func outputWrite(frame: [UInt8]) -> OutputWrite? {
        guard frame.count >= 2 else { return nil }
        let id = frame[0]
        let declared = strippedOutputLen(id: id) ?? frame.count - 1
        let n = min(declared, frame.count - 1)
        return OutputWrite(charUUID: outputCharUUID(id: id), payload: Array(frame[1 ..< 1 + n]))
    }

    /// Outgoing FEATURE (`kind == 1`, HID_RAW_FEATURE): the frame is `[0x01][0x87 …]` — strip
    /// the leading 0x01 channel report-id and write the remainder to `100F6C34` (the
    /// hardware-proven gyro/lizard path). FEATURE frames deliberately arrive WHOLE from the host
    /// (64 B, un-trimmed), so trailing zero-padding is passed through — the firmware accepts the
    /// zero-padded form (Android sends `disableLizard` padded to 64 B the same way). NO 0xC0
    /// segment wrapper: proven unnecessary on-device for both feature and output writes.
    /// `nil` for a frame too short to carry a command.
    static func featurePayload(frame: [UInt8]) -> [UInt8]? {
        guard frame.count >= 2 else { return nil }
        return Array(frame.dropFirst())
    }
}
