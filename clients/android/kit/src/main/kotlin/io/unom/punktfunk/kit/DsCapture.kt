package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.usb.UsbDevice
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.view.InputDevice

/**
 * One captured Sony pad (DualSense / DualSense Edge / DualShock 4) over USB — stream mode only.
 * The capture exists to fix what the InputDevice path structurally can't: rumble depends on the
 * phone's kernel exposing force feedback (many don't), and adaptive triggers / lightbar / player
 * LEDs have NO platform API at all. Claiming the pad's HID interface makes all of it work on any
 * phone, plus gyro + touchpad the standard path never captured.
 *
 * Unlike [Sc2Capture] there is no raw passthrough — the host's DualSense/DS4 backends consume
 * only typed events — and no UI mode: an UNcaptured Sony pad is a perfectly good InputDevice, so
 * outside a stream the ordinary path drives the console UI and this class isn't constructed.
 * That also makes the InputDevice path the automatic fallback whenever the capture doesn't
 * engage (toggle off, permission denied, Bluetooth).
 *
 * Input: parse ([DsDevice.parseState]) → typed mirror on an [GamepadRouter.ExternalPad] (buttons
 * diffed, axes on-change — the exit chord participates like any pad) + the rich plane (touch
 * normalized to the wire's 0..65535 screen space on-change; motion forwarded per report in raw
 * device units, the wire's contract). The wire slot is claimed when the capture engages, with the
 * first parsed report as the fallback for a claim that found no free index, and freed on
 * unplug/[stop], so indices never leak.
 *
 * Feedback: implements [GamepadFeedback.PadFeedbackSink] — rumble / trigger / lightbar / player
 * LED events addressed to this pad's wire index become USB output reports on the physical pad
 * ([DsDevice] builders). Rendering runs on the feedback poll threads; [HidUsbLink.writeRaw] is
 * thread-safe (bounded newest-wins queue, submitted by the reader thread). A USB pad holds its
 * rumble level until written zero, so a backstop timer re-arms per command and writes the stop
 * itself if the poll thread stalls — the engine's explicit zeros remain the real stop mechanism.
 */
class DsCapture(
    context: Context,
    private val router: GamepadRouter,
) : GamepadFeedback.PadFeedbackSink {
    private val usb = HidUsbLink(
        context,
        HidUsbLink.Config(
            tag = TAG,
            threadName = "pf-ds-usb",
            deviceMatch = { it.vendorId == DsDevice.VID_SONY && it.productId in DsDevice.USB_PIDS },
            // No ifaceFilter: the pad's audio interfaces are not HID class, so the link's built-in
            // class check already leaves them (and the pad's headset routing) to Android; the
            // single HID interface is the only claim.
        ),
        ::onReport,
        ::onLinkClosed,
    )

    @Volatile private var model: DsDevice.Model? = null
    @Volatile private var pad: GamepadRouter.ExternalPad? = null

    // Typed-mirror diff state (wire units) + rich-plane on-change mirrors. Link thread only.
    private val state = DsDevice.State()
    private var wireButtons = 0
    private val lastAxis = IntArray(6) { Int.MIN_VALUE }
    private val lastTouchActive = BooleanArray(2)
    private val lastTouchX = IntArray(2) { -1 }
    private val lastTouchY = IntArray(2) { -1 }

    // DS4 composed feedback (its writes are full-state — see DsDevice.ds4Report). Feedback threads.
    // The lightbar starts at hid-sony's player-1 blue so the first composed write (usually a
    // rumble, before any host Led lands) doesn't black the bar out.
    @Volatile private var ds4Low = 0
    @Volatile private var ds4High = 0
    @Volatile private var ds4Rgb = 0x000040

    // Rumble backstop: a USB pad holds its level until told zero, so a stalled poll thread would
    // leave the motors running — re-armed per command, cancelled by an explicit (0,0).
    private val mainHandler = Handler(Looper.getMainLooper())
    @Volatile private var backstop: Runnable? = null

    /** Fired (link thread) when the capture engages or drops — the Controllers screen's status. */
    @Volatile
    var onActiveChanged: ((active: Boolean) -> Unit)? = null

    /**
     * Tier-A pad audio, bound by the app layer (which owns the session handle).
     *
     * [start] is called once the router has assigned this pad a wire index, which the host uses to
     * address the `0xD1` stream. [stop] is called **before** the USB link closes — on [stop] and on
     * unplug alike — and must not return until nothing is still writing to the descriptor.
     */
    interface PadAudioHook {
        fun start(pad: Int, fd: Int)
        fun stop(pad: Int)
    }

    @Volatile
    var padAudio: PadAudioHook? = null

    /** True once [PadAudioHook.start] has run for the current capture, so it fires exactly once. */
    @Volatile private var padAudioStarted = false

    /**
     * The renderer's OWN connection to the pad.
     *
     * It must not share [usb]'s descriptor: two transfer engines on one usbfs descriptor reap each
     * other's completions (see [HidUsbLink.openAuxConnection]), which strands both the HID reader
     * and the audio ring. Closed only after the hook's stop has returned.
     */
    @Volatile private var padAudioConn: android.hardware.usb.UsbDeviceConnection? = null

    val isActive: Boolean get() = model != null

    /** First attached Sony USB pad, for the permission flow. Needs no permission to enumerate. */
    fun findUsbDevice(): UsbDevice? = usb.findDevice()

    /**
     * Start capturing [dev] (permission already granted). Claims the HID interface — the kernel
     * driver detaches and the pad's InputDevice node vanishes; its router slot (if the router
     * already opened one from the pre-claim InputDevice) is released HERE, at claim time, rather
     * than waiting for the system's removal callback — so the freed wire index is deterministic
     * for this capture's ExternalPad instead of racing the first report against the callback. A
     * released sibling that still exists as an InputDevice (a same-model Bluetooth pad) lazily
     * reopens a slot on its next input event, so over-matching self-heals.
     */
    fun startUsb(dev: UsbDevice): Boolean {
        if (model != null) return false
        val m = DsDevice.modelFor(dev.productId) ?: return false
        if (!usb.start(dev)) return false
        model = m
        for (id in InputDevice.getDeviceIds()) {
            val d = InputDevice.getDevice(id) ?: continue
            if (d.vendorId == dev.vendorId && d.productId == dev.productId) router.releaseDevice(id)
        }
        // Release the firmware's lightbar animation once so host lightbar writes take effect
        // (the same init hid-playstation/SDL send on open).
        if (m != DsDevice.Model.DUALSHOCK4) usb.writeRaw(0, DsDevice.ds5InitReport(m))
        Log.i(TAG, "Sony pad captured over USB: PID=0x%04x model=%s".format(dev.productId, m))
        ensureSlot(m)
        onActiveChanged?.invoke(true)
        return true
    }

    /** Stop the link and free the wire slot (host tears the virtual pad down). Idempotent. */
    fun stop() {
        // Before anything touches the link: the pad-audio renderer borrows this connection's
        // descriptor, and `usb.stop()` closes it. The hook does not return until its thread is
        // joined, so ordering this first is what makes the borrow sound.
        stopPadAudio()
        val m = model
        if (m != null) {
            // The interfaces are about to release with the kernel driver still detached — a
            // mid-rumble teardown would leave the motors running with nobody to stop them.
            // EP0-direct (the reader thread is stopping; the queue would never drain).
            // Nothing can retry after this point, so a failure is worth saying out loud: it is
            // the difference between a quiet pad and one that buzzes until it is unplugged.
            if (!usb.writeControl(stopReport(m))) Log.w(TAG, "teardown rumble stop was not written")
            // Motors silenced above; this hands back the lightbar, player LEDs and adaptive
            // triggers the game was holding, which outlive the link just as stubbornly.
            resetRichFeedback(m)
        }
        disarmBackstop()
        usb.stop()
        val wasActive = model != null
        model = null
        releaseSlot()
        if (wasActive) onActiveChanged?.invoke(false)
    }

    // ---- link callbacks (link thread) ----

    private fun onReport(report: ByteArray, len: Int) {
        val m = model ?: return
        if (!DsDevice.parseState(m, report, len, state)) return
        // Normally claimed already, at capture time; this is the retry for a capture that engaged
        // while every wire index was taken.
        val p = pad ?: ensureSlot(m) ?: return // all 16 taken — drop until one frees
        mirrorTyped(p)
        mirrorRich(p, m)
    }

    /**
     * Claim this capture's wire slot and start pad audio on it. Idempotent; null when all 16
     * indices are taken.
     *
     * Claimed when the capture engages rather than on the first report, because a pad that reports
     * nothing is still a pad: with the lazy claim, a captured-but-silent pad left the host with no
     * arrival, hence no virtual pad, no pad-audio capability and so no `0xD1` — a renderer sitting
     * at zero frames, indistinguishable from a broken pipeline (it took a physical replug to
     * clear). Callable from the main thread (capture start) and the link thread (the fallback).
     */
    @Synchronized
    private fun ensureSlot(m: DsDevice.Model): GamepadRouter.ExternalPad? {
        pad?.let { return it }
        val p = router.openExternal(m.pref) ?: return null
        pad = p
        Log.i(TAG, "captured $m → wire pad ${p.index}")
        // The wire index exists from here on, and the host addresses pad audio by it.
        startPadAudio(p.index)
        return p
    }

    /** Hand the renderer its own descriptor. Caller holds the monitor; fires once per capture. */
    private fun startPadAudio(index: Int) {
        val hook = padAudio ?: return
        if (padAudioStarted) return
        // A dedicated connection, NOT usb.fileDescriptor — see padAudioConn.
        val conn = usb.openAuxConnection()
        val fd = conn?.fileDescriptor ?: -1
        if (fd < 0) {
            conn?.close()
            Log.w(TAG, "pad audio: could not open a second USB connection")
            return
        }
        padAudioConn = conn
        padAudioStarted = true
        // Real-world self test, opt-in: `adb shell setprop debug.punktfunk.pad_audio_selftest 3`
        // drives the voice coils for N seconds through the actual client path before the renderer
        // takes over — the one check that proves the descriptor, the interface claim and the write
        // path all work on THIS device, without needing a host to be streaming. Same convention as
        // debug.punktfunk.force_parts.
        val secs = runCatching {
            Class.forName("android.os.SystemProperties")
                .getMethod("get", String::class.java, String::class.java)
                .invoke(null, "debug.punktfunk.pad_audio_selftest", "0") as String
        }.getOrNull()?.toIntOrNull() ?: 0
        if (secs > 0) {
            // Diagnostic mode: the self test OWNS this descriptor for the capture, and the renderer
            // must not also drive it — two engines on one usbfs descriptor reap each other's
            // completions, which is precisely the fault this test exists to expose.
            Thread({
                val r = NativeBridge.nativePadAudioSelfTest(fd, secs, 60)
                Log.i(TAG, "pad audio self-test → ${if (r > 0) "PASS ($r frames)" else "FAIL ($r)"}")
            }, "pf-pad-selftest").start()
        } else {
            // B6: hand the coils back before the first haptics frame. Any rumble earlier in this
            // session asserted HAPTICS_SELECT, which firmware-mutes them, and nothing else ever
            // clears it — so without this the stream renders into a muted actuator and looks for
            // all the world like the host is sending nothing.
            restoreAudioHaptics()
            hook.start(index, fd)
        }
    }

    /**
     * B6: clear the rumble/haptics-select bits so the pad's voice coils answer the audio-haptics
     * path again. EP0-direct, like the other out-of-band writes here: this has to land even when
     * the interrupt-OUT queue is busy or draining, and it is idempotent.
     */
    private fun restoreAudioHaptics() {
        val m = model ?: return
        if (m == DsDevice.Model.DUALSHOCK4) return // no voice coils, no audio-haptics path
        if (!usb.writeControl(DsDevice.ds5AudioHapticsReport(m))) {
            Log.w(TAG, "pad audio: could not hand the coils back to audio haptics")
        }
    }

    /**
     * Stop the renderer, then close the connection whose descriptor it borrows — in that order.
     *
     * Runs on [stop] and on unplug alike. Skipping it on unplug left the render thread writing to a
     * descriptor whose device was gone, leaked the connection, and — because the started flag stayed
     * set and the native tier-A registry stayed armed for that index — cost the pad both its pad
     * audio and its wire rumble on the way back in.
     */
    @Synchronized
    private fun stopPadAudio() {
        if (!padAudioStarted) return
        padAudioStarted = false
        // The hook's stop joins the render thread, so nothing is using the descriptor once it
        // returns — only then is it safe to close the connection that owns it.
        pad?.let { padAudio?.stop(it.index) }
        padAudioConn?.close()
        padAudioConn = null
    }

    private fun onLinkClosed() {
        Log.i(TAG, "Sony USB link closed (unplug)")
        // Before releaseSlot(), which forgets the wire index the renderer is addressed by.
        stopPadAudio()
        disarmBackstop()
        val wasActive = model != null
        model = null
        releaseSlot()
        // Release the transport too: the link only *signals* the drop, so without this an unplug
        // left its connection open, its interfaces claimed and its detach receiver registered.
        usb.stop()
        if (wasActive) onActiveChanged?.invoke(false)
    }

    /** Diff the parsed state onto the per-transition plane (buttons + axes, on change only). */
    private fun mirrorTyped(p: GamepadRouter.ExternalPad) {
        var changed = state.buttons xor wireButtons
        while (changed != 0) {
            val bit = changed and -changed // lowest changed bit
            p.button(bit, state.buttons and bit != 0)
            changed = changed and bit.inv()
        }
        wireButtons = state.buttons
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
     * The rich plane: touch contacts normalized to the wire's 0..65535 screen space, forwarded
     * on change per slot; motion forwarded every report (raw device units — the wire is a unit
     * passthrough into the host's virtual pad, and sensor noise makes per-report dedup pointless).
     */
    private fun mirrorRich(p: GamepadRouter.ExternalPad, m: DsDevice.Model) {
        for (f in 0 until 2) {
            if (state.touchActive[f]) {
                val x = (state.touchX[f].coerceIn(0, m.touchW - 1) * 65535) / (m.touchW - 1)
                val y = (state.touchY[f].coerceIn(0, m.touchH - 1) * 65535) / (m.touchH - 1)
                if (!lastTouchActive[f] || x != lastTouchX[f] || y != lastTouchY[f]) {
                    p.touch(f, true, x, y)
                    lastTouchActive[f] = true
                    lastTouchX[f] = x
                    lastTouchY[f] = y
                }
            } else if (lastTouchActive[f]) {
                p.touch(f, false, lastTouchX[f], lastTouchY[f])
                lastTouchActive[f] = false
            }
        }
        p.motion(state.gyro, state.accel)
    }

    private fun releaseSlot() {
        // Lift any still-touching finger so the host's virtual touchpad doesn't hold a contact.
        val p = pad
        if (p != null) {
            for (f in 0 until 2) if (lastTouchActive[f]) p.touch(f, false, lastTouchX[f], lastTouchY[f])
        }
        p?.close()
        pad = null
        wireButtons = 0
        lastAxis.fill(Int.MIN_VALUE)
        lastTouchActive.fill(false)
        lastTouchX.fill(-1)
        lastTouchY.fill(-1)
    }

    // ---- PadFeedbackSink (feedback poll threads) ----

    override fun ownsPad(pad: Int): Boolean = pad == this.pad?.index

    override fun rumble(pad: Int, low: Int, high: Int, backstopMs: Long) {
        val m = model ?: return
        val stop = low == 0 && high == 0
        if (!stop) armBackstop(backstopMs)
        val sent = if (m == DsDevice.Model.DUALSHOCK4) {
            ds4Low = low
            ds4High = high
            writeDs4()
        } else {
            usb.writeRaw(0, DsDevice.ds5RumbleReport(m, low, high), OutReportQueue.KEY_RUMBLE)
        }
        if (stop) {
            // Disarm only once the stop is actually on its way. Dropping the net *before* the
            // write — as this used to — meant a discarded stop left the motors running with
            // nothing scheduled to try again; a USB pad holds its last level until told zero.
            if (sent) disarmBackstop() else armBackstop(STOP_RETRY_MS)
            // B6: the stop report just re-asserted HAPTICS_SELECT on its way past, so if a
            // haptics stream is live the coils it drives were muted by the very write that
            // silenced the motors. Give them back.
            if (sent && padAudioStarted) restoreAudioHaptics()
        }
    }

    override fun led(pad: Int, r: Int, g: Int, b: Int) {
        val m = model ?: return
        if (m == DsDevice.Model.DUALSHOCK4) {
            ds4Rgb = (r shl 16) or (g shl 8) or b
            writeDs4()
        } else {
            usb.writeRaw(0, DsDevice.ds5LightbarReport(m, r, g, b))
        }
    }

    override fun playerLeds(pad: Int, bits: Int) {
        val m = model ?: return
        if (m == DsDevice.Model.DUALSHOCK4) return // no player LEDs on a DS4 (host never sends any)
        usb.writeRaw(0, DsDevice.ds5PlayerLedsReport(m, bits))
    }

    override fun trigger(pad: Int, which: Int, effect: ByteArray) {
        val m = model ?: return
        if (m == DsDevice.Model.DUALSHOCK4) return // no adaptive triggers on a DS4
        usb.writeRaw(0, DsDevice.ds5TriggerReport(m, which, effect))
    }

    // Coalescable: the DS4's write is full-state (motors AND lightbar, rebuilt from the current
    // fields on every call), so a newer one supersedes an older one wholesale — nothing is lost by
    // collapsing a backlog of them down to the last.
    private fun writeDs4() = usb.writeRaw(
        0,
        DsDevice.ds4Report(
            ds4Low,
            ds4High,
            (ds4Rgb shr 16) and 0xFF,
            (ds4Rgb shr 8) and 0xFF,
            ds4Rgb and 0xFF,
        ),
        OutReportQueue.KEY_RUMBLE,
    )

    /**
     * Hand the pad back neutral: adaptive triggers released, lightbar dark, player LEDs clear.
     *
     * Rumble stops the moment nothing renews it, but these are LATCHED in the controller's
     * firmware — they outlive the stream, the app, and being unplugged. Ending a session while a
     * game held a weapon's trigger resistance left the physical trigger stiff afterwards, with
     * nothing to release it but another game that happens to set one.
     *
     * EP0-direct like the rumble stop above: the reader thread is stopping, so the interrupt-OUT
     * queue would never drain. Writes are best-effort — the pad may already be gone.
     */
    private fun resetRichFeedback(m: DsDevice.Model) {
        if (m == DsDevice.Model.DUALSHOCK4) {
            // No adaptive triggers or player LEDs on a DS4, and its write is full-state, so
            // blacking the lightbar is a single composed report.
            ds4Rgb = 0
            usb.writeControl(DsDevice.ds4Report(0, 0, 0, 0, 0))
            return
        }
        // An all-zero effect block is mode 0x00 — no effect — which is what releases the trigger.
        for (which in 0..1) {
            usb.writeControl(
                DsDevice.ds5TriggerReport(m, which, ByteArray(DsDevice.TRIGGER_EFFECT_LEN)),
            )
        }
        usb.writeControl(DsDevice.ds5LightbarReport(m, 0, 0, 0))
        usb.writeControl(DsDevice.ds5PlayerLedsReport(m, 0))
    }

    /** The report that stops the motors. The DS4's is a full-state write, so it zeroes the
     *  composed motor state and carries the current lightbar rather than blacking it out. */
    private fun stopReport(m: DsDevice.Model): ByteArray = if (m == DsDevice.Model.DUALSHOCK4) {
        ds4Low = 0
        ds4High = 0
        DsDevice.ds4Report(
            0,
            0,
            (ds4Rgb shr 16) and 0xFF,
            (ds4Rgb shr 8) and 0xFF,
            ds4Rgb and 0xFF,
        )
    } else {
        DsDevice.ds5RumbleReport(m, 0, 0)
    }

    /** (Re)arm the stalled-poll-thread net: write a rumble stop at the command's backstop. */
    private fun armBackstop(ms: Long) {
        backstop?.let { mainHandler.removeCallbacks(it) }
        val r = Runnable {
            backstop = null
            val m = model ?: return@Runnable
            // The net itself can be refused (a full queue, a connection going away). Re-arm rather
            // than give up: this is the last thing between a stalled poll thread and a pad that
            // buzzes until it is unplugged. It stops re-arming as soon as the link closes, which
            // clears `model` and disarms.
            if (!usb.writeRaw(0, stopReport(m), OutReportQueue.KEY_RUMBLE)) armBackstop(STOP_RETRY_MS)
        }
        backstop = r
        mainHandler.postDelayed(r, ms.coerceAtLeast(1))
    }

    private fun disarmBackstop() {
        backstop?.let { mainHandler.removeCallbacks(it) }
        backstop = null
    }

    private companion object {
        const val TAG = "DsCapture"

        /** How soon to retry a rumble stop whose write was rejected. Short: the motors are running
         *  and the host has already moved on, so nothing else is coming to silence them. */
        const val STOP_RETRY_MS = 100L
    }
}
