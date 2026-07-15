package io.unom.punktfunk.kit

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.hardware.usb.UsbRequest
import android.os.Build
import android.util.Log
import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.TimeoutException

/**
 * USB transport for a Steam Controller 2 — wired (`28DE:1302`) or through the wireless Puck
 * dongle (`1304`/`1305`). Claims the controller interface(s) — detaching the OS input stack, so
 * the pad can't double-drive the ordinary InputDevice path — runs a multiplexed [UsbRequest]
 * read loop, keeps lizard mode off on the firmware watchdog cadence, and replays the host's raw
 * writes (Steam's rumble output reports / settings feature reports) back to the device.
 *
 * **The Puck claims ALL controller interfaces (2..5):** the dongle hosts up to four pads, one
 * HID interface each, and there is no way to know which slot a controller bonded to — claiming
 * only interface 2 read silence while Android's input stack kept the others (the round-2
 * on-glass symptom: the pad surfaced as a generic InputDevice → Xbox360). Whichever interface
 * streams state becomes the write target for rumble/settings.
 *
 * **Unplug is signalled, never inferred from silence:** a quiet controller is not a missing one
 * (round 2's wired disconnect was the 5 s silence heuristic firing on an idle pad). The real
 * signals are [UsbManager.ACTION_USB_DEVICE_DETACHED] for this device, or `requestWait`
 * returning sustained hard errors (every transfer fails instantly once the fd is dead).
 */
class Sc2UsbLink(
    private val context: Context,
    private val onReport: (report: ByteArray, len: Int) -> Unit,
    private val onClosed: () -> Unit,
) {
    private val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager

    /** One claimed interface: its endpoints + the read state the reader thread owns. */
    private class Claim(
        val iface: UsbInterface,
        val epIn: UsbEndpoint,
        val epOut: UsbEndpoint?,
    ) {
        val inBuf: ByteBuffer = ByteBuffer.allocate(64)
        var inReq: UsbRequest? = null
        var outReq: UsbRequest? = null
        var outBusy = false
        var reports = 0L
    }

    private var connection: UsbDeviceConnection? = null
    private var device: UsbDevice? = null
    private var claims: List<Claim> = emptyList()

    /** The claim whose IN endpoint last produced data — where rumble/settings writes go.
     *  Written by the reader thread, read by the feedback thread (feature control transfers). */
    @Volatile private var activeClaim: Claim? = null

    /** Pending OUT reports (Steam's forwarded haptics), submitted by the reader thread — only
     *  one thread may drive a connection's [UsbRequest]s ([UsbDeviceConnection.requestWait]
     *  returns ANY completed request; a second waiter would steal the reader's completions). */
    private val outQueue = ConcurrentLinkedQueue<ByteArray>()

    private var reader: Thread? = null
    private var detachReceiver: BroadcastReceiver? = null

    @Volatile private var running = false

    /** First attached SC2 (wired or Puck), or null. Does not need USB permission to enumerate. */
    fun findDevice(): UsbDevice? = usb.deviceList.values.firstOrNull {
        it.vendorId == Sc2Device.VID_VALVE && it.productId in Sc2Device.USB_PIDS
    }

    /**
     * Claim [dev]'s controller interface(s) and start the read loop. The caller has already
     * obtained USB permission. Returns false when nothing could be claimed.
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
        val claimed = claimControllerInterfaces(dev, conn)
        if (claimed.isEmpty()) {
            Log.e(TAG, "no claimable SC2 interface on ${dev.deviceName} (PID=0x%04x)".format(dev.productId))
            conn.close()
            return false
        }
        connection = conn
        device = dev
        claims = claimed
        running = true
        Log.i(
            TAG,
            "SC2 USB link up: PID=0x%04x ifaces=%s".format(
                dev.productId,
                claimed.joinToString {
                    "%d(in=0x%02x out=%s)".format(
                        it.iface.id, it.epIn.address,
                        it.epOut?.let { e -> "0x%02x".format(e.address) } ?: "-",
                    )
                },
            ),
        )
        // The REAL unplug signal — silence never is (an idle pad may simply stop streaming).
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context?, intent: Intent?) {
                if (intent?.action != UsbManager.ACTION_USB_DEVICE_DETACHED) return
                val gone: UsbDevice? = intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
                if (gone?.deviceName == dev.deviceName) {
                    Log.i(TAG, "SC2 USB detached (${dev.deviceName})")
                    if (running) {
                        running = false
                        onClosed()
                    }
                }
            }
        }
        detachReceiver = receiver
        val filter = IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED)
        if (Build.VERSION.SDK_INT >= 33) {
            context.registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            context.registerReceiver(receiver, filter)
        }
        claimed.forEach { sendFeature(conn, it.iface.id, Sc2Device.DISABLE_LIZARD) }
        reader = Thread({ readLoop(conn, claimed) }, "pf-sc2-usb").apply {
            isDaemon = true
            start()
        }
        return true
    }

    /**
     * Claim every candidate controller interface: the wired pad's single HID interface, or ALL
     * of a Puck's controller slots (interfaces 2..5 — the controller may be bonded to any of
     * them). `force = true` detaches the kernel/OS driver, so the pad also vanishes from
     * Android's own input stack while captured.
     */
    private fun claimControllerInterfaces(dev: UsbDevice, conn: UsbDeviceConnection): List<Claim> {
        val dongle = dev.productId != Sc2Device.PID_WIRED
        val out = mutableListOf<Claim>()
        for (i in 0 until dev.interfaceCount) {
            val iface = dev.getInterface(i)
            if (dongle && iface.id !in Sc2Device.DONGLE_IFACES) continue
            val hidOrVendor = iface.interfaceClass == UsbConstants.USB_CLASS_HID ||
                iface.interfaceClass == 0xFF
            if (!hidOrVendor) continue
            var inEp: UsbEndpoint? = null
            var outEp: UsbEndpoint? = null
            for (e in 0 until iface.endpointCount) {
                val ep = iface.getEndpoint(e)
                val usable = ep.type == UsbConstants.USB_ENDPOINT_XFER_INT ||
                    ep.type == UsbConstants.USB_ENDPOINT_XFER_BULK
                if (!usable) continue
                if (ep.direction == UsbConstants.USB_DIR_IN && inEp == null) inEp = ep
                if (ep.direction == UsbConstants.USB_DIR_OUT && outEp == null) outEp = ep
            }
            if (inEp == null) continue
            if (conn.claimInterface(iface, true)) {
                out.add(Claim(iface, inEp, outEp))
            } else {
                Log.w(TAG, "could not claim iface ${iface.id}")
            }
        }
        return out
    }

    /**
     * The multiplexed read loop: one IN request queued per claimed interface at all times, OUT
     * writes submitted from [outQueue], completions routed via [UsbRequest.getClientData].
     */
    private fun readLoop(conn: UsbDeviceConnection, claims: List<Claim>) {
        val live = claims.filter { c ->
            val req = UsbRequest()
            if (!req.initialize(conn, c.epIn)) {
                Log.w(TAG, "UsbRequest.initialize(IN, iface ${c.iface.id}) failed")
                return@filter false
            }
            req.clientData = c
            c.inReq = req
            c.epOut?.let { ep ->
                val o = UsbRequest()
                if (o.initialize(conn, ep)) {
                    o.clientData = c
                    c.outReq = o
                } else {
                    Log.w(TAG, "UsbRequest.initialize(OUT, iface ${c.iface.id}) failed — output reports via EP0")
                }
            }
            c.inBuf.clear()
            req.queue(c.inBuf)
        }
        if (live.isEmpty()) {
            Log.e(TAG, "no IN request could be queued")
            finishReader(claims)
            return
        }
        val scratch = ByteArray(64)
        var lastLizard = android.os.SystemClock.elapsedRealtime()
        var errorsSince = 0L // elapsedRealtime of the first hard error in the current streak
        try {
            while (running) {
                val now = android.os.SystemClock.elapsedRealtime()
                if (now - lastLizard >= Sc2Device.LIZARD_REFRESH_MS) {
                    // Refresh on every claimed interface until one is known-active (the dongle
                    // relays it per-slot); afterwards the active one suffices.
                    val target = activeClaim
                    if (target != null) sendFeature(conn, target.iface.id, Sc2Device.DISABLE_LIZARD)
                    else live.forEach { sendFeature(conn, it.iface.id, Sc2Device.DISABLE_LIZARD) }
                    lastLizard = now
                }
                // Submit the next pending OUT report on the active (else first) interface.
                val outTarget = (activeClaim ?: live.first()).takeIf { it.outReq != null && !it.outBusy }
                if (outTarget != null) {
                    outQueue.poll()?.let { data ->
                        if (outTarget.outReq!!.queue(ByteBuffer.wrap(data))) outTarget.outBusy = true
                    }
                }
                val done = try {
                    conn.requestWait(READ_TIMEOUT_MS)
                } catch (_: TimeoutException) {
                    // A quiet controller is NOT an unplug — keep listening indefinitely; the
                    // detach broadcast is the real signal.
                    errorsSince = 0L
                    continue
                }
                if (done == null) {
                    // Hard error. On a real unplug these storm continuously (the detach
                    // broadcast usually beats us to it); tolerate transient ones.
                    if (errorsSince == 0L) errorsSince = now
                    if (now - errorsSince >= ERROR_UNPLUG_MS) {
                        Log.i(TAG, "SC2 USB request errors persisting ${now - errorsSince} ms — treating as unplug")
                        break
                    }
                    continue
                }
                errorsSince = 0L
                val claim = done.clientData as? Claim ?: continue
                if (done === claim.inReq) {
                    val n = claim.inBuf.position()
                    if (n > 0) {
                        claim.inBuf.flip()
                        claim.inBuf.get(scratch, 0, n)
                        if (claim.reports++ == 0L) {
                            Log.i(
                                TAG,
                                "SC2 first report on iface %d: id=0x%02x len=%d".format(
                                    claim.iface.id, scratch[0].toInt() and 0xFF, n,
                                ),
                            )
                        }
                        activeClaim = claim
                        onReport(scratch, n)
                    }
                    claim.inBuf.clear()
                    if (!claim.inReq!!.queue(claim.inBuf)) {
                        Log.i(TAG, "re-queue(IN, iface ${claim.iface.id}) failed — treating as unplug")
                        break
                    }
                } else if (done === claim.outReq) {
                    claim.outBusy = false
                }
            }
        } finally {
            finishReader(claims)
        }
        if (running) {
            running = false
            onClosed()
        }
    }

    private fun finishReader(claims: List<Claim>) {
        for (c in claims) {
            runCatching { c.inReq?.cancel(); c.inReq?.close() }
            runCatching { c.outReq?.cancel(); c.outReq?.close() }
            c.inReq = null
            c.outReq = null
        }
    }

    /**
     * Replay one raw report from the host on the device: kind 0 = output report (Steam's `0x80`
     * rumble & friends — the active interface's interrupt-OUT, else a `SET_REPORT(Output)`
     * control transfer), kind 1 = feature report (`SET_REPORT(Feature)`). [data] is the full
     * report, id byte first, exactly as hidapi framed it host-side.
     */
    fun writeRaw(kind: Int, data: ByteArray) {
        if (data.isEmpty()) return
        when (kind) {
            0 -> {
                if ((activeClaim ?: claims.firstOrNull())?.outReq != null) {
                    // Interrupt-OUT rides UsbRequests submitted by the reader thread. Bounded,
                    // newest-wins: these are level-styled commands the host re-sends anyway.
                    while (outQueue.size >= 32) outQueue.poll()
                    outQueue.offer(data)
                } else {
                    setReport(REPORT_TYPE_OUTPUT, data)
                }
            }
            1 -> setReport(REPORT_TYPE_FEATURE, data)
        }
    }

    private fun setReport(type: Int, data: ByteArray) {
        val conn = connection ?: return
        val ifId = (activeClaim ?: claims.firstOrNull())?.iface?.id ?: return
        sendReport(conn, ifId, type, data)
    }

    private fun sendFeature(conn: UsbDeviceConnection, ifaceId: Int, data: ByteArray) {
        sendReport(conn, ifaceId, REPORT_TYPE_FEATURE, data)
    }

    /**
     * HID `SET_REPORT` control transfer with hidapi's report-id framing: a non-zero leading byte
     * is the report id (sent in wValue AND kept in the payload); a zero leading byte means
     * "unnumbered" (id 0 in wValue, id byte stripped from the payload). EP0 is independent of
     * the interrupt endpoints, so this is safe alongside the reader thread's requestWait.
     */
    private fun sendReport(conn: UsbDeviceConnection, ifaceId: Int, type: Int, data: ByteArray) {
        val id = data[0].toInt() and 0xFF
        val payload = if (id == 0) data.copyOfRange(1, data.size) else data
        conn.controlTransfer(
            0x21, // host→device, class, interface
            0x09, // SET_REPORT
            (type shl 8) or id,
            ifaceId,
            payload,
            payload.size,
            WRITE_TIMEOUT_MS,
        )
    }

    /** Stop the read loop and release the interfaces. Idempotent; does not fire [onClosed]. */
    fun stop() {
        running = false
        detachReceiver?.let { runCatching { context.unregisterReceiver(it) } }
        detachReceiver = null
        runCatching { reader?.join(1000) }
        reader = null
        outQueue.clear()
        activeClaim = null
        for (c in claims) runCatching { connection?.releaseInterface(c.iface) }
        claims = emptyList()
        runCatching { connection?.close() }
        connection = null
        device = null
    }

    private companion object {
        const val TAG = "Sc2UsbLink"
        const val READ_TIMEOUT_MS = 100L
        const val WRITE_TIMEOUT_MS = 250
        /** Hard `requestWait` ERRORS (not timeouts) persisting this long = the fd is dead. */
        const val ERROR_UNPLUG_MS = 2000L
        const val REPORT_TYPE_OUTPUT = 0x02
        const val REPORT_TYPE_FEATURE = 0x03
    }
}
