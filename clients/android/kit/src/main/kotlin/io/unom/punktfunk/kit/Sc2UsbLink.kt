package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbDevice
import java.util.concurrent.ConcurrentHashMap

/**
 * USB transport for a Steam Controller: the Steam Controller 2 wired (`28DE:1302`) or through
 * its Puck (`1304`/`1305`), or the 2015 pad wired (`1102`) or through its dongle (`1142`). The
 * Steam Controller specialization of the shared [HidUsbLink] transport (which owns the claim,
 * read loop, write queue, and unplug handling); this class contributes only what is specific:
 *
 * **A dongle claims ALL controller interfaces:** it hosts up to four pads, one HID interface
 * each, and there is no way to know which slot a controller bonded to — claiming only one read
 * silence while Android's input stack kept the others. Each report names its interface, and each
 * pad's rumble and settings go back to its own.
 *
 * **Lizard mode:** the SC2's firmware watchdog re-enables its kb/mouse emulation after a few
 * seconds of silence, so [Sc2Device.DISABLE_LIZARD] + [Sc2Device.NORMALIZE_JOYSTICKS] are re-sent
 * on SDL's cadence. The 2015 pad has no watchdog: [Sc1Device.LIZARD_OFF] goes once to each pad as
 * its first report arrives, since a dongle pad can connect long after the claim. Both get lizard
 * mode back on release, so the pad drives the OS again at once.
 */
class Sc2UsbLink(
    context: Context,
    onReport: (report: ByteArray, len: Int, iface: Int) -> Unit,
    onClosed: () -> Unit,
) {
    private val triton = HidUsbLink(
        context,
        HidUsbLink.Config(
            tag = "Sc2UsbLink",
            threadName = "pf-sc2-usb",
            deviceMatch = {
                it.vendorId == Sc2Device.VID_VALVE && it.productId in Sc2Device.USB_PIDS
            },
            // Wired: every HID/vendor interface; dongle: only the controller slots 2..5.
            ifaceFilter = { dev, iface ->
                dev.productId == Sc2Device.PID_WIRED || iface.id in Sc2Device.DONGLE_IFACES
            },
            keepAliveFeatures = listOf(Sc2Device.DISABLE_LIZARD, Sc2Device.NORMALIZE_JOYSTICKS),
            keepAliveMs = Sc2Device.LIZARD_REFRESH_MS,
            releaseFeatures = listOf(Sc2Device.ENABLE_LIZARD),
        ),
        onReport,
        onClosed,
    )

    /** 2015 pads whose lizard mode is off, by interface. A dongle pad that powers off leaves. */
    private val settled = ConcurrentHashMap.newKeySet<Int>()

    private val classic: HidUsbLink = HidUsbLink(
        context,
        HidUsbLink.Config(
            tag = "Sc1UsbLink",
            threadName = "pf-sc1-usb",
            deviceMatch = {
                it.vendorId == Sc2Device.VID_VALVE && it.productId in Sc1Device.USB_PIDS
            },
            // The wired pad's keyboard and mouse interfaces stay with Android, silent once
            // lizard mode is off.
            ifaceFilter = { dev, iface ->
                if (dev.productId == Sc1Device.PID_WIRED) {
                    iface.id == Sc1Device.WIRED_IFACE
                } else {
                    iface.id in Sc1Device.DONGLE_IFACES
                }
            },
            releaseFeatures = Sc1Device.LIZARD_ON,
        ),
        { report, len, iface ->
            if (Sc1Device.isWirelessDisconnect(report, len)) {
                settled.remove(iface)
            } else if (settled.add(iface)) {
                // EP0 blocks; the reader must not.
                Thread({ Sc1Device.LIZARD_OFF.forEach { classicFeature(it, iface) } }, "pf-sc1-settle").start()
            }
            onReport(report, len, iface)
        },
        onClosed,
    )

    @Volatile private var link = triton

    /** Whether the claimed device is a 2015 pad; meaningful once [start] returned true. */
    val isClassic: Boolean get() = link === classic

    /** First attached Steam Controller or dongle, or null. Does not need USB permission. */
    fun findDevice(): UsbDevice? = triton.findDevice() ?: classic.findDevice()

    /**
     * Claim [dev]'s controller interface(s) and start the read loop. The caller has already
     * obtained USB permission. Returns false when nothing could be claimed.
     */
    fun start(dev: UsbDevice): Boolean {
        link = if (dev.productId in Sc1Device.USB_PIDS) classic else triton
        settled.clear()
        return link.start(dev)
    }

    /**
     * Replay one raw report from the host on the device: kind 0 = output report (Steam's `0x80`
     * rumble & friends), kind 1 = feature report. [data] is the full report, id byte first,
     * exactly as hidapi framed it host-side, for the pad on [iface]. Rumble coalesces per
     * [Sc2Device.outputCoalesceKey].
     */
    fun writeRaw(kind: Int, data: ByteArray, iface: Int): Boolean = when {
        link !== classic -> triton.writeRaw(kind, data, Sc2Device.outputCoalesceKey(data), iface)
        kind == 1 -> classicFeature(data, iface)
        else -> classic.writeRaw(kind, data, iface = iface)
    }

    /**
     * One feature query and its reply on [iface], id first, blocking up to a second. A 2015 pad
     * answers unnumbered, so its reply gets the id 0 the host expects in front.
     */
    fun exchange(request: ByteArray, iface: Int): ByteArray? {
        if (link !== classic) return triton.exchange(request, iface = iface)
        if (request.size < 2 || !classicFeature(request, iface)) return null
        repeat(10) {
            Thread.sleep(20)
            val reply = classic.getReport(HidUsbLink.REPORT_TYPE_FEATURE, 0, 64, iface)
            if (reply != null && reply.size > 1 && reply[0] == request[1]) return byteArrayOf(0) + reply
        }
        return null
    }

    /**
     * A 2015 pad takes a feature report only as 64 bytes after id 0, and its dongle refuses one
     * while the radio is busy: retry for 25 ms, as SDL does.
     */
    private fun classicFeature(data: ByteArray, iface: Int): Boolean {
        val frame = data.copyOf(65)
        repeat(50) {
            if (classic.writeRaw(1, frame, iface = iface)) return true
            Thread.sleep(0, 500_000)
        }
        return false
    }

    fun serialNumber(): String? = link.serialNumber()

    /** One output report on EP0 to [iface], for a write that must land while the link stops. */
    fun writeControl(frame: ByteArray, iface: Int): Boolean = link.writeControl(frame, iface)

    /** Restore lizard mode, stop the read loop, release the interfaces. Idempotent; fires no callback. */
    fun stop() = link.stop()
}
