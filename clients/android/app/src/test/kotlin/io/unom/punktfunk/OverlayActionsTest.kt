package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The Kotlin twin of pf-client-core's `overlay_actions` tests — the same blobs, the same
 * outcomes, so the two parsers cannot drift. Run: `./gradlew :app:testDebugUnitTest`.
 */
class OverlayActionsTest {
    private val full = """{"v":2,
        "ring":["end_stream","shortcut:s1","host:power.sleep","stats",null,"pad"],
        "shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]}],
        "pad":{"layout":"sticks","opacity":0.3,"scale":1.2}}"""

    @Test
    fun roundTripsThroughJson() {
        val cfg = OverlayConfig.parse(full)
        assertEquals(SlotId.Shortcut("s1"), cfg.ring[1])
        assertEquals(SlotId.Host("power.sleep"), cfg.ring[2])
        assertNull(cfg.ring[4])
        assertEquals("sticks", cfg.pad.layout)
        assertEquals(listOf("ctrl", "shift", "escape"), cfg.shortcut("s1")!!.keys)
        assertEquals(cfg, OverlayConfig.parse(cfg.toJson()))
    }

    @Test
    fun shortRingsPadAndLongRingsTruncate() {
        val short = OverlayConfig.parse("""{"ring":["mic"]}""", RingPlatform.DESKTOP)
        assertEquals(SlotId.Mic, short.ring[0])
        assertTrue(short.ring.drop(1).all { it == null })
        assertEquals(6, short.ring.size)
        val long = OverlayConfig.parse(
            """{"ring":["mic","mic","mic","mic","mic","mic","stats","stats"]}""",
            RingPlatform.DESKTOP,
        )
        assertEquals(6, long.ring.size)
        assertTrue(long.ring.all { it == SlotId.Mic })
    }

    @Test
    fun unknownIdsAndDanglingShortcutsAreEmptySlots() {
        val cfg = OverlayConfig.parse("""{"ring":["teleport","shortcut:nope","host:","stats"]}""")
        assertNull("a newer client's id degrades to empty", cfg.ring[0])
        assertNull("no such shortcut", cfg.ring[1])
        assertNull("a host id needs a name", cfg.ring[2])
        assertEquals(SlotId.Stats, cfg.ring[3])
    }

    @Test
    fun emptyOrBrokenBlobsAreThePlatformDefault() {
        val touch = OverlayConfig.platformDefault(RingPlatform.TOUCH)
        val desktop = OverlayConfig.platformDefault(RingPlatform.DESKTOP)
        assertEquals(touch, OverlayConfig.parse(""))
        assertEquals(touch, OverlayConfig.parse(null))
        assertEquals(desktop, OverlayConfig.parse("{not json", RingPlatform.DESKTOP))
        assertEquals(SlotId.Pad, touch.ring[5])
        assertEquals(SlotId.SendText, desktop.ring[5])
        val cfg = OverlayConfig.parse("""{"v":2,"ring":[]}""")
        assertEquals(PadConfig(), cfg.pad)
        assertTrue(cfg.ring.all { it == null })
    }

    @Test
    fun keyNamesMapToWindowsVks() {
        assertEquals(0x11, keyVk("ctrl"))
        assertEquals(0x10, keyVk("Shift"))
        assertEquals(0x1B, keyVk("escape"))
        assertEquals(0x09, keyVk("tab"))
        assertEquals(0x41, keyVk("a"))
        assertEquals(0x5A, keyVk("z"))
        assertEquals(0x30, keyVk("0"))
        assertEquals(0x70, keyVk("f1"))
        assertEquals(0x7B, keyVk("f12"))
        assertNull(keyVk("f25"))
        assertNull(keyVk("hyper"))
        assertNull(keyVk(""))
        assertEquals("Ctrl+Shift+Esc", chordChip(listOf("ctrl", "shift", "escape")))
        assertEquals("Win", keyLegend("win"))
        assertEquals("PgUp", keyLegend("pageup"))
        assertEquals("F4", keyLegend("f4"))
        assertEquals("←", keyLegend("left"))
    }

    @Test
    fun slotIdsAreStableStrings() {
        for (id in listOf(
            "end_stream", "disconnect_linger", "touch_mode", "keyboard", "stats", "mic", "pad",
            "send_text", "host:power.reboot", "shortcut:s2",
        )) {
            assertEquals(id, SlotId.parse(id)!!.id)
        }
    }
}
