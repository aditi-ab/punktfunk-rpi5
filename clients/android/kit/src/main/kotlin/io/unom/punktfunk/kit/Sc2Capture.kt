package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbDevice
import android.util.Log
import java.nio.ByteBuffer

/**
 * One captured Steam Controller 2 — the glue between a transport link ([Sc2UsbLink] /
 * [Sc2BleLink]) and one of two consumers:
 *
 * **Stream mode** (`router != null`, owned by StreamScreen):
 * - **Raw plane (the point):** every input report is forwarded verbatim
 *   ([GamepadRouter.ExternalPad.hidReport]) for the host's as-is virtual `28DE:1302` pad, which
 *   Steam Input drives like the physical controller.
 * - **Typed mirror:** buttons/sticks/triggers are ALSO diffed onto the ordinary per-transition
 *   plane, so the emergency exit chord works, and a host that degraded the kind (no UHID → the
 *   Xbox 360 pad) still gets a playable controller.
 * - **Raw return:** the host's hidraw writes (Steam's `0x80` rumble output reports, lizard/IMU
 *   feature settings) arrive via [GamepadFeedback.onHidRaw] → [onHidRaw] → the link, landing on
 *   the real controller's motors/firmware.
 *
 * **UI mode** (`router == null`, owned by MainActivity while NOT streaming): the lizard-mode
 * kb/mouse never produces gamepad events, so an uncaptured SC2 can't drive the console UI at
 * all. Here the parsed state is edge-detected into [onUiKey] navigation transitions instead
 * (D-pad + face buttons + Start/Select; the left stick synthesizes one D-pad step per push,
 * mirroring MainActivity's stick-to-focus behavior for ordinary pads).
 *
 * The wire slot is claimed lazily on the FIRST state report — a Puck with no controller powered
 * on stays invisible to the host — and released (with a wireless-disconnect event or on [stop])
 * so pad indices never leak. Report callbacks arrive on the link's own thread; the router's slot
 * table and chord timer are thread-safe for this (same contract as the feedback poll threads),
 * and UI-mode consumers hop to the main thread themselves.
 */
class Sc2Capture(
    context: Context,
    private val router: GamepadRouter? = null,
) {
    private val usb = Sc2UsbLink(context, ::onReport, ::onLinkClosed)
    private val ble = Sc2BleLink(context, ::onReport, ::onLinkClosed)
    private var activeLink: Int = LINK_NONE

    /** True when the USB link is a Puck dongle — the only transport whose wireless-status
     *  reports are authoritative. A WIRED pad also emits them, truthfully reporting "no radio
     *  link" — acting on that tore the slot down 255 ms after creation (first on-glass run). */
    private var dongleLink = false

    private var pad: GamepadRouter.ExternalPad? = null
    private val rawBuf: ByteBuffer = ByteBuffer.allocateDirect(64)

    // Typed-mirror diff state (wire units).
    private val state = Sc2Device.State()
    private var wireButtons = 0
    private val lastAxis = IntArray(6) { Int.MIN_VALUE }

    /** Report ids seen so far — each logged once, for remote diagnosis of what the pad emits. */
    private val seenIds = HashSet<Int>()

    // UI-mode state (router == null): held navigation keys + the stick's current synth direction.
    private var uiHeld = HashSet<Int>()
    private var uiStickDir = 0

    /**
     * UI-mode sink: one navigation key transition (an Android `KeyEvent.KEYCODE_*`), invoked on
     * the LINK thread — the consumer hops to the main thread. Set before [startUsb]/[startBle].
     */
    @Volatile
    var onUiKey: ((keyCode: Int, down: Boolean) -> Unit)? = null

    /**
     * Fired (link thread) when the capture engages or drops — lets the app surface "SC2
     * connected" in the console-UI gate and the Controllers screen.
     */
    @Volatile
    var onActiveChanged: ((active: Boolean) -> Unit)? = null

    val isActive: Boolean get() = activeLink != LINK_NONE

    /** First attached SC2/Puck USB device, for the permission flow. */
    fun findUsbDevice(): UsbDevice? = usb.findDevice()

    /**
     * The first already-bonded BLE Steam Controller's address, or null. The caller checks
     * BLUETOOTH_CONNECT first (without it the bonded list reads as empty anyway).
     */
    fun pairedBleAddress(): String? = ble.pairedControllers().firstOrNull()?.address

    /** Start capturing [dev] over USB (permission already granted). */
    fun startUsb(dev: UsbDevice): Boolean {
        if (activeLink != LINK_NONE) return false
        val ok = usb.start(dev)
        if (ok) {
            activeLink = LINK_USB
            dongleLink = dev.productId != Sc2Device.PID_WIRED
            onActiveChanged?.invoke(true)
        }
        return ok
    }

    /** Start capturing the bonded BLE controller at [address]. */
    fun startBle(address: String): Boolean {
        if (activeLink != LINK_NONE) return false
        val ok = ble.start(address)
        if (ok) {
            activeLink = LINK_BLE
            onActiveChanged?.invoke(true)
        }
        return ok
    }

    /** Replay a host raw write on the physical pad — wire to [GamepadFeedback.onHidRaw]. */
    fun onHidRaw(padIndex: Int, kind: Int, data: ByteArray) {
        if (padIndex != pad?.index) return // addressed to some other controller
        when (activeLink) {
            LINK_USB -> usb.writeRaw(kind, data)
            LINK_BLE -> ble.writeRaw(kind, data)
        }
    }

    /** Stop the link and free the wire slot (host tears the virtual pad down). Idempotent. */
    fun stop() {
        val wasActive = activeLink != LINK_NONE
        when (activeLink) {
            LINK_USB -> usb.stop()
            LINK_BLE -> ble.stop()
        }
        activeLink = LINK_NONE
        dongleLink = false
        releaseSlot()
        releaseUiKeys()
        if (wasActive) onActiveChanged?.invoke(false)
    }

    // ---- link callbacks (link thread) ----

    private fun onReport(report: ByteArray, len: Int) {
        val id = report[0].toInt() and 0xFF
        if (seenIds.add(id)) Log.i(TAG, "SC2 report id=0x%02x seen (len=%d)".format(id, len))
        // Wireless status: authoritative ONLY through a Puck dongle (powering the pad off frees
        // its wire index + the host's virtual device). A wired/BLE pad emits it too — truthfully
        // saying "no radio link" — and must NOT tear the slot down (SDL's wired path likewise
        // marks the controller connected unconditionally and reconnects on any state report).
        if ((id == Sc2Device.ID_WIRELESS || id == Sc2Device.ID_WIRELESS_X) && len >= 2) {
            if (dongleLink && (report[1].toInt() and 0xFF) == Sc2Device.WIRELESS_DISCONNECT) {
                Log.i(TAG, "Puck reports controller powered off — releasing wire slot")
                releaseSlot()
                releaseUiKeys()
            }
            return
        }
        if (!Sc2Device.parseState(report, len, state)) {
            // Battery/status and future report types still belong to the as-is stream.
            forwardRaw(report, len)
            return
        }
        if (router == null) {
            mirrorUi()
            return
        }
        val p = pad ?: router.openExternal(Gamepad.PREF_STEAMCONTROLLER2)?.also {
            pad = it
            Log.i(TAG, "SC2 captured → wire pad ${it.index} (as-is passthrough)")
        } ?: return // all 16 wire indices taken — drop until one frees
        forwardRaw(report, len)
        mirrorTyped(p)
    }

    private fun forwardRaw(report: ByteArray, len: Int) {
        val p = pad ?: return
        val n = len.coerceAtMost(rawBuf.capacity())
        rawBuf.clear()
        rawBuf.put(report, 0, n)
        p.hidReport(rawBuf, n)
    }

    /** Diff the parsed state onto the per-transition plane (buttons + axes, on change only). */
    private fun mirrorTyped(p: GamepadRouter.ExternalPad) {
        val wired = Sc2Device.wireButtons(state.buttons)
        var changed = wired xor wireButtons
        while (changed != 0) {
            val bit = changed and -changed // lowest changed bit
            p.button(bit, wired and bit != 0)
            changed = changed and bit.inv()
        }
        wireButtons = wired
        axis(p, Gamepad.AXIS_LS_X, state.lsX)
        axis(p, Gamepad.AXIS_LS_Y, state.lsY)
        axis(p, Gamepad.AXIS_RS_X, state.rsX)
        axis(p, Gamepad.AXIS_RS_Y, state.rsY)
        axis(p, Gamepad.AXIS_LT, state.lt)
        axis(p, Gamepad.AXIS_RT, state.rt)
    }

    private fun axis(p: GamepadRouter.ExternalPad, id: Int, v: Int) {
        if (lastAxis[id] == v) return
        lastAxis[id] = v
        p.axis(id, v)
    }

    /**
     * UI mode: edge-detect the parsed state into navigation key transitions. Buttons map to
     * their Android keycodes (press AND release, so the focus system sees real holds); the left
     * stick synthesizes ONE D-pad step per push past half deflection — the same single-move
     * behavior MainActivity gives ordinary pads' sticks.
     */
    private fun mirrorUi() {
        val sink = onUiKey ?: return
        val held = HashSet<Int>(8)
        var i = 0
        while (i < UI_KEY_MAP.size) {
            if (state.buttons and UI_KEY_MAP[i] != 0) held.add(UI_KEY_MAP[i + 1])
            i += 2
        }
        for (key in held) if (key !in uiHeld) sink(key, true)
        for (key in uiHeld) if (key !in held) sink(key, false)
        uiHeld = held
        // Left stick → a HELD D-pad direction (device convention: +y = up): pressed while
        // deflected, released on centre/direction change. The console UI's probe machinery
        // turns a held direction into its own auto-repeat, exactly like a physical D-pad; the
        // focus-hook path moves once per press edge either way.
        val dir = when {
            state.lsX <= -STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_LEFT
            state.lsX >= STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_RIGHT
            state.lsY >= STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_UP
            state.lsY <= -STICK_NAV -> android.view.KeyEvent.KEYCODE_DPAD_DOWN
            else -> 0
        }
        if (dir != uiStickDir) {
            // The D-pad bits share these keycodes; don't release a direction the physical
            // D-pad itself still holds (uiHeld tracks the button-sourced state).
            if (uiStickDir != 0 && uiStickDir !in uiHeld) sink(uiStickDir, false)
            if (dir != 0 && dir !in uiHeld) sink(dir, true)
            uiStickDir = dir
        }
    }

    /** Release every held UI-mode key (link drop / stop) so nothing sticks in the focus system. */
    private fun releaseUiKeys() {
        val sink = onUiKey
        if (sink != null) {
            for (key in uiHeld) sink(key, false)
            if (uiStickDir != 0 && uiStickDir !in uiHeld) sink(uiStickDir, false)
        }
        uiHeld = HashSet()
        uiStickDir = 0
    }

    private fun onLinkClosed() {
        Log.i(TAG, "SC2 link closed (unplug / power-off)")
        activeLink = LINK_NONE
        dongleLink = false
        releaseSlot()
        releaseUiKeys()
        onActiveChanged?.invoke(false)
    }

    private fun releaseSlot() {
        pad?.close()
        pad = null
        wireButtons = 0
        lastAxis.fill(Int.MIN_VALUE)
    }

    private companion object {
        const val TAG = "Sc2Capture"
        const val LINK_NONE = 0
        const val LINK_USB = 1
        const val LINK_BLE = 2

        /** Half deflection (device i16 range) — the stick-to-focus threshold. */
        const val STICK_NAV = 16384

        /** UI-mode mapping: SC2 button bit → Android keycode, as (bit, key) pairs. */
        val UI_KEY_MAP = intArrayOf(
            Sc2Device.DPAD_UP, android.view.KeyEvent.KEYCODE_DPAD_UP,
            Sc2Device.DPAD_DOWN, android.view.KeyEvent.KEYCODE_DPAD_DOWN,
            Sc2Device.DPAD_LEFT, android.view.KeyEvent.KEYCODE_DPAD_LEFT,
            Sc2Device.DPAD_RIGHT, android.view.KeyEvent.KEYCODE_DPAD_RIGHT,
            Sc2Device.A, android.view.KeyEvent.KEYCODE_BUTTON_A,
            Sc2Device.B, android.view.KeyEvent.KEYCODE_BUTTON_B,
            Sc2Device.X, android.view.KeyEvent.KEYCODE_BUTTON_X,
            Sc2Device.Y, android.view.KeyEvent.KEYCODE_BUTTON_Y,
            Sc2Device.LB, android.view.KeyEvent.KEYCODE_BUTTON_L1,
            Sc2Device.RB, android.view.KeyEvent.KEYCODE_BUTTON_R1,
            Sc2Device.MENU, android.view.KeyEvent.KEYCODE_BUTTON_START,
            Sc2Device.VIEW, android.view.KeyEvent.KEYCODE_BUTTON_SELECT,
        )
    }
}
