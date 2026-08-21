package io.unom.punktfunk.kit

import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import java.util.concurrent.ConcurrentHashMap
import kotlin.math.roundToInt

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

    // Extended bits (Moonlight `buttonFlags2 << 16` namespace — `input.rs::gamepad`): the four
    // back grips (Steam L4/L5/R4/R5 ≙ Elite P1–P4), touchpad click, and the misc/QAM button.
    // Android's standard InputDevice path never produces these; the SC2 capture link does.
    const val BTN_PADDLE1 = 0x10000
    const val BTN_PADDLE2 = 0x20000
    const val BTN_PADDLE3 = 0x40000
    const val BTN_PADDLE4 = 0x80000
    const val BTN_TOUCHPAD = 0x100000
    const val BTN_MISC1 = 0x200000

    // Axis ids — must equal `input.rs::gamepad::AXIS_*`.
    const val AXIS_LS_X = 0
    const val AXIS_LS_Y = 1
    const val AXIS_RS_X = 2
    const val AXIS_RS_Y = 3
    const val AXIS_LT = 4
    const val AXIS_RT = 5

    // Motion wire units — must equal punktfunk-core `input.rs::gamepad::MOTION_*`. Every motion
    // sender on this client goes through the two converters below, so a scale that ever has to
    // change changes in ONE place: the gyro program's first finding was a client sending 40× hot
    // because a second copy of the number had drifted.
    const val MOTION_GYRO_LSB_PER_DEG_S = 20
    const val MOTION_ACCEL_LSB_PER_G = 10_000

    /** Standard gravity, `punktfunk-core`'s `G` — the divisor that turns m/s² into g. */
    const val GRAVITY = 9.80665f

    /** [MOTION_GYRO_LSB_PER_DEG_S] restated for Android's rad/s sensors: 1 rad/s ⇒ ~1145.9 raw. */
    const val MOTION_GYRO_LSB_PER_RAD_S = MOTION_GYRO_LSB_PER_DEG_S * 180f / Math.PI.toFloat()

    /** One angular-rate component, Android's rad/s → the wire's signed-16 raw units. */
    fun motionGyroWire(radPerSec: Float): Int =
        (radPerSec * MOTION_GYRO_LSB_PER_RAD_S).roundToInt().coerceIn(-32768, 32767)

    /**
     * One acceleration component, Android's m/s² → the wire's signed-16 raw units. Android reports
     * specific force (the axis pointing up reads +1 g at rest), which is the DualSense report's own
     * convention — no sign flip, and a pad lying flat lands on the host's neutral +1 g exactly.
     */
    fun motionAccelWire(mPerSecSq: Float): Int =
        (mPerSecSq / GRAVITY * MOTION_ACCEL_LSB_PER_G).roundToInt().coerceIn(-32768, 32767)

    // GamepadPref wire bytes — must equal punktfunk-core `config.rs::GamepadPref::to_u8`.
    const val PREF_AUTO = 0
    const val PREF_XBOX360 = 1
    const val PREF_DUALSENSE = 2
    const val PREF_XBOXONE = 3
    const val PREF_DUALSHOCK4 = 4
    const val PREF_STEAMCONTROLLER = 5
    const val PREF_STEAMDECK = 6
    const val PREF_DUALSENSEEDGE = 7
    const val PREF_SWITCHPRO = 8
    const val PREF_STEAMCONTROLLER2 = 9
    const val PREF_STEAMCONTROLLER2_PUCK = 10

    // USB vendor ids of the controllers we can identify by VID/PID.
    private const val VID_SONY = 0x054C
    private const val VID_MICROSOFT = 0x045E
    private const val VID_VALVE = 0x28DE
    private const val VID_NINTENDO = 0x057E

    // Sony product ids. DualSense (PS5), DualSense Edge, and DualShock 4 (PS4) map to distinct
    // host pad types — the Edge's back paddles get native slots on the virtual Edge (Android
    // forwards no paddle input yet, but the identity + rich planes match the physical pad).
    private val PID_DUALSENSE = setOf(0x0CE6)
    private val PID_DUALSENSEEDGE = setOf(0x0DF2)
    private val PID_DUALSHOCK4 = setOf(0x05C4, 0x09CC)

    // Nintendo: Switch Pro Controller — the host builds the virtual hid-nintendo pad (correct
    // glyphs + positional layout). The Switch 2 Pro Controller (0x2069) and a Joy-Con 2 pair
    // (0x2068) are the same full pad surface and ride the same virtual pad (SDL folds them to
    // its NINTENDO_SWITCH_PRO type too).
    private val PID_SWITCHPRO = setOf(0x2009, 0x2069, 0x2068)

    // Valve: Steam Deck built-in controller (0x1205); classic Steam Controller wired (0x1102) /
    // dongle (0x1142). The host builds the virtual hid-steam pad; rich-input capture (paddles /
    // trackpads / gyro) is out of scope on Android (no rich-input plane yet), so only the standard
    // buttons + sticks reach the host for now — parity with the desktop type resolution.
    private val PID_STEAMDECK = setOf(0x1205)
    private val PID_STEAMCONTROLLER = setOf(0x1102, 0x1142)

    // Steam Controller 2: wired (0x1302), BLE (0x1303), and Puck dongles (0x1304/0x1305).
    // Sc2Capture normally claims these directly; the plain InputDevice path is only a degraded
    // fallback. Keep Puck distinct so even that path requests the native multi-interface identity.
    private val PID_STEAMCONTROLLER2 = setOf(0x1302, 0x1303)
    private val PID_STEAMCONTROLLER2_PUCK = setOf(0x1304, 0x1305)

    // Microsoft Xbox One / Series product ids (wired + the common Bluetooth/dongle revisions). All
    // behave like Xbox 360 on the host minus the glyph identity, so they share one pref byte.
    // The Bluetooth revisions (0x02E0/0x02FD Xbox One S, 0x0B05/0x0B22 Elite Series 2 and its
    // Core) are here for the same reason as the wired ones: they are the pads a couch actually
    // pairs to a TV box, and without them an Elite streams under the Xbox 360 identity.
    private val PID_XBOXONE = setOf(
        0x02D1, 0x02DD, 0x02E0, 0x02E3, 0x02EA, 0x02FD,
        0x0B00, 0x0B05, 0x0B12, 0x0B13, 0x0B20, 0x0B22,
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
            vid == VID_SONY && pid in PID_DUALSENSEEDGE -> PREF_DUALSENSEEDGE
            vid == VID_SONY && pid in PID_DUALSHOCK4 -> PREF_DUALSHOCK4
            vid == VID_MICROSOFT && pid in PID_XBOXONE -> PREF_XBOXONE
            vid == VID_VALVE && pid in PID_STEAMDECK -> PREF_STEAMDECK
            vid == VID_VALVE && pid in PID_STEAMCONTROLLER -> PREF_STEAMCONTROLLER
            vid == VID_VALVE && pid in PID_STEAMCONTROLLER2_PUCK ->
                PREF_STEAMCONTROLLER2_PUCK
            vid == VID_VALVE && pid in PID_STEAMCONTROLLER2 -> PREF_STEAMCONTROLLER2
            vid == VID_NINTENDO && pid in PID_SWITCHPRO -> PREF_SWITCHPRO
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

    /**
     * True when [dev] is a controller someone can actually hold: a pad source ([isPad]) that is a
     * REAL device carrying real pad hardware — a stick, a HAT, or the A/B face buttons.
     *
     * [isPad] alone answers "did this event come from a pad source", which is the right question
     * for ROUTING an event and the wrong one for "is a controller attached". Devices publish
     * inputs that claim `SOURCE_GAMEPAD`/`SOURCE_JOYSTICK` while being no such thing — OEM
     * game-mode overlays and the gaming-phone shoulder triggers among them — and one of those is
     * enough to pin the console UI on forever: a pad that was never there cannot disconnect, so
     * "With a controller" has no way back to the touch UI.
     *
     * The capability probe is what separates them: a source class is a claim, a stick or a face
     * button is hardware. It is not a complete defence — an OEM device that declares `BTN_GAMEPAD`
     * and a pair of axes is indistinguishable from a pad at this layer — so the master switch stays
     * the guaranteed way out. `isVirtual` only means "device id < 0" (the platform's own synthetic
     * device), which is worth excluding but catches none of the above.
     */
    fun looksLikeController(dev: InputDevice?): Boolean {
        val d = dev ?: return false
        return looksLikeController(
            padSource = isPad(d),
            virtual = d.isVirtual,
            hasStick = d.getMotionRange(MotionEvent.AXIS_X, InputDevice.SOURCE_JOYSTICK) != null ||
                d.getMotionRange(MotionEvent.AXIS_HAT_X, InputDevice.SOURCE_JOYSTICK) != null,
            // `hasKeys` answers for the DEVICE, so a pad with no sticks at all (an arcade stick,
            // a d-pad-only pad) still counts.
            hasFaceButtons = d.hasKeys(KeyEvent.KEYCODE_BUTTON_A, KeyEvent.KEYCODE_BUTTON_B)
                .any { it },
        )
    }

    /** [looksLikeController]'s decision, over plain facts — the seam its truth table is tested at
     * (an [InputDevice] cannot be built off a device). */
    fun looksLikeController(
        padSource: Boolean,
        virtual: Boolean,
        hasStick: Boolean,
        hasFaceButtons: Boolean,
    ): Boolean = padSource && !virtual && (hasStick || hasFaceButtons)

    /**
     * All connected controllers, in system enumeration order — the devices that answer "is a pad
     * attached", so the filter is [looksLikeController] rather than the looser [isPad].
     */
    fun pads(): List<InputDevice> = InputDevice.getDeviceIds().toList()
        .mapNotNull { InputDevice.getDevice(it) }
        .filter { looksLikeController(it) }

    /** First connected gamepad/joystick [InputDevice], or null when none is attached. */
    fun firstPad(): InputDevice? = pads().firstOrNull()

    /**
     * True when a Steam Controller 2 is attached as an ORDINARY [InputDevice] — which, for a pad
     * this client wants to capture, means an uncaptured one still in lizard mode.
     *
     * Deliberately not filtered by [isPad]: lizard mode emulates a keyboard and mouse, so an SC2
     * is never a gamepad source and every other pad-shaped query in the client steps right past
     * it. That is also why this is worth having — a wired or Puck SC2 is found by enumerating USB
     * (no permission needed), but a BLE-paired one is invisible until `BLUETOOTH_CONNECT` is
     * granted, and asking for Bluetooth on the chance that someone might own one is not something
     * to put in front of every user. This is the permission-free signal that the pad is genuinely
     * there, so the request can be made to the people it helps and to nobody else.
     *
     * A false negative is survivable by design (the Controllers screen offers the grant outright),
     * so this matches only the identities we know rather than reaching for every Valve device — a
     * Steam Deck's own controller and a classic Steam Controller are not SC2s and must not
     * conjure a Bluetooth prompt.
     */
    fun sc2InputDevicePresent(): Boolean =
        InputDevice.getDeviceIds().asSequence().mapNotNull { InputDevice.getDevice(it) }.any {
            it.vendorId == VID_VALVE &&
                (it.productId in PID_STEAMCONTROLLER2 || it.productId in PID_STEAMCONTROLLER2_PUCK)
        }

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
     * The BTN_* bit for one key event from a SOURCE_GAMEPAD device — [buttonBit] plus the
     * Select-family button of every pad that carries no `BUTTON_SELECT` scancode at all.
     *
     * Plenty of controllers deliver that button as the plain `KEYCODE_BACK` a remote's Back uses,
     * with no `BUTTON_SELECT` behind it: it is the Android-TV shape, where every input device is
     * expected to offer Back, and a pad reaches it whether the vendor prints "Back" on the button
     * (NVIDIA's SHIELD controller) or "Select"/"View" (most pads in an Android mode). Which one is
     * on the couch cannot be told from here, and does not need to be — the keycode is what routes.
     *
     * Read through [buttonBit] alone that button mapped to nothing, so it fell out of the
     * streaming branch unconsumed and reached the activity's back stack, which is the
     * deliberate-quit exit: ONE press of Select dropped the session and the host logged a client
     * quit. `KEYCODE_BACK` is in fact the ONLY keycode that can get there from a pad — a mapped
     * button is consumed here, anything with a VK is consumed on the keycode path, volume/power go
     * to the system, and a FLAG_FALLBACK BACK is swallowed — which is what identifies this as the
     * cause of such a report without knowing the hardware.
     *
     * It also meant such a pad could not produce [BTN_BACK] at all, so every shortcut built on
     * Select — the emergency exit chord this client's own start banner advertises, the mic mute,
     * the stats tier — was unreachable on exactly the devices whose users have no keyboard.
     *
     * A pad that DOES carry `BUTTON_SELECT` is unaffected in both directions: it never had the
     * bug, and this changes nothing for it.
     *
     * FLAG_FALLBACK events are excluded: those are the synthetic BACK the framework raises after
     * an unconsumed `BUTTON_*` press (a pad reporting L2/R2 as keys, say), not a button anyone
     * touched, and forwarding one would put a phantom Select on the wire. `MainActivity` drops
     * them on the keycode path for the same reason.
     *
     * Callers must gate on `SOURCE_GAMEPAD` before asking, exactly as [buttonBit]'s `KEYCODE_DPAD_*`
     * rows require: a remote's or keyboard's BACK shares this keycode and has to keep leaving the
     * stream — for a device with no pad on it, Back IS the documented way out.
     */
    fun padButtonBit(keyCode: Int, flags: Int): Int = when {
        keyCode != KeyEvent.KEYCODE_BACK -> buttonBit(keyCode)
        flags and KeyEvent.FLAG_FALLBACK != 0 -> 0
        else -> BTN_BACK
    }

    // ---------------------------------------------------------------------------------------
    // Controllers Android has no key layout for
    //
    // Android turns a pad's raw evdev scancode into a `KeyEvent.keyCode` through a KEY LAYOUT
    // file matched on USB VID/PID (`Vendor_054c_Product_0ce6.kl` & co.). A pad with no matching
    // file falls back to AOSP's `Generic.kl`, which assigns keycodes by SCANCODE POSITION —
    // `0x130`→BUTTON_A, `0x131`→BUTTON_B, `0x132`→BUTTON_C, and so on up. That is only right if
    // the pad's buttons happen to sit at the positions the file assumes, and a HID gamepad with
    // no kernel driver behind it numbers its buttons 1..n straight through IN ITS OWN REPORT
    // ORDER — so every keycode after the first divergence is somebody else's button.
    //
    // Reported from a Fire TV Stick 4K Max (2026-08-20): a DualSense and an Xbox Elite Series 2,
    // both over Bluetooth, both identified correctly but with buttons landing on the wrong
    // actions ("L1 being L2"). Neither has a layout there — AOSP ships none for the Elite
    // Series 2 over Bluetooth (`045e:0b05`) on ANY version, and the DualSense's
    // (`054c:0ce6`) both postdates Fire OS and carries `requires_kernel_config
    // CONFIG_HID_PLAYSTATION`, which a Fire TV kernel does not have. A DualSense reporting
    // straight through puts L2 on `0x136`, which `Generic.kl` calls BUTTON_L1: the reported
    // symptom exactly.
    //
    // The fix is to resolve buttons from the SCANCODE, which is the pad's own report position and
    // is immune to the layout file — the same reason [Keymap.toVk] reads `scanCode` for keyboards.
    // Two things keep it from breaking a pad that already works:
    //
    //  1. Nothing is corrected on a pad that names its triggers ([padButtons]). A descriptor
    //     well-formed enough to call them Accelerator/Brake puts its buttons at the standard
    //     positions too, and that is the fact — not the model — that separates the two firmwares
    //     of the SAME Xbox pad, only the older of which needs any of this.
    //  2. Past that gate the correction still applies ONLY where the delivered keycode is what
    //     `Generic.kl` would have said ([genericKeyCode]). A different keycode means a
    //     device-specific layout IS in force and knows this pad better than we do.
    //
    // Moonlight carries the same two tables AND the same gate (`ControllerHandler`'s
    // `isNonStandardDualShock4` / `isNonStandardXboxBtController`, the latter on `gasRange == null`),
    // which is why both pads work there on the same box.
    //
    // The first cut of this asked `hasKeys(BUTTON_C, BUTTON_Z)` on its own, on the reasoning that a
    // pad numbering straight through reaches keycodes no controller has a button for. It does — but
    // so does every pad that merely DECLARES six buttons, because `hid-input` allocates `BTN_A + n`
    // straight through for the whole descriptor whether or not the pad ever presses them. That fired
    // the correction on pads Android was already reading correctly (2026-08-21: an Xbox pad
    // answering X with Y, Y with LB, and both shoulders with a menu button), and it could not have
    // done otherwise: the signal is identical on the firmware that needs correcting and the one that
    // does not. Declaration is not report order. Only the axes tell them apart.

    /** [MotionEvent] axis id meaning "this pad has no such axis" — see [PadMap]. */
    const val AXIS_NONE = -1

    /**
     * The report order a controller's buttons are numbered in, and with it which scancode carries
     * which physical button. Resolved once per device by [padButtons] from what the device
     * declares; [correct] then maps one scancode to the keycode it should have produced.
     */
    enum class PadButtons {
        /**
         * The keycode Android delivered is already right — a device-specific key layout is in
         * force, or the generic one happens to agree. [correct] changes nothing.
         */
        NATIVE,

        /**
         * A Sony pad numbering straight through with no kernel driver behind it: □ ✕ ○ △ L1 R1
         * L2 R2 Create Options L3 R3 PS, i.e. `0x130`..`0x13c` in that order. The analog trigger
         * value rides `AXIS_RX`/`AXIS_RY` on such a pad, so the digital L2/R2 fold to keycodes
         * [buttonBit] deliberately drops — the wire carries the axis, never both.
         */
        GENERIC_SONY,

        /**
         * An Xbox-layout pad numbering straight through: A B X Y LB RB View Menu LS RS, i.e.
         * `0x130`..`0x139`. Also the fallback for an unbranded pad, which near-universally
         * clones the Xbox layout — the same assumption [styleFor] makes for its glyphs.
         */
        GENERIC_XBOX,

        /**
         * A Sony pad WITH a kernel driver (`hid-playstation` / `hid-sony`) but still no key
         * layout — the combination an Android 11 box on a 5.10 kernel lands in. Such a driver
         * emits the modern Linux gamepad codes, where `0x133` is BTN_NORTH (△) and `0x134` is
         * BTN_WEST (□); `Generic.kl` reads those two as BUTTON_X and BUTTON_Y, so exactly the
         * face pair comes out swapped and nothing else is wrong.
         */
        SONY_MODERN,
        ;

        /**
         * The keycode scancode [scan] should have produced, given Android delivered [keyCode].
         *
         * Returns [keyCode] untouched unless it is precisely what [genericKeyCode] would have
         * said for [scan] — anything else is a device-specific layout's answer, which outranks
         * this table. That guard is what makes the correction idempotent and safe to run on
         * every pad: it can only ever fire where Android was guessing in the first place.
         */
        fun correct(scan: Int, keyCode: Int): Int {
            if (this == NATIVE) return keyCode
            if (keyCode != genericKeyCode(scan)) return keyCode
            val fixed = when (this) {
                GENERIC_SONY -> when (scan) {
                    0x130 -> KeyEvent.KEYCODE_BUTTON_X // □
                    0x131 -> KeyEvent.KEYCODE_BUTTON_A // ✕
                    0x132 -> KeyEvent.KEYCODE_BUTTON_B // ○
                    0x133 -> KeyEvent.KEYCODE_BUTTON_Y // △
                    0x134 -> KeyEvent.KEYCODE_BUTTON_L1
                    0x135 -> KeyEvent.KEYCODE_BUTTON_R1
                    0x136 -> KeyEvent.KEYCODE_BUTTON_L2 // analog: AXIS_RX
                    0x137 -> KeyEvent.KEYCODE_BUTTON_R2 // analog: AXIS_RY
                    0x138 -> KeyEvent.KEYCODE_BUTTON_SELECT // Create / Share
                    0x139 -> KeyEvent.KEYCODE_BUTTON_START // Options
                    0x13a -> KeyEvent.KEYCODE_BUTTON_THUMBL
                    0x13b -> KeyEvent.KEYCODE_BUTTON_THUMBR
                    0x13c -> KeyEvent.KEYCODE_BUTTON_MODE // PS
                    // 0x13d touchpad click / 0x13e mute: no wire button, dropped as before.
                    else -> KeyEvent.KEYCODE_UNKNOWN
                }
                GENERIC_XBOX -> when (scan) {
                    0x132 -> KeyEvent.KEYCODE_BUTTON_X
                    0x133 -> KeyEvent.KEYCODE_BUTTON_Y
                    0x134 -> KeyEvent.KEYCODE_BUTTON_L1
                    0x135 -> KeyEvent.KEYCODE_BUTTON_R1
                    0x136 -> KeyEvent.KEYCODE_BUTTON_SELECT // View
                    0x137 -> KeyEvent.KEYCODE_BUTTON_START // Menu
                    0x138 -> KeyEvent.KEYCODE_BUTTON_THUMBL
                    0x139 -> KeyEvent.KEYCODE_BUTTON_THUMBR
                    else -> keyCode // 0x130 A / 0x131 B already agree
                }
                // Only the face pair; every other row of Generic.kl is right for these codes.
                SONY_MODERN -> when (scan) {
                    0x133 -> KeyEvent.KEYCODE_BUTTON_Y // BTN_NORTH = △
                    0x134 -> KeyEvent.KEYCODE_BUTTON_X // BTN_WEST  = □
                    else -> keyCode
                }
                NATIVE -> keyCode
            }
            return fixed
        }
    }

    /**
     * AOSP `Generic.kl`'s gamepad rows — the layout Android falls back to when no device-specific
     * key layout matches the pad's VID/PID. Scancodes outside it answer [KeyEvent.KEYCODE_UNKNOWN],
     * which never equals a real delivered keycode, so [PadButtons.correct]'s guard leaves those
     * events alone.
     */
    fun genericKeyCode(scan: Int): Int = when (scan) {
        0x130 -> KeyEvent.KEYCODE_BUTTON_A
        0x131 -> KeyEvent.KEYCODE_BUTTON_B
        0x132 -> KeyEvent.KEYCODE_BUTTON_C
        0x133 -> KeyEvent.KEYCODE_BUTTON_X
        0x134 -> KeyEvent.KEYCODE_BUTTON_Y
        0x135 -> KeyEvent.KEYCODE_BUTTON_Z
        0x136 -> KeyEvent.KEYCODE_BUTTON_L1
        0x137 -> KeyEvent.KEYCODE_BUTTON_R1
        0x138 -> KeyEvent.KEYCODE_BUTTON_L2
        0x139 -> KeyEvent.KEYCODE_BUTTON_R2
        0x13a -> KeyEvent.KEYCODE_BUTTON_SELECT
        0x13b -> KeyEvent.KEYCODE_BUTTON_START
        0x13c -> KeyEvent.KEYCODE_BUTTON_MODE
        0x13d -> KeyEvent.KEYCODE_BUTTON_THUMBL
        0x13e -> KeyEvent.KEYCODE_BUTTON_THUMBR
        else -> KeyEvent.KEYCODE_UNKNOWN
    }

    /**
     * How one controller must be read: its button report order plus the axes its right stick and
     * analog triggers actually arrive on. Resolved once per device by [padMap].
     */
    class PadMap(
        val buttons: PadButtons,
        val rightStickX: Int = MotionEvent.AXIS_Z,
        val rightStickY: Int = MotionEvent.AXIS_RZ,
        /**
         * The trigger axes, or [AXIS_NONE] for a pad Android already names them on — that case
         * keeps folding LTRIGGER with BRAKE and RTRIGGER with GAS by max, which is what pads that
         * report one pair, the other, or both have always needed.
         */
        val leftTrigger: Int = AXIS_NONE,
        val rightTrigger: Int = AXIS_NONE,
        /** Those trigger axes rest at −1 rather than 0, measured off the device's own range. */
        val triggersSigned: Boolean = false,
    ) {
        /** One resolved trigger axis value, folded to the 0..1 the wire scale expects. */
        fun level(v: Float): Float = if (triggersSigned) (v + 1f) / 2f else v
    }

    /** The map every pad with a key layout uses: Android's own names, unchanged. */
    private val NATIVE_MAP = PadMap(PadButtons.NATIVE)

    /**
     * Resolved [PadMap]s, keyed by [InputDevice.getDescriptor] — the device's stable identity
     * hash, so a pad that reconnects is recognised and a model resolves once for the process.
     * Nothing here depends on a live connection, so entries never need evicting.
     */
    private val padMaps = ConcurrentHashMap<String, PadMap>()

    /**
     * Which report order [dev]'s buttons follow — [namedTriggers] is whether the pad reports its
     * triggers under a name Android knows (see [padMap]), and [declaresCZ] whether it declares
     * BUTTON_C and BUTTON_Z.
     *
     * `namedTriggers` decides it, and a pad that has them is [PadButtons.NATIVE] whatever else it
     * says. A HID gamepad describes its triggers either as the Accelerator/Brake usages, which
     * become `ABS_GAS`/`ABS_BRAKE` and axis names Android has words for, or as two more generic
     * axes on `ABS_Z`/`ABS_RZ`, which it does not — and a report descriptor well-formed enough to
     * name its triggers puts its buttons at the standard positions too, the ones `Generic.kl`
     * already reads correctly. It is the same fact Moonlight decides this on (`gasRange == null`
     * beside the `"Xbox Wireless Controller"` name), and it is the one that separates the two
     * firmwares of the SAME pad: an Xbox Wireless Controller over Bluetooth reports GAS/BRAKE
     * after its firmware update and Z/Rz before it, and only the older one needs correcting.
     *
     * `declaresCZ` cannot make that call and must never be asked to. `hasKeys` answers for what a
     * device DECLARES, not what it reports: `hid-input` allocates `BTN_A + n` straight through for
     * every button in the descriptor, so BTN_C (`0x132`) and BTN_Z (`0x135`) are set on any pad
     * declaring six or more — a standard-layout pad that never presses either included. Read alone
     * it fired the correction on pads whose buttons were already right, which is how an Xbox pad
     * came to answer X with Y and Y with LB (field reports, 2026-08-21). It stays as the narrower
     * question it can answer — WHICH straight-through order, once `namedTriggers` has established
     * there is one — where a false positive costs nothing.
     */
    fun padButtons(dev: InputDevice, namedTriggers: Boolean): PadButtons {
        val has = dev.hasKeys(KeyEvent.KEYCODE_BUTTON_C, KeyEvent.KEYCODE_BUTTON_Z, 0)
        return padButtons(namedTriggers, dev.vendorId == VID_SONY, declaresCZ = has[0] && has[1])
    }

    /** [padButtons]'s choice over plain facts — the seam its truth table is tested at (an
     * [InputDevice] cannot be built off a device). */
    fun padButtons(namedTriggers: Boolean, sony: Boolean, declaresCZ: Boolean): PadButtons = when {
        namedTriggers -> PadButtons.NATIVE
        declaresCZ && sony -> PadButtons.GENERIC_SONY
        declaresCZ -> PadButtons.GENERIC_XBOX
        sony -> PadButtons.SONY_MODERN
        else -> PadButtons.NATIVE
    }

    /**
     * The [PadMap] for [dev] — its button report order and the axes its right stick and triggers
     * arrive on, resolved once per device model and cached.
     *
     * Axes get the same treatment as buttons: a pad Android has a layout for names its triggers
     * LTRIGGER/RTRIGGER (or BRAKE/GAS, or BRAKE/THROTTLE) and is left exactly as it was. A pad
     * with NONE of those names is one Android never mapped, and its triggers are sitting on two
     * raw axes under the names the HID report gave them. Which two depends on the same report
     * order the buttons did:
     *
     *  - a Sony pad reporting straight through lays out X, Y, Z, Rz, Rx, Ry = left stick, right
     *    stick, then the triggers — so the right stick is already right and only the triggers
     *    (`AXIS_RX`/`AXIS_RY`) are missed;
     *  - every other such pad puts the right stick on Rx/Ry and the triggers on Z/Rz, which is
     *    the shape that makes pulling a trigger swing the right stick.
     *
     * Whether those axes idle at −1 is MEASURED from the device's own range rather than assumed,
     * so a pad that reports an honest 0..1 is not rescaled to a permanent half-pull.
     */
    fun padMap(dev: InputDevice?): PadMap {
        if (dev == null) return NATIVE_MAP
        padMaps[dev.descriptor]?.let { return it }
        fun has(a: Int) = axis(dev, a) != null
        val named = (has(MotionEvent.AXIS_LTRIGGER) && has(MotionEvent.AXIS_RTRIGGER)) ||
            (has(MotionEvent.AXIS_BRAKE) && has(MotionEvent.AXIS_GAS)) ||
            (has(MotionEvent.AXIS_BRAKE) && has(MotionEvent.AXIS_THROTTLE))
        val buttons = padButtons(dev, namedTriggers = named)
        val rx = axis(dev, MotionEvent.AXIS_RX)
        val hasRxRy = rx != null && has(MotionEvent.AXIS_RY)
        // Whichever pair the fallback is about to pick, ask THAT one where it rests.
        val restsNegative = if (buttons == PadButtons.GENERIC_SONY) {
            (rx?.min ?: 0f) < -0.5f
        } else {
            (axis(dev, MotionEvent.AXIS_Z)?.min ?: 0f) < -0.5f
        }
        val map = padMap(buttons, namedTriggers = named, hasRxRy = hasRxRy, restsNegative = restsNegative)
        padMaps[dev.descriptor] = map
        return map
    }

    /**
     * The axis half of [padMap], decided from four facts about the device so it can be pinned
     * without one — see `PadButtonsTest`. [namedTriggers] is whether the pad calls its triggers
     * anything Android knows (LTRIGGER/RTRIGGER, BRAKE/GAS, BRAKE/THROTTLE); if it does, nothing
     * here applies and the pad is read exactly as it always was. [restsNegative] is measured off
     * whichever axis pair the fallback picks, never assumed.
     */
    fun padMap(
        buttons: PadButtons,
        namedTriggers: Boolean,
        hasRxRy: Boolean,
        restsNegative: Boolean,
    ): PadMap = when {
        namedTriggers || !hasRxRy -> PadMap(buttons)
        // X, Y, Z, Rz, Rx, Ry = left stick, right stick, triggers. The sticks already read right.
        buttons == PadButtons.GENERIC_SONY -> PadMap(
            buttons,
            leftTrigger = MotionEvent.AXIS_RX,
            rightTrigger = MotionEvent.AXIS_RY,
            triggersSigned = restsNegative,
        )
        // Right stick on Rx/Ry and triggers on Z/Rz — the shape in which reading Z/Rz as the
        // right stick makes pulling a trigger swing it.
        else -> PadMap(
            buttons,
            rightStickX = MotionEvent.AXIS_RX,
            rightStickY = MotionEvent.AXIS_RY,
            leftTrigger = MotionEvent.AXIS_Z,
            rightTrigger = MotionEvent.AXIS_RZ,
            triggersSigned = restsNegative,
        )
    }

    /** [dev]'s range for one joystick [axis], under either source class a pad reports on. */
    private fun axis(dev: InputDevice, axis: Int): InputDevice.MotionRange? =
        dev.getMotionRange(axis, InputDevice.SOURCE_JOYSTICK)
            ?: dev.getMotionRange(axis, InputDevice.SOURCE_GAMEPAD)

    /**
     * The keycode [event] should have carried, given the controller it came from — [event]'s own
     * keycode for every pad Android has a key layout for, and the scancode's true button for one
     * it does not (see the block comment above [PadButtons]).
     *
     * A drop-in for `event.keyCode` at every gamepad reader: the console UI's navigation, the
     * Controllers screen's tester, and the streaming branch all route through it, so a mis-mapped
     * pad is fixed in the menus and in the game at once. Events from anything that is not a
     * controller, and events with no scancode (soft keyboards, synthetic events), pass through
     * untouched.
     */
    fun padKeyCode(event: KeyEvent): Int {
        val dev = event.device ?: return event.keyCode
        if (event.scanCode == 0 || !isPad(dev)) return event.keyCode
        return padMap(dev).buttons.correct(event.scanCode, event.keyCode)
    }

    /**
     * Maps one controller's joystick MotionEvents to axis (+ HAT→dpad) sends on wire pad index [pad],
     * **on change only**. Holds the previous axis/hat state so an unchanged frame emits nothing. One
     * instance per forwarded controller (owned by [GamepadRouter], which routes each device's events
     * to its own mapper so a second pad can't clobber the first); call [reset] on that slot closing
     * (disconnect / session stop) so nothing sticks on the host (which has no client-side held-state
     * knowledge).
     *
     * The router only ever feeds this a qualifying event from the mapper's own device — a real
     * gamepad (its source classes include GAMEPAD), never a controller's joystick-classified sibling
     * node (DualSense/DS4 motion sensors), which reports every pad axis as 0. [onMotion] therefore
     * folds the event straight in without re-qualifying it.
     */
    class AxisMapper(
        private val handle: Long,
        private val pad: Int,
        /** Which axes this controller's right stick and triggers arrive on — see [padMap]. */
        private val map: PadMap = NATIVE_MAP,
    ) {
        // Sentinel so the first real value (incl. 0) always sends once after attach (Linux parity).
        private val last = IntArray(6) { Int.MIN_VALUE }
        private var hatX = 0 // -1 / 0 / +1
        private var hatY = 0

        /** Fold one joystick ACTION_MOVE from this mapper's controller onto its pad index. */
        fun onMotion(event: MotionEvent) {
            // Sticks: Android floats −1..1, +y = down → ±32767, negate Y for the wire's +y = up.
            sendAxis(AXIS_LS_X, stick(event.getAxisValue(MotionEvent.AXIS_X)))
            sendAxis(AXIS_LS_Y, stick(-event.getAxisValue(MotionEvent.AXIS_Y)))
            sendAxis(AXIS_RS_X, stick(event.getAxisValue(map.rightStickX)))
            sendAxis(AXIS_RS_Y, stick(-event.getAxisValue(map.rightStickY)))

            // Triggers: pads report LTRIGGER/RTRIGGER or BRAKE/GAS (some mirror both) — merge
            // with max, the same fold as the Controllers screen probe, so a pad that reports
            // only one pair and a pad that reports both behave identically; 0..1 → 0..255. A pad
            // reporting NONE of those names is one Android has no key layout for, and [map]
            // carries the raw axes its triggers really landed on instead.
            val lt = resolved(event, map.leftTrigger, MotionEvent.AXIS_LTRIGGER, MotionEvent.AXIS_BRAKE)
            val rt = resolved(event, map.rightTrigger, MotionEvent.AXIS_RTRIGGER, MotionEvent.AXIS_GAS)
            sendAxis(AXIS_LT, trigger(lt))
            sendAxis(AXIS_RT, trigger(rt))

            // HAT → dpad button transitions. Android BATCHES joystick ACTION_MOVEs, so a rapid d-pad
            // tap (press+release inside one batch window) lives only in the historical samples — the
            // final getAxisValue would show the HAT already back at rest and miss the tap entirely.
            // Feed every historical HAT sample (oldest→newest) through the same transition logic
            // before the current one, so each edge is emitted. (Sticks/triggers stay latest-wins:
            // only the final value matters for an analog axis.)
            for (h in 0 until event.historySize) {
                applyHat(
                    sign(event.getHistoricalAxisValue(MotionEvent.AXIS_HAT_X, h)),
                    sign(event.getHistoricalAxisValue(MotionEvent.AXIS_HAT_Y, h)),
                )
            }
            applyHat(
                sign(event.getAxisValue(MotionEvent.AXIS_HAT_X)),
                sign(event.getAxisValue(MotionEvent.AXIS_HAT_Y)),
            )
        }

        /** Emit dpad button deltas for one HAT sample (`hx`/`hy` each −1/0/+1), tracking held state. */
        private fun applyHat(hx: Int, hy: Int) {
            if (hx != hatX) {
                if (hatX < 0) btn(BTN_DPAD_LEFT, false) else if (hatX > 0) btn(BTN_DPAD_RIGHT, false)
                if (hx < 0) btn(BTN_DPAD_LEFT, true) else if (hx > 0) btn(BTN_DPAD_RIGHT, true)
                hatX = hx
            }
            if (hy != hatY) {
                if (hatY < 0) btn(BTN_DPAD_UP, false) else if (hatY > 0) btn(BTN_DPAD_DOWN, false)
                if (hy < 0) btn(BTN_DPAD_UP, true) else if (hy > 0) btn(BTN_DPAD_DOWN, true)
                hatY = hy
            }
        }

        /** Release-all: zero every axis and clear the held dpad (all on this mapper's pad index). */
        fun reset() {
            for (id in 0..5) sendAxis(id, 0)
            if (hatX < 0) btn(BTN_DPAD_LEFT, false) else if (hatX > 0) btn(BTN_DPAD_RIGHT, false)
            if (hatY < 0) btn(BTN_DPAD_UP, false) else if (hatY > 0) btn(BTN_DPAD_DOWN, false)
            hatX = 0
            hatY = 0
        }

        /**
         * One trigger's 0..1 value: [resolvedAxis] when this pad needed one resolved for it,
         * else the max of the two names Android gives a trigger it does know.
         */
        private fun resolved(event: MotionEvent, resolvedAxis: Int, named: Int, alias: Int): Float =
            if (resolvedAxis == AXIS_NONE) {
                maxOf(event.getAxisValue(named), event.getAxisValue(alias))
            } else {
                map.level(event.getAxisValue(resolvedAxis))
            }

        private fun sendAxis(id: Int, v: Int) {
            if (last[id] == v) return
            last[id] = v
            NativeBridge.nativeSendGamepadAxis(handle, id, v, pad)
        }

        private fun btn(bit: Int, down: Boolean) = NativeBridge.nativeSendGamepadButton(handle, bit, down, pad)

        // −1..1 float → ±32767 i16 (matches the Apple client's 32767 scale).
        private fun stick(v: Float): Int = (v.coerceIn(-1f, 1f) * 32767f).toInt()

        // 0..1 float → 0..255.
        private fun trigger(v: Float): Int = (v.coerceIn(0f, 1f) * 255f).toInt()

        private fun sign(v: Float): Int = if (v < -0.5f) -1 else if (v > 0.5f) 1 else 0
    }
}
