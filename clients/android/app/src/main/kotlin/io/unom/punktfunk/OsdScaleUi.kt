package io.unom.punktfunk

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density

/**
 * Design units to a TV screen's height for the streaming chrome — the stats HUD and the
 * quick-action ring. The Skia console draws a TV at the same scale, so the chrome over a stream
 * matches the console under it. The TV density bucket is no guide: every 1080p set is 320 dpi,
 * 960×540 dp, which drew the ring across two thirds of the screen.
 *
 * Physical screen size is deliberately not an input: `DisplayMetrics.xdpi` is invented on many TV
 * boxes. Android only — the Apple TV client sizes its own chrome (`StreamHUDView`).
 */
const val COUCH_HEIGHT = 800f

/** Pixels per `dp` for the streaming chrome: the couch scale on a TV, `density` elsewhere. */
fun osdDensity(context: android.content.Context, density: Float): Float {
    if (!isTvDevice(context)) return density
    val m = context.resources.displayMetrics
    return minOf(m.widthPixels, m.heightPixels) / COUCH_HEIGHT
}

/**
 * Draws [content] at this device's overlay scale by replacing [LocalDensity], so every `dp` and
 * `sp` inside moves together — no metric is scaled by hand and none can be missed. `fontScale`
 * passes through untouched: the system text size the user chose still applies on top of this.
 *
 * [CompositionLocalProvider] emits no layout node, so a `BoxScope.align` built by the caller still
 * lands on the content's own node — the overlays stay where they were placed.
 */
@Composable
fun OsdScaled(content: @Composable () -> Unit) {
    val context = LocalContext.current
    val density = LocalDensity.current
    // Device-fixed: the leanback feature and a TV's display size do not change at runtime.
    val scaled = remember(context, density.density) { osdDensity(context, density.density) }
    CompositionLocalProvider(
        LocalDensity provides Density(scaled, density.fontScale),
        content = content,
    )
}
