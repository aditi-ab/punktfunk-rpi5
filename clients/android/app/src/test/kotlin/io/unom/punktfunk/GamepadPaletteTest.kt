package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

// The console UI's background palettes. These assertions are the CONTRACT the Rust
// (`pf-console-ui::library::tint`) and Swift (`GamepadPalette.tint`) ports have to reproduce — the
// same ids, the same rotation orientation, the same in-gamut results — so one `ui_palette` value
// names the same colour family on every client.
class GamepadPaletteTest {
    /** The brightest pool of the field — the colour a palette is judged by. */
    private val violetPool = Triple(0.49, 0.39, 0.95)

    /**
     * The brand default must be the IDENTITY transform. Every existing install already sees the
     * shipped violet backdrop, and a palette table that quietly restyled it would be a regression
     * dressed as a feature.
     */
    @Test
    fun violetIsTheUntouchedShippedField() {
        val violet = GamepadPalette.named("violet")
        assertEquals("violet", GamepadPalette.ALL.first().id)
        assertTrue(violet.isIdentity)
        assertEquals(violetPool, violet.tint(violetPool))
        // An unknown name is a newer client's palette, not an error.
        assertEquals("violet", GamepadPalette.named("chartreuse").id)
        assertEquals("violet", GamepadPalette.named("").id)
    }

    /** The ids and their order are the cross-client contract (strip order, and the L1/R1 cycle). */
    @Test
    fun tableMatchesTheOtherClients() {
        assertEquals(
            listOf("violet", "tide", "forest", "ember", "rose", "graphite"),
            GamepadPalette.ALL.map { it.id },
        )
        assertEquals(
            listOf("Violet", "Tide", "Forest", "Ember", "Rose", "Graphite"),
            GamepadPalette.ALL.map { it.name },
        )
    }

    /**
     * A rotation moves the hue while roughly holding luminance, and the saturation scale collapses
     * toward grey — the same four checks the Rust and Swift tests make.
     */
    @Test
    fun tintRotatesHueAndScalesSaturation() {
        assertTrue(violetPool.third > violetPool.first && violetPool.third > violetPool.second)

        // +105° (Ember) turns the blue-dominant pool red-dominant…
        val ember = GamepadPalette.named("ember").tint(violetPool)
        assertTrue("$ember should be warm", ember.first > ember.third)
        // …−130° (Forest) turns it green-dominant…
        val forest = GamepadPalette.named("forest").tint(violetPool)
        assertTrue("$forest", forest.second > forest.first && forest.second > forest.third)
        // …and −70° (Tide) lands on a cyan whose green and blue both beat red.
        val tide = GamepadPalette.named("tide").tint(violetPool)
        assertTrue("$tide", tide.second > tide.first && tide.third > tide.first)

        // Graphite's saturation scale leaves the channels nearly equal…
        val grey = GamepadPalette.named("graphite").tint(violetPool)
        val channels = listOf(grey.first, grey.second, grey.third)
        assertTrue("$grey", channels.max() - channels.min() < 0.08)
        // …at about the source's luminance (it desaturates, it doesn't dim).
        val luma = 0.2126 * violetPool.first + 0.7152 * violetPool.second + 0.0722 * violetPool.third
        assertEquals(luma, grey.second, 0.05)
    }

    /**
     * Every palette stays in gamut on every colour the field is built from — an out-of-range
     * channel would clamp differently on each platform's rasteriser.
     */
    @Test
    fun everyPaletteStaysInGamut() {
        val field = listOf(
            Triple(0.075, 0.060, 0.160), Triple(0.34, 0.27, 0.72), Triple(0.30, 0.26, 0.74),
            Triple(0.42, 0.20, 0.54), Triple(0.49, 0.39, 0.95), Triple(0.28, 0.31, 0.84),
            Triple(0.16, 0.26, 0.64), Triple(0.45, 0.23, 0.60), Triple(0.53, 0.31, 0.75),
            Triple(0.35, 0.35, 0.91), Triple(0.19, 0.28, 0.70), Triple(0.22, 0.18, 0.54),
            Triple(0.24, 0.20, 0.58),
        )
        for (palette in GamepadPalette.ALL) {
            for (c in field) {
                val t = palette.tint(c)
                for (v in listOf(t.first, t.second, t.third)) {
                    assertTrue("${palette.id} $c → $t", v in 0.0..1.0)
                }
            }
        }
    }

    /**
     * Every settings row lands in exactly one tab — a row missing from the tab map is a setting
     * that became unreachable on a TV, which is precisely what this screen exists to prevent.
     */
    @Test
    fun everySettingsRowHasATab() {
        val rows = buildSettingsRows(Settings(), hasBodyVibrator = true, av1Capable = true) {}
        assertTrue(rows.isNotEmpty())
        assertEquals(rows.size, rows.map { it.id }.toSet().size)
        // Profiles is built separately (from the catalog), so no settings row claims it.
        assertTrue(rows.none { it.tab == GpTab.PROFILES })
        for (t in listOf(GpTab.STREAM, GpTab.VIDEO, GpTab.AUDIO, GpTab.CONTROLLER, GpTab.INTERFACE)) {
            assertTrue("$t is empty", rows.any { it.tab == t })
        }
    }

    /** The Background row steps the shared `ui_palette` key and wraps on A, like every choice row. */
    @Test
    fun backgroundRowStepsTheSharedKey() {
        var s = Settings()
        fun rows() = buildSettingsRows(s, hasBodyVibrator = false, av1Capable = false) { s = it }
        fun palette() = rows().first { it.id == "palette" }

        assertEquals("violet", s.uiPalette)
        assertEquals("Violet", palette().value)
        assertTrue("already the first = thud", !palette().adjust(-1))
        assertTrue(palette().adjust(1))
        assertEquals(GamepadPalette.ALL[1].id, s.uiPalette)

        // A from the last entry wraps home.
        s = s.copy(uiPalette = GamepadPalette.ALL.last().id)
        palette().activate()
        assertEquals("violet", s.uiPalette)

        // A store written by a newer client shows the palette that is actually drawing.
        s = s.copy(uiPalette = "chartreuse")
        assertEquals("Violet", palette().value)
    }
}
