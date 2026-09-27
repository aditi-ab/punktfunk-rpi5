package io.unom.punktfunk.kit.discovery

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The wake-and-wait loop both Android shells run, on a fake clock. */
class WakeLoopTest {
    private var clock = 0L
    private var sent = 0
    private val ticks = mutableListOf<Triple<Int, Boolean, Boolean>>()

    private fun wake(isOnline: () -> Boolean, cancelled: () -> Boolean = { false }) = WakeLoop.run(
        send = { sent++ },
        isOnline = isOnline,
        cancelled = cancelled,
        onTick = { s, t, o -> ticks += Triple(s, t, o) },
        now = { clock },
        sleep = { clock += it },
    )

    /** Asked before sent: a host that is already up never gets a packet. */
    @Test
    fun an_awake_host_gets_no_packet() {
        assertTrue(wake(isOnline = { true }))
        assertEquals(0, sent)
        assertEquals(listOf(Triple(0, false, true)), ticks)
    }

    @Test
    fun a_silent_host_gets_a_packet_every_six_seconds_until_the_timeout() {
        assertFalse(wake(isOnline = { false }))
        // 0, 6, … 84: fifteen packets inside the 90 s budget, none at the timeout itself.
        assertEquals(15, sent)
        assertEquals(Triple(WakeLoop.TIMEOUT_S, true, false), ticks.last())
    }

    @Test
    fun a_host_that_answers_ends_the_wait() {
        assertTrue(wake(isOnline = { clock >= 20_000 }))
        assertEquals(4, sent) // 0, 6, 12, 18
        assertEquals(Triple(20, false, true), ticks.last())
    }

    /** A cancelled wait reports nothing: the card it would update may belong to another host. */
    @Test
    fun a_cancelled_wait_stops_without_a_status() {
        var probes = 0
        assertFalse(wake(isOnline = { probes++; false }, cancelled = { probes >= 1 }))
        assertEquals(0, sent)
        assertTrue(ticks.isEmpty())
    }
}
