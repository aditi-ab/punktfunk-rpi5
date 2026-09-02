package io.unom.punktfunk

import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density

/**
 * Draws the streaming chrome at the user's overlay scale by scaling [LocalDensity] for [content],
 * so every `dp` and `sp` inside grows together — no metric is scaled by hand and none can be
 * missed. `fontScale` is passed through untouched: the system text size the user already chose
 * still applies on top of this.
 *
 * [CompositionLocalProvider] emits no layout node, so a `BoxScope.align` built by the caller still
 * lands on the content's own node — the overlays stay where they were placed.
 */
@Composable
fun OsdScaled(pref: Double, content: @Composable () -> Unit) {
    val context = LocalContext.current
    val configuration = LocalConfiguration.current
    // Re-read on configuration change: a foldable crosses the tablet break by unfolding.
    val deviceClass = remember(context, configuration) { osdDeviceClass(context) }
    val density = LocalDensity.current
    val scale = OsdScale.resolve(pref, deviceClass).toFloat()
    CompositionLocalProvider(
        LocalDensity provides Density(density.density * scale, density.fontScale),
        content = content,
    )
}
