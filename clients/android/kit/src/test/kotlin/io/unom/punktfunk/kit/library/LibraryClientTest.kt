package io.unom.punktfunk.kit.library

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The Android mgmt client's contract with the desktop and Apple ones. */
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
}
