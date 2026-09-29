package io.unom.punktfunk

import android.app.Activity
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import androidx.window.layout.FoldingFeature
import androidx.window.layout.WindowInfoTracker
import io.unom.punktfunk.kit.NativeBridge
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.map

/*
 * A hinge across the screen (design/touch-client-overlay.md §4.4, design/android-dual-screen.md
 * §3). A tabletop foldable or a two-panel device held like a DS is two screens: the picture takes
 * the upper one, and the lower one takes the companion panel when it has room, else the virtual
 * pad alone while it is up. The posture is the whole trigger, so laying the device flat again
 * puts the picture back over the full panel with nothing to undo.
 */

/** The halves in px: the picture keeps [videoPx], the hinge eats [hingePx], the lower half takes the rest. */
internal data class FoldSplit(val videoPx: Int, val hingePx: Int) {
    /** The lower half is at least 40 % of [height]: room for the companion panel, pad or not. */
    fun carriesCompanion(height: Int): Boolean = (height - videoPx - hingePx) * 5 >= height * 2
}

/**
 * The split for a hinge at [hinge] across a [size] container, or null when this fold cannot
 * carry one. A hinge that does not span the full width folds the screen left/right (book
 * posture, held like a paperback — no flat half to put a pad on), and a hinge close to either
 * edge leaves a half that is too small to be either a picture or a controller.
 *
 * Both rects are window px: the stream screen is edge-to-edge with the system bars hidden, so the
 * window and this container are the same rectangle.
 */
internal fun foldSplit(hinge: IntRect, size: IntSize): FoldSplit? {
    if (size.width <= 0 || size.height <= 0) return null
    if (hinge.left > 0 || hinge.right < size.width) return null
    val video = hinge.top.coerceIn(0, size.height)
    val gap = (hinge.bottom - hinge.top).coerceIn(0, size.height - video)
    val min = size.height / 5
    if (video < min || size.height - video - gap < min) return null
    return FoldSplit(video, gap)
}

/**
 * The hinge splitting this window — half-opened, or a gap between two panels even when laid
 * flat — or null on a flat, shut or single-panel device. Recomposes as the hinge moves, so
 * opening the device mid-stream splits the screen and closing it joins it again. Every change
 * writes the fold features to the log ring.
 */
@Composable
internal fun rememberFoldHinge(): IntRect? {
    val activity = LocalContext.current as? Activity
    val hinges = remember(activity) {
        if (activity == null) {
            flowOf(null)
        } else {
            WindowInfoTracker.getOrCreate(activity).windowLayoutInfo(activity).map { info ->
                val folds = info.displayFeatures.filterIsInstance<FoldingFeature>()
                runCatching {
                    NativeBridge.nativeLogDisplay(
                        "fold " + folds.joinToString(" | ") {
                            "state=${it.state} orientation=${it.orientation} separating=${it.isSeparating} " +
                                "occlusion=${it.occlusionType} bounds=${it.bounds.toShortString()}"
                        }.ifEmpty { "none" },
                    )
                }
                folds.firstOrNull { it.state == FoldingFeature.State.HALF_OPENED || it.isSeparating }
                    ?.bounds
                    ?.let { IntRect(it.left, it.top, it.right, it.bottom) }
            }
        }
    }
    return hinges.collectAsState(null).value
}
