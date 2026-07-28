package io.unom.punktfunk

import android.view.InputDevice
import android.view.MotionEvent
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.roundToInt

/** True when any connected input device is a pointer (USB/BT mouse, or a touchpad driving one). */
fun hasPhysicalMouse(): Boolean = InputDevice.getDeviceIds().any { id ->
    InputDevice.getDevice(id)?.supportsSource(InputDevice.SOURCE_MOUSE) == true
}

/**
 * Physical mouse → wire, in two modes (the iPadOS/desktop model):
 *  * **uncaptured** (default): hover/drag positions forward as absolute cursor moves
 *    (`MouseMoveAbs`, host-normalized against the window size) — desktop-style pointing. The
 *    local cursor is hidden over the stream (StreamScreen sets a TYPE_NULL pointer icon); the
 *    host's own cursor, composited into the video, is the one you see.
 *  * **captured**: the OS pointer is grabbed ([android.view.View.requestPointerCapture]) and raw
 *    relative deltas forward as `MouseMove` — FPS mouse-look. Engaged at stream start / by
 *    clicking into the stream when the "Capture pointer for games" setting is on, and toggled
 *    any time by Ctrl+Alt+Shift+Q (the cross-client chord). Focus loss releases it (the OS
 *    guarantees that); a click re-engages.
 *
 * Buttons ride [MotionEvent.ACTION_BUTTON_PRESS]/RELEASE edges (left/middle/right/back/forward →
 * wire 1/2/3/4/5), the wheel rides [MotionEvent.ACTION_SCROLL] with fractional accumulation so
 * high-resolution wheels don't lose sub-notch travel. Held buttons are tracked and flushed on
 * capture loss / stream exit so nothing sticks on the host. Events reach this class from
 * MainActivity's dispatch overrides (uncaptured) and the capture view's captured-pointer listener.
 */
class MouseForwarder(
    private val handle: Long,
    private val invertScroll: Boolean,
    private val captureWanted: Boolean,
    private val surfaceSize: () -> Pair<Int, Int>,
) {
    /** Capture plumbing, owned by StreamScreen (the focusable capture view). */
    var onRequestCapture: (() -> Unit)? = null
    var onReleaseCapture: (() -> Unit)? = null

    /** Live capture state, updated from [android.app.Activity.onPointerCaptureChanged]. */
    var captured = false
        private set

    /** Chord-released: no auto re-engage (start / click) until the user opts back in. */
    private var userReleased = false

    private val heldButtons = mutableSetOf<Int>()
    private var scrollAccV = 0f
    private var scrollAccH = 0f
    private var moveAccX = 0f
    private var moveAccY = 0f

    /** Uncaptured mouse events on the TOUCH stream (position while a button is down). */
    fun onTouchEvent(ev: MotionEvent): Boolean {
        when (ev.actionMasked) {
            MotionEvent.ACTION_DOWN -> {
                if (captureWanted && !captured && !userReleased) {
                    // The engaging click: grab the pointer and swallow the click (desktop
                    // parity — the click that captures never reaches the host). The paired
                    // BUTTON_RELEASE is dropped by the held-set guard in [button].
                    onRequestCapture?.invoke()
                    return true
                }
                sendAbs(ev)
            }
            MotionEvent.ACTION_MOVE -> sendAbs(ev)
            // Button edges are documented on the generic stream, but be robust to either.
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
        }
        return true
    }

    /** Uncaptured mouse events on the GENERIC stream (hover motion, wheel, button edges). */
    fun onGenericMotion(ev: MotionEvent): Boolean {
        when (ev.actionMasked) {
            MotionEvent.ACTION_HOVER_MOVE -> sendAbs(ev)
            MotionEvent.ACTION_SCROLL -> wheel(ev)
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
            MotionEvent.ACTION_HOVER_ENTER, MotionEvent.ACTION_HOVER_EXIT -> {}
            else -> return false
        }
        return true
    }

    /**
     * Captured-pointer events (the view holds [android.view.View.requestPointerCapture]): x/y ARE
     * the relative deltas ([InputDevice.SOURCE_MOUSE_RELATIVE]), batched samples included. A
     * captured touchpad reports absolute finger coordinates instead — not handled (the touch
     * gesture layer is the touchpad story); returning false leaves those to the framework.
     */
    fun onCapturedPointer(ev: MotionEvent): Boolean {
        if (!ev.isFromSource(InputDevice.SOURCE_MOUSE_RELATIVE)) return false
        when (ev.actionMasked) {
            MotionEvent.ACTION_MOVE -> {
                var dx = 0f
                var dy = 0f
                for (i in 0 until ev.historySize) {
                    dx += ev.getHistoricalX(i)
                    dy += ev.getHistoricalY(i)
                }
                dx += ev.x
                dy += ev.y
                moveAccX += dx
                moveAccY += dy
                val ox = moveAccX.toInt() // truncate toward zero — sub-pixel remainder kept w/ sign
                val oy = moveAccY.toInt()
                if (ox != 0 || oy != 0) {
                    NativeBridge.nativeSendPointerMove(handle, ox, oy)
                    moveAccX -= ox
                    moveAccY -= oy
                }
            }
            MotionEvent.ACTION_BUTTON_PRESS -> button(ev.actionButton, true)
            MotionEvent.ACTION_BUTTON_RELEASE -> button(ev.actionButton, false)
            MotionEvent.ACTION_SCROLL -> wheel(ev)
        }
        return true
    }

    /** Ctrl+Alt+Shift+Q: release the grab, or (re-)engage it — works even when auto-capture is off. */
    fun toggleCapture() {
        if (captured) {
            userReleased = true
            onReleaseCapture?.invoke()
        } else {
            userReleased = false
            onRequestCapture?.invoke()
        }
    }

    /** Auto-engage at stream start (setting on + a mouse actually present). */
    fun engageFromStart() {
        if (captureWanted && !captured && !userReleased && hasPhysicalMouse()) {
            onRequestCapture?.invoke()
        }
    }

    /** From [android.app.Activity.onPointerCaptureChanged] — the OS is the source of truth. */
    fun onCaptureChanged(has: Boolean) {
        captured = has
        // Losing the grab (focus loss, chord) must not leave buttons held on the host.
        if (!has) flushButtons()
    }

    /** Stream teardown: lift anything held and let the grab go. */
    fun release() {
        flushButtons()
        if (captured) onReleaseCapture?.invoke()
    }

    private fun sendAbs(ev: MotionEvent) {
        val (w, h) = surfaceSize()
        if (w <= 0 || h <= 0) return
        NativeBridge.nativeSendPointerAbs(
            handle,
            ev.x.roundToInt().coerceIn(0, w - 1),
            ev.y.roundToInt().coerceIn(0, h - 1),
            w,
            h,
        )
    }

    private fun wheel(ev: MotionEvent) {
        val dir = if (invertScroll) -1f else 1f
        // Android: AXIS_VSCROLL + = up/away, AXIS_HSCROLL + = right — the wire's convention too.
        scrollAccV += ev.getAxisValue(MotionEvent.AXIS_VSCROLL) * 120f * dir
        scrollAccH += ev.getAxisValue(MotionEvent.AXIS_HSCROLL) * 120f * dir
        val v = scrollAccV.toInt()
        if (v != 0) {
            NativeBridge.nativeSendScroll(handle, 0, v)
            scrollAccV -= v
        }
        val h = scrollAccH.toInt()
        if (h != 0) {
            NativeBridge.nativeSendScroll(handle, 1, h)
            scrollAccH -= h
        }
    }

    /**
     * A mouse side button that arrived as a KEY event rather than a BUTTON_* motion edge.
     *
     * Not every mouse reports its side buttons the same way. One that puts them on the HID button
     * page (BTN_SIDE/BTN_EXTRA) gets `BUTTON_BACK`/`BUTTON_FORWARD` in the motion button state and
     * lands in [button]. One that puts them on the consumer page (AC Back / AC Forward — common on
     * Bluetooth mice, and the shape Android TV boxes tend to see) produces ONLY synthesized
     * `KEYCODE_BACK`/`KEYCODE_FORWARD` key events, so [button] never fires and the side buttons are
     * dead on the wire. This is the key-shaped entry point for those.
     *
     * Devices that report BOTH send the key first and the motion edge second (that is the order the
     * input reader synthesizes them in), so both paths funnel into the same held-set and the
     * add/remove guard collapses the pair into a single wire press.
     */
    fun sideButtonKey(back: Boolean, down: Boolean) = press(if (back) 4 else 5, down)

    private fun button(actionButton: Int, down: Boolean) {
        val b = when (actionButton) {
            MotionEvent.BUTTON_PRIMARY -> 1
            MotionEvent.BUTTON_TERTIARY -> 2
            MotionEvent.BUTTON_SECONDARY -> 3
            MotionEvent.BUTTON_BACK -> 4
            MotionEvent.BUTTON_FORWARD -> 5
            else -> return
        }
        press(b, down)
    }

    private fun press(b: Int, down: Boolean) {
        if (down) {
            // add() is false when the button is already held — the second delivery of a button
            // this device reports on two paths at once. Sending the down again would double-press
            // it on the host.
            if (heldButtons.add(b)) NativeBridge.nativeSendPointerButton(handle, b, true)
        } else if (heldButtons.remove(b)) {
            // Only release what we pressed — drops the release of a swallowed engaging click
            // and anything that raced a capture transition.
            NativeBridge.nativeSendPointerButton(handle, b, false)
        }
    }

    private fun flushButtons() {
        heldButtons.forEach { NativeBridge.nativeSendPointerButton(handle, it, false) }
        heldButtons.clear()
    }
}
