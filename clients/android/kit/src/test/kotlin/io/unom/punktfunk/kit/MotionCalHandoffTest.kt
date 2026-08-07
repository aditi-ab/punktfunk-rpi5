package io.unom.punktfunk.kit

import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The claim/read hand-off that lets [DsCapture] read a pad's motion calibration off the claiming
 * thread. What is pinned here is what happens when the read does NOT come back inside its claim:
 * an unplug, a stop, or a re-claim can all land first, and a straggler that published anyway would
 * scale a pad it never read — the one hazard the threading introduces.
 */
class MotionCalHandoffTest {
    /**
     * A calibration whose gyro scale is [rawLsbPerDegS] raw LSB per °/s, so two of them are told
     * apart by what they do and not only by identity.
     */
    private fun cal(rawLsbPerDegS: Int): DsDevice.MotionCal {
        val speed = 500 // speed_plus = speed_minus, so speed_2x = 1000
        val span = rawLsbPerDegS * 1000 // |plus − bias| + |minus − bias| = span
        val blob = ByteArray(41)
        fun put(o: Int, v: Int) {
            blob[o] = (v and 0xFF).toByte()
            blob[o + 1] = ((v shr 8) and 0xFF).toByte()
        }
        blob[0] = 0x05
        for (i in 0 until 3) {
            put(7 + 4 * i, span / 2) // plus
            put(9 + 4 * i, -span / 2) // minus
            put(23 + 4 * i, 8192) // accel plus / minus: nominal, not what this test is about
            put(25 + 4 * i, -8192)
        }
        put(19, speed)
        put(21, speed)
        return DsDevice.MotionCal.parse(blob, 0x05)
    }

    @Test
    fun `a claim's calibration is invisible until its read lands`() {
        val h = MotionCalHandoff()
        assertNull("nothing is claimed yet", h.current)
        val claim = h.begin()
        assertNull("the read is still in flight — the parse must not run", h.current)
        val read = cal(16)
        assertTrue(h.publish(claim, read))
        assertSame(read, h.current)
    }

    @Test
    fun `a read that outlived its claim publishes nothing`() {
        val h = MotionCalHandoff()
        val claim = h.begin()
        h.end() // unplug, or DsCapture.stop, while the read was in flight
        assertFalse("a straggler may not publish into a dead claim", h.publish(claim, cal(16)))
        assertNull(h.current)
    }

    @Test
    fun `a new claim never inherits the previous pad's calibration`() {
        val h = MotionCalHandoff()
        val first = h.begin()
        val hot = cal(4) // a pad whose gyro reads 4 raw LSB per °/s
        assertTrue(h.publish(first, hot))

        // Re-claimed without an end() in between — the pad was swapped while a read was in flight.
        val second = h.begin()
        assertNotEquals(first, second)
        assertNull("the next pad starts with no calibration, not the last one's", h.current)
        assertFalse("the first pad's read may not scale the second pad", h.publish(first, hot))
        assertNull(h.current)

        val slow = cal(32)
        assertTrue(h.publish(second, slow))
        assertSame(slow, h.current)
        // And the two really are different scales, so the assertions above are about a real
        // difference rather than two names for the same numbers.
        assertNotEquals(hot.gyroToWire(0, 100), slow.gyroToWire(0, 100))
    }

    @Test
    fun `ending a claim twice still refuses every outstanding token`() {
        val h = MotionCalHandoff()
        val claim = h.begin()
        h.end() // DsCapture.stop
        h.end() // …and the unplug that followed it
        assertFalse(h.publish(claim, cal(16)))
        val next = h.begin()
        assertNotEquals(claim, next)
        assertTrue(h.publish(next, cal(16)))
    }
}
