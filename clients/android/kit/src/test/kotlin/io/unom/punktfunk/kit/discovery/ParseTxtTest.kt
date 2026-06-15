package io.unom.punktfunk.kit.discovery

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Pure JVM test of the mDNS TXT parser (no Android types). Run: `./gradlew :kit:testDebugUnitTest`. */
class ParseTxtTest {
    private fun b(s: String): ByteArray = s.toByteArray(Charsets.UTF_8)

    @Test
    fun parsesFullRecord() {
        val fp = "a".repeat(64)
        val t = parseTxt(
            mapOf(
                "proto" to b("punktfunk/1"),
                "fp" to b(fp),
                "pair" to b("required"),
                "id" to b("host-123"),
            ),
        )
        assertEquals("punktfunk/1", t.proto)
        assertEquals(fp, t.fp)
        assertEquals("host-123", t.id)
        assertTrue(t.isPunktfunk)
        assertTrue(t.pairingRequired)
    }

    @Test
    fun optionalPairingAndMissingKeys() {
        val t = parseTxt(mapOf("proto" to b("punktfunk/1"), "pair" to b("optional")))
        assertFalse(t.pairingRequired)
        assertNull(t.fp)
        assertNull(t.id)
    }

    @Test
    fun emptyMapYieldsAllNull() {
        val t = parseTxt(emptyMap())
        assertNull(t.proto)
        assertNull(t.fp)
        assertNull(t.pair)
        assertNull(t.id)
        assertFalse(t.isPunktfunk)
        assertFalse(t.pairingRequired)
    }

    @Test
    fun nullAndEmptyValuesTreatedAsAbsent() {
        // NSD delivers present-but-empty TXT keys as null / empty ByteArray.
        val t = parseTxt(mapOf("fp" to null, "id" to ByteArray(0), "proto" to b("punktfunk/1")))
        assertNull(t.fp)
        assertNull(t.id)
        assertTrue(t.isPunktfunk)
    }

    @Test
    fun nonPunktfunkProtoIsNotAccepted() {
        assertFalse(parseTxt(mapOf("proto" to b("moonlight/7"))).isPunktfunk)
    }
}
