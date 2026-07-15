package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.hardware.usb.UsbRequest
import android.util.Log
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.TimeoutException

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

    /** Pending OUT reports (Steam's forwarded haptics), submitted by the reader thread — see
     *  [readLoop] for why only one thread may drive this connection's [UsbRequest]s. */
    private val outQueue = ConcurrentLinkedQueue<ByteArray>()

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
        reader = Thread({ readLoop(conn, claimed.second, claimed.third) }, "pf-sc2-usb").apply {
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

    /**
     * The read loop, built on [UsbRequest] — NOT `bulkTransfer()`: Android only supports bulk
     * transactions on bulk endpoints, and the SC2's endpoints are INTERRUPT. `bulkTransfer()`
     * returned the first (already-buffered) report and then `-1` forever, which the first
     * on-glass run surfaced as a 250 ms create→unplug flap. One IN request stays queued at all
     * times; OUT writes (Steam's forwarded rumble) are queued from [writeRaw]'s thread onto
     * [outQueue] and submitted HERE, because `requestWait` returns ANY completed request on the
     * connection — a second thread waiting would steal the reader's completions.
     */
    private fun readLoop(conn: UsbDeviceConnection, epIn: UsbEndpoint, epOut: UsbEndpoint?) {
        val inReq = UsbRequest()
        if (!inReq.initialize(conn, epIn)) {
            Log.e(TAG, "UsbRequest.initialize(IN) failed")
            if (running) {
                running = false
                onClosed()
            }
            return
        }
        val outReq = epOut?.let { ep ->
            UsbRequest().takeIf { it.initialize(conn, ep) }
                ?: run { Log.w(TAG, "UsbRequest.initialize(OUT) failed — output reports dropped"); null }
        }
        val inBuf = ByteBuffer.allocate(64)
        val scratch = ByteArray(64)
        var outBusy = false
        var lastLizard = 0L
        var quietSince = 0L // elapsedRealtime of the first silent/failed wait in the streak; 0 = healthy
        var reports = 0L
        try {
            inBuf.clear()
            if (!inReq.queue(inBuf)) {
                Log.e(TAG, "queue(IN) failed")
                return
            }
            while (running) {
                val now = android.os.SystemClock.elapsedRealtime()
                if (now - lastLizard >= Sc2Device.LIZARD_REFRESH_MS) {
                    writeFeature(Sc2Device.DISABLE_LIZARD)
                    lastLizard = now
                }
                // Submit the next pending OUT report while the OUT slot is idle.
                if (!outBusy && outReq != null) {
                    outQueue.poll()?.let { data ->
                        if (outReq.queue(ByteBuffer.wrap(data))) outBusy = true
                    }
                }
                val done = try {
                    conn.requestWait(READ_TIMEOUT_MS.toLong())
                } catch (_: TimeoutException) {
                    // Normal while the pad is quiet; a SUSTAINED silence is the unplug signal
                    // (a healthy SC2 streams state continuously at its 1 kHz interval).
                    if (quietSince == 0L) quietSince = now
                    if (now - quietSince >= UNPLUG_AFTER_MS) {
                        Log.i(TAG, "SC2 USB silent for ${now - quietSince} ms (after $reports reports) — treating as unplug")
                        break
                    }
                    continue
                }
                when {
                    done === inReq -> {
                        if (quietSince != 0L) {
                            Log.i(TAG, "SC2 USB reads recovered after ${now - quietSince} ms")
                            quietSince = 0L
                        }
                        val n = inBuf.position()
                        if (n > 0) {
                            inBuf.flip()
                            inBuf.get(scratch, 0, n)
                            if (reports++ == 0L) {
                                Log.i(TAG, "SC2 USB first report: id=0x%02x len=%d".format(scratch[0].toInt() and 0xFF, n))
                            }
                            onReport(scratch, n)
                        }
                        inBuf.clear()
                        if (!inReq.queue(inBuf)) {
                            Log.i(TAG, "re-queue(IN) failed — treating as unplug")
                            break
                        }
                    }
                    done === outReq -> outBusy = false
                    done == null -> {
                        // requestWait error — the connection is gone (unplug / claim revoked).
                        Log.i(TAG, "SC2 USB requestWait error (after $reports reports) — treating as unplug")
                        break
                    }
                }
            }
        } finally {
            runCatching { inReq.cancel(); inReq.close() }
            runCatching { outReq?.cancel(); outReq?.close() }
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
                if (epOut != null) {
                    // Interrupt-OUT rides UsbRequests submitted by the reader thread. Bounded,
                    // newest-wins: these are level-styled commands the host re-sends anyway.
                    while (outQueue.size >= 32) outQueue.poll()
                    outQueue.offer(data)
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
        outQueue.clear()
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
        /** Sustained read-failure window treated as an unplug (a streaming pad reports every
         *  few ms; even an idle one shouldn't go silent for this long). */
        const val UNPLUG_AFTER_MS = 5000L
        const val REPORT_TYPE_OUTPUT = 0x02
        const val REPORT_TYPE_FEATURE = 0x03
    }
}
