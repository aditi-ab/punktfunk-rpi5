// IMU liveness gate for the raw SC2 state-report feed — the policy proven against real hardware
// on the bench (2026-06-08), kept aligned with the Kotlin sibling (`Sc2ImuGate.kt`); the two
// implementations must not drift.
//
// The controller streams gyro/accel only after the host writes `SETTING_IMU_MODE` (reg 0x30);
// until then the IMU block — including its leading u32 timestamp — is FROZEN at a stale non-zero
// resting sample (byte-frozen across 600 frames in the 2026-06-08 capture). Forwarded verbatim,
// that constant non-zero gyro reads to Steam's desktop config as a *constant* rotation and flies
// the cursor (hardware-confirmed both directions 2026-06-08). So: pass the IMU through only
// while its timestamp is advancing, and zero the whole block (timestamp included) while frozen.
// Self-correcting, no hardcoded gyro-enable: on the desktop the IMU stays off → frozen → zeros →
// calm cursor; a gyro game makes Steam send the enable (feature `01 87 03 30 18 00`, replayed to
// the pad by `Sc2Capture.onHidRaw`) → the timestamp starts ticking → live data flows.
//
// Gated shapes: `0x42` (USB state, 54 B wire) and `0x45` (BLE state, 46 B wire) — both are
// `[report id][pack(1) TritonMTUNoQuat_t]`, so the IMU block (u32 timestamp + 3× i16 accel +
// 3× i16 gyro) sits at wire offset 30 (struct offset 29 + the id byte) in both. `0x47` is
// deliberately NOT gated: its layout diverges from byte 18 (inserted trackpad timestamp), no
// capture pins its IMU offset down, and the char it rides is not even subscribed here.
//
// Single-threaded by contract: `apply` runs where the reports are handled; `reset` runs from the
// same teardown paths that already touch the slot state (the `Sc2Capture` locking contract).
// Pure logic, no CoreBluetooth — `Sc2ImuGateTests` pins the whole state machine.

import Foundation

final class Sc2ImuGate {
    /// Wire offset of `TritonMTUNoQuat_t.imu` — struct offset 29 + 1 report-id byte; identical
    /// in the 0x42 and 0x45 shapes (both carry the same pack(1) struct).
    static let imuOffset = 30

    /// u32 timestamp + 3× i16 accel + 3× i16 gyro.
    static let imuLen = 16

    /// Unchanged-timestamp frames before declaring the IMU frozen (bench-tuned 2026-06-08:
    /// three repeats still pass, the fourth freezes).
    static let staleLimit = 4

    private var lastTs: UInt32 = 0
    private var haveTs = false
    private var stale = 0

    /// Re-arm (forget the timestamp history) — called at capture start, on BLE disconnect, and
    /// on every slot teardown, so whatever connects next must re-prove its IMU live before the
    /// block passes through.
    func reset() {
        lastTs = 0
        haveTs = false
        stale = 0
    }

    /// Gate `report` in place, before it is forwarded. Non-state ids and reports too short to
    /// carry a full IMU block pass through untouched; a state report whose IMU timestamp has not
    /// advanced for `staleLimit` consecutive frames — or that has no history yet (unknown until
    /// it moves, so treated as frozen) — gets its IMU block zeroed. A live stream tolerates
    /// short repeats (the report rate can exceed the IMU sample rate).
    func apply(_ report: inout [UInt8]) {
        guard report.count >= Self.imuOffset + Self.imuLen else { return }
        switch report[0] {
        case Sc2Device.idState, Sc2Device.idStateBLE: break
        default: return
        }
        let o = Self.imuOffset
        let ts = UInt32(report[o]) | (UInt32(report[o + 1]) << 8)
            | (UInt32(report[o + 2]) << 16) | (UInt32(report[o + 3]) << 24)
        let live: Bool
        if !haveTs {
            haveTs = true
            stale = Self.staleLimit // unknown until it moves → treat as frozen
            live = false
        } else if ts != lastTs {
            stale = 0
            live = true
        } else {
            if stale < Self.staleLimit { stale += 1 }
            live = stale < Self.staleLimit
        }
        lastTs = ts
        if !live {
            for i in o ..< o + Self.imuLen {
                report[i] = 0
            }
        }
    }
}
