package io.unom.punktfunk

import androidx.compose.ui.graphics.Color

// The console (gamepad) UI's background colour families, and the ink each one calls for.
//
// A palette is a short ordered ramp of DISTINCT hues, not one hue at several brightnesses. The
// field samples that ramp so several tones show at once and pool into each other, the way a real
// gradient poster does. An earlier version rotated ONE field's hue per palette, which is why every
// non-default palette read flat and monotone.
//
// A palette also owns the UI sitting on it: [accent] is the focus wash / selected pill / switch
// colour, and [light] flips the ink so a pale field gets dark text instead of white.
//
// The table, [ramp] and [CELL_RAMP] are mirrored in `pf-console-ui`'s `library.rs` (Rust) and the
// Apple client's `GamepadPalette.swift` under the same ids, so one `ui_palette` value is one look
// on every client. Keep the three copies in step: a palette added here without the others is a
// value the other clients silently render as Violet.

/** One background colour family. */
class GamepadPalette(
    /** The stored `ui_palette` value ([Settings.uiPalette]). */
    val id: String,
    /** What the settings row shows. */
    val name: String,
    /**
     * The colour ramp, dark end first. Empty = the brand default's explicit field, kept
     * bit-identical to what every install already sees.
     */
    val stops: List<Triple<Double, Double, Double>>,
    /** The field's ground — what it settles onto and what the calm mix lifts toward. */
    val ground: Triple<Double, Double, Double>,
    /** The UI accent: focus wash, selected tab pill, switch track. */
    val accent: Triple<Double, Double, Double>,
    /** A pale field: the UI flips to dark ink and the legibility scrims go white. */
    val light: Boolean,
) {
    /** Four drifting blob colours, spread across the ramp so the field shows several hues. */
    val blobColors: List<Color> by lazy {
        val s = stops.ifEmpty { VIOLET_BLOBS }
        (0..3).map { color(ramp(s, 0.15 + 0.25 * it)) }
    }

    /** The field's ground as a Compose colour. */
    val groundColor: Color by lazy { color(ground) }

    /** The accent as a Compose colour. */
    val accentColor: Color by lazy { color(accent) }

    companion object {
        /**
         * Where each of the 16 mesh cells samples the ramp on the clients that draw a mesh. Kept
         * here so the three ports stay one table even though this client approximates the field
         * with blobs.
         */
        val CELL_RAMP = listOf(
            0.10, -0.06, 0.04, -0.12,
            -0.08, 0.14, -0.10, 0.06,
            0.06, -0.12, 0.16, -0.04,
            -0.10, 0.08, -0.06, 0.12,
        )

        /** The brand default's blob ramp — the colours the pre-palette field used. */
        private val VIOLET_BLOBS = listOf(
            Triple(0.53, 0.47, 0.96), Triple(0.24, 0.20, 0.72), Triple(0.62, 0.30, 0.80),
            Triple(0.22, 0.38, 0.86), Triple(0.53, 0.47, 0.96),
        )

        /**
         * The twelve shipped palettes: the brand default, five more dark fields, then six pale
         * ones. Cycling order runs dark → light, so stepping the row walks the range one way.
         */
        val ALL = listOf(
            // --- dark fields (white ink) ---
            GamepadPalette(
                "violet", "Violet", emptyList(),
                ground = Triple(0.075, 0.060, 0.160),
                accent = Triple(0.525, 0.471, 0.961), light = false,
            ),
            GamepadPalette(
                // Deep indigo climbing through violet into a hot magenta.
                "nebula", "Nebula",
                listOf(
                    Triple(0.07, 0.05, 0.20), Triple(0.26, 0.14, 0.54), Triple(0.52, 0.20, 0.72),
                    Triple(0.82, 0.26, 0.62), Triple(0.98, 0.46, 0.68),
                ),
                ground = Triple(0.055, 0.040, 0.135),
                accent = Triple(0.95, 0.42, 0.72), light = false,
            ),
            GamepadPalette(
                // Ink-blue water: teal → cerulean → a violet undertow.
                "abyss", "Abyss",
                listOf(
                    Triple(0.02, 0.10, 0.17), Triple(0.04, 0.28, 0.42), Triple(0.07, 0.46, 0.63),
                    Triple(0.16, 0.38, 0.78), Triple(0.26, 0.22, 0.58),
                ),
                ground = Triple(0.018, 0.070, 0.130),
                accent = Triple(0.26, 0.76, 0.92), light = false,
            ),
            GamepadPalette(
                // Banked coals: plum embers → crimson → burnt orange → gold.
                "ember", "Ember",
                listOf(
                    Triple(0.16, 0.03, 0.10), Triple(0.45, 0.06, 0.12), Triple(0.72, 0.18, 0.06),
                    Triple(0.90, 0.42, 0.08), Triple(0.95, 0.68, 0.18),
                ),
                ground = Triple(0.090, 0.035, 0.040),
                accent = Triple(0.98, 0.62, 0.26), light = false,
            ),
            GamepadPalette(
                // Forest floor into moss and a lime break.
                "moss", "Moss",
                listOf(
                    Triple(0.03, 0.11, 0.09), Triple(0.06, 0.27, 0.20), Triple(0.09, 0.45, 0.31),
                    Triple(0.28, 0.61, 0.28), Triple(0.58, 0.77, 0.31),
                ),
                ground = Triple(0.025, 0.085, 0.070),
                accent = Triple(0.48, 0.86, 0.46), light = false,
            ),
            GamepadPalette(
                // Neutral, but never flat: barely-there saturation that still travels from a cool
                // charcoal to a warm stone.
                "graphite", "Graphite",
                listOf(
                    Triple(0.06, 0.07, 0.11), Triple(0.15, 0.18, 0.25), Triple(0.30, 0.31, 0.35),
                    Triple(0.45, 0.42, 0.38), Triple(0.60, 0.56, 0.49),
                ),
                ground = Triple(0.055, 0.055, 0.070),
                accent = Triple(0.78, 0.80, 0.86), light = false,
            ),
            // --- pale fields (dark ink) ---
            GamepadPalette(
                // The holographic foil: rose → lilac → periwinkle → aqua, with a white bloom.
                "holo", "Holo",
                listOf(
                    Triple(0.99, 0.72, 0.90), Triple(0.80, 0.60, 0.98), Triple(0.58, 0.62, 0.99),
                    Triple(0.55, 0.86, 0.98), Triple(0.94, 0.98, 1.00),
                ),
                ground = Triple(0.96, 0.92, 0.99),
                accent = Triple(0.42, 0.28, 0.86), light = true,
            ),
            GamepadPalette(
                // The poster sunset: periwinkle → magenta → scarlet → tangerine → gold.
                "sunset", "Sunset",
                listOf(
                    Triple(0.55, 0.45, 0.92), Triple(0.86, 0.31, 0.66), Triple(0.97, 0.26, 0.34),
                    Triple(0.99, 0.51, 0.18), Triple(1.00, 0.80, 0.22),
                ),
                ground = Triple(0.98, 0.74, 0.34),
                accent = Triple(0.64, 0.13, 0.44), light = true,
            ),
            GamepadPalette(
                // Peach into blush and lilac — the softest of the set.
                "bloom", "Bloom",
                listOf(
                    Triple(1.00, 0.86, 0.72), Triple(0.99, 0.73, 0.79), Triple(0.95, 0.65, 0.89),
                    Triple(0.82, 0.68, 0.96), Triple(0.73, 0.79, 0.99),
                ),
                ground = Triple(0.99, 0.90, 0.89),
                accent = Triple(0.72, 0.24, 0.55), light = true,
            ),
            GamepadPalette(
                // First light: pale gold → coral → lilac.
                "dawn", "Dawn",
                listOf(
                    Triple(1.00, 0.92, 0.70), Triple(1.00, 0.80, 0.62), Triple(0.99, 0.66, 0.62),
                    Triple(0.90, 0.62, 0.78), Triple(0.77, 0.69, 0.95),
                ),
                ground = Triple(1.00, 0.93, 0.82),
                accent = Triple(0.82, 0.33, 0.28), light = true,
            ),
            GamepadPalette(
                // Sea glass: mint → aqua → a pale sky.
                "mint", "Mint",
                listOf(
                    Triple(0.82, 0.98, 0.90), Triple(0.62, 0.94, 0.88), Triple(0.55, 0.88, 0.95),
                    Triple(0.63, 0.82, 0.99), Triple(0.82, 0.87, 1.00),
                ),
                ground = Triple(0.90, 0.98, 0.96),
                accent = Triple(0.04, 0.42, 0.40), light = true,
            ),
            GamepadPalette(
                // Near-white, but iridescent rather than flat — rose, sky, mint and cream in turn.
                "opal", "Opal",
                listOf(
                    Triple(0.98, 0.92, 0.96), Triple(0.87, 0.93, 0.99), Triple(0.91, 0.99, 0.95),
                    Triple(0.99, 0.96, 0.88), Triple(0.94, 0.90, 0.99),
                ),
                ground = Triple(0.97, 0.96, 0.99),
                accent = Triple(0.36, 0.32, 0.44), light = true,
            ),
        )

        /**
         * The palette stored under [id], falling back to the brand default — an unknown name is a
         * palette a newer client shipped, not a reason to draw nothing.
         */
        fun named(id: String): GamepadPalette = ALL.firstOrNull { it.id == id } ?: ALL[0]

        /** Sample an ordered colour ramp at [t] ∈ [0, 1] (linear between neighbouring stops). */
        fun ramp(
            stops: List<Triple<Double, Double, Double>>,
            t: Double,
        ): Triple<Double, Double, Double> {
            if (stops.isEmpty()) return Triple(0.0, 0.0, 0.0)
            if (stops.size == 1) return stops[0]
            val x = t.coerceIn(0.0, 1.0) * (stops.size - 1)
            val i = x.toInt().coerceAtMost(stops.size - 2)
            val f = x - i
            val (ar, ag, ab) = stops[i]
            val (br, bg, bb) = stops[i + 1]
            return Triple(ar + (br - ar) * f, ag + (bg - ag) * f, ab + (bb - ab) * f)
        }

        fun color(c: Triple<Double, Double, Double>): Color =
            Color(c.first.toFloat(), c.second.toFloat(), c.third.toFloat())
    }
}
