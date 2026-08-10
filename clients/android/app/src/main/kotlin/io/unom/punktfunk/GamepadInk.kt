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
    /**
     * The near-opaque ground a MODAL card sits on. A dialog can't be glass: it has to occlude the
     * screen it covers, and it carries [fg] text — which is why this must follow the palette. It
     * was a hardcoded near-black indigo, so on a pale palette the card's dark ink landed on a dark
     * card and the dialogs were unreadable.
     */
    val card: Color,
    /**
     * What dims the screen BEHIND a modal. Always dark, whatever the field: a scrim's job is to
     * push the backdrop down, and a pale field lit with more white doesn't recede — it glares. A
     * pale one needs less of it, because it has further to fall.
     */
    val modalScrim: Color,
    /**
     * The light a glass surface catches along its top edge. White either way — a highlight is a
     * specular, not a tint — but a pale field's frost is already bright, so it takes MORE to read
     * as an edge against the pastel showing through it.
     */
    val highlight: Color,
    /**
     * What a failure says itself in — the pairing error, and anything else the console has to
     * refuse in words. Follows the palette because it lands on [card], not on the field: the salmon
     * that reads on a dark modal is washed out on a near-white one.
     */
    val danger: Color,
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
            val (gr, gg, gb) = p.ground
            if (!p.light) {
                return GamepadInk(
                    fg = Color.White,
                    accent = accent,
                    onAccent = onAccent,
                    glass = Color.White.copy(alpha = 0.08f),
                    shade = Color.Black,
                    shadeScale = 1f,
                    isLight = false,
                    // The palette's own ground, lifted just off it so the card reads as a surface
                    // ABOVE the field rather than a hole in it. For the brand violet that lands on
                    // the #1A1730 the dialogs were hardcoded to, which is where the number came from.
                    card = Color(
                        (gr + 0.030).toFloat().coerceAtMost(1f),
                        (gg + 0.030).toFloat().coerceAtMost(1f),
                        (gb + 0.040).toFloat().coerceAtMost(1f),
                        0.94f,
                    ),
                    modalScrim = Color.Black.copy(alpha = 0.62f),
                    highlight = Color.White.copy(alpha = 0.30f),
                    danger = Color(0xFFE0736F),
                )
            }
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
                // Near-white rather than near-black: the card carries this palette's DARK ink.
                card = Color.White.copy(alpha = 0.94f),
                // Lighter than the dark field's: a pastel backdrop is closer to the card already,
                // so the same 0.62 would read as a bruise rather than a recession.
                modalScrim = Color.Black.copy(alpha = 0.38f),
                highlight = Color.White.copy(alpha = 0.55f),
                // Deepened for the near-white card the pale palettes' modals use — the dark
                // field's salmon has nothing like enough contrast against it.
                danger = Color(0xFFB3352F),
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
