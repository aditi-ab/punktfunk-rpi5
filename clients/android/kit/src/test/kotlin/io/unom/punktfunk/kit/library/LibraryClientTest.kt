package io.unom.punktfunk.kit.library

import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The Android mgmt client's contract with the desktop and Apple ones, and the shared wake fetch. */
class LibraryClientTest {
    @Test
    fun an_ipv6_host_is_bracketed_in_the_base_url() {
        assertEquals("https://192.168.1.9:47990", mgmtBase("192.168.1.9", 47990))
        assertEquals("https://desk.local:47990", mgmtBase("desk.local", 47990))
        assertEquals("https://[fe80::1]:47990", mgmtBase("fe80::1", 47990))
        assertEquals("https://[fe80::1]:47990", mgmtBase("[fe80::1]", 47990))
    }

    /** The pinned-host check compares against the name OkHttp hands the verifier. */
    @Test
    fun the_pinned_host_is_named_the_way_okhttp_names_it() {
        assertEquals("2001:db8::1", urlHost("2001:DB8:0:0:0:0:0:1"))
        assertEquals("fe80::1", urlHost("[fe80::1]"))
        assertEquals("desk.local", urlHost("Desk.local"))
        assertEquals("192.168.1.9", urlHost("192.168.1.9"))
    }

    @Test
    fun a_403_reads_as_not_paired_like_a_401() {
        assertTrue(LibraryClient.refused(401) is LibraryResult.Unauthorized)
        assertTrue(LibraryClient.refused(403) is LibraryResult.Unauthorized)
        assertEquals(LibraryResult.Error("the host refused it (500)"), LibraryClient.refused(500))
    }

    @Test
    fun both_shells_file_a_catalog_under_the_record_id_else_the_pin() {
        val desk = KnownHost("192.168.1.9", 9777, "Desk", "ab".repeat(32), paired = true, id = "desk")
        assertEquals("desk", LibraryCache.keyFor(desk, desk.fpHex))
        assertEquals("ab".repeat(32), LibraryCache.keyFor(null, "ab".repeat(32)))
    }

    private val unreachable = LibraryResult.Error("couldn't reach the host")

    private class Run {
        var packets = 0
        var fetches = 0
        var wakingCalls = 0
        var slept = 0L
    }

    private fun fetch(
        waking: Boolean,
        answers: (Int) -> LibraryResult,
        run: Run,
        isCancelled: () -> Boolean = { false },
    ) = LibraryClient.acrossWake(
        waking = waking,
        fetch = { answers(run.fetches++) },
        wake = { run.packets++ },
        isCancelled = isCancelled,
        onWaking = { run.wakingCalls++ },
        sleep = { run.slept += it },
    )

    /**
     * No packet without auto-wake on and a MAC to send to, and nothing may say "waking". A packet
     * here would load the native library, which a JVM test does not have.
     */
    @Test
    fun a_packet_needs_auto_wake_and_a_mac() {
        var waking = 0
        for ((macs, autoWake) in listOf(listOf("aa:bb:cc:dd:ee:ff") to false, emptyList<String>() to true)) {
            val res = LibraryClient.fetchAcrossWake(
                "192.168.1.9", 47990, "", "", "",
                macs = macs, autoWake = autoWake, onWaking = { waking++ },
            )
            assertTrue(res is LibraryResult.Unauthorized)
        }
        assertEquals(0, waking)
        val run = Run()
        fetch(waking = false, answers = { unreachable }, run = run)
        assertEquals(0, run.packets)
        assertEquals(1, run.fetches)
    }

    @Test
    fun a_waking_fetch_retries_across_the_boot_window_and_resends() {
        val run = Run()
        assertEquals(unreachable, fetch(waking = true, answers = { unreachable }, run = run))
        assertEquals(LibraryClient.WAKE_ATTEMPTS, run.fetches)
        assertEquals(1, run.wakingCalls)
        // The first packet, then one after every second miss (none after the last).
        assertEquals(6, run.packets)
        assertEquals((LibraryClient.WAKE_ATTEMPTS - 1) * LibraryClient.WAKE_RETRY_MS, run.slept)
    }

    /** Waiting does not pair a device: a refusal ends the loop at once. */
    @Test
    fun a_settled_answer_ends_the_retries() {
        val run = Run()
        val refusal = LibraryClient.refused(403)
        assertEquals(refusal, fetch(waking = true, answers = { if (it < 2) unreachable else refusal }, run = run))
        assertEquals(3, run.fetches)
        val ok = Run()
        assertTrue(fetch(waking = true, answers = { LibraryResult.Ok(emptyList()) }, run = ok) is LibraryResult.Ok)
        assertEquals(1, ok.fetches)
    }

    @Test
    fun a_cancelled_fetch_stops_asking() {
        val run = Run()
        fetch(waking = true, answers = { unreachable }, run = run, isCancelled = { run.fetches >= 2 })
        assertEquals(2, run.fetches)
    }
}
