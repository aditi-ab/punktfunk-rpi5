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
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Generic USB transport for a client-captured HID controller — the device-agnostic half of what
 * [Sc2UsbLink] pioneered, now shared with the Sony capture ([DsCapture]). Claims the controller
 * interface(s) — `force = true` detaches the kernel/OS driver, so a captured pad can't
 * double-drive the ordinary InputDevice path — runs a multiplexed [UsbRequest] read loop, and
 * writes the host/capture's reports back to the device (interrupt-OUT when the interface has one,
 * else EP0 `SET_REPORT`).
 *
 * Everything device-specific is [Config]: which attached device to pick, which of its interfaces
 * to claim, and an optional keep-alive (feature reports re-sent on a firmware-watchdog cadence —
 * the SC2's lizard-mode refresh; a DualSense needs none).
 *
 * **Unplug is signalled, never inferred from silence:** a quiet controller is not a missing one
 * (an SC2 on-glass round tripped exactly this — a 5 s silence heuristic firing on an idle pad).
 * The real signals are [UsbManager.ACTION_USB_DEVICE_DETACHED] for this device, or `requestWait`
 * returning sustained hard errors (every transfer fails instantly once the fd is dead).
 */
class HidUsbLink(
    private val context: Context,
    private val config: Config,
    private val onReport: (report: ByteArray, len: Int) -> Unit,
    private val onClosed: () -> Unit,
) {
    /**
     * The per-device knowledge this transport is parameterized by. [ifaceFilter] narrows WHICH
     * HID/vendor-class interfaces get claimed (the class check itself is built in) — e.g. the SC2
     * Puck's controller slots, or the DualSense's single HID interface among its audio siblings.
     * [keepAliveFeatures] are full feature reports (id byte first) re-sent to the streaming
     * interface every [keepAliveMs] AND once at claim time; empty = no keep-alive.
     */
    class Config(
        val tag: String,
        val threadName: String,
        val deviceMatch: (UsbDevice) -> Boolean,
        val ifaceFilter: (UsbDevice, UsbInterface) -> Boolean = { _, _ -> true },
        val keepAliveFeatures: List<ByteArray> = emptyList(),
        val keepAliveMs: Long = 0,
    )

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

    /** The claim whose IN endpoint last produced data — where output/feature writes go.
     *  Written by the reader thread, read by the feedback thread (feature control transfers). */
    @Volatile private var activeClaim: Claim? = null

    /** Pending OUT reports, submitted by the reader thread — only one thread may drive a
     *  connection's [UsbRequest]s ([UsbDeviceConnection.requestWait] returns ANY completed
     *  request; a second waiter would steal the reader's completions). See [OutReportQueue] for
     *  what gets discarded when it fills, and why that is not simply "the oldest". */
    private val outQueue = OutReportQueue()

    private var reader: Thread? = null
    private var detachReceiver: BroadcastReceiver? = null

    @Volatile private var running = false

    /** Latches on the first "this link is down" signal so [onClosed] fires exactly once, however
     *  many of the racing detectors (detach broadcast, reader error streak, failed re-queue) see
     *  it. Reset by [start]. */
    private val down = AtomicBoolean(false)

    /** First attached matching device, or null. Does not need USB permission to enumerate. */
    fun findDevice(): UsbDevice? = usb.deviceList.values.firstOrNull(config.deviceMatch)

    /**
     * Open a SECOND connection to the same device, for a consumer that needs its own descriptor.
     *
     * **Not a convenience — a correctness requirement.** `UsbDeviceConnection.requestWait()`
     * returns *any* completed request on that connection, and the same is true of the usbfs reap
     * ioctl underneath it: two independent transfer engines sharing one descriptor steal each
     * other's completions. This link's reader owns its connection exclusively (see the note on
     * [outQueue]), so anything else driving transfers on this device — the isochronous audio
     * renderer — must open its own.
     *
     * usbfs allows the same device to be opened many times, and claims are per (descriptor,
     * interface), so a claim made on this connection does not conflict with one made on that.
     *
     * The caller owns the returned connection and must close it.
     */
    fun openAuxConnection(): UsbDeviceConnection? {
        val dev = device ?: return null
        return usb.openDevice(dev)
    }

    /**
     * The open connection's usbfs file descriptor, or -1 when the link is not running.
     *
     * Handed to native code that drives interfaces this link deliberately does NOT claim — the
     * pad's isochronous audio endpoint (see `pad_audio` on the native side), which Android's own
     * USB API cannot reach because `UsbRequest` rejects anything that is not bulk or interrupt.
     * usbfs claims are per interface, so a native claim of the audio interface leaves this link's
     * HID claim untouched.
     *
     * **The borrower must stop using it before [stop] runs**: closing the connection while a
     * transfer is in flight pulls the descriptor out from under the kernel.
     */
    val fileDescriptor: Int get() = connection?.fileDescriptor ?: -1

    /**
     * Claim [dev]'s controller interface(s) and start the read loop. The caller has already
     * obtained USB permission. Returns false when nothing could be claimed.
     */
    fun start(dev: UsbDevice): Boolean {
        if (!usb.hasPermission(dev)) {
            Log.e(config.tag, "no USB permission for ${dev.deviceName}")
            return false
        }
        val conn = usb.openDevice(dev) ?: run {
            Log.e(config.tag, "openDevice failed for ${dev.deviceName}")
            return false
        }
        val claimed = claimControllerInterfaces(dev, conn)
        if (claimed.isEmpty()) {
            Log.e(config.tag, "no claimable interface on ${dev.deviceName} (PID=0x%04x)".format(dev.productId))
            conn.close()
            return false
        }
        connection = conn
        device = dev
        claims = claimed
        down.set(false)
        running = true
        Log.i(
            config.tag,
            "USB link up: PID=0x%04x ifaces=%s".format(
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
                    Log.i(config.tag, "USB detached (${dev.deviceName})")
                    linkDown()
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
        if (config.keepAliveFeatures.isNotEmpty()) {
            claimed.forEach { sendKeepAlive(conn, it.iface.id) }
        }
        reader = Thread({ readLoop(conn, claimed) }, config.threadName).apply {
            isDaemon = true
            start()
        }
        return true
    }

    /**
     * Claim every candidate controller interface: HID (or vendor-class) interfaces that pass the
     * config's [Config.ifaceFilter], with an INT/BULK IN endpoint (OUT optional — the fallback is
     * EP0 `SET_REPORT`). `force = true` detaches the kernel/OS driver, so the pad also vanishes
     * from Android's own input stack while captured.
     */
    private fun claimControllerInterfaces(dev: UsbDevice, conn: UsbDeviceConnection): List<Claim> {
        val out = mutableListOf<Claim>()
        for (i in 0 until dev.interfaceCount) {
            val iface = dev.getInterface(i)
            if (!config.ifaceFilter(dev, iface)) continue
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
                Log.w(config.tag, "could not claim iface ${iface.id}")
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
                Log.w(config.tag, "UsbRequest.initialize(IN, iface ${c.iface.id}) failed")
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
                    Log.w(config.tag, "UsbRequest.initialize(OUT, iface ${c.iface.id}) failed — output reports via EP0")
                }
            }
            c.inBuf.clear()
            req.queue(c.inBuf)
        }
        if (live.isEmpty()) {
            Log.e(config.tag, "no IN request could be queued")
            finishReader(claims)
            // `start` already returned true, so without this the owner would sit waiting on a
            // capture that never streams and never reports itself dead.
            linkDown()
            return
        }
        val scratch = ByteArray(64)
        var lastKeepAlive = android.os.SystemClock.elapsedRealtime()
        var errorsSince = 0L // elapsedRealtime of the first hard error in the current streak
        try {
            while (running) {
                val now = android.os.SystemClock.elapsedRealtime()
                if (config.keepAliveFeatures.isNotEmpty() && config.keepAliveMs > 0 &&
                    now - lastKeepAlive >= config.keepAliveMs
                ) {
                    // Refresh the firmware settings on the streaming interface (else every live
                    // one, before a streaming interface is known) — replaying also repairs state
                    // some other consumer changed after capture started.
                    val target = activeClaim
                    if (target != null) sendKeepAlive(conn, target.iface.id)
                    else live.forEach { sendKeepAlive(conn, it.iface.id) }
                    lastKeepAlive = now
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
                        Log.i(config.tag, "USB request errors persisting ${now - errorsSince} ms — treating as unplug")
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
                                config.tag,
                                "first report on iface %d: id=0x%02x len=%d".format(
                                    claim.iface.id, scratch[0].toInt() and 0xFF, n,
                                ),
                            )
                        }
                        activeClaim = claim
                        onReport(scratch, n)
                    }
                    claim.inBuf.clear()
                    if (!claim.inReq!!.queue(claim.inBuf)) {
                        Log.i(config.tag, "re-queue(IN, iface ${claim.iface.id}) failed — treating as unplug")
                        break
                    }
                } else if (done === claim.outReq) {
                    claim.outBusy = false
                }
            }
        } finally {
            finishReader(claims)
        }
        linkDown()
    }

    /**
     * Report the link down, exactly once, from whichever detector noticed first — the detach
     * broadcast (main thread) or the reader thread on its way out.
     *
     * This only *signals*; releasing the connection and the interfaces stays the owner's job, via
     * the [stop] its `onClosed` handler calls. Previously nothing released them on this path: the
     * detach receiver flipped a flag and fired the callback, so an unplug left the connection open,
     * the interfaces claimed (the pad could not return to Android's own input stack) and the
     * receiver still registered — and a re-plug overwrote the field holding it, leaking a receiver
     * that stayed live for the process's lifetime.
     */
    private fun linkDown() {
        running = false
        if (down.compareAndSet(false, true)) onClosed()
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
     * Write one raw report to the device: kind 0 = output report (the active interface's
     * interrupt-OUT, else a `SET_REPORT(Output)` control transfer), kind 1 = feature report
     * (`SET_REPORT(Feature)`). [data] is the full report, id byte first, hidapi framing.
     *
     * [coalesce] tells the pending-OUT queue whether a newer report of the same kind may replace
     * this one — [OutReportQueue.KEY_RUMBLE] for motor levels, the default [OutReportQueue.NO_COALESCE]
     * for one-shots (lightbar, player LEDs, trigger effects) the sender will not repeat.
     *
     * Returns whether the report reached the device or is queued for it. A caller that is writing
     * a **stop** needs this: a discarded stop has nothing behind it, so it must not be mistaken
     * for one that landed.
     */
    fun writeRaw(kind: Int, data: ByteArray, coalesce: Int = OutReportQueue.NO_COALESCE): Boolean {
        if (data.isEmpty()) return false
        return when (kind) {
            0 -> {
                if ((activeClaim ?: claims.firstOrNull())?.outReq != null) {
                    // Interrupt-OUT rides UsbRequests submitted by the reader thread.
                    outQueue.offer(data, coalesce)
                } else {
                    setReport(REPORT_TYPE_OUTPUT, data)
                }
            }
            1 -> setReport(REPORT_TYPE_FEATURE, data)
            else -> false
        }
    }

    private fun setReport(type: Int, data: ByteArray): Boolean {
        val conn = connection ?: return false
        val ifId = (activeClaim ?: claims.firstOrNull())?.iface?.id ?: return false
        return sendReport(conn, ifId, type, data)
    }

    /**
     * Write one output report EP0-direct (`SET_REPORT(Output)`), bypassing the interrupt-OUT
     * queue — for a teardown write that must land while the reader thread is stopping and the
     * queue would never drain (e.g. a rumble stop before the interfaces release). Safe from any
     * thread: EP0 control transfers are independent of the reader's `requestWait`.
     */
    fun writeControl(data: ByteArray): Boolean =
        data.isNotEmpty() && setReport(REPORT_TYPE_OUTPUT, data)

    private fun sendKeepAlive(conn: UsbDeviceConnection, ifaceId: Int) {
        for (f in config.keepAliveFeatures) sendReport(conn, ifaceId, REPORT_TYPE_FEATURE, f)
    }

    /**
     * HID `SET_REPORT` control transfer with hidapi's report-id framing: a non-zero leading byte
     * is the report id (sent in wValue AND kept in the payload); a zero leading byte means
     * "unnumbered" (id 0 in wValue, id byte stripped from the payload). EP0 is independent of
     * the interrupt endpoints, so this is safe alongside the reader thread's requestWait.
     */
    private fun sendReport(
        conn: UsbDeviceConnection,
        ifaceId: Int,
        type: Int,
        data: ByteArray,
    ): Boolean {
        val id = data[0].toInt() and 0xFF
        val payload = if (id == 0) data.copyOfRange(1, data.size) else data
        // controlTransfer returns the byte count, or a negative value on failure — a failed write
        // must be reported as such, not swallowed (a dropped rumble stop has nothing behind it).
        val n = runCatching {
            conn.controlTransfer(
                0x21, // host→device, class, interface
                0x09, // SET_REPORT
                (type shl 8) or id,
                ifaceId,
                payload,
                payload.size,
                WRITE_TIMEOUT_MS,
            )
        }.getOrDefault(-1)
        return n >= 0
    }

    /**
     * Stop the read loop and release the interfaces. Idempotent; does not fire [onClosed].
     *
     * Safe to call from the `onClosed` handler itself — that is how an unplug now gets cleaned up,
     * and it arrives on the reader thread, which must not try to join itself.
     */
    fun stop() {
        running = false
        // Claim the down-latch so the reader's own exit does not report a close the owner asked for.
        down.set(true)
        detachReceiver?.let { runCatching { context.unregisterReceiver(it) } }
        detachReceiver = null
        if (reader !== Thread.currentThread()) {
            runCatching { reader?.join(1000) }
            // Only forget the thread once it is actually gone: clearing it while it still runs
            // would let a later stop() skip the join and free the connection under it.
            reader = null
        }
        outQueue.clear()
        activeClaim = null
        for (c in claims) runCatching { connection?.releaseInterface(c.iface) }
        claims = emptyList()
        runCatching { connection?.close() }
        connection = null
        device = null
    }

    private companion object {
        const val READ_TIMEOUT_MS = 100L
        const val WRITE_TIMEOUT_MS = 250
        /** Hard `requestWait` ERRORS (not timeouts) persisting this long = the fd is dead. */
        const val ERROR_UNPLUG_MS = 2000L
        const val REPORT_TYPE_OUTPUT = 0x02
        const val REPORT_TYPE_FEATURE = 0x03
    }
}
