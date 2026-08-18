package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

// The console UI's background palettes. These assertions are the CONTRACT the Rust
// (`pf-console-ui::library`) and Swift (`GamepadPalette.swift`) ports reproduce — the same ids in
// the same order, the same light/dark split, the same ramp — so one `ui_palette` value is one look
// on every client.
class GamepadPaletteTest {

    private fun luma(c: Triple<Double, Double, Double>) =
        0.2126 * c.first + 0.7152 * c.second + 0.0722 * c.third

    /** Hue angle in degrees, or null for something too grey to have one. */
    private fun hue(c: Triple<Double, Double, Double>): Double? {
        val (r, g, b) = c
        val max = maxOf(r, g, b)
        val min = minOf(r, g, b)
        val d = max - min
        if (d < 0.04) return null
        val h = when (max) {
            r -> 60.0 * (((g - b) / d) % 6.0)
            g -> 60.0 * ((b - r) / d + 2.0)
            else -> 60.0 * ((r - g) / d + 4.0)
        }
        return (h + 360.0) % 360.0
    }

    /** Ids, order and the light/dark split are the cross-client contract. */
    @Test
    fun tableMatchesTheOtherClients() {
        assertEquals(
            listOf(
                "violet", "oled", "nebula", "abyss", "ember", "moss", "graphite",
                "holo", "sunset", "bloom", "dawn", "mint", "opal",
            ),
            GamepadPalette.ALL.map { it.id },
        )
        // Dark fields lead, pale ones follow, so stepping the row walks one direction.
        val firstLight = GamepadPalette.ALL.indexOfFirst { it.light }
        assertEquals(7, firstLight)
        assertTrue(GamepadPalette.ALL.drop(firstLight).all { it.light })
        // An unknown name is a newer client's palette, not an error.
        assertEquals("violet", GamepadPalette.named("chartreuse").id)
        assertEquals("violet", GamepadPalette.named("").id)
        // The brand default keeps the shipped field rather than a generated ramp.
        assertTrue(GamepadPalette.named("violet").stops.isEmpty())
    }

    /**
     * A palette must read as SEVERAL hues, not one hue at several brightnesses — that was exactly
     * the complaint about the hue-rotation model this replaced.
     */
    @Test
    fun everyPaletteIsMultiTone() {
        for (p in GamepadPalette.ALL) {
            val stops = p.stops.ifEmpty { continue }
            val hues = stops.mapNotNull { hue(it) }
            assertTrue("${p.id}: too few coloured stops", hues.size >= 3)
            var spread = 0.0
            for (a in hues) {
                for (b in hues) {
                    val d = Math.abs(a - b) % 360.0
                    spread = maxOf(spread, minOf(d, 360.0 - d))
                }
            }
            // Graphite and Opal are deliberately near-neutral; the rest must travel.
            val floor = if (p.id == "graphite" || p.id == "opal") 20.0 else 45.0
            assertTrue("${p.id} spans only $spread° of hue", spread >= floor)
        }
    }

    /**
     * OLED is the one palette whose selling point is measurable: it has to be genuinely black,
     * not merely the darkest of the dark fields. The blob field this client draws samples the
     * ramp at 0.15/0.40/0.65/0.90, so its darkest blob lands in the all-black head of the ramp.
     */
    @Test
    fun oledIsActuallyBlack() {
        val oled = GamepadPalette.named("oled")
        assertEquals(Triple(0.0, 0.0, 0.0), oled.ground)
        assertEquals(0f, oled.blobColors[0].red, 1e-6f)
        assertEquals(0f, oled.blobColors[0].green, 1e-6f)
        assertEquals(0f, oled.blobColors[0].blue, 1e-6f)
        val mean = oled.stops.sumOf { luma(it) } / oled.stops.size
        val darkestOther = GamepadPalette.ALL
            .filter { it.id != "oled" && it.stops.isNotEmpty() }
            .minOf { p -> p.stops.sumOf { luma(it) } / p.stops.size }
        assertTrue("oled means $mean, barely under $darkestOther", mean < darkestOther / 2)
    }

    /** A pale palette really is pale — its ink flips, so a mislabelled one is unreadable. */
    @Test
    fun palettesAreHonestAboutLightness() {
        for (p in GamepadPalette.ALL) {
            if (p.light) {
                assertTrue("${p.id}'s ground is dark", luma(p.ground) > 0.6)
                assertTrue("${p.id}'s accent is too pale", luma(p.accent) < 0.45)
            } else {
                assertTrue("${p.id}'s ground is light", luma(p.ground) < 0.2)
                assertTrue("${p.id}'s accent is too dark", luma(p.accent) > 0.25)
            }
        }
    }

    /** The ramp is the shared sampling rule the Rust and Swift ports reproduce. */
    @Test
    fun rampInterpolatesBetweenStops() {
        val stops = listOf(
            Triple(0.0, 0.0, 0.0), Triple(1.0, 0.0, 0.0), Triple(1.0, 1.0, 1.0),
        )
        assertEquals(Triple(0.0, 0.0, 0.0), GamepadPalette.ramp(stops, 0.0))
        assertEquals(Triple(1.0, 1.0, 1.0), GamepadPalette.ramp(stops, 1.0))
        assertEquals(Triple(1.0, 0.0, 0.0), GamepadPalette.ramp(stops, 0.5))
        assertEquals(0.5, GamepadPalette.ramp(stops, 0.25).first, 1e-9)
        // Out of range clamps rather than throwing.
        assertEquals(Triple(0.0, 0.0, 0.0), GamepadPalette.ramp(stops, -3.0))
        assertEquals(Triple(1.0, 1.0, 1.0), GamepadPalette.ramp(stops, 9.0))
        assertEquals(Triple(0.0, 0.0, 0.0), GamepadPalette.ramp(emptyList(), 0.5))
    }

    /** The ink a palette calls for: white on a dark field, near-black on a pale one. */
    @Test
    fun inkFollowsTheField() {
        val dark = GamepadInk.of(GamepadPalette.named("violet"))
        assertTrue(!dark.isLight)
        assertEquals(1f, dark.fg.red, 1e-6f)
        assertEquals(1f, dark.shadeScale, 1e-6f)

        val light = GamepadInk.of(GamepadPalette.named("holo"))
        assertTrue(light.isLight)
        assertTrue("pale fields need dark ink", light.fg.red < 0.3f)
        // A pale field's scrims must pull far less, or they bleach the gradient.
        assertTrue(light.shadeScale < 0.5f)
    }

    

    // The settings-rows tests that lived here pinned the Compose console's row
    // catalog (`buildSettingsRows`). That console is gone — the shared Skia shell owns the rows
    // now, pinned in Rust (`pf-console-ui/src/screens/settings.rs`).
}
