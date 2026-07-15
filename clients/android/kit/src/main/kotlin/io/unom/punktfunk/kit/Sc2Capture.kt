package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbDevice
import android.util.Log
import java.nio.ByteBuffer

/**
 * One captured Steam Controller 2 for one stream session — the glue between a transport link
 * ([Sc2UsbLink] / [Sc2BleLink]) and the wire:
 *
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
 * The wire slot is claimed lazily on the FIRST state report — a Puck with no controller powered
 * on stays invisible to the host — and released (with a wireless-disconnect event or on [stop])
 * so pad indices never leak. Report callbacks arrive on the link's own thread; the router's slot
 * table and chord timer are thread-safe for this (same contract as the feedback poll threads).
 */
class Sc2Capture(
    context: Context,
    private val router: GamepadRouter,
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
        }
        return ok
    }

    /** Start capturing the bonded BLE controller at [address]. */
    fun startBle(address: String): Boolean {
        if (activeLink != LINK_NONE) return false
        val ok = ble.start(address)
        if (ok) activeLink = LINK_BLE
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
        when (activeLink) {
            LINK_USB -> usb.stop()
            LINK_BLE -> ble.stop()
        }
        activeLink = LINK_NONE
        dongleLink = false
        releaseSlot()
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
            }
            return
        }
        if (!Sc2Device.parseState(report, len, state)) {
            // Battery/status and future report types still belong to the as-is stream.
            forwardRaw(report, len)
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

    private fun onLinkClosed() {
        Log.i(TAG, "SC2 link closed (unplug / power-off)")
        activeLink = LINK_NONE
        releaseSlot()
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
    }
}
