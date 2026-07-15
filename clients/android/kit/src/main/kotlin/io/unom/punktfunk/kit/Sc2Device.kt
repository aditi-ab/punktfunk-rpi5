package io.unom.punktfunk.kit

/**
 * Steam Controller 2 (2026, Valve "Ibex" / SDL "Triton") protocol constants + the light state
 * parser the CLIENT needs. The full report rides the wire verbatim (`nativeSendPadHidReport` →
 * the host's as-is virtual pad); this parser only extracts what the client itself consumes: the
 * button word for the typed mirror + exit chord, and sticks/triggers for the degrade path.
 *
 * Protocol ground truth: SDL's `SDL_hidapi_steam_triton.c` + `steam/controller_structs.h`
 * (Valve-maintained), mirrored host-side in `punktfunk-host`'s `triton_proto.rs`.
 */
object Sc2Device {
    const val VID_VALVE = 0x28DE

    /** Wired controller. */
    const val PID_WIRED = 0x1302

    /** Direct BLE identity (transport handled by [Sc2BleLink], not USB). */
    const val PID_BLE = 0x1303

    /** The wireless Puck dongles (Proteus / Nereid) — controller on USB interfaces 2..5. */
    const val PID_DONGLE_PROTEUS = 0x1304
    const val PID_DONGLE_NEREID = 0x1305

    val USB_PIDS = setOf(PID_WIRED, PID_DONGLE_PROTEUS, PID_DONGLE_NEREID)

    /** Dongle interface range that carries controllers (SDL: "interfaces 2..5, currently"). */
    val DONGLE_IFACES = 2..5

    // Input report ids (`ETritonReportIDTypes`). State layouts share every offset the client
    // reads (seq/buttons/triggers/sticks); 0x47 only diverges from byte 18 (trackpad timestamp).
    const val ID_STATE = 0x42
    const val ID_BATTERY = 0x43
    const val ID_STATE_BLE = 0x45
    const val ID_WIRELESS_X = 0x46
    const val ID_STATE_TIMESTAMP = 0x47
    const val ID_WIRELESS = 0x79

    /** Wireless status payload byte: controller connected/disconnected through the Puck. */
    const val WIRELESS_DISCONNECT = 1
    const val WIRELESS_CONNECT = 2

    // Button bits in the state report's u32 (SDL `TritonButtons`).
    const val A = 0x00000001
    const val B = 0x00000002
    const val X = 0x00000004
    const val Y = 0x00000008
    const val QAM = 0x00000010
    const val R3 = 0x00000020
    const val VIEW = 0x00000040
    const val R4 = 0x00000080
    const val R5 = 0x00000100
    const val RB = 0x00000200
    const val DPAD_DOWN = 0x00000400
    const val DPAD_RIGHT = 0x00000800
    const val DPAD_LEFT = 0x00001000
    const val DPAD_UP = 0x00002000
    const val MENU = 0x00004000
    const val L3 = 0x00008000
    const val STEAM = 0x00010000
    const val L4 = 0x00020000
    const val L5 = 0x00040000
    const val LB = 0x00080000
    const val RPAD_CLICK = 0x00400000

    /**
     * The feature report that turns lizard mode (built-in keyboard/mouse emulation) off:
     * `[report id 1][ID_SET_SETTINGS_VALUES 0x87][length 3][SETTING_LIZARD_MODE 9]
     * [LIZARD_MODE_OFF u16]`, zero-padded to the 64-byte feature size. The firmware watchdog
     * re-enables lizard mode after a few seconds of silence, so this is re-sent every
     * [LIZARD_REFRESH_MS] (SDL's cadence) — and the host's Steam sends its own through the raw
     * plane once it grabs the virtual pad, which lands here too.
     */
    val DISABLE_LIZARD: ByteArray = ByteArray(64).also {
        it[0] = 0x01 // feature report id
        it[1] = 0x87.toByte() // ID_SET_SETTINGS_VALUES
        it[2] = 3 // one ControllerSetting {u8 num, u16 value}
        it[3] = 9 // SETTING_LIZARD_MODE
        // [4..6] = LIZARD_MODE_OFF (0) — already zero
    }

    /**
     * Force firmware-calibrated signed i16 stick coordinates. Steam sends this during physical
     * controller initialization (`SETTING_ENABLE_RAW_JOYSTICK` = 0x2e, value 0); without it a
     * controller previously opened in raw mode reports ADC coordinates around 0..3200, which a
     * Triton consumer interprets as only a few percent of full travel.
     */
    val NORMALIZE_JOYSTICKS: ByteArray = ByteArray(64).also {
        it[0] = 0x01 // feature report id
        it[1] = 0x87.toByte() // ID_SET_SETTINGS_VALUES
        it[2] = 3 // one ControllerSetting {u8 num, u16 value}
        it[3] = 0x2E // SETTING_ENABLE_RAW_JOYSTICK
        // [4..6] = disabled (0) — firmware emits calibrated signed i16 values
    }

    const val LIZARD_REFRESH_MS = 3000L

    /** Wire mapping: SC2 button bit → punktfunk `Gamepad.BTN_*`, the inverse of the host's
     *  typed-fallback mapping (`triton_proto::from_gamepad`): paddles R4/L4/R5/L5 =
     *  PADDLE1/2/3/4, QAM = MISC1, right-pad click = the touchpad wire bit. */
    private val WIRE_MAP = intArrayOf(
        A, Gamepad.BTN_A,
        B, Gamepad.BTN_B,
        X, Gamepad.BTN_X,
        Y, Gamepad.BTN_Y,
        LB, Gamepad.BTN_LB,
        RB, Gamepad.BTN_RB,
        VIEW, Gamepad.BTN_BACK,
        MENU, Gamepad.BTN_START,
        STEAM, Gamepad.BTN_GUIDE,
        L3, Gamepad.BTN_LS_CLICK,
        R3, Gamepad.BTN_RS_CLICK,
        DPAD_UP, Gamepad.BTN_DPAD_UP,
        DPAD_DOWN, Gamepad.BTN_DPAD_DOWN,
        DPAD_LEFT, Gamepad.BTN_DPAD_LEFT,
        DPAD_RIGHT, Gamepad.BTN_DPAD_RIGHT,
        QAM, Gamepad.BTN_MISC1,
        R4, Gamepad.BTN_PADDLE1,
        L4, Gamepad.BTN_PADDLE2,
        R5, Gamepad.BTN_PADDLE3,
        L5, Gamepad.BTN_PADDLE4,
        RPAD_CLICK, Gamepad.BTN_TOUCHPAD,
    )

    /** Translate an SC2 button word into the wire `Gamepad.BTN_*` bitmask. */
    fun wireButtons(sc2: Int): Int {
        var out = 0
        var i = 0
        while (i < WIRE_MAP.size) {
            if (sc2 and WIRE_MAP[i] != 0) out = out or WIRE_MAP[i + 1]
            i += 2
        }
        return out
    }

    /** The typed-mirror fields of one state report (buttons/sticks/triggers only). */
    class State {
        var buttons = 0 // SC2 bit layout
        var lsX = 0; var lsY = 0 // i16, +y = up (device convention = wire convention)
        var rsX = 0; var rsY = 0
        var lt = 0; var rt = 0 // 0..255 (device 0..32767 scaled down)
    }

    /**
     * Parse the client-consumed fields out of a state report (`0x42`/`0x45`/`0x47` — identical
     * offsets for everything read here) into [out]. Returns false for non-state / short reports.
     */
    fun parseState(report: ByteArray, len: Int, out: State): Boolean {
        if (len < 18) return false
        when (report[0].toInt() and 0xFF) {
            ID_STATE, ID_STATE_BLE, ID_STATE_TIMESTAMP -> {}
            else -> return false
        }
        fun i16(o: Int) = ((report[o + 1].toInt() shl 8) or (report[o].toInt() and 0xFF)).toShort().toInt()
        out.buttons = (report[2].toInt() and 0xFF) or
            ((report[3].toInt() and 0xFF) shl 8) or
            ((report[4].toInt() and 0xFF) shl 16) or
            ((report[5].toInt() and 0xFF) shl 24)
        out.lt = (i16(6).coerceIn(0, 32767)) shr 7
        out.rt = (i16(8).coerceIn(0, 32767)) shr 7
        out.lsX = i16(10); out.lsY = i16(12)
        out.rsX = i16(14); out.rsY = i16(16)
        return true
    }
}
