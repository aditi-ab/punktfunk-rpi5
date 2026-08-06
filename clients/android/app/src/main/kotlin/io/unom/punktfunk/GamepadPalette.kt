package io.unom.punktfunk

import androidx.compose.ui.graphics.Color
import kotlin.math.cos
import kotlin.math.sin
import kotlin.math.sqrt

// The console (gamepad) UI's background colour families.
//
// A palette is NOT a second hand-tuned colour field: it is a hue rotation + saturation scale
// applied to the ONE field GamepadAuroraBackground already draws, so every palette inherits its
// structure (dark base, bright drifting pools) and the brand default is exactly the shipped look —
// `violet` is the identity transform.
//
// The table and the `tint` maths are mirrored in `pf-console-ui`'s `library.rs` (Rust) and the
// Apple client's `GamepadPalette.swift` under the same ids, so the shared `ui_palette` setting
// names the same colour family on every client. Keep the three copies in step: a palette added
// here without the others is a value the other clients will silently render as Violet.

/**
 * One background colour family. [hueDegrees] rotates about the grey axis (positive runs
 * red → green → blue) and [saturation] scales saturation about luminance.
 */
class GamepadPalette(
    /** The stored `ui_palette` value ([Settings.uiPalette]). */
    val id: String,
    /** What the settings row shows. */
    val name: String,
    val hueDegrees: Double,
    val saturation: Double,
) {
    /** True for the identity transform, so the default path skips the per-colour work. */
    val isIdentity: Boolean get() = hueDegrees == 0.0 && saturation == 1.0

    /** Apply this palette to one packed sRGB colour, keeping its alpha. */
    fun tint(c: Color): Color {
        if (isIdentity) return c
        val (r, g, b) = tint(Triple(c.red.toDouble(), c.green.toDouble(), c.blue.toDouble()))
        return Color(r.toFloat(), g.toFloat(), b.toFloat(), c.alpha)
    }

    /**
     * Rotate `c` about the grey axis by [hueDegrees] (Rodrigues — the same rotation, in the same
     * orientation, that the desktop console's shader uses for its ±8° warm/cool sway) and scale
     * its saturation about luminance. Clamped, because a large rotation can push a channel out of
     * gamut.
     */
    fun tint(c: Triple<Double, Double, Double>): Triple<Double, Double, Double> {
        val (r, g, b) = c
        val a = Math.toRadians(hueDegrees)
        val cs = cos(a)
        val sn = sin(a)
        val invSqrt3 = 1.0 / sqrt(3.0)
        val grey = (r + g + b) / 3.0 * (1.0 - cs)
        // The `sn` term is cross(k, c) with k = (1,1,1)/√3.
        val rr = r * cs + (b - g) * invSqrt3 * sn + grey
        val rg = g * cs + (r - b) * invSqrt3 * sn + grey
        val rb = b * cs + (g - r) * invSqrt3 * sn + grey
        val luma = 0.2126 * rr + 0.7152 * rg + 0.0722 * rb
        fun mix(v: Double) = (luma + (v - luma) * saturation).coerceIn(0.0, 1.0)
        return Triple(mix(rr), mix(rg), mix(rb))
    }

    companion object {
        /**
         * The six shipped palettes, in cycling order: the brand violet, then cool → warm, then
         * the neutral.
         */
        val ALL = listOf(
            GamepadPalette("violet", "Violet", 0.0, 1.0),
            GamepadPalette("tide", "Tide", -70.0, 1.0),
            GamepadPalette("forest", "Forest", -130.0, 0.9),
            GamepadPalette("ember", "Ember", 105.0, 1.0),
            GamepadPalette("rose", "Rose", 60.0, 0.95),
            GamepadPalette("graphite", "Graphite", 0.0, 0.12),
        )

        /**
         * The palette stored under [id], falling back to the brand default — an unknown name is a
         * palette a newer client shipped, not a reason to draw nothing.
         */
        fun named(id: String): GamepadPalette = ALL.firstOrNull { it.id == id } ?: ALL[0]
    }
}
