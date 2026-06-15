package io.unom.punktfunk.kit

import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent

/**
 * Android gamepad capture → punktfunk/1 gamepad wire (the `input.rs::gamepad` contract; the host
 * accumulates the incremental events into its virtual xpad). The Android analogue of the Linux
 * client's `gamepad.rs` (SDL3) and the Apple client's `GamepadCapture.swift` (GameController) — all
 * three emit byte-identical events. Single-pad model: exactly one controller forwarded as pad 0.
 *
 * Buttons arrive as KeyEvents (SOURCE_GAMEPAD); sticks/triggers/HAT arrive as joystick MotionEvents
 * (SOURCE_JOYSTICK, ACTION_MOVE). The D-pad is sent as BTN_DPAD_* buttons (no hat axis on the wire),
 * decomposed from either KEYCODE_DPAD_* (gamepad source) or AXIS_HAT_X/Y.
 *
 * Normalization (wire = XInput/Moonlight): sticks i16 ±32767 with **+y = up**; triggers 0..255.
 * Android AXIS_Y/AXIS_RZ are +y = down, so Y is negated. No deadzone here — the host/game owns it
 * (parity with the Linux/Apple clients).
 */
object Gamepad {
    // Button bits — must equal punktfunk-core `input.rs::gamepad::BTN_*`.
    const val BTN_DPAD_UP = 0x0001
    const val BTN_DPAD_DOWN = 0x0002
    const val BTN_DPAD_LEFT = 0x0004
    const val BTN_DPAD_RIGHT = 0x0008
    const val BTN_START = 0x0010
    const val BTN_BACK = 0x0020
    const val BTN_LS_CLICK = 0x0040
    const val BTN_RS_CLICK = 0x0080
    const val BTN_LB = 0x0100
    const val BTN_RB = 0x0200
    const val BTN_GUIDE = 0x0400
    const val BTN_A = 0x1000
    const val BTN_B = 0x2000
    const val BTN_X = 0x4000
    const val BTN_Y = 0x8000

    // Axis ids — must equal `input.rs::gamepad::AXIS_*`.
    const val AXIS_LS_X = 0
    const val AXIS_LS_Y = 1
    const val AXIS_RS_X = 2
    const val AXIS_RS_Y = 3
    const val AXIS_LT = 4
    const val AXIS_RT = 5

    /**
     * Gamepad `KEYCODE_*` → BTN_* bit, or 0 if not a gamepad button we forward. A/B/X/Y are
     * positional (Xbox layout; Nintendo relabeling needs device-type detection, deferred).
     * `KEYCODE_DPAD_*` are included but must only be routed here when the event is from a gamepad
     * (a keyboard's arrow keys share these keycodes and belong to the VK path) — see MainActivity.
     * L2/R2 are forwarded as the analog trigger axes, never as buttons.
     */
    fun buttonBit(keyCode: Int): Int = when (keyCode) {
        KeyEvent.KEYCODE_BUTTON_A -> BTN_A
        KeyEvent.KEYCODE_BUTTON_B -> BTN_B
        KeyEvent.KEYCODE_BUTTON_X -> BTN_X
        KeyEvent.KEYCODE_BUTTON_Y -> BTN_Y
        KeyEvent.KEYCODE_BUTTON_L1 -> BTN_LB
        KeyEvent.KEYCODE_BUTTON_R1 -> BTN_RB
        KeyEvent.KEYCODE_BUTTON_THUMBL -> BTN_LS_CLICK
        KeyEvent.KEYCODE_BUTTON_THUMBR -> BTN_RS_CLICK
        KeyEvent.KEYCODE_BUTTON_START -> BTN_START
        KeyEvent.KEYCODE_BUTTON_SELECT -> BTN_BACK
        KeyEvent.KEYCODE_BUTTON_MODE -> BTN_GUIDE
        KeyEvent.KEYCODE_DPAD_UP -> BTN_DPAD_UP
        KeyEvent.KEYCODE_DPAD_DOWN -> BTN_DPAD_DOWN
        KeyEvent.KEYCODE_DPAD_LEFT -> BTN_DPAD_LEFT
        KeyEvent.KEYCODE_DPAD_RIGHT -> BTN_DPAD_RIGHT
        else -> 0
    }

    /**
     * Maps joystick MotionEvents to axis (+ HAT→dpad) sends for one session, **on change only**.
     * Holds the previous axis/hat state so an unchanged frame emits nothing. One instance per
     * session; call [reset] on release-all (focus loss / disconnect / session stop) so nothing
     * sticks on the host (which has no client-side held-state knowledge).
     */
    class AxisMapper(private val handle: Long) {
        // Sentinel so the first real value (incl. 0) always sends once after attach (Linux parity).
        private val last = IntArray(6) { Int.MIN_VALUE }
        private var hatX = 0 // -1 / 0 / +1
        private var hatY = 0

        /** Returns true if this was a joystick ACTION_MOVE we consumed. */
        fun onMotion(event: MotionEvent): Boolean {
            if (!event.isFromSource(InputDevice.SOURCE_JOYSTICK)) return false
            if (event.actionMasked != MotionEvent.ACTION_MOVE) return false

            // Sticks: Android floats −1..1, +y = down → ±32767, negate Y for the wire's +y = up.
            sendAxis(AXIS_LS_X, stick(event.getAxisValue(MotionEvent.AXIS_X)))
            sendAxis(AXIS_LS_Y, stick(-event.getAxisValue(MotionEvent.AXIS_Y)))
            sendAxis(AXIS_RS_X, stick(event.getAxisValue(MotionEvent.AXIS_Z)))
            sendAxis(AXIS_RS_Y, stick(-event.getAxisValue(MotionEvent.AXIS_RZ)))

            // Triggers: LTRIGGER/RTRIGGER if present, else BRAKE/GAS; 0..1 float → 0..255.
            sendAxis(AXIS_LT, trigger(firstNonZero(event, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE)))
            sendAxis(AXIS_RT, trigger(firstNonZero(event, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS)))

            // HAT → dpad button transitions (track previous, emit only the deltas).
            val hx = sign(event.getAxisValue(MotionEvent.AXIS_HAT_X))
            if (hx != hatX) {
                if (hatX < 0) btn(BTN_DPAD_LEFT, false) else if (hatX > 0) btn(BTN_DPAD_RIGHT, false)
                if (hx < 0) btn(BTN_DPAD_LEFT, true) else if (hx > 0) btn(BTN_DPAD_RIGHT, true)
                hatX = hx
            }
            val hy = sign(event.getAxisValue(MotionEvent.AXIS_HAT_Y))
            if (hy != hatY) {
                if (hatY < 0) btn(BTN_DPAD_UP, false) else if (hatY > 0) btn(BTN_DPAD_DOWN, false)
                if (hy < 0) btn(BTN_DPAD_UP, true) else if (hy > 0) btn(BTN_DPAD_DOWN, true)
                hatY = hy
            }
            return true
        }

        /** Release-all: zero every axis and clear the held dpad. */
        fun reset() {
            for (id in 0..5) sendAxis(id, 0)
            if (hatX < 0) btn(BTN_DPAD_LEFT, false) else if (hatX > 0) btn(BTN_DPAD_RIGHT, false)
            if (hatY < 0) btn(BTN_DPAD_UP, false) else if (hatY > 0) btn(BTN_DPAD_DOWN, false)
            hatX = 0
            hatY = 0
        }

        private fun sendAxis(id: Int, v: Int) {
            if (last[id] == v) return
            last[id] = v
            NativeBridge.nativeSendGamepadAxis(handle, id, v)
        }

        private fun btn(bit: Int, down: Boolean) = NativeBridge.nativeSendGamepadButton(handle, bit, down)

        // −1..1 float → ±32767 i16 (matches the Apple client's 32767 scale).
        private fun stick(v: Float): Int = (v.coerceIn(-1f, 1f) * 32767f).toInt()

        // 0..1 float → 0..255.
        private fun trigger(v: Float): Int = (v.coerceIn(0f, 1f) * 255f).toInt()

        private fun sign(v: Float): Int = if (v < -0.5f) -1 else if (v > 0.5f) 1 else 0

        private fun firstNonZero(e: MotionEvent, a: Int, b: Int): Float {
            val va = e.getAxisValue(a)
            return if (va != 0f) va else e.getAxisValue(b)
        }
    }
}
