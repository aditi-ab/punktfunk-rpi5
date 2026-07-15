package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.util.Log

/**
 * USB transport for a Steam Controller 2 — wired (`28DE:1302`) or through the wireless Puck
 * dongle (`1304`/`1305`, controller on interfaces 2..5). Claims the Valve vendor interface
 * (detaching any kernel/OS consumer), runs a read loop on its interrupt-IN endpoint, keeps
 * lizard mode off on the firmware watchdog cadence, and replays the host's raw writes (Steam's
 * rumble output reports / settings feature reports) back to the device.
 *
 * One controller per link in v1: on a dongle the first claimable controller interface wins
 * (multi-pad-per-Puck is a follow-up).
 */
class Sc2UsbLink(
    context: Context,
    private val onReport: (report: ByteArray, len: Int) -> Unit,
    private val onClosed: () -> Unit,
) {
    private val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager

    private var connection: UsbDeviceConnection? = null
    private var iface: UsbInterface? = null
    private var epIn: UsbEndpoint? = null
    private var epOut: UsbEndpoint? = null

    private var reader: Thread? = null

    @Volatile private var running = false

    /** First attached SC2 (wired or Puck), or null. Does not need USB permission to enumerate. */
    fun findDevice(): UsbDevice? = usb.deviceList.values.firstOrNull {
        it.vendorId == Sc2Device.VID_VALVE && it.productId in Sc2Device.USB_PIDS
    }

    /**
     * Claim [dev] and start the read + lizard-heartbeat loop. The caller has already obtained
     * USB permission ([UsbManager.hasPermission]). Returns false when no controller interface
     * could be claimed.
     */
    fun start(dev: UsbDevice): Boolean {
        if (!usb.hasPermission(dev)) {
            Log.e(TAG, "no USB permission for ${dev.deviceName}")
            return false
        }
        val conn = usb.openDevice(dev) ?: run {
            Log.e(TAG, "openDevice failed for ${dev.deviceName}")
            return false
        }
        val claimed = claimControllerInterface(dev, conn) ?: run {
            Log.e(TAG, "no claimable SC2 interface on ${dev.deviceName} (PID=0x%04x)".format(dev.productId))
            conn.close()
            return false
        }
        connection = conn
        iface = claimed.first
        epIn = claimed.second
        epOut = claimed.third
        running = true
        Log.i(
            TAG,
            "SC2 USB link up: PID=0x%04x iface=%d in=0x%02x out=%s".format(
                dev.productId, claimed.first.id, claimed.second.address,
                claimed.third?.let { "0x%02x".format(it.address) } ?: "control",
            ),
        )
        writeFeature(Sc2Device.DISABLE_LIZARD)
        reader = Thread({ readLoop(conn, claimed.second) }, "pf-sc2-usb").apply {
            isDaemon = true
            start()
        }
        return true
    }

    /**
     * Pick the controller interface: vendor-defined (0xFF) class with an interrupt/bulk IN
     * endpoint, restricted to interfaces 2..5 on a Puck dongle (the SDL-documented controller
     * range — the other interfaces are the dongle's own control/lizard endpoints).
     */
    private fun claimControllerInterface(
        dev: UsbDevice,
        conn: UsbDeviceConnection,
    ): Triple<UsbInterface, UsbEndpoint, UsbEndpoint?>? {
        val dongle = dev.productId != Sc2Device.PID_WIRED
        val candidates = (0 until dev.interfaceCount)
            .map { dev.getInterface(it) }
            .filter { !dongle || it.id in Sc2Device.DONGLE_IFACES }
            .sortedByDescending {
                when (it.interfaceClass) {
                    0xFF -> 2 // vendor-defined first — the Valve gamepad interface
                    UsbConstants.USB_CLASS_HID -> 1
                    else -> 0
                }
            }
        for (candidate in candidates) {
            var inEp: UsbEndpoint? = null
            var outEp: UsbEndpoint? = null
            for (i in 0 until candidate.endpointCount) {
                val ep = candidate.getEndpoint(i)
                val usable = ep.type == UsbConstants.USB_ENDPOINT_XFER_INT ||
                    ep.type == UsbConstants.USB_ENDPOINT_XFER_BULK
                if (!usable) continue
                if (ep.direction == UsbConstants.USB_DIR_IN && inEp == null) inEp = ep
                if (ep.direction == UsbConstants.USB_DIR_OUT && outEp == null) outEp = ep
            }
            if (inEp == null) continue
            // force=true detaches the kernel/OS driver — while claimed, the controller vanishes
            // from Android's own input stack (no double input alongside our capture).
            if (conn.claimInterface(candidate, true)) return Triple(candidate, inEp, outEp)
            Log.w(TAG, "could not claim iface ${candidate.id}, trying next")
        }
        return null
    }

    private fun readLoop(conn: UsbDeviceConnection, ep: UsbEndpoint) {
        val buf = ByteArray(64)
        var lastLizard = 0L
        var failures = 0
        while (running) {
            val now = android.os.SystemClock.elapsedRealtime()
            if (now - lastLizard >= Sc2Device.LIZARD_REFRESH_MS) {
                writeFeature(Sc2Device.DISABLE_LIZARD)
                lastLizard = now
            }
            val n = conn.bulkTransfer(ep, buf, buf.size, READ_TIMEOUT_MS)
            when {
                n > 0 -> {
                    failures = 0
                    onReport(buf, n)
                }
                n == 0 -> {} // empty read — keep going
                else -> {
                    // -1 covers both timeout (normal, idle controller) and unplug. A real unplug
                    // makes every subsequent transfer fail instantly, so many consecutive fast
                    // failures = the device is gone.
                    if (++failures >= 64) {
                        Log.i(TAG, "SC2 USB read failing persistently — treating as unplug")
                        break
                    }
                }
            }
        }
        if (running) {
            running = false
            onClosed()
        }
    }

    /**
     * Replay one raw report from the host on the device: kind 0 = output report (Steam's `0x80`
     * rumble & friends — interrupt-OUT when the interface has one, else a `SET_REPORT(Output)`
     * control transfer), kind 1 = feature report (`SET_REPORT(Feature)`). [data] is the full
     * report, id byte first, exactly as hidapi framed it host-side.
     */
    fun writeRaw(kind: Int, data: ByteArray) {
        if (data.isEmpty()) return
        when (kind) {
            0 -> {
                val out = epOut
                val conn = connection ?: return
                if (out != null) {
                    conn.bulkTransfer(out, data, data.size, WRITE_TIMEOUT_MS)
                } else {
                    setReport(REPORT_TYPE_OUTPUT, data)
                }
            }
            1 -> writeFeature(data)
        }
    }

    private fun writeFeature(data: ByteArray) {
        setReport(REPORT_TYPE_FEATURE, data)
    }

    /**
     * HID `SET_REPORT` control transfer with hidapi's report-id framing: a non-zero leading byte
     * is the report id (sent in wValue AND kept in the payload); a zero leading byte means
     * "unnumbered" (id 0 in wValue, id byte stripped from the payload).
     */
    private fun setReport(type: Int, data: ByteArray) {
        val conn = connection ?: return
        val ifId = iface?.id ?: return
        val id = data[0].toInt() and 0xFF
        val payload = if (id == 0) data.copyOfRange(1, data.size) else data
        conn.controlTransfer(
            0x21, // host→device, class, interface
            0x09, // SET_REPORT
            (type shl 8) or id,
            ifId,
            payload,
            payload.size,
            WRITE_TIMEOUT_MS,
        )
    }

    /** Stop the read loop and release the interface. Idempotent; does not fire [onClosed]. */
    fun stop() {
        running = false
        runCatching { reader?.join(1000) }
        reader = null
        runCatching { iface?.let { connection?.releaseInterface(it) } }
        runCatching { connection?.close() }
        connection = null
        iface = null
        epIn = null
        epOut = null
    }

    private companion object {
        const val TAG = "Sc2UsbLink"
        const val READ_TIMEOUT_MS = 100
        const val WRITE_TIMEOUT_MS = 250
        const val REPORT_TYPE_OUTPUT = 0x02
        const val REPORT_TYPE_FEATURE = 0x03
    }
}
