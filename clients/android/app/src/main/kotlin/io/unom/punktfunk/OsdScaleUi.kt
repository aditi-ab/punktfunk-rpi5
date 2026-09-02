package io.unom.punktfunk

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density

/**
 * How much larger than this screen's normal UI the streaming chrome — the stats HUD and the
 * quick-action ring — draws on a TV. `dp` normalises pixel density, not viewing distance, and a
 * living-room set is roughly 3x further away than a phone; the chrome need not grow 3x, though,
 * because it is read in glances and the ring is a stick target rather than dense text. 1.75 clears
 * the 10-foot legibility floor without walling off the game.
 *
 * Physical screen size is deliberately not an input: `DisplayMetrics.xdpi` is invented on many TV
 * boxes, so a screen-inch rule would mis-size the very case this exists for. Android only — the
 * Apple TV client sizes its own chrome (`StreamHUDView`'s tvOS padding and inset).
 */
const val TV_OSD_SCALE = 1.75f

/** The overlay multiplier for this device. 1 anywhere held or sat in front of. */
fun osdScale(context: android.content.Context): Float =
    if (isTvDevice(context)) TV_OSD_SCALE else 1f

/**
 * Draws [content] at this device's overlay scale by scaling [LocalDensity], so every `dp` and `sp`
 * inside grows together — no metric is scaled by hand and none can be missed. `fontScale` passes
 * through untouched: the system text size the user already chose still applies on top of this.
 *
 * [CompositionLocalProvider] emits no layout node, so a `BoxScope.align` built by the caller still
 * lands on the content's own node — the overlays stay where they were placed.
 */
@Composable
fun OsdScaled(content: @Composable () -> Unit) {
    val context = LocalContext.current
    // Device-fixed: the leanback feature and the ui-mode service behind it cannot change at runtime.
    val scale = remember(context) { osdScale(context) }
    val density = LocalDensity.current
    CompositionLocalProvider(
        LocalDensity provides Density(density.density * scale, density.fontScale),
        content = content,
    )
}
