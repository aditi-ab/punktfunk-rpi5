package io.unom.punktfunk.kit

/**
 * Sony DualSense / DualSense Edge / DualShock 4 **USB** protocol constants: the input-report
 * parser and the output-report builders the capture link ([DsCapture]) needs. Unlike the SC2's
 * as-is passthrough, nothing rides the wire raw here — the host's DualSense/DS4 backends consume
 * only typed events (`dualsense_proto.rs` discards `RichInput::HidReport`), so the client parses
 * the pad's input reports into the ordinary button/axis wire + the rich touch/motion plane, and
 * renders the host's feedback (rumble / adaptive triggers / lightbar / player LEDs) by composing
 * USB output reports itself.
 *
 * Protocol ground truth: the Linux kernel's `hid-playstation` / `hid-sony` structs, SDL's
 * `SDL_hidapi_ps5.c` / `SDL_hidapi_ps4.c`, mirrored host-side in `punktfunk-host`'s
 * `dualsense_proto.rs` / `dualshock4_proto.rs` — this file is the byte-exact inverse of those
 * serializers (offsets cross-referenced below). USB only: over Bluetooth the reports shift
 * (`0x31` + CRC32) AND Android exposes no raw path to a Classic pad anyway, so the BT case never
 * reaches this code — an uncaptured pad stays on the ordinary InputDevice path.
 */
object DsDevice {
    const val VID_SONY = 0x054C
    const val PID_DUALSENSE = 0x0CE6
    const val PID_DUALSENSE_EDGE = 0x0DF2
    const val PID_DUALSHOCK4_V1 = 0x05C4
    const val PID_DUALSHOCK4_V2 = 0x09CC

    val USB_PIDS = setOf(PID_DUALSENSE, PID_DUALSENSE_EDGE, PID_DUALSHOCK4_V1, PID_DUALSHOCK4_V2)

    /**
     * One captured model: its `GamepadPref` wire byte (the virtual pad the host builds — matching
     * the physical one), its output-report size (the descriptor-declared size the firmware
     * expects: DS5 48 = id + 47, Edge 64 = id + 63, DS4 32 = id + 31), and its touchpad extent
     * (`dualsense_proto::DS_TOUCH_W/H`, `dualshock4_proto::DS4_TOUCH_*`) for normalizing touches
     * onto the wire's 0..65535 space.
     */
    enum class Model(val pref: Int, val outputSize: Int, val touchW: Int, val touchH: Int) {
        DUALSENSE(Gamepad.PREF_DUALSENSE, 48, 1920, 1080),
        DUALSENSE_EDGE(Gamepad.PREF_DUALSENSEEDGE, 64, 1920, 1080),
        DUALSHOCK4(Gamepad.PREF_DUALSHOCK4, 32, 1920, 942),
    }

    /** The captured [Model] for a USB PID, or null for anything we don't capture. */
    fun modelFor(pid: Int): Model? = when (pid) {
        PID_DUALSENSE -> Model.DUALSENSE
        PID_DUALSENSE_EDGE -> Model.DUALSENSE_EDGE
        PID_DUALSHOCK4_V1, PID_DUALSHOCK4_V2 -> Model.DUALSHOCK4
        else -> null
    }

    /**
     * The client-consumed fields of one input report. `buttons` is already the WIRE bitmask
     * (`Gamepad.BTN_*`) — the parse maps device bits straight to the wire, the exact inverse of
     * the host's `DsState::from_gamepad` (BTN_A ↔ cross, BTN_B ↔ circle, BTN_X ↔ square,
     * BTN_Y ↔ triangle; positional, not glyph-order). Gyro/accel stay in raw device units — the
     * wire's `Motion` is a unit passthrough into the virtual pad's report. Touch coordinates stay
     * device-raw here; [DsCapture] normalizes against the model's extent when forwarding.
     */
    class State {
        var buttons = 0
        var lsX = 0; var lsY = 0 // wire i16, +y = up (device is +y down — inverted in the parse)
        var rsX = 0; var rsY = 0
        var lt = 0; var rt = 0 // 0..255
        val gyro = IntArray(3) // raw i16 units (pitch/yaw/roll)
        val accel = IntArray(3)
        val touchActive = BooleanArray(2)
        val touchX = IntArray(2) // raw device coords (0..touchW-1 / 0..touchH-1)
        val touchY = IntArray(2)
    }

    // DS5 USB input report 0x01 (64 B) — offsets mirror the host serializer
    // (`dualsense_proto.rs::serialize_state`): [1..7) sticks + triggers, [8] hat|face,
    // [9]/[10] buttons, [16..28) gyro+accel, [33..41) two 4-byte touch points.
    private const val DS5_INPUT_ID = 0x01
    // report[8] high nibble (`dualsense_proto::btn0`).
    private const val DS5_SQUARE = 0x10
    private const val DS5_CROSS = 0x20
    private const val DS5_CIRCLE = 0x40
    private const val DS5_TRIANGLE = 0x80
    // report[9] (`btn1`).
    private const val DS5_L1 = 0x01
    private const val DS5_R1 = 0x02
    private const val DS5_CREATE = 0x10
    private const val DS5_OPTIONS = 0x20
    private const val DS5_L3 = 0x40
    private const val DS5_R3 = 0x80
    // report[10] (`btn2`); the FN/BACK bits exist only on the Edge.
    private const val DS5_PS = 0x01
    private const val DS5_TOUCHPAD = 0x02
    private const val DS5_MUTE = 0x04
    private const val EDGE_FN_LEFT = 0x10
    private const val EDGE_FN_RIGHT = 0x20
    private const val EDGE_BACK_LEFT = 0x40
    private const val EDGE_BACK_RIGHT = 0x80

    // DS4 USB input report 0x01 (64 B) — offsets mirror `dualshock4_proto.rs::serialize_state`:
    // [1..5) sticks, [5] hat|face, [6]/[7] buttons, [8]/[9] triggers, [13..25) gyro+accel,
    // [35..43) two touch points (same 4-byte packing as the DS5).
    private const val DS4_L1 = 0x01
    private const val DS4_R1 = 0x02
    private const val DS4_SHARE = 0x10
    private const val DS4_OPTIONS = 0x20
    private const val DS4_L3 = 0x40
    private const val DS4_R3 = 0x80
    private const val DS4_PS = 0x01
    private const val DS4_TOUCHPAD = 0x02

    /**
     * Parse one USB input report (`0x01`) into [out]. Returns false for any other report id or a
     * short read (the pad also emits `0x09`-family getMAC responses etc. on EP0 — those never hit
     * the interrupt endpoint, but be defensive). Motion/touch fields update only when the report
     * is long enough to carry them (it always is on glass — 64-byte interrupt transfers).
     */
    fun parseState(model: Model, report: ByteArray, len: Int, out: State): Boolean =
        if (model == Model.DUALSHOCK4) {
            parseDs4(report, len, out)
        } else {
            parseDs5(model, report, len, out)
        }

    private fun parseDs5(model: Model, r: ByteArray, len: Int, out: State): Boolean {
        if (len < 11 || (r[0].toInt() and 0xFF) != DS5_INPUT_ID) return false
        out.lsX = stickX(u8(r, 1))
        out.lsY = stickY(u8(r, 2))
        out.rsX = stickX(u8(r, 3))
        out.rsY = stickY(u8(r, 4))
        out.lt = u8(r, 5)
        out.rt = u8(r, 6)
        val b8 = u8(r, 8)
        val b9 = u8(r, 9)
        val b10 = u8(r, 10)
        var w = hatBits(b8 and 0x0F)
        if (b8 and DS5_CROSS != 0) w = w or Gamepad.BTN_A
        if (b8 and DS5_CIRCLE != 0) w = w or Gamepad.BTN_B
        if (b8 and DS5_SQUARE != 0) w = w or Gamepad.BTN_X
        if (b8 and DS5_TRIANGLE != 0) w = w or Gamepad.BTN_Y
        if (b9 and DS5_L1 != 0) w = w or Gamepad.BTN_LB
        if (b9 and DS5_R1 != 0) w = w or Gamepad.BTN_RB
        // L2/R2 digital bits ride the analog axes instead (wire convention).
        if (b9 and DS5_CREATE != 0) w = w or Gamepad.BTN_BACK
        if (b9 and DS5_OPTIONS != 0) w = w or Gamepad.BTN_START
        if (b9 and DS5_L3 != 0) w = w or Gamepad.BTN_LS_CLICK
        if (b9 and DS5_R3 != 0) w = w or Gamepad.BTN_RS_CLICK
        if (b10 and DS5_PS != 0) w = w or Gamepad.BTN_GUIDE
        if (b10 and DS5_TOUCHPAD != 0) w = w or Gamepad.BTN_TOUCHPAD
        if (b10 and DS5_MUTE != 0) w = w or Gamepad.BTN_MISC1
        if (model == Model.DUALSENSE_EDGE) {
            // Wire paddle order matches the host's `edge_paddle_bits` inverse: PADDLE1/2 =
            // right/left BACK (the primary pair, Steam R4/L4 convention), PADDLE3/4 = right/left Fn.
            if (b10 and EDGE_BACK_RIGHT != 0) w = w or Gamepad.BTN_PADDLE1
            if (b10 and EDGE_BACK_LEFT != 0) w = w or Gamepad.BTN_PADDLE2
            if (b10 and EDGE_FN_RIGHT != 0) w = w or Gamepad.BTN_PADDLE3
            if (b10 and EDGE_FN_LEFT != 0) w = w or Gamepad.BTN_PADDLE4
        }
        out.buttons = w
        if (len >= 28) {
            for (i in 0 until 3) out.gyro[i] = i16(r, 16 + 2 * i)
            for (i in 0 until 3) out.accel[i] = i16(r, 22 + 2 * i)
        }
        if (len >= 41) {
            unpackTouch(r, 33, out, 0)
            unpackTouch(r, 37, out, 1)
        }
        return true
    }

    private fun parseDs4(r: ByteArray, len: Int, out: State): Boolean {
        if (len < 10 || (r[0].toInt() and 0xFF) != DS5_INPUT_ID) return false // DS4 shares id 0x01
        out.lsX = stickX(u8(r, 1))
        out.lsY = stickY(u8(r, 2))
        out.rsX = stickX(u8(r, 3))
        out.rsY = stickY(u8(r, 4))
        val b5 = u8(r, 5)
        val b6 = u8(r, 6)
        val b7 = u8(r, 7)
        out.lt = u8(r, 8)
        out.rt = u8(r, 9)
        var w = hatBits(b5 and 0x0F)
        if (b5 and DS5_CROSS != 0) w = w or Gamepad.BTN_A
        if (b5 and DS5_CIRCLE != 0) w = w or Gamepad.BTN_B
        if (b5 and DS5_SQUARE != 0) w = w or Gamepad.BTN_X
        if (b5 and DS5_TRIANGLE != 0) w = w or Gamepad.BTN_Y
        if (b6 and DS4_L1 != 0) w = w or Gamepad.BTN_LB
        if (b6 and DS4_R1 != 0) w = w or Gamepad.BTN_RB
        if (b6 and DS4_SHARE != 0) w = w or Gamepad.BTN_BACK
        if (b6 and DS4_OPTIONS != 0) w = w or Gamepad.BTN_START
        if (b6 and DS4_L3 != 0) w = w or Gamepad.BTN_LS_CLICK
        if (b6 and DS4_R3 != 0) w = w or Gamepad.BTN_RS_CLICK
        if (b7 and DS4_PS != 0) w = w or Gamepad.BTN_GUIDE
        if (b7 and DS4_TOUCHPAD != 0) w = w or Gamepad.BTN_TOUCHPAD
        out.buttons = w
        if (len >= 25) {
            for (i in 0 until 3) out.gyro[i] = i16(r, 13 + 2 * i)
            for (i in 0 until 3) out.accel[i] = i16(r, 19 + 2 * i)
        }
        if (len >= 43) {
            unpackTouch(r, 35, out, 0)
            unpackTouch(r, 39, out, 1)
        }
        return true
    }

    /** hat nibble (0=N … 7=NW, 8+=neutral) → wire dpad bits — inverse of the host's `hat()`. */
    private fun hatBits(h: Int): Int = when (h) {
        0 -> Gamepad.BTN_DPAD_UP
        1 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_RIGHT
        2 -> Gamepad.BTN_DPAD_RIGHT
        3 -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_RIGHT
        4 -> Gamepad.BTN_DPAD_DOWN
        5 -> Gamepad.BTN_DPAD_DOWN or Gamepad.BTN_DPAD_LEFT
        6 -> Gamepad.BTN_DPAD_LEFT
        7 -> Gamepad.BTN_DPAD_UP or Gamepad.BTN_DPAD_LEFT
        else -> 0
    }

    /**
     * One 4-byte touch point (shared DS5/DS4 packing — `dualsense_proto::pack_touch`): byte0
     * bit7 = NOT active + contact id in bits 0..6; 12-bit x/y split across bytes 1..3.
     */
    private fun unpackTouch(r: ByteArray, o: Int, out: State, slot: Int) {
        val b0 = u8(r, o)
        out.touchActive[slot] = b0 and 0x80 == 0
        out.touchX[slot] = u8(r, o + 1) or ((u8(r, o + 2) and 0x0F) shl 8)
        out.touchY[slot] = (u8(r, o + 2) shr 4) or (u8(r, o + 3) shl 4)
    }

    private fun u8(r: ByteArray, o: Int): Int = r[o].toInt() and 0xFF

    private fun i16(r: ByteArray, o: Int): Int =
        ((r[o + 1].toInt() shl 8) or (r[o].toInt() and 0xFF)).toShort().toInt()

    // Device stick byte (0..255, centre 0x80, +y down) → wire i16 (+y up) — the exact inverse of
    // the host's `to_u8` mapping (`lx = to_u8(x)`, `ly = 255 - to_u8(y)`).
    private fun stickX(raw: Int): Int = raw * 257 - 32768

    private fun stickY(raw: Int): Int = (255 - raw) * 257 - 32768

    // ---- Output reports ----
    //
    // Every write is valid-flag-selective: only the flagged channel applies, the firmware keeps
    // the rest (the same contract the host's `parse_ds_output` mirrors — an unflagged parse would
    // turn every rumble into a lightbar-off). The DS4 is the exception: its builder writes the
    // full composed motors+LED state each time with both flags, SDL's proven-on-hardware shape.

    // DS5 output report 0x02, report-relative offsets (`dualsense_proto::parse_ds_output`):
    // [1] valid_flag0 (bit0 compat vibration, bit1 haptics select, bit2 R2 block, bit3 L2 block),
    // [2] valid_flag1 (bit2 lightbar, bit4 player LEDs), [3]/[4] motors, [11..22) R2 effect,
    // [22..33) L2 effect, [39] valid_flag2 (bit1 lightbar-setup enable, bit2 vibration2),
    // [42] lightbar_setup, [44] player LEDs, [45..48) RGB.
    private const val DS5_FLAG0_COMPAT_VIBRATION = 0x01
    private const val DS5_FLAG0_HAPTICS_SELECT = 0x02
    private const val DS5_FLAG0_R2_EFFECT = 0x04
    private const val DS5_FLAG0_L2_EFFECT = 0x08
    private const val DS5_FLAG1_LIGHTBAR = 0x04
    private const val DS5_FLAG1_PLAYER_LEDS = 0x10
    private const val DS5_FLAG2_LIGHTBAR_SETUP = 0x02
    private const val DS5_FLAG2_VIBRATION2 = 0x04
    private const val DS5_LIGHTBAR_SETUP_LIGHT_OUT = 0x02

    /** The 11-byte adaptive-trigger effect block length (mode byte + 10 parameters). */
    const val TRIGGER_EFFECT_LEN = 11

    private fun newDs5(model: Model): ByteArray = ByteArray(model.outputSize).also { it[0] = 0x02 }

    /**
     * One-time capture-start report (DS5/Edge): release the firmware's lightbar animation
     * (`LIGHTBAR_SETUP_LIGHT_OUT`) so subsequent host lightbar writes take effect — the same
     * init both hid-playstation and SDL send on open. No-op fields otherwise.
     */
    fun ds5InitReport(model: Model): ByteArray = newDs5(model).also {
        it[39] = DS5_FLAG2_LIGHTBAR_SETUP.toByte()
        it[42] = DS5_LIGHTBAR_SETUP_LIGHT_OUT.toByte()
    }

    /**
     * DS5/Edge rumble at the wire's u16 amplitudes ([low] = heavy/left motor, [high] =
     * light/right — the host parses `[3]` as high and `[4]` as low, mirrored here). Flags both
     * the classic compat-vibration path AND `VIBRATION2` (firmware ≥ 2.24's full-range replot;
     * older firmware ignores the unknown flag2 bit) — the host parser accepts either.
     */
    fun ds5RumbleReport(model: Model, low: Int, high: Int): ByteArray = newDs5(model).also {
        it[1] = (DS5_FLAG0_COMPAT_VIBRATION or DS5_FLAG0_HAPTICS_SELECT).toByte()
        it[39] = DS5_FLAG2_VIBRATION2.toByte()
        it[3] = amp8(high).toByte()
        it[4] = amp8(low).toByte()
    }

    /**
     * DS5/Edge adaptive-trigger effect: [which] 0 = L2, 1 = R2; [effect] is the raw 11-byte
     * trigger block from the wire (`HidOutput::Trigger` — the game's bytes verbatim), copied to
     * the same offsets the host parsed it from ([11..22) R2 / [22..33) L2).
     */
    fun ds5TriggerReport(model: Model, which: Int, effect: ByteArray): ByteArray = newDs5(model).also {
        val at = if (which == 1) 11 else 22
        it[1] = (if (which == 1) DS5_FLAG0_R2_EFFECT else DS5_FLAG0_L2_EFFECT).toByte()
        val n = effect.size.coerceAtMost(TRIGGER_EFFECT_LEN)
        System.arraycopy(effect, 0, it, at, n)
    }

    /** DS5/Edge lightbar RGB. */
    fun ds5LightbarReport(model: Model, r: Int, g: Int, b: Int): ByteArray = newDs5(model).also {
        it[2] = DS5_FLAG1_LIGHTBAR.toByte()
        it[45] = r.toByte()
        it[46] = g.toByte()
        it[47] = b.toByte()
    }

    /** DS5/Edge player-indicator LEDs (low 5 bits, hid-playstation pattern). */
    fun ds5PlayerLedsReport(model: Model, bits: Int): ByteArray = newDs5(model).also {
        it[2] = DS5_FLAG1_PLAYER_LEDS.toByte()
        it[44] = (bits and 0x1F).toByte()
    }

    // DS4 output report 0x05 (32 B), report-relative (`dualshock4_proto::parse_ds4_output`):
    // [1] valid_flag0 (bit0 motors, bit1 LED, bit2 blink), [4] weak/right motor, [5] strong/left,
    // [6..9) RGB, [9]/[10] blink on/off.
    private const val DS4_FLAG0_MOTORS = 0x01
    private const val DS4_FLAG0_LED = 0x02

    /**
     * One full-state DS4 write: motors + lightbar together, both flags set — the composed-state
     * shape SDL uses against real hardware (per-channel selective writes are unproven on DS4
     * firmware, unlike the DS5's). [DsCapture] holds the composition. Blink stays untouched.
     */
    fun ds4Report(low: Int, high: Int, r: Int, g: Int, b: Int): ByteArray =
        ByteArray(Model.DUALSHOCK4.outputSize).also {
            it[0] = 0x05
            it[1] = (DS4_FLAG0_MOTORS or DS4_FLAG0_LED).toByte()
            it[4] = amp8(high).toByte()
            it[5] = amp8(low).toByte()
            it[6] = r.toByte()
            it[7] = g.toByte()
            it[8] = b.toByte()
        }

    // Wire u16 amplitude → motor byte; a nonzero command never collapses to 0 (parity with the
    // vibrator path's toAmplitude).
    private fun amp8(v16: Int): Int {
        val a = (v16 ushr 8) and 0xFF
        return if (v16 != 0 && a == 0) 1 else a
    }
}
