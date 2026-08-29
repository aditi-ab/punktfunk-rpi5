// The frozen-timestamp IMU gate's full truth table — the state machine proven against real
// hardware (2026-06-08: a frozen non-zero IMU block drives Steam's desktop gyro-mouse; the
// timestamp-gated passthrough is the fix), re-pinned here for the Apple side. The two
// implementations (Sc2ImuGate.kt / Sc2ImuGate.swift) must not drift:
// first sample frozen, ts-change → live, STALE_LIMIT (4) unchanged frames → refrozen, reset
// re-arms, and only the 0x42/0x45 state shapes are ever touched.

import XCTest

@testable import PunktfunkKit

final class Sc2ImuGateTests: XCTestCase {
    /// A 46-byte state report whose IMU block (wire offset 30, 16 bytes) is a nonzero resting
    /// sample with `ts` planted in its leading u32 — the frozen-capture shape.
    private func report(id: UInt8 = Sc2Device.idStateBLE, ts: UInt32, count: Int = 46) -> [UInt8] {
        var r = [UInt8](repeating: 0, count: count)
        r[0] = id
        guard count >= Sc2ImuGate.imuOffset + Sc2ImuGate.imuLen else { return r }
        let o = Sc2ImuGate.imuOffset
        for i in o ..< o + Sc2ImuGate.imuLen {
            r[i] = 0xAA // a non-zero "resting accel" fill — what must never leak while frozen
        }
        r[o] = UInt8(ts & 0xFF)
        r[o + 1] = UInt8((ts >> 8) & 0xFF)
        r[o + 2] = UInt8((ts >> 16) & 0xFF)
        r[o + 3] = UInt8((ts >> 24) & 0xFF)
        return r
    }

    private func imuBlock(_ r: [UInt8]) -> [UInt8] {
        Array(r[Sc2ImuGate.imuOffset ..< Sc2ImuGate.imuOffset + Sc2ImuGate.imuLen])
    }

    func testFirstSampleIsFrozen() {
        let gate = Sc2ImuGate()
        var r = report(ts: 0x1234_5678)
        gate.apply(&r)
        // Unknown until it moves → treated as frozen: the whole 16-byte block (timestamp
        // included) is zeroed, and nothing outside it is touched.
        XCTAssertEqual(imuBlock(r), [UInt8](repeating: 0, count: Sc2ImuGate.imuLen))
        XCTAssertEqual(r[0], Sc2Device.idStateBLE)
        XCTAssertEqual(Array(r[1 ..< Sc2ImuGate.imuOffset]), [UInt8](repeating: 0, count: 29))
    }

    func testAdvancingTimestampPassesThrough() {
        let gate = Sc2ImuGate()
        var first = report(ts: 100)
        gate.apply(&first)
        var second = report(ts: 101)
        gate.apply(&second)
        XCTAssertEqual(second, report(ts: 101), "an advancing timestamp must pass untouched")
    }

    func testRefreezesAfterStaleLimitUnchangedFrames() {
        let gate = Sc2ImuGate()
        var r = report(ts: 100)
        gate.apply(&r) // first sample: frozen
        r = report(ts: 101)
        gate.apply(&r) // advanced: live
        XCTAssertEqual(r, report(ts: 101))
        // A live stream tolerates short repeats (report rate > IMU sample rate): the first
        // STALE_LIMIT-1 unchanged frames still pass…
        for i in 1 ..< Sc2ImuGate.staleLimit {
            var same = report(ts: 101)
            gate.apply(&same)
            XCTAssertEqual(same, report(ts: 101), "repeat \(i) of \(Sc2ImuGate.staleLimit) must pass")
        }
        // …and the STALE_LIMITth declares it frozen again.
        var frozen = report(ts: 101)
        gate.apply(&frozen)
        XCTAssertEqual(imuBlock(frozen), [UInt8](repeating: 0, count: Sc2ImuGate.imuLen))
        // Self-correcting: the next advance re-opens the gate at once.
        var revived = report(ts: 102)
        gate.apply(&revived)
        XCTAssertEqual(revived, report(ts: 102))
    }

    func testResetRearmsTheFirstSampleRule() {
        let gate = Sc2ImuGate()
        var r = report(ts: 1)
        gate.apply(&r)
        r = report(ts: 2)
        gate.apply(&r)
        XCTAssertEqual(r, report(ts: 2)) // live
        gate.reset()
        // Whatever connects next must re-prove its IMU live — even an "advancing" timestamp is
        // history-less after reset and starts frozen.
        var next = report(ts: 3)
        gate.apply(&next)
        XCTAssertEqual(imuBlock(next), [UInt8](repeating: 0, count: Sc2ImuGate.imuLen))
    }

    func testTimestampShapeAndNonStateIdsAreExempt() {
        let gate = Sc2ImuGate()
        // 0x47 diverges from byte 18 (inserted trackpad timestamp) — never gated, AND never
        // consumes gate history: the 0x45 after it is still the first sample.
        var ts47 = report(id: Sc2Device.idStateTimestamp, ts: 7)
        gate.apply(&ts47)
        XCTAssertEqual(ts47, report(id: Sc2Device.idStateTimestamp, ts: 7))
        var battery = report(id: Sc2Device.idBattery, ts: 7)
        gate.apply(&battery)
        XCTAssertEqual(battery, report(id: Sc2Device.idBattery, ts: 7))
        var first45 = report(ts: 7)
        gate.apply(&first45)
        XCTAssertEqual(
            imuBlock(first45), [UInt8](repeating: 0, count: Sc2ImuGate.imuLen),
            "0x47/battery must not have seeded the timestamp history")
    }

    func testShortReportPassesUntouched() {
        let gate = Sc2ImuGate()
        // One byte short of a full IMU block — no gating, no zeroing, no history.
        var short = report(ts: 9, count: Sc2ImuGate.imuOffset + Sc2ImuGate.imuLen - 1)
        let before = short
        gate.apply(&short)
        XCTAssertEqual(short, before)
    }
}
