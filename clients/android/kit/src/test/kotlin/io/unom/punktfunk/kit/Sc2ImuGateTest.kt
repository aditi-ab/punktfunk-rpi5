package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM tests of [Sc2ImuGate], the IMU liveness gate (the frozen/live/refrozen cases below
 * are the ones that matter on hardware, including a bench-captured frozen frame).
 * Offsets per the 2026-06-07 USB capture: both
 * state shapes are `[id][pack(1) TritonMTUNoQuat_t]`, IMU block (u32 timestamp + 6× i16) at
 * wire offset 30. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class Sc2ImuGateTest {
    private val off = Sc2ImuGate.IMU_OFFSET // 30
    private val imuLen = Sc2ImuGate.IMU_LEN // 16

    /** A 46-byte BLE-shape state report (`[0x45][45-byte payload]`) with IMU timestamp [ts]. */
    private fun bleState(ts: Int, mutate: (ByteArray) -> Unit = {}): ByteArray =
        ByteArray(46).also {
            it[0] = Sc2Device.ID_STATE_BLE.toByte()
            it[off] = ts.toByte()
            it[off + 1] = (ts ushr 8).toByte()
            it[off + 2] = (ts ushr 16).toByte()
            it[off + 3] = (ts ushr 24).toByte()
            mutate(it)
        }

    private fun imuIsZero(r: ByteArray): Boolean =
        (off until off + imuLen).all { r[it] == 0.toByte() }

    // FROZEN: frames whose IMU timestamp never advances (gyro disabled on the
    // controller, the real default). The stale non-zero IMU must come out zeroed on EVERY frame
    // (the first is zeroed too: no history = unknown until it moves), while non-IMU fields
    // still pass through.
    @Test
    fun frozenTimestampZeroesTheImuBlock() {
        val gate = Sc2ImuGate()
        repeat(8) { // > STALE_LIMIT
            val r = bleState(0) {
                for (i in 0 until imuLen) it[off + i] = (0xC0 + i).toByte() // constant IMU
                it[10] = 0xAB.toByte() // live sLeftStickX low byte (struct offset 9)
            }
            gate.apply(r, r.size)
            assertEquals(0xAB.toByte(), r[10]) // non-IMU field preserved
            assertTrue(imuIsZero(r)) // frozen IMU zeroed
        }
    }

    // LIVE: the timestamp advances each frame (gyro enabled, e.g. Steam wrote
    // SETTING_IMU_MODE). The IMU must pass through so real motion reaches Steam — the
    // regression an unconditional zeroing would wrongly clobber.
    @Test
    fun advancingTimestampPassesTheImuThrough() {
        val gate = Sc2ImuGate()
        for (frame in 0 until 8) {
            val ts = 0x1000 + frame * 0x40
            val r = bleState(ts) {
                it[off + 4] = 0x11 // accel sample bytes
                it[off + 5] = 0x22
            }
            gate.apply(r, r.size)
            if (frame == 0) {
                assertTrue(imuIsZero(r)) // no history yet — armed until it moves
            } else {
                assertEquals(ts.toByte(), r[off]) // timestamp survived
                assertEquals(0x11.toByte(), r[off + 4]) // accel survived
                assertEquals(0x22.toByte(), r[off + 5])
            }
        }
    }

    // A live stream tolerates up to STALE_LIMIT-1 consecutive repeats (the report rate can
    // exceed the IMU sample rate); the STALE_LIMITth unchanged frame is declared frozen.
    @Test
    fun staleLimitBoundsRepeatTolerance() {
        val gate = Sc2ImuGate()
        gate.apply(bleState(0x100), 46) // arm
        gate.apply(bleState(0x140), 46) // advance → proven live
        repeat(Sc2ImuGate.STALE_LIMIT - 1) {
            val r = bleState(0x140) { it[off + 4] = 0x11 }
            gate.apply(r, r.size)
            assertEquals(0x11.toByte(), r[off + 4]) // repeat within tolerance: still live
        }
        val r = bleState(0x140) { it[off + 4] = 0x11 }
        gate.apply(r, r.size)
        assertTrue(imuIsZero(r)) // STALE_LIMITth consecutive repeat: frozen
    }

    // reset() re-arms: after a reconnect the pad must re-prove liveness even if its first
    // timestamp happens to differ from the last pre-reset one.
    @Test
    fun resetRearmsTheGate() {
        val gate = Sc2ImuGate()
        gate.apply(bleState(0x100), 46)
        val live = bleState(0x140) { it[off + 4] = 0x11 }
        gate.apply(live, live.size)
        assertEquals(0x11.toByte(), live[off + 4]) // proven live

        gate.reset()
        val first = bleState(0x180) { it[off + 4] = 0x11 }
        gate.apply(first, first.size)
        assertTrue(imuIsZero(first)) // history forgotten — frozen until it moves again
        val second = bleState(0x1C0) { it[off + 4] = 0x11 }
        gate.apply(second, second.size)
        assertEquals(0x11.toByte(), second[off + 4]) // advancing again → live
    }

    // The USB 0x42 shape (54 B wire) carries the same pack(1) struct → same offset-30 gate.
    @Test
    fun usbStateShapeIsGatedAtTheSameOffset() {
        val gate = Sc2ImuGate()
        fun usb(ts: Int): ByteArray = ByteArray(54).also {
            it[0] = Sc2Device.ID_STATE.toByte()
            it[off] = ts.toByte()
            it[off + 1] = (ts ushr 8).toByte()
            it[off + 4] = 0x33
        }
        val r0 = usb(0x0500)
        gate.apply(r0, r0.size)
        assertTrue(imuIsZero(r0)) // first frame armed
        val r1 = usb(0x0540)
        gate.apply(r1, r1.size)
        assertEquals(0x33.toByte(), r1[off + 4]) // advancing → live
    }

    // Non-state ids (battery 0x43, wireless 0x79) — and 0x47, whose layout diverges from
    // byte 18 (inserted trackpad timestamp) so offset 30 is NOT its IMU — pass untouched.
    @Test
    fun nonStateAndTimestampShapesAreUntouched() {
        val gate = Sc2ImuGate()
        val ids = intArrayOf(Sc2Device.ID_BATTERY, Sc2Device.ID_WIRELESS, Sc2Device.ID_STATE_TIMESTAMP)
        for (id in ids) {
            repeat(8) {
                val r = ByteArray(46) { 0x5A }
                r[0] = id.toByte()
                gate.apply(r, r.size)
                for (i in 1 until r.size) assertEquals(0x5A.toByte(), r[i])
            }
        }
    }

    // A truncated state report (no full IMU block on board) passes untouched — len is the
    // report's byte count, not the (possibly larger) scratch buffer's.
    @Test
    fun shortReportsAreUntouched() {
        val gate = Sc2ImuGate()
        val r = ByteArray(64) { 0x5A }
        r[0] = Sc2Device.ID_STATE_BLE.toByte()
        gate.apply(r, 30) // ends right where the IMU block would start
        for (i in 1 until r.size) assertEquals(0x5A.toByte(), r[i])
    }

    // A bench-captured controller frame (2026-06-08, live on hardware):
    // sticks/buttons live, IMU tail arrived FROZEN (C3 7A C3 13 …). The gamepad fields must
    // survive verbatim and the frozen IMU must zero out (no gyro-mouse cursor-fly).
    @Test
    fun realCapturedFrozenFrameIsScrubbedButPlayable() {
        val gate = Sc2ImuGate()
        val real = intArrayOf(
            0x45,
            0x00, 0x00, 0x10, 0x31, 0x00, 0x00, 0x00, 0x00, // seq, buttons (pressed)
            0xFD, 0x14, 0x41, 0xB9, 0x84, 0xFA, 0x01, 0x80, // triggers + sticks (off-center)
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // pads
            0xC3, 0x7A, 0xC3, 0x13, 0x02, 0x10, 0xE9, 0x0F, 0x5C, 0x3C, 0x00, 0x00, // frozen IMU
            0xFF, 0xFF, 0x00, 0x00, 0x00,
        ).map { it.toByte() }.toByteArray()
        assertEquals(46, real.size)
        gate.apply(real, real.size)
        assertEquals(0x10.toByte(), real[3]) // buttons survive verbatim
        assertEquals(0x31.toByte(), real[4])
        assertEquals(0xFD.toByte(), real[9]) // sticks survive verbatim
        assertEquals(0x14.toByte(), real[10])
        assertEquals(0x80.toByte(), real[16])
        assertEquals(0xC3.toByte(), real[29]) // last pre-IMU byte (struct offset 28) survives
        assertTrue(imuIsZero(real)) // the frozen C3 7A C3 13 … tail is zeroed
    }
}
