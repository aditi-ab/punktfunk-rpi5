// IMU liveness gate for the raw SC2 state-report feed. The Kotlin and pf-client-core gates
// replay the same trace (`clients/shared/sc2-vectors.json`).
//
// The controller streams gyro/accel only after the host writes `SETTING_IMU_MODE` (reg 0x30).
// Until then the IMU block, timestamp included, is frozen at a non-zero resting sample, which
// Steam's desktop config reads as a constant rotation that flies the cursor. So the IMU passes
// only while its timestamp advances; a frozen block is zeroed. A gyro game makes Steam send the
// enable (feature `01 87 03 30 18 00`, replayed by `Sc2Capture.onHidRaw`) and data flows.
//
// Gated shapes: `0x42` (cabled, 54 B) and `0x45` (wireless, 46 B: BLE pads and Puck slots over
// USB). Both are `[report id][pack(1) TritonMTUNoQuat_t]`, so the IMU block (u32 timestamp +
// 3× i16 accel + 3× i16 gyro) sits at wire offset 30. `0x47` is not gated: its layout diverges
// from byte 18 and the char it rides is not subscribed here.
//
// Single-threaded by contract: `apply` runs where the reports are handled; `reset` runs from the
// same teardown paths that touch the slot state (the `Sc2Capture` locking contract).

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
