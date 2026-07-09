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

    // GamepadPref wire bytes — must equal punktfunk-core `config.rs::GamepadPref::to_u8`.
    const val PREF_AUTO = 0
    const val PREF_XBOX360 = 1
    const val PREF_DUALSENSE = 2
    const val PREF_XBOXONE = 3
    const val PREF_DUALSHOCK4 = 4
    const val PREF_STEAMCONTROLLER = 5
    const val PREF_STEAMDECK = 6

    // USB vendor ids of the controllers we can identify by VID/PID.
    private const val VID_SONY = 0x054C
    private const val VID_MICROSOFT = 0x045E
    private const val VID_VALVE = 0x28DE
    private const val VID_NINTENDO = 0x057E

    // Sony product ids. DualSense (PS5) and DualShock 4 (PS4) map to distinct host pad types.
    private val PID_DUALSENSE = setOf(0x0CE6, 0x0DF2)
    private val PID_DUALSHOCK4 = setOf(0x05C4, 0x09CC)

    // Valve: Steam Deck built-in controller (0x1205); classic Steam Controller wired (0x1102) /
    // dongle (0x1142). The host builds the virtual hid-steam pad; rich-input capture (paddles /
    // trackpads / gyro) is out of scope on Android (no rich-input plane yet), so only the standard
    // buttons + sticks reach the host for now — parity with the desktop type resolution.
    private val PID_STEAMDECK = setOf(0x1205)
    private val PID_STEAMCONTROLLER = setOf(0x1102, 0x1142)

    // Microsoft Xbox One / Series product ids (wired + the common Bluetooth/dongle revisions). All
    // behave like Xbox 360 on the host minus the glyph identity, so they share one pref byte.
    private val PID_XBOXONE = setOf(
        0x02D1, 0x02DD, 0x02E3, 0x02EA, 0x0B00, 0x0B12, 0x0B13, 0x0B20,
    )

    /**
     * Resolve a connected controller's [GamepadPref] wire byte from its USB VID/PID, mirroring the
     * Linux client's `pref_for_type` (SDL3 `GamepadType`) and the Apple client's GameController type
     * auto-resolution. Android exposes no controller-type enum, so we match `getVendorId()` /
     * `getProductId()`. Used only when the user picked "Automatic" — an explicit choice is honored as
     * is. An unrecognized pad (or none) falls back to [PREF_XBOX360], the safe XInput default the
     * host always supports. Never returns [PREF_AUTO] (the host would then decide) — once we have a
     * physical pad we resolve it concretely, matching the other native clients.
     */
    fun prefFor(dev: InputDevice?): Int {
        if (dev == null) return PREF_XBOX360
        val vid = dev.vendorId
        val pid = dev.productId
        return when {
            vid == VID_SONY && pid in PID_DUALSENSE -> PREF_DUALSENSE
            vid == VID_SONY && pid in PID_DUALSHOCK4 -> PREF_DUALSHOCK4
            vid == VID_MICROSOFT && pid in PID_XBOXONE -> PREF_XBOXONE
            vid == VID_VALVE && pid in PID_STEAMDECK -> PREF_STEAMDECK
            vid == VID_VALVE && pid in PID_STEAMCONTROLLER -> PREF_STEAMCONTROLLER
            else -> PREF_XBOX360
        }
    }

    /**
     * The glyph family a controller's physical buttons belong to, for the console UI's hint bar —
     * so a DualSense user sees ✕/○/□/△ shapes and a Switch pad its monochrome lettering instead of
     * Xbox's coloured letters. PURELY visual: the wire mapping ([buttonBit]) is unaffected.
     */
    enum class PadStyle { GENERIC, XBOX, PLAYSTATION, NINTENDO }

    /**
     * Resolve the [PadStyle] for a connected controller by USB vendor id. Vendor alone is enough —
     * every pad a vendor ships wears its family's glyphs (any Sony pad has the shapes, any Nintendo
     * pad the −/+ system buttons), so unlike [prefFor] no PID table is needed. Valve renders as
     * [PadStyle.XBOX]: Steam pads carry A/B/X/Y in Xbox positions. Unknown vendors (8BitDo & co.,
     * which near-universally clone the Xbox layout) fall back to [PadStyle.GENERIC], drawn with the
     * Xbox convention.
     */
    fun styleFor(dev: InputDevice?): PadStyle = when (dev?.vendorId) {
        VID_SONY -> PadStyle.PLAYSTATION
        VID_MICROSOFT, VID_VALVE -> PadStyle.XBOX
        VID_NINTENDO -> PadStyle.NINTENDO
        else -> PadStyle.GENERIC
    }

    /** True when [dev]'s source classes include gamepad or joystick. */
    fun isPad(dev: InputDevice?): Boolean {
        val s = dev?.sources ?: return false
        return s and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD ||
            s and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK
    }

    /** All connected gamepad/joystick [InputDevice]s, in system enumeration order. */
    fun pads(): List<InputDevice> =
        InputDevice.getDeviceIds().toList().mapNotNull { InputDevice.getDevice(it) }.filter { isPad(it) }

    /** First connected gamepad/joystick [InputDevice], or null when none is attached. */
    fun firstPad(): InputDevice? = pads().firstOrNull()

    /**
     * The [GamepadPref] wire byte to send for the user's [setting] (the persisted gamepad index). A
     * non-Auto setting is passed through unchanged; "Automatic" ([PREF_AUTO]) resolves to a concrete
     * type from the first connected controller via [prefFor] (so the host gets the right pad even
     * though Android can't tell it the controller type any other way).
     */
    fun resolvePref(setting: Int): Int =
        if (setting == PREF_AUTO) prefFor(firstPad()) else setting

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
     *
     * Single-source: only ONE qualifying controller feeds pad 0. Events must come from a device
     * whose source classes include GAMEPAD (see [onMotion]) and the mapper pins itself to the
     * first such device — a controller's joystick-classified sibling nodes (DualSense/DS4 motion
     * sensors) and any second pad report every axis as 0, and folding them into the same state
     * flapped a held trigger/stick between its value and 0 on every event interleave.
     */
    class AxisMapper(private val handle: Long) {
        // Sentinel so the first real value (incl. 0) always sends once after attach (Linux parity).
        private val last = IntArray(6) { Int.MIN_VALUE }
        private var hatX = 0 // -1 / 0 / +1
        private var hatY = 0

        /** deviceId of the controller pad 0 is pinned to; −1 until the first qualifying event. */
        private var deviceId = -1

        /** Returns true if this was a joystick ACTION_MOVE we consumed. */
        fun onMotion(event: MotionEvent): Boolean {
            if (!event.isFromSource(InputDevice.SOURCE_JOYSTICK)) return false
            if (event.actionMasked != MotionEvent.ACTION_MOVE) return false
            // Only a true gamepad drives pad 0. A joystick ACTION_MOVE's own source is plain
            // JOYSTICK for every sender, so qualify by the DEVICE's source classes: a real pad
            // carries the GAMEPAD (button) class too, its sensor/touchpad sibling nodes and
            // joystick-class remotes don't — and those report every pad axis as 0 (see the
            // class doc for the held-trigger flap this caused).
            val dev = event.device ?: return false
            if (dev.sources and InputDevice.SOURCE_GAMEPAD != InputDevice.SOURCE_GAMEPAD) return false
            // Single-pad model: pin to the first qualifying controller so a second pad (or its
            // stick drift) can't fight pad 0; re-adopt only once the pinned device is gone.
            if (deviceId != event.deviceId) {
                if (deviceId != -1) {
                    if (InputDevice.getDevice(deviceId) != null) return false
                    reset() // the pinned pad is gone — lift its held state before adopting
                }
                deviceId = event.deviceId
            }

            // Sticks: Android floats −1..1, +y = down → ±32767, negate Y for the wire's +y = up.
            sendAxis(AXIS_LS_X, stick(event.getAxisValue(MotionEvent.AXIS_X)))
            sendAxis(AXIS_LS_Y, stick(-event.getAxisValue(MotionEvent.AXIS_Y)))
            sendAxis(AXIS_RS_X, stick(event.getAxisValue(MotionEvent.AXIS_Z)))
            sendAxis(AXIS_RS_Y, stick(-event.getAxisValue(MotionEvent.AXIS_RZ)))

            // Triggers: pads report LTRIGGER/RTRIGGER or BRAKE/GAS (some mirror both) — merge
            // with max, the same fold as the Controllers screen probe, so a pad that reports
            // only one pair and a pad that reports both behave identically; 0..1 → 0..255.
            sendAxis(
                AXIS_LT,
                trigger(
                    maxOf(
                        event.getAxisValue(MotionEvent.AXIS_LTRIGGER),
                        event.getAxisValue(MotionEvent.AXIS_BRAKE),
                    ),
                ),
            )
            sendAxis(
                AXIS_RT,
                trigger(
                    maxOf(
                        event.getAxisValue(MotionEvent.AXIS_RTRIGGER),
                        event.getAxisValue(MotionEvent.AXIS_GAS),
                    ),
                ),
            )

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
    }
}
