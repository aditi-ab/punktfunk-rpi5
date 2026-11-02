package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM tests of [Sc1Device], the 2015 Steam Controller parser. The button table is pinned by
 * `clients/shared/sc1-vectors.json`, which the Apple client replays too.
 * Run: `./gradlew :kit:testDebugUnitTest`.
 */
class Sc1DeviceTest {

    private val rows: List<Pair<Int, Int>> by lazy {
        val file = File("../../shared/sc1-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        val a = JSONObject(file.readText()).getJSONArray("buttons")
        (0 until a.length()).map { a.getJSONObject(it).run { getInt("sc1") to getInt("wire") } }
    }

    /** A 64-byte state frame: [word] at 8..11, triggers at 11/12, then 16..24. */
    private fun state(word: Int, lt: Int = 0, s16: Int = 0, s18: Int = 0, s20: Int = 0, s22: Int = 0) =
        ByteArray(64).also { r ->
            r[0] = 1; r[2] = Sc1Device.TYPE_STATE.toByte(); r[3] = 0x3C
            r[8] = word.toByte(); r[9] = (word shr 8).toByte(); r[10] = (word shr 16).toByte()
            r[11] = lt.toByte()
            for ((o, v) in listOf(16 to s16, 18 to s18, 20 to s20, 22 to s22)) {
                r[o] = v.toByte(); r[o + 1] = (v shr 8).toByte()
            }
        }

    @Test
    fun `buttons match the shared vectors`() {
        for ((sc1, wire) in rows) {
            // A finger on the left pad keeps a pad click from reading as a stick click.
            val word = sc1 or 0x080000
            assertEquals("sc1 bit 0x${Integer.toHexString(sc1)}", wire, Sc2Device.wireButtons(Sc1Device.sc2Buttons(word)))
        }
    }

    @Test
    fun `left pad and stick share their bytes`() {
        val s = Sc2Device.State()
        assertTrue(Sc1Device.parseState(state(0, s16 = 1000, s18 = -2000), 64, s))
        assertEquals(1000, s.lsX); assertEquals(-2000, s.lsY)
        // Pad touched while the stick rests: the bytes are the pad, the stick centres.
        Sc1Device.parseState(state(0x080000, s16 = 5000), 64, s)
        assertEquals(0, s.lsX)
        // Pad touched and the stick interleaved: the stick keeps its last reading.
        Sc1Device.parseState(state(0, s16 = 3000), 64, s)
        Sc1Device.parseState(state(0x880000, s16 = 9000), 64, s)
        assertEquals(3000, s.lsX)
    }

    @Test
    fun `the right pad is the right stick only while touched`() {
        val s = Sc2Device.State()
        Sc1Device.parseState(state(0x100000, s20 = -700, s22 = 800), 64, s)
        assertEquals(-700, s.rsX); assertEquals(800, s.rsY)
        Sc1Device.parseState(state(0, s20 = -700, s22 = 800), 64, s)
        assertEquals(0, s.rsX)
    }

    @Test
    fun `a pulled trigger reaches full scale`() {
        val s = Sc2Device.State()
        Sc1Device.parseState(state(0, lt = 255), 64, s)
        assertEquals(255, s.lt)
        Sc1Device.parseState(state(0, lt = 0), 64, s)
        assertEquals(0, s.lt)
    }

    @Test
    fun `old firmware's stick click is a pad click with no finger`() {
        assertEquals(Gamepad.BTN_LS_CLICK, Sc2Device.wireButtons(Sc1Device.sc2Buttons(0x020000)))
        assertEquals(0, Sc2Device.wireButtons(Sc1Device.sc2Buttons(0x0A0000)))
    }

    @Test
    fun `frames are told apart by type`() {
        val s = Sc2Device.State()
        val wireless = byteArrayOf(1, 0, 3, 1, 1)
        assertFalse(Sc1Device.parseState(wireless, wireless.size, s))
        assertTrue(Sc1Device.isWirelessDisconnect(wireless, wireless.size))
        assertFalse(Sc1Device.isWirelessDisconnect(byteArrayOf(1, 0, 3, 1, 2), 5))
        // A Steam Controller 2 report never reads as a 2015 frame.
        assertFalse(Sc1Device.isFrame(byteArrayOf(0x42, 0, 1), 3, Sc1Device.TYPE_STATE))
    }

    @Test
    fun `feature frames are id 0 then 64 bytes`() {
        for (f in Sc1Device.LIZARD_OFF + Sc1Device.LIZARD_ON) {
            assertEquals(65, f.size)
            assertEquals(0, f[0].toInt())
        }
        val buzz = Sc1Device.hapticPulse(1, 2000, 2000, 75)
        assertEquals(listOf(0, 0x8F, 7, 1, 0xD0, 0x07, 0xD0, 0x07, 75, 0), buzz.take(10).map { it.toInt() and 0xFF })
    }
}
