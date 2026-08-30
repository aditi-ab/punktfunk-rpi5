package io.unom.punktfunk

import android.os.SystemClock
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerId
import androidx.compose.ui.input.pointer.PointerInputChange
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.PointerType
import androidx.compose.ui.input.pointer.changedToDownIgnoreConsumed
import androidx.compose.ui.input.pointer.changedToUpIgnoreConsumed
import androidx.compose.ui.input.pointer.positionChanged
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.hypot
import kotlin.math.roundToInt

// Touch-gesture tuning (px / ms). TAP_SLOP: movement under this still counts as a tap, not a drag.
// TAP_DRAG_MS: a new touch within this long after a tap starts a left-button drag. LONG_PRESS_MS:
// one finger held still this long presses the left button and drags until it lifts. SCROLL_DIV:
// px of two-finger pan per wheel notch (smaller = faster scroll).
private const val TAP_SLOP = 12f
private const val TAP_DRAG_MS = 250L
private const val LONG_PRESS_MS = 500L
private const val SCROLL_DIV = 4f

// The dial (design/touch-client-overlay.md §2.1): a two-finger TWIST opens the quick-action ring.
// DIAL_ARM_DEG: below this rotation the gesture is still a scroll candidate — natural scrolls
// rotate a few degrees, and this is what absorbs them. DIAL_COMMIT_DEG: the ring commits and stays
// open after the fingers lift. DIAL_SLOP: centroid travel beyond this before arming means scroll.
private const val DIAL_ARM_DEG = 10f
private const val DIAL_COMMIT_DEG = 30f
private const val DIAL_SLOP = 2 * TAP_SLOP

/** The twist's progress, for the ring: [Turn] on every move once armed, then [Commit] at the
 *  commit angle, or [Cancel] when the fingers lift short of it (or wind it back). */
sealed class DialEvent {
    /** [progress] 0…1 drives the ring's unwind; [clockwise] is the hand's direction; [x]/[y]
     *  (container px) the centroid the ring is centred on. */
    data class Turn(val progress: Float, val clockwise: Boolean, val x: Float, val y: Float) : DialEvent()
    object Commit : DialEvent()
    object Cancel : DialEvent()
}

// Three-finger vertical swipe: the fraction of the view height the centroid must travel to
// summon (up) / dismiss (down) the local soft keyboard.
private const val KB_SWIPE_FRACTION = 0.10f

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
 * two-finger drag = scroll; tap-then-press-and-drag OR press-and-hold-then-drag = left-drag
 * (text selection / moving windows); three-finger tap = [onCycleStats] (cycle the stats-HUD tier);
 * three-finger swipe up/down = [onKeyboard] (summon/dismiss the local soft keyboard, for
 * typing on the host).
 */
/**
 * Real multi-touch passthrough ([TouchMode.TOUCH]): every finger forwards as a host touchscreen
 * contact (down/move/up with a stable per-finger id), with NO gesture interpretation — taps,
 * drags and multi-finger input mean whatever the remote app decides. Coordinates are overlay
 * pixels with the overlay size as the surface, exactly like the absolute-mouse path (the host
 * normalizes and maps into the output). On teardown (stream leaves composition) every still-held
 * contact is lifted so nothing stays stuck on the host.
 */
/**
 * The picture's rect inside a container of [size] for a stream of [aspect] (width / height): the
 * same centre-aligned aspect fit the video surface is laid out with. The gesture layer spans the
 * whole container so a finger on a letterbox bar still counts, and every absolute mapping —
 * direct pointer, passthrough, the pen lane — measures against this rect, clamped, because a
 * contact outside the picture has no host position of its own. `aspect <= 0` (unknown) fills.
 */
internal fun videoFitRect(size: IntSize, aspect: Float): IntRect {
    val w = size.width
    val h = size.height
    if (aspect <= 0f || w <= 0 || h <= 0) return IntRect(IntOffset.Zero, size)
    return if (w.toFloat() / h > aspect) {
        val vw = (h * aspect).roundToInt() // wider container: bars left and right
        val left = (w - vw) / 2
        IntRect(left, 0, left + vw, h)
    } else {
        val vh = (w / aspect).roundToInt() // taller container: bars top and bottom
        val top = (h - vh) / 2
        IntRect(0, top, w, top + vh)
    }
}

/** [x] in container pixels → picture-surface pixels, clamped to the picture's edge. */
private fun IntRect.clampX(x: Float): Int = (x - left).roundToInt().coerceIn(0, width - 1)
private fun IntRect.clampY(y: Float): Int = (y - top).roundToInt().coerceIn(0, height - 1)

/** Whether this change belongs to the stylus lane (only when a pen-capable host is live). */
private fun isStylus(c: PointerInputChange, stylus: StylusStream?): Boolean =
    stylus != null && (c.type == PointerType.Stylus || c.type == PointerType.Eraser)

/** [awaitFirstDown] with the stylus lane split out: pen events feed [stylus] and never start a
 *  mouse/touch gesture. Toward a pen-less host ([stylus] == null) a stylus stays a finger. */
private suspend fun AwaitPointerEventScope.awaitFirstFingerDown(
    stylus: StylusStream?,
    videoAspect: Float,
): PointerInputChange {
    while (true) {
        val ev = awaitPointerEvent()
        stylus?.intercept(ev, videoFitRect(size, videoAspect))
        val down = ev.changes.firstOrNull {
            it.changedToDownIgnoreConsumed() && !isStylus(it, stylus)
        }
        if (down != null) return down
    }
}

internal suspend fun PointerInputScope.streamTouchPassthrough(
    handle: Long,
    stylus: StylusStream?,
    videoAspect: Float,
) {
    val ids = mutableMapOf<PointerId, Int>()
    fun alloc(p: PointerId): Int {
        var id = 0
        while (ids.containsValue(id)) id++
        ids[p] = id
        return id
    }
    try {
        awaitPointerEventScope {
            while (true) {
                val ev = awaitPointerEvent()
                val r = videoFitRect(size, videoAspect)
                stylus?.intercept(ev, r)
                val sw = r.width
                val sh = r.height
                if (sw <= 0 || sh <= 0) continue
                for (c in ev.changes) {
                    if (isStylus(c, stylus)) continue // the pen plane owns it
                    val x = r.clampX(c.position.x)
                    val y = r.clampY(c.position.y)
                    when {
                        c.changedToDownIgnoreConsumed() ->
                            NativeBridge.nativeSendTouch(handle, alloc(c.id), 0, x, y, sw, sh)
                        c.changedToUpIgnoreConsumed() ->
                            ids.remove(c.id)?.let {
                                NativeBridge.nativeSendTouch(handle, it, 2, 0, 0, sw, sh)
                            }
                        c.positionChanged() ->
                            ids[c.id]?.let { id ->
                                // Batched MotionEvents coalesce intermediate points into the
                                // historical list — forward them in order so a fast swipe keeps
                                // its real curvature on the host (usually empty during a stream:
                                // unbuffered dispatch is requested, so this costs nothing).
                                for (hs in c.historical) {
                                    NativeBridge.nativeSendTouch(
                                        handle, id, 1,
                                        r.clampX(hs.position.x), r.clampY(hs.position.y),
                                        sw, sh,
                                    )
                                }
                                NativeBridge.nativeSendTouch(handle, id, 1, x, y, sw, sh)
                            }
                    }
                    c.consume()
                }
            }
        }
    } finally {
        // Lift anything still down (composition/session teardown mid-touch).
        ids.values.forEach { NativeBridge.nativeSendTouch(handle, it, 2, 0, 0, 1, 1) }
    }
}

internal suspend fun PointerInputScope.streamTouchInput(
    handle: Long,
    stylus: StylusStream?,
    videoAspect: Float,
    trackpad: Boolean,
    invertScroll: Boolean,
    onCycleStats: () -> Unit,
    onKeyboard: (show: Boolean) -> Unit,
    onDial: (DialEvent) -> Unit,
) {
    val scrollDir = if (invertScroll) -1 else 1
    var lastTapUp = 0L
    var lastTapX = 0f
    var lastTapY = 0f
    fun moveAbs(x: Float, y: Float) {
        val r = videoFitRect(size, videoAspect)
        if (r.width <= 0 || r.height <= 0) return
        NativeBridge.nativeSendPointerAbs(handle, r.clampX(x), r.clampY(y), r.width, r.height)
    }
    awaitEachGesture {
        val down = awaitFirstFingerDown(stylus, videoAspect)
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
        // The left button this gesture holds (tap-drag from the start, or a long press later);
        // released exactly once, in the `finally`, so a teardown mid-drag never strands it.
        var dragHeld = isDrag
        val downT = down.uptimeMillis

        var moved = false
        var maxFingers = 1
        var scrolling = false
        var scrollCount = 0 // pointer count the scroll centroid is anchored at
        // A scroll notch went on the wire: a scroll for the gesture's lifetime, never a dial.
        var scrollEmitted = false
        // The twist: the finger-to-finger vector when the pair formed, the centroid then, and
        // whether it has armed (owns the gesture) / committed (the ring stays open).
        var dialIds: Pair<PointerId, PointerId>? = null
        var dialVx = 0f
        var dialVy = 0f
        var dialAnchorX = 0f
        var dialAnchorY = 0f
        var dialArmed = false
        var dialCommitted = false
        // Keyboard-swipe state: the 3+-finger centroid anchor (per finger count, like the
        // scroll anchor) and a once-per-gesture latch.
        var kbCount = 0
        var kbAnchorX = 0f
        var kbAnchorY = 0f
        var kbFired = false
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

        try {
            while (true) {
                // A still finger raises no event, so the long press is a timeout: while one finger
                // is down and nothing has moved, wait at most until the hold time; running out
                // means "held still that long" and picks up the drag.
                val ev = if (!dragHeld && !moved && maxFingers == 1) {
                    val remaining = LONG_PRESS_MS - (SystemClock.uptimeMillis() - downT)
                    if (remaining <= 0) null else withTimeoutOrNull(remaining) { awaitPointerEvent() }
                } else {
                    awaitPointerEvent()
                }
                if (ev == null) {
                    dragHeld = true
                    NativeBridge.nativeSendPointerButton(handle, 1, true)
                    continue
                }
                stylus?.intercept(ev, videoFitRect(size, videoAspect))
                val pressed = ev.changes.filter { it.pressed && !isStylus(it, stylus) }
                    .sortedBy { it.id.value }
                // Any change of the pair ends the twist: a lift short of commit winds the ring
                // back in; a committed ring stays open and the UI owns it from here.
                if (pressed.size != 2 && dialIds != null) {
                    if (dialArmed && !dialCommitted) onDial(DialEvent.Cancel)
                    dialIds = null
                    dialArmed = false
                    dialCommitted = false
                }
                if (pressed.isEmpty()) {
                    upTime = ev.changes.firstOrNull()?.uptimeMillis ?: upTime
                    break
                }
                if (pressed.size > maxFingers) maxFingers = pressed.size
                // Dropping below three fingers forgets the keyboard-swipe anchor, so a 3→2→3
                // bounce re-anchors instead of reading the count change as swipe travel.
                if (pressed.size < 3) kbCount = 0

                if (pressed.size == 2) {
                    val cx = (pressed.sumOf { it.position.x.toDouble() } / pressed.size).toFloat()
                    val cy = (pressed.sumOf { it.position.y.toDouble() } / pressed.size).toFloat()
                    // The dial first (design §2.1): a twist of the finger-to-finger vector past
                    // DIAL_ARM_DEG with the centroid still owns the gesture; a scroll notch
                    // already sent, or centroid travel past DIAL_SLOP, means the hand is
                    // scrolling. A pinch with no rotation is nothing.
                    val (a, b) = pressed
                    val ids = a.id to b.id
                    if (dialIds != ids && !scrollEmitted) {
                        dialIds = ids
                        dialVx = b.position.x - a.position.x
                        dialVy = b.position.y - a.position.y
                        dialAnchorX = cx
                        dialAnchorY = cy
                    }
                    if (dialIds == ids && (dialArmed || !scrollEmitted)) {
                        val vx = b.position.x - a.position.x
                        val vy = b.position.y - a.position.y
                        val phi = Math.toDegrees(
                            atan2(dialVx * vy - dialVy * vx, dialVx * vx + dialVy * vy).toDouble(),
                        ).toFloat() // signed; + = clockwise on a y-down screen
                        val travel = hypot(cx - dialAnchorX, cy - dialAnchorY)
                        if (dialArmed || (travel < DIAL_SLOP && abs(phi) >= DIAL_ARM_DEG)) {
                            if (!dialArmed) {
                                dialArmed = true
                                moved = true // a twist is never a tap…
                                scrolling = true // …and dropping to one finger must not jerk the cursor
                            }
                            val p = ((abs(phi) - DIAL_ARM_DEG) / (DIAL_COMMIT_DEG - DIAL_ARM_DEG))
                                .coerceIn(0f, 1f)
                            onDial(DialEvent.Turn(p, phi > 0f, cx, cy))
                            if (p >= 1f && !dialCommitted) {
                                dialCommitted = true
                                onDial(DialEvent.Commit)
                            } else if (p <= 0f && dialCommitted) {
                                dialCommitted = false
                                onDial(DialEvent.Cancel)
                            }
                            ev.changes.forEach { it.consume() }
                            continue
                        }
                        // Undecided: under DIAL_SLOP of travel and under DIAL_ARM_DEG of turn the
                        // pair may still become a twist, so no scroll notch goes out yet — a
                        // notch is final (scrollEmitted) and a real twist drifts its centroid
                        // past SCROLL_DIV long before it turns 10°. The anchor follows the
                        // centroid so the scroll starts smoothly once the slop is crossed.
                        if (!scrollEmitted && travel < DIAL_SLOP) {
                            scrolling = true
                            scrollCount = 2
                            prevCx = cx
                            prevCy = cy
                            continue
                        }
                    }
                    // Two fingers → scroll by the centroid delta; never move the cursor.
                    // (Re-)anchor whenever the finger COUNT changes, not just on scroll start: the
                    // centroid of three fingers sits far from the centroid of two, and real fingers
                    // never land (or lift) in the same input frame — so the 2→3 transition would
                    // otherwise read as a scroll notch, sending a phantom wheel tick to the host AND
                    // setting `moved`, which disqualified the tap classification below and made the
                    // 3-finger stats tap unreachable on real hardware.
                    if (!scrolling || pressed.size != scrollCount) {
                        scrolling = true
                        scrollCount = pressed.size
                        prevCx = cx
                        prevCy = cy
                    }
                    val sy = ((prevCy - cy) / SCROLL_DIV).toInt() // finger up → wheel up
                    val sx = ((cx - prevCx) / SCROLL_DIV).toInt()
                    if (sy != 0) {
                        NativeBridge.nativeSendScroll(handle, 0, sy * 120 * scrollDir)
                        prevCy = cy
                        moved = true
                        scrollEmitted = true
                    }
                    if (sx != 0) {
                        NativeBridge.nativeSendScroll(handle, 1, sx * 120 * scrollDir)
                        prevCx = cx
                        moved = true
                        scrollEmitted = true
                    }
                } else if (pressed.size >= 3) {
                    // Three+ fingers → the keyboard swipe, never scroll (the documented
                    // vocabulary is TWO-finger scroll; 3+ only fell into the scroll path as an
                    // accident of its old `>= 2` bound). Anchor the centroid per finger count
                    // (same reasoning as the scroll anchor above) and fire once per gesture when
                    // the vertical travel crosses the threshold: up = show, down = hide.
                    val cx = (pressed.sumOf { it.position.x.toDouble() } / pressed.size).toFloat()
                    val cy = (pressed.sumOf { it.position.y.toDouble() } / pressed.size).toFloat()
                    if (pressed.size != kbCount) {
                        kbCount = pressed.size
                        kbAnchorX = cx
                        kbAnchorY = cy
                    } else {
                        val dy = cy - kbAnchorY
                        // Real centroid travel disqualifies the tap classification below (else a
                        // sub-threshold swipe would still fire the three-finger stats tap).
                        if (abs(dy) > TAP_SLOP || abs(cx - kbAnchorX) > TAP_SLOP) moved = true
                        if (!kbFired && abs(dy) >= size.height * KB_SWIPE_FRACTION) {
                            kbFired = true
                            onKeyboard(dy < 0) // finger up → show, finger down → hide
                        }
                    }
                    // Leaving the scroll state stale would read the 3→2 centroid jump as a wheel
                    // notch; clearing it makes a return to two fingers re-anchor fresh. Same for
                    // the trackpad's tracked finger: its prev position froze while 3+ fingers were
                    // down, so dropping straight back to one finger must re-anchor (zero delta),
                    // not replay the whole 3-finger phase as one cursor jump.
                    scrolling = false
                    scrollCount = 0
                    trackId = PointerId(Long.MIN_VALUE)
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
                        // Direct: cursor follows the finger — historical points first (batched
                        // MotionEvent samples), so the host cursor traces the finger's real path.
                        for (hs in p.historical) moveAbs(hs.position.x, hs.position.y)
                        moveAbs(p.position.x, p.position.y)
                    }
                }
                ev.changes.forEach { it.consume() }
            }

            if (!dragHeld && !moved) {
                when {
                    maxFingers >= 3 -> onCycleStats() // in-stream HUD verbosity cycle
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
        } finally {
            if (dragHeld) NativeBridge.nativeSendPointerButton(handle, 1, false) // end the drag
        }
    }
}
