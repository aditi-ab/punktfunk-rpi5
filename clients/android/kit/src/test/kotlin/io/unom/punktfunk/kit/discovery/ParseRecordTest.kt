package io.unom.punktfunk.kit.discovery

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the native-record parser (`key␟name␟addr␟port␟fp␟pair`), the Kotlin half of the
 * discovery JNI seam. No Android types. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class ParseRecordTest {
    private val s = '\u001F' // field separator (must match the Rust side, discovery.rs FIELD_SEP)

    private fun rec(vararg f: String) = f.joinToString(s.toString())

    @Test
    fun parsesFullRecord() {
        val fp = "a".repeat(64)
        val h = parseHostRecord(rec("host-123", "home-worker-2", "192.168.1.70", "9777", fp, "required"))!!
        assertEquals("host-123", h.key)
        assertEquals("home-worker-2", h.name)
        assertEquals("192.168.1.70", h.host)
        assertEquals(9777, h.port)
        assertEquals(fp, h.fingerprint)
        assertTrue(h.pairingRequired)
    }

    @Test
    fun optionalPairingAndEmptyFingerprint() {
        val h = parseHostRecord(rec("id", "name", "10.0.0.5", "9777", "", "optional"))!!
        assertNull(h.fingerprint)
        assertEquals(false, h.pairingRequired)
    }

    @Test
    fun sevenFieldRecordHasNoOs() {
        // A native lib predating the 8th field: `os` defaults empty, everything else parses.
        val h = parseHostRecord(rec("k", "n", "10.0.0.5", "9777", "", "optional", "aa:bb:cc:dd:ee:ff"))!!
        assertEquals(listOf("aa:bb:cc:dd:ee:ff"), h.mac)
        assertEquals("", h.os)
    }

    @Test
    fun eighthFieldCarriesTheOsChain() {
        val h = parseHostRecord(
            rec("k", "n", "10.0.0.5", "9777", "", "optional", "", "linux/fedora/bazzite"),
        )!!
        assertEquals("linux/fedora/bazzite", h.os)
    }

    @Test
    fun osChainIsSanitizedAsUntrustedInput() {
        // mDNS is unauthenticated: junk is dropped, case folds, token/count caps apply.
        val h = parseHostRecord(rec("k", "n", "10.0.0.5", "9777", "", "optional", "", "Linux/Fe do!ra"))!!
        assertEquals("linux/fedora", h.os)
        assertEquals("", sanitizeOsChain("///!!!"))
        assertEquals("a/b/c/d/e", sanitizeOsChain("a/b/c/d/e/f/g"))
    }

    @Test
    fun iconWalkIsMostSpecificFirstWithAliases() {
        assertEquals(listOf("bazzite", "fedora", "linux"), osIconTokens("linux/fedora/bazzite"))
        assertEquals(listOf("steam", "arch", "linux"), osIconTokens("linux/arch/steamos"))
        assertEquals(listOf("apple"), osIconTokens("macos"))
        assertTrue(osIconTokens("").isEmpty())
    }

    @Test
    fun emptyKeyFallsBackToAddrPort() {
        // Host advertised no `id` TXT → the native side leaves the key blank; we synthesize addr:port.
        val h = parseHostRecord(rec("", "name", "10.0.0.5", "9777", "", "required"))!!
        assertEquals("10.0.0.5:9777", h.key)
    }

    @Test
    fun emptyNameFallsBackToAddr() {
        val h = parseHostRecord(rec("k", "", "10.0.0.5", "9777", "", "optional"))!!
        assertEquals("10.0.0.5", h.name)
    }

    @Test
    fun rejectsTooFewFields() {
        assertNull(parseHostRecord("only${'\u001F'}three${'\u001F'}fields"))
        assertNull(parseHostRecord(""))
    }

    @Test
    fun rejectsBadPortOrAddress() {
        assertNull(parseHostRecord(rec("k", "n", "10.0.0.5", "notaport", "", "required")))
        assertNull(parseHostRecord(rec("k", "n", "10.0.0.5", "0", "", "required")))
        assertNull(parseHostRecord(rec("k", "n", "10.0.0.5", "70000", "", "required")))
        assertNull(parseHostRecord(rec("k", "n", "", "9777", "", "required")))
    }
}
