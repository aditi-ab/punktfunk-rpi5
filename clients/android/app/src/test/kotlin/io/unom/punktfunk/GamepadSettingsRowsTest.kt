package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The controller-navigable settings rows: what the master forwarding switch governs, and that a
 * governed row is inert rather than merely dim.
 *
 * The touch settings and the desktop console have carried this relationship for a while (`enabled =
 * s.gamepadForwarding` / `RowSpec.enabled`); this screen dimmed nothing and stepped everything, so
 * these tests pin both halves — the flag AND the refusal to write.
 */
class GamepadSettingsRowsTest {

    /** Rows for a given forwarding state, capturing whatever a row writes back. */
    private fun rows(
        forwarding: Boolean,
        sink: MutableList<Settings> = mutableListOf(),
    ): List<GpRow> = buildSettingsRows(
        Settings(gamepadForwarding = forwarding),
        hasBodyVibrator = true,
        hasGyroscope = true,
        av1Capable = true,
    ) { sink += it }

    private fun row(rows: List<GpRow>, id: String): GpRow =
        rows.first { it.id == id }

    /** Every row that only means something while a controller is actually being forwarded. */
    private val governed = listOf("padType", "systemButtons", "guideGesture", "sc2", "dsCapture")

    @Test
    fun `forwarding off dims every row that depends on it`() {
        val off = rows(forwarding = false)
        for (id in governed) {
            assertFalse("$id should be dimmed with forwarding off", row(off, id).enabled)
        }
        // The master switch itself stays live — otherwise it could never be turned back on.
        assertTrue(row(off, "padForward").enabled)
    }

    @Test
    fun `forwarding on leaves them all live`() {
        val on = rows(forwarding = true)
        for (id in governed) {
            assertTrue("$id should be live with forwarding on", row(on, id).enabled)
        }
    }

    @Test
    fun `a dimmed row is inert - liveRow withholds it and nothing is written`() {
        val writes = mutableListOf<Settings>()
        val off = rows(forwarding = false, sink = writes)
        for (id in governed) {
            val i = off.indexOfFirst { it.id == id }
            assertNull("$id must not be reachable while dimmed", liveRow(off, i))
            // What the screen actually does on left/right/A — the whole point is that it no-ops.
            liveRow(off, i)?.adjust(1)
            liveRow(off, i)?.adjust(-1)
            liveRow(off, i)?.activate()
        }
        assertEquals("a dimmed row wrote a setting", emptyList<Settings>(), writes)
    }

    @Test
    fun `the same rows do write once forwarding is on`() {
        val writes = mutableListOf<Settings>()
        val on = rows(forwarding = true, sink = writes)
        val i = on.indexOfFirst { it.id == "sc2" }
        assertNotNull(liveRow(on, i))
        liveRow(on, i)?.activate()
        assertEquals(1, writes.size)
        assertFalse("activate flips the toggle", writes[0].sc2Capture)
    }

    /**
     * R18: the Sony passthrough toggle the touch settings have always had. It matters most exactly
     * where this screen is the only one reachable — a TV box has no touch interface to fall back to.
     */
    @Test
    fun `the DualSense passthrough toggle is present, next to its SC2 twin`() {
        val on = rows(forwarding = true)
        val ids = on.map { it.id }
        assertTrue("dsCapture row is missing", "dsCapture" in ids)
        assertEquals(
            "the two passthrough rows belong side by side",
            ids.indexOf("sc2") + 1,
            ids.indexOf("dsCapture"),
        )
        // Drawn as a switch, and reading the persisted default.
        assertEquals(true, row(on, "dsCapture").toggled)
    }

    /**
     * The activation-mode row is a sub-setting of the Controller-optimized UI switch, so it is
     * OFFERED only while that switch is on — hidden rather than dimmed, because with the switch
     * off this whole screen is about to be replaced by the touch UI and a dimmed row there would
     * be one last thing to step past on the way out.
     */
    @Test
    fun `the activation-mode row follows the switch it belongs to`() {
        fun ids(enabled: Boolean) = buildSettingsRows(
            Settings(gamepadUiEnabled = enabled),
            hasBodyVibrator = false, hasGyroscope = false, av1Capable = false,
        ) {}.map { it.id }

        val on = ids(enabled = true)
        assertTrue("the mode row is missing", "gamepadUIMode" in on)
        assertEquals(
            "the mode belongs directly under the switch it qualifies",
            on.indexOf("gamepadUI") + 1,
            on.indexOf("gamepadUIMode"),
        )
        val off = ids(enabled = false)
        assertFalse("the mode row must not outlive its switch", "gamepadUIMode" in off)
        assertTrue("the switch itself stays, or it could never be turned back on", "gamepadUI" in off)
    }

    /** Stepping the mode row writes the shared `gamepad_ui_mode` value, and wraps on A. */
    @Test
    fun `the activation-mode row steps the shared key`() {
        var s = Settings()
        fun mode() = buildSettingsRows(
            s, hasBodyVibrator = false, hasGyroscope = false, av1Capable = false,
        ) { s = it }.first { it.id == "gamepadUIMode" }

        assertEquals(GAMEPAD_UI_WHEN_CONNECTED, s.gamepadUiMode)
        assertEquals("With a controller", mode().value)
        assertFalse("already the first = thud", mode().adjust(-1))
        assertTrue(mode().adjust(1))
        assertEquals(GAMEPAD_UI_ALWAYS, s.gamepadUiMode)
        // A from the last entry wraps home.
        mode().activate()
        assertEquals(GAMEPAD_UI_WHEN_CONNECTED, s.gamepadUiMode)
    }
}
