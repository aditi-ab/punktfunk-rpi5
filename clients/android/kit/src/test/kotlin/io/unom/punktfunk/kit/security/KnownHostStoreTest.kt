package io.unom.punktfunk.kit.security

import org.junit.Assert.assertEquals
import org.junit.Test

/** Unit tests for the pure MAC-parsing helper backing the host edit form. */
class KnownHostStoreTest {
    @Test
    fun parsesAndNormalizesSingleMac() {
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), KnownHostStore.parseMacs("AA:BB:CC:DD:EE:FF"))
    }

    @Test
    fun parsesMultipleSeparators() {
        val expected = listOf("aa:bb:cc:dd:ee:ff", "11:22:33:44:55:66")
        assertEquals(expected, KnownHostStore.parseMacs("aa:bb:cc:dd:ee:ff, 11:22:33:44:55:66"))
        assertEquals(expected, KnownHostStore.parseMacs("aa:bb:cc:dd:ee:ff 11:22:33:44:55:66"))
        assertEquals(expected, KnownHostStore.parseMacs("aa:bb:cc:dd:ee:ff\n11:22:33:44:55:66"))
    }

    @Test
    fun dropsMalformedEntries() {
        // Not six octets / bad hex / wrong width are all dropped; an empty field clears the MAC.
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs(""))
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("not-a-mac"))
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("aa:bb:cc:dd:ee"))     // 5 octets
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("gg:bb:cc:dd:ee:ff"))  // non-hex
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("aaa:bb:cc:dd:ee:ff")) // wrong width
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("aa:bb:cc:dd:ee:-1")) // signed octet
        assertEquals(emptyList<String>(), KnownHostStore.parseMacs("+a:-b:+c:-d:+e:-f")) // signed octets
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), KnownHostStore.parseMacs("junk, aa:bb:cc:dd:ee:ff"))
    }
}
