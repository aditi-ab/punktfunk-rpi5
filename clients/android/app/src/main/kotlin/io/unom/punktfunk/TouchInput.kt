package io.unom.punktfunk

import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.ui.input.pointer.PointerInputScope
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.abs
import kotlin.math.hypot
import kotlin.math.roundToInt

// Touch-gesture tuning (px / ms). TAP_SLOP: movement under this still counts as a tap, not a drag.
// TAP_DRAG_MS: a new touch within this long after a tap starts a left-button drag. SCROLL_DIV: px of
// two-finger pan per wheel notch (smaller = faster scroll).
private const val TAP_SLOP = 12f
private const val TAP_DRAG_MS = 250L
private const val SCROLL_DIV = 4f

// Trackpad-mode pointer ballistics (relative one-finger motion). POINTER_SENS: base finger-px →
// host-px gain (~1:1, never twitchy). The rest is mild acceleration so a flick crosses the screen
// while a slow drag stays precise: above ACCEL_SPEED_FLOOR px/ms the gain ramps by ACCEL_GAIN per
// px/ms, capped at ACCEL_MAX (so a fast swipe can't fling the cursor uncontrollably).
private const val POINTER_SENS = 1.3f
private const val ACCEL_GAIN = 0.6f
private const val ACCEL_SPEED_FLOOR = 0.3f
private const val ACCEL_MAX = 3.0f

/**
 * Touch → mouse, run inside the stream overlay's `pointerInput`. Two models, chosen by the
 * Trackpad-mode setting:
 *  * trackpad (default): the cursor STAYS where it is on touch-down and moves by the finger's
 *    relative delta (MouseMove) with mild pointer acceleration — swipe to nudge, lift and
 *    re-swipe to walk it across, tap to click where it is. This is what makes the cursor
 *    reachable on a small screen.
 *  * direct (opt-out): the cursor jumps to the finger and follows it (MouseMoveAbs,
 *    host-normalized against the overlay size), the old "direct pointing" behaviour.
 *
 * Both share the same gesture vocabulary: tap = left click; two-finger tap = right click;
 * two-finger drag = scroll; tap-then-press-and-drag = left-drag (text selection / moving
 * windows); three-finger tap = [onToggleStats] (the stats HUD).
 */
internal suspend fun PointerInputScope.streamTouchInput(
    handle: Long,
    trackpad: Boolean,
    onToggleStats: () -> Unit,
) {
    var lastTapUp = 0L
    var lastTapX = 0f
    var lastTapY = 0f
    fun moveAbs(x: Float, y: Float) {
        val sw = size.width
        val sh = size.height
        if (sw <= 0 || sh <= 0) return
        NativeBridge.nativeSendPointerAbs(
            handle,
            x.coerceIn(0f, (sw - 1).toFloat()).roundToInt(),
            y.coerceIn(0f, (sh - 1).toFloat()).roundToInt(),
            sw,
            sh,
        )
    }
    awaitEachGesture {
        val down = awaitFirstDown(requireUnconsumed = false)
        val startX = down.position.x
        val startY = down.position.y
        // A touch landing just after a quick tap nearby = tap-and-drag: hold the left
        // button for this whole gesture (laptop-trackpad convention).
        val isDrag = down.uptimeMillis - lastTapUp < TAP_DRAG_MS &&
            abs(startX - lastTapX) < TAP_SLOP && abs(startY - lastTapY) < TAP_SLOP
        lastTapUp = 0L // consume the arming either way
        // Direct mode jumps the cursor to the finger; trackpad mode leaves it put (the
        // whole point — you nudge it with swipes instead).
        if (!trackpad) moveAbs(startX, startY)
        if (isDrag) NativeBridge.nativeSendPointerButton(handle, 1, true)

        var moved = false
        var maxFingers = 1
        var scrolling = false
        var prevCx = startX
        var prevCy = startY
        var upTime = down.uptimeMillis
        // Trackpad relative-motion state: the tracked finger, its last position/time, and
        // the sub-pixel remainder so a slow drag isn't lost to Int truncation.
        var trackId = down.id
        var prevX = startX
        var prevY = startY
        var prevT = down.uptimeMillis
        var accX = 0f
        var accY = 0f

        while (true) {
            val ev = awaitPointerEvent()
            val pressed = ev.changes.filter { it.pressed }
            if (pressed.isEmpty()) {
                upTime = ev.changes.firstOrNull()?.uptimeMillis ?: upTime
                break
            }
            if (pressed.size > maxFingers) maxFingers = pressed.size

            if (pressed.size >= 2) {
                // Two fingers → scroll by the centroid delta; never move the cursor.
                val cx = (pressed.sumOf { it.position.x.toDouble() } / pressed.size).toFloat()
                val cy = (pressed.sumOf { it.position.y.toDouble() } / pressed.size).toFloat()
                if (!scrolling) {
                    scrolling = true
                    prevCx = cx
                    prevCy = cy
                }
                val sy = ((prevCy - cy) / SCROLL_DIV).toInt() // finger up → wheel up
                val sx = ((cx - prevCx) / SCROLL_DIV).toInt()
                if (sy != 0) {
                    NativeBridge.nativeSendScroll(handle, 0, sy * 120)
                    prevCy = cy
                    moved = true
                }
                if (sx != 0) {
                    NativeBridge.nativeSendScroll(handle, 1, sx * 120)
                    prevCx = cx
                    moved = true
                }
            } else if (!scrolling) {
                // One finger (skipped once a gesture turned into a scroll, so dropping
                // back to one finger doesn't jerk the cursor).
                val p = pressed.firstOrNull { it.id == down.id } ?: pressed.first()
                if (abs(p.position.x - startX) > TAP_SLOP ||
                    abs(p.position.y - startY) > TAP_SLOP
                ) {
                    moved = true
                }
                if (trackpad) {
                    // Relative: move by the finger delta × (sensitivity × acceleration),
                    // carrying the sub-pixel remainder. Re-anchor (zero delta this frame)
                    // if the tracked finger changed, so lifting one of several fingers
                    // never jumps the cursor.
                    if (p.id != trackId) {
                        trackId = p.id
                        prevX = p.position.x
                        prevY = p.position.y
                        prevT = p.uptimeMillis
                    }
                    val dx = p.position.x - prevX
                    val dy = p.position.y - prevY
                    val dt = (p.uptimeMillis - prevT).coerceAtLeast(1L)
                    prevX = p.position.x
                    prevY = p.position.y
                    prevT = p.uptimeMillis
                    val speed = hypot(dx, dy) / dt // finger px per ms
                    val accel = (1f + ACCEL_GAIN * (speed - ACCEL_SPEED_FLOOR).coerceAtLeast(0f))
                        .coerceAtMost(ACCEL_MAX)
                    accX += dx * POINTER_SENS * accel
                    accY += dy * POINTER_SENS * accel
                    val outX = accX.toInt() // truncates toward zero → remainder kept w/ sign
                    val outY = accY.toInt()
                    if (outX != 0 || outY != 0) {
                        NativeBridge.nativeSendPointerMove(handle, outX, outY)
                        accX -= outX
                        accY -= outY
                    }
                } else {
                    moveAbs(p.position.x, p.position.y) // direct: cursor follows the finger
                }
            }
            ev.changes.forEach { it.consume() }
        }

        if (isDrag) {
            NativeBridge.nativeSendPointerButton(handle, 1, false) // end the drag
        } else if (!moved) {
            when {
                maxFingers >= 3 -> onToggleStats() // in-stream HUD toggle
                maxFingers == 2 -> { // two-finger tap → right click
                    NativeBridge.nativeSendPointerButton(handle, 3, true)
                    NativeBridge.nativeSendPointerButton(handle, 3, false)
                }
                else -> { // tap → left click (at the cursor's current spot), arm tap-drag
                    NativeBridge.nativeSendPointerButton(handle, 1, true)
                    NativeBridge.nativeSendPointerButton(handle, 1, false)
                    lastTapUp = upTime
                    lastTapX = startX
                    lastTapY = startY
                }
            }
        }
    }
}
