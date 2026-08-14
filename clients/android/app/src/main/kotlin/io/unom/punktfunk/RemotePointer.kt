package io.unom.punktfunk

import android.os.Handler
import android.os.Looper
import android.view.Choreographer
import android.view.KeyEvent
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.hypot

// Hold this long on SELECT (pointer-mode toggle) / PLAY-PAUSE (keyboard toggle) for the long-press
// action instead of the tap action.
private const val LONG_PRESS_MS = 800L

// D-pad glide ballistics, in screen-widths per second: start slow enough to hit a close button,
// ramp over RAMP_S seconds of continuous hold so crossing the desktop doesn't take all day.
private const val SPEED_MIN = 0.14f
private const val SPEED_MAX = 0.70f
private const val RAMP_S = 1.2f

/**
 * Android TV remote as a pointer — the Android analogue of the Apple client's Siri-remote pointer,
 * adapted for D-pad-only remotes (most Android TV remotes have no touch surface). For the
 * "TV as a desktop client" use case, where a plain remote is often the only thing in hand.
 *
 * While streaming on a TV, **hold SELECT ≈ 0.8 s** to toggle pointer mode. While active:
 *  * D-pad (held) glides the host cursor with ramping acceleration (relative `MouseMove`,
 *    Choreographer-paced, diagonal-normalized);
 *  * SELECT tap = left click; PLAY/PAUSE tap = right click (Siri-remote parity);
 *  * PLAY/PAUSE held = toggle the on-screen keyboard; BACK = leave pointer mode
 *    (a second BACK then leaves the stream as usual).
 * While inactive, everything except the SELECT long-press passes through untouched (D-pad =
 * arrow keys, SELECT tap = Enter — synthesized on release, since the down was held back to
 * disambiguate the long-press).
 *
 * Only consulted for non-gamepad key events on TV devices (MainActivity gates the calls); all
 * state lives on the main thread.
 */
class RemotePointer(
    private val handle: Long,
    private val surfaceWidth: () -> Int,
    private val onActiveChanged: (Boolean) -> Unit,
    private val onKeyboardToggle: () -> Unit,
) {
    var active = false
        private set

    /**
     * Whether this session's access includes the POINTER grant — StreamScreen keeps it live from
     * the access poll. Ungranted, the SELECT long-press stops entering pointer mode (a mode whose
     * every action the host would drop; the Access chip says why), and a revocation while the
     * mode is on leaves it cleanly ([setGranted]). Everything else passes through untouched,
     * exactly as when the mode is off — the remote stays a remote.
     */
    private var granted = true

    /** Update the POINTER grant; revoking while pointer mode is on leaves the mode. Main thread. */
    fun setGranted(ok: Boolean) {
        granted = ok
        if (!ok && active) toggle()
    }

    private val handler = Handler(Looper.getMainLooper())
    private val held = mutableSetOf<Int>() // D-pad keycodes currently down
    private var moveAccX = 0f
    private var moveAccY = 0f
    private var lastFrameNs = 0L
    private var rampSec = 0f
    private var tickerRunning = false
    private var centerLongFired = false
    private var playLongFired = false

    private val centerLong = Runnable {
        centerLongFired = true
        toggle()
    }
    private val playLong = Runnable {
        playLongFired = true
        onKeyboardToggle()
    }

    private val frame = object : Choreographer.FrameCallback {
        override fun doFrame(nowNs: Long) {
            if (!tickerRunning) return
            if (held.isEmpty() || !active) {
                tickerRunning = false
                return
            }
            val dt = if (lastFrameNs == 0L) {
                1f / 60f
            } else {
                ((nowNs - lastFrameNs) / 1e9f).coerceIn(0.001f, 0.1f)
            }
            lastFrameNs = nowNs
            rampSec += dt
            var vx = 0f
            var vy = 0f
            if (KeyEvent.KEYCODE_DPAD_LEFT in held) vx -= 1f
            if (KeyEvent.KEYCODE_DPAD_RIGHT in held) vx += 1f
            if (KeyEvent.KEYCODE_DPAD_UP in held) vy -= 1f
            if (KeyEvent.KEYCODE_DPAD_DOWN in held) vy += 1f
            val mag = hypot(vx, vy)
            if (mag > 0f) {
                val w = surfaceWidth().coerceAtLeast(640)
                val speed = w * (SPEED_MIN + (SPEED_MAX - SPEED_MIN) * (rampSec / RAMP_S).coerceAtMost(1f))
                moveAccX += vx / mag * speed * dt
                moveAccY += vy / mag * speed * dt
                val ox = moveAccX.toInt() // truncate toward zero — sub-pixel remainder kept
                val oy = moveAccY.toInt()
                if (ox != 0 || oy != 0) {
                    NativeBridge.nativeSendPointerMove(handle, ox, oy)
                    moveAccX -= ox
                    moveAccY -= oy
                }
            }
            Choreographer.getInstance().postFrameCallback(this)
        }
    }

    /** One remote key event; true = consumed. Ignore key repeats — the ticker owns motion. */
    fun onKey(event: KeyEvent): Boolean {
        val down = event.action == KeyEvent.ACTION_DOWN
        when (event.keyCode) {
            KeyEvent.KEYCODE_DPAD_CENTER -> {
                if (down) {
                    if (event.repeatCount == 0) {
                        centerLongFired = false
                        handler.postDelayed(centerLong, LONG_PRESS_MS)
                    }
                } else {
                    handler.removeCallbacks(centerLong)
                    if (!centerLongFired) {
                        if (active) {
                            click(1)
                        } else {
                            // The down was held back to disambiguate the long-press, so the
                            // normal path never saw it — synthesize the Enter here instead.
                            NativeBridge.nativeSendKey(handle, 0x0D, true, 0)
                            NativeBridge.nativeSendKey(handle, 0x0D, false, 0)
                        }
                    }
                }
                return true
            }
            KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN,
            KeyEvent.KEYCODE_DPAD_LEFT, KeyEvent.KEYCODE_DPAD_RIGHT,
            -> {
                if (!active) return false
                if (down) {
                    if (held.add(event.keyCode) && held.size == 1) startTicker()
                } else {
                    held.remove(event.keyCode)
                }
                return true
            }
            KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> {
                if (!active) return false // inactive: the media-key VK path owns it
                if (down) {
                    if (event.repeatCount == 0) {
                        playLongFired = false
                        handler.postDelayed(playLong, LONG_PRESS_MS)
                    }
                } else {
                    handler.removeCallbacks(playLong)
                    if (!playLongFired) click(3)
                }
                return true
            }
            KeyEvent.KEYCODE_BACK -> {
                if (!active) return false
                if (!down) toggle() // leave pointer mode; the next BACK leaves the stream
                return true
            }
            else -> return false
        }
    }

    /** Stream teardown: stop timers/ticker; nothing wire-held to flush (clicks are edges). */
    fun release() {
        handler.removeCallbacks(centerLong)
        handler.removeCallbacks(playLong)
        active = false
        held.clear()
        tickerRunning = false
    }

    private fun toggle() {
        if (!active && !granted) return // never enter a mode whose input can't land
        active = !active
        if (!active) {
            held.clear()
            tickerRunning = false
        }
        onActiveChanged(active)
    }

    private fun startTicker() {
        rampSec = 0f
        lastFrameNs = 0L
        if (!tickerRunning) {
            tickerRunning = true
            Choreographer.getInstance().postFrameCallback(frame)
        }
    }

    private fun click(button: Int) {
        NativeBridge.nativeSendPointerButton(handle, button, true)
        NativeBridge.nativeSendPointerButton(handle, button, false)
    }
}
