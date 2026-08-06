package io.unom.punktfunk

import androidx.compose.runtime.compositionLocalOf
import androidx.compose.ui.graphics.Color

// The ink the console (gamepad) UI draws with under the chosen background palette.
//
// The console screens were white-on-dark throughout with the brand violet hardcoded as the accent.
// Both had to become palette-derived at once: a pale field needs dark text or it is unreadable,
// and a violet focus wash on a copper field is exactly the clash this exists to fix.
//
// Published as a CompositionLocal rather than passed down, so a leaf (a row, a hint pill, a card)
// can ask for the right colour without every caller in between knowing about palettes. The Apple
// client uses an environment value and `pf-console-ui` a thread-local for the same reason.

/** Everything about the console's look that follows the chosen palette. */
class GamepadInk(
    /** Primary text/glyph colour. */
    val fg: Color,
    /** Focus wash, selected tab pill, switch track — the palette's own accent. */
    val accent: Color,
    /** What reads ON the accent (a filled pill's label, a switch knob). */
    val onAccent: Color,
    /** The base fill every glass surface starts from, at its resting opacity. */
    val glass: Color,
    /** What a wash laid UNDER text tends toward: black on a dark field, white on a pale one. */
    val shade: Color,
    /**
     * How hard those washes go. A pale field needs far less — mixing toward white at the dark
     * field's strength bleaches the chroma straight out of the gradient.
     */
    val shadeScale: Float,
    /** True when the field is pale, for the few places that branch rather than blend. */
    val isLight: Boolean,
) {
    /** The foreground at [alpha]. */
    fun fg(alpha: Float): Color = fg.copy(alpha = alpha)

    /** The accent at [alpha]. */
    fun accent(alpha: Float): Color = accent.copy(alpha = alpha)

    /** A wash under text: [alpha] is the dark-field strength, scaled for a pale one. */
    fun shade(alpha: Float): Color = shade.copy(alpha = alpha * shadeScale)

    companion object {
        fun of(p: GamepadPalette): GamepadInk {
            val accent = p.accentColor
            // Chosen by luminance, not by `light`: an accent is picked for contrast against the
            // GLASS, not against the field.
            val accentLuma =
                0.2126 * p.accent.first + 0.7152 * p.accent.second + 0.0722 * p.accent.third
            val onAccent = if (accentLuma > 0.55) Color.Black else Color.White
            if (!p.light) {
                return GamepadInk(
                    fg = Color.White,
                    accent = accent,
                    onAccent = onAccent,
                    glass = Color.White.copy(alpha = 0.08f),
                    shade = Color.Black,
                    shadeScale = 1f,
                    isLight = false,
                )
            }
            val (gr, gg, gb) = p.ground
            return GamepadInk(
                // Tinted toward the palette's own ground so it doesn't read as a foreign grey.
                fg = Color((gr * 0.16).toFloat(), (gg * 0.14).toFloat(), (gb * 0.20).toFloat()),
                accent = accent,
                onAccent = onAccent,
                // More body than the dark glass carries: white frost over a bright gradient has
                // far less separating it from its backdrop than dark glass over a dark one.
                glass = Color.White.copy(alpha = 0.55f),
                shade = Color.White,
                shadeScale = 0.45f,
                isLight = true,
            )
        }

        /** The shipped dark look — what a preview or a test composition gets. */
        val DARK = of(GamepadPalette.named("violet"))
    }
}

/**
 * The ink of the palette currently drawing, for everything under [App]. Provided from the live
 * settings alongside [LocalGamepadPalette], so a change on the gamepad settings screen re-inks
 * every console surface at once.
 */
val LocalGamepadInk = compositionLocalOf { GamepadInk.DARK }
