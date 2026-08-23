package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.input.InputManager
import android.os.Handler
import android.os.Looper
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import java.util.Collections
import java.util.concurrent.ConcurrentHashMap

/**
 * Multi-controller router for one stream session — the Android analogue of the Linux client's gamepad
 * `Worker`/`Slot` model (`pf-client-core/src/gamepad.rs`) over the shared native-plane wire contract
 * (`punktfunk-core/src/input.rs`). Each physical controller (Android `deviceId`) gets a STABLE
 * lowest-free wire pad index (0..15) held for its lifetime and freed only on disconnect, so a pad
 * dropping never renumbers the others (a game must not see its players shuffle). Every forwarded event
 * carries that pad index; a [NativeBridge.nativeSendGamepadArrival] declaring the pad's type is sent
 * once BEFORE its first input, a [NativeBridge.nativeSendGamepadRemove] on disconnect. Per-device axis
 * state lives in each slot's [Gamepad.AxisMapper] so a second controller can't clobber the first.
 * Feedback (rumble / HID) is routed BACK to the originating device by pad index via [deviceForPad].
 *
 * Selection: forward EVERY real controller (the Linux client's single-player pin has no Android UI
 * surface yet — Automatic is the only mode). Lifetime matches the session: constructed on stream
 * attach (opening a slot for every already-connected pad, so its Arrival lands before any input),
 * released on detach.
 *
 * A single controller lands on wire index 0, so its per-transition button/axis wire is byte-identical
 * to the old single-pad path (plus the Arrival/Remove declarations the contract requires — which an
 * older host simply ignores).
 *
 * Threading: slot mutation + dispatch run on the main thread (Android input dispatch and the
 * InputManager hot-plug callbacks both land there). [deviceForPad] is read from the feedback poll
 * threads, [padPresent]/[padHasOwnMotion] from the phone-gyro thread and [deviceMotion] from the
 * pad-sensor thread, so the slot table is a [ConcurrentHashMap].
 */
class GamepadRouter(
    context: Context,
    private val handle: Long,
    private val setting: Int,
    /**
     * Forward this device's controllers to the host at all (`Settings.gamepadForwarding`,
     * default true). Off is for a couch whose controller reaches the host another way — USB
     * passthrough such as VirtualHere, or a pad plugged into the host itself — where forwarding
     * as well would give the host two pads for one pair of hands.
     *
     * Off still opens slots and tracks held state; it only stops the wire sends. That is
     * deliberate: the exit, mic and stats chords are read off the same slots, and a couch that lost
     * its quit shortcut because a forwarding preference was off would be the worse bug. Nothing is
     * claimed by keeping a slot — the Android input stack shares controllers — unlike the USB
     * capture links, which `StreamScreen` does not start at all while this is off.
     */
    forwarding: Boolean = true,
    /**
     * Forward raw guide/QAM presses (`Settings.systemButtons` resolved — auto = forward on
     * Android, where the press reaches the app on most devices; `local` exists for
     * cross-client profile parity with the Gaming-Mode clients). Off keeps them entirely
     * with this device.
     */
    private val systemForward: Boolean = true,
    /**
     * The hold-Select guide gesture (`Settings.guideGesture` resolved — auto = off on
     * Android): holding Select ALONE ≥ [GUIDE_HOLD_MS] sends the HOST's guide button, down
     * until release — so a long hold is the host's long-press, a Gaming-Mode host's QAM. A
     * Select tap is delivered on release (delayed by up to the threshold); a Select pressed
     * while other buttons are down passes through untouched, so the exit/mic chords keep
     * working. pf-client-core's `SelectGesture`, on the main-thread handler.
     */
    private val guideGesture: Boolean = false,
) {

    /** The ctor's forwarding preference, fixed for the session — one term of [forwarding]. */
    private val forwardingSetting = forwarding

    /**
     * Whether this session's access includes the GAMEPAD grant ([SessionAccess.GAMEPAD]) —
     * seeded from the Welcome and kept live by `StreamScreen`'s access poll (an `AccessUpdate`
     * can revoke or restore it mid-session, latest-wins). Gates exactly what the forwarding
     * preference gates: the wire sends, never the slots — the exit/mic/stats chords must keep
     * working on a Controller-less access level too (they are local controls that happen to be
     * read off pad buttons). The host enforces regardless; this stops the client paying to send
     * events that will be dropped. Volatile: the sensor and USB-capture threads read it per
     * sample through [forwarding].
     */
    @Volatile
    var gamepadGranted: Boolean = true

    /** Send on the wire at all — the forwarding preference AND the session's GAMEPAD grant. */
    private val forwarding: Boolean get() = forwardingSetting && gamepadGranted

    /** One forwarded controller: its stable wire pad index, per-device axis state, and held buttons. */
    private class Slot(
        val index: Int,
        val mapper: Gamepad.AxisMapper,
        /**
         * Whether motion sent for this pad can reach the game at all, asked once at open off the
         * kind it declared ([NativeBridge.nativePadMotionReaches]). False means the host built it a
         * backend with no motion plane, so [deviceMotion] drops the sample here rather than paying
         * to send one the host will decode and discard — at a controller's full sensor rate, for
         * the whole session. The capture-link pads carry the same flag on [ExternalPad].
         */
        val motionReaches: Boolean = true,
        /**
         * Whether [Gamepad.BTN_MISC1] means a MUTE button on this particular pad — the one bit
         * whose physical meaning differs per controller, and the gate on the mic toggle in
         * [slotButton].
         *
         * A DualSense has one; a Steam Controller 2 puts its QAM button on the same wire bit
         * (`Sc2Device`), and QAM must not mute anyone's microphone. Asked once at open, off the
         * fact each path actually knows: the report order for an [InputDevice] (only
         * [Gamepad.PadButtons.GENERIC_SONY] mints this bit there), the declared pad kind for a
         * capture link.
         */
        val hasMuteButton: Boolean = false,
    ) {
        /** Forwarded button bits currently held (Gamepad.BTN_*) — for release-on-close + chord detection. */
        var held = 0

        // Hold-Select→guide gesture state ([guideGesture]): the pending Select's hold
        // timer / a delivered tap's owed release (both on the main handler), and whether
        // the held Select was transformed into a synthetic guide.
        var pendingGuide: Runnable? = null
        var pendingTapUp: Runnable? = null
        var selectAsGuide = false
    }

    /** deviceId → slot. Concurrent: the feedback poll threads read it via [deviceForPad]. */
    private val slots = ConcurrentHashMap<Int, Slot>()

    /**
     * deviceIds whose own gyro [PadSensors] is currently reading — see [setDeviceHasSensorMotion].
     * Written on the main thread, read from the phone-gyro thread, hence a concurrent set.
     */
    private val sensorDevices: MutableSet<Int> =
        Collections.newSetFromMap(ConcurrentHashMap<Int, Boolean>())

    /**
     * Invoked (main thread) with the deviceId whenever a slot closes — hot-unplug, a capture link's
     * [releaseDevice] claim, or session teardown. `StreamScreen` wires this to
     * `GamepadFeedback.onDeviceRemoved` so a disconnected pad's rumble / lights bindings are
     * released promptly instead of leaking until the feedback threads stop, and to
     * [PadSensors.onSlotClosed] so the controller's own sensor listeners come off with it.
     */
    var onSlotClosed: ((deviceId: Int) -> Unit)? = null

    /**
     * Invoked (main thread) with the deviceId whenever a slot opens for a REAL controller — the
     * hot-plug callback or the first input from a pad the session started without. Not fired for
     * [openExternal]: a capture link's pad has no [InputDevice] behind it and streams motion from
     * its own IMU already. `StreamScreen` wires this to [PadSensors.onSlotOpened].
     *
     * Slots opened in `init` (every controller already connected) predate any assignment here, so
     * a listener must sweep [forwardedDevices] once when it starts. Both happen on the main thread
     * inside one composition block, so nothing can slip between the sweep and the assignment.
     */
    var onSlotOpened: ((deviceId: Int) -> Unit)? = null

    /**
     * Invoked (main thread) when the emergency-exit chord has been HELD for [EXIT_HOLD_MS] — the caller
     * leaves the stream. `StreamScreen` wires this to the deliberate-quit exit.
     */
    var onExitChord: (() -> Unit)? = null

    /**
     * Invoked (main thread) with `true` the moment the exit chord completes and the hold countdown
     * starts, and `false` when it's cancelled (a button lifted early) or the timer elapses. `StreamScreen`
     * wires this to a "hold to quit" hint so the hold is discoverable — the chord no longer quits on a
     * quick press, and without an on-screen cue that reads as the shortcut being broken.
     */
    var onExitArmed: ((armed: Boolean) -> Unit)? = null

    /**
     * Invoked (main thread) each time the mic-mute chord ([MIC_CHORD], Select + Y) is COMPLETED on
     * a pad, or a pad's own mute button ([Gamepad.BTN_MISC1] — a DualSense's) is pressed — the
     * couch equivalent of the stream's on-screen mute button, which a gamepad user
     * cannot reach. `StreamScreen` wires it to the mute toggle. Unlike the exit chord this fires
     * immediately: muting is the kind of thing you want to have already happened, and the on-screen
     * indicator makes an accidental toggle self-evident. The buttons still go to the host — the
     * chord adds a meaning to them rather than swallowing them, exactly as the exit chord does.
     */
    var onMicChord: (() -> Unit)? = null

    /**
     * Invoked (main thread) each time the stats chord ([STATS_CHORD], Select + X) is COMPLETED on a
     * pad — one verbosity tier of the in-stream statistics overlay per completion. It exists
     * because a controller in both hands has no other way to the numbers: the three-finger tap
     * needs a touchscreen AND one of the pointer touch models, so a TV or a gamepad-only session
     * has none. `StreamScreen` wires it to the live tier cycle.
     *
     * Fires immediately and once per chord like [onMicChord], and like it the buttons still go to
     * the host — the chord adds a local meaning to them rather than swallowing them. The Apple
     * client's `GamepadCapture.statsChord` is the same two buttons; a shortcut that differs per
     * platform is worse than no shortcut.
     */
    var onStatsChord: (() -> Unit)? = null

    /**
     * Invoked (main thread) once per pad when a captured controller WITH a gyro turns out to be in
     * a session whose virtual pad has no motion plane — its motion is not being sent, because every
     * sample would be decoded and dropped host-side.
     *
     * It exists because the failure is otherwise completely silent: the gyro just does nothing, and
     * from the couch that is indistinguishable from a broken sensor. The fix is the Controller type
     * setting, so whatever shows this has to name it. `StreamScreen` wires it to a brief notice.
     */
    var onMotionUnreachable: (() -> Unit)? = null

    private val mainHandler = Handler(Looper.getMainLooper())
    /** The pending exit-chord hold timer, or null when the chord isn't currently armed. */
    private var pendingExit: Runnable? = null

    private val inputManager = context.getSystemService(InputManager::class.java)
    private val listener = object : InputManager.InputDeviceListener {
        override fun onInputDeviceAdded(deviceId: Int) {
            InputDevice.getDevice(deviceId)?.let { if (isForwardable(it)) openSlot(it) }
        }

        override fun onInputDeviceRemoved(deviceId: Int) = closeSlot(deviceId)
        override fun onInputDeviceChanged(deviceId: Int) {}
    }

    init {
        inputManager?.registerInputDeviceListener(listener, mainHandler)
        // Open a slot for every controller already connected when the session starts — the pads that
        // will never fire onInputDeviceAdded during this session; their Arrival lands before any input.
        for (id in InputDevice.getDeviceIds()) {
            InputDevice.getDevice(id)?.let { if (isForwardable(it)) openSlot(it) }
        }
    }

    /**
     * One gamepad button transition for the device that produced [event] (already resolved to BTN_*
     * bit [bit]). Opens the device's slot (declaring its type) if unseen, forwards the bit on the
     * slot's pad index, and tracks held state. Completing the emergency stream-exit chord (Select +
     * Start + L1 + R1) on any one pad ARMS a [EXIT_HOLD_MS] hold timer rather than leaving instantly
     * ([onExitArmed] fires so the UI can show a "hold to quit" hint); [onExitChord] fires only if the
     * chord is still held at expiry (a brief accidental brush is ignored), matching `DISCONNECT_HOLD`
     * on the SDL/Apple clients. Any controller can leave.
     */
    fun onButton(event: KeyEvent, bit: Int) {
        val slot = slotFor(event.device) ?: return
        when (event.action) {
            // repeatCount guard: don't re-send a held button as auto-repeat.
            KeyEvent.ACTION_DOWN -> slotButton(slot, bit, down = true, send = event.repeatCount == 0)
            KeyEvent.ACTION_UP -> slotButton(slot, bit, down = false, send = true)
        }
    }

    /**
     * Is this bit's WIRE SEND kept with this device, though the bit is otherwise tracked normally?
     *
     * Exactly one is: a real mute button ([Slot.hasMuteButton]) under the "local" [systemForward]
     * policy. It is tracked — the mic toggle in [slotButton] is edge-triggered off held state —
     * but not forwarded, so every send site has to ask, including [releaseHeld]'s close-time
     * flush, or a mute held across a disconnect would put a release on the wire for a press that
     * never went out. Every other system button under that policy leaves [slotButton] at the top
     * and never reaches a send at all.
     */
    private fun localOnly(slot: Slot, bit: Int): Boolean =
        !systemForward && bit == Gamepad.BTN_MISC1 && slot.hasMuteButton

    /**
     * One button transition on [slot] — the shared body behind [onButton] and an [ExternalPad]'s
     * transitions: forward the wire event, track held state, arm/disarm the exit chord, and fire
     * the instant chords ([MIC_CHORD], [STATS_CHORD], and the mute button's own mic toggle).
     */
    private fun slotButton(slot: Slot, bit: Int, down: Boolean, send: Boolean) {
        // Raw system buttons stay local under the "local" policy — no wire send and no held
        // tracking, symmetric on both edges so nothing leaks into the chords either. A Steam
        // Controller 2's QAM button is BTN_MISC1 and keeps exactly that behaviour.
        //
        // A real MUTE button ([Slot.hasMuteButton]) is deliberately exempt: that policy's own
        // words are "keeps them entirely with this device", and toggling this device's microphone
        // is precisely what a mute button does with itself. Returning here would have left the
        // button present and silently dead under `local`, for a reason nobody would ever find. It
        // loses its wire send instead (see [localOnly]) and keeps the held tracking the toggle's
        // edge-trigger reads. It cannot leak into a chord — MISC1 is in none of them.
        if (!systemForward &&
            (bit == Gamepad.BTN_GUIDE || (bit == Gamepad.BTN_MISC1 && !slot.hasMuteButton))
        ) {
            return
        }
        if (down) {
            if (guideGesture && send) {
                // A Select pressed ALONE is held back until it resolves: a tap (delivered
                // on release), a combo member (the next button flushes it as a real
                // press), or — past GUIDE_HOLD_MS — a synthetic guide. Held state records
                // it either way, so the exit/mic chords read as if the gesture didn't
                // exist (Select+Y still fires the mic toggle: the flush sends Select's
                // down before Y's).
                if (bit == Gamepad.BTN_BACK && slot.held == 0) {
                    slot.held = slot.held or bit
                    armGuide(slot)
                    return
                }
                flushPendingSelect(slot)
            }
            if (send && forwarding && !localOnly(slot, bit)) {
                NativeBridge.nativeSendGamepadButton(handle, bit, true, slot.index)
            }
            val wasHeld = slot.held
            slot.held = slot.held or bit
            // Full chord now held on this pad → start the hold countdown (idempotent while held).
            if (slot.held and EXIT_CHORD == EXIT_CHORD) armExit()
            // Mic mute and the stats-tier cycle, each edge-triggered on the button that COMPLETES
            // its chord (see [completesChord]) — the two meanings this client gives Select plus a
            // face button. Both leave the press on the wire: the game still gets its buttons.
            //
            // A pad's own mute button is a second trigger for the SAME toggle, not a new
            // mechanism — so it gets the same edge-trigger, expressed as the one-button chord it
            // is. That is load-bearing rather than tidy: [onButton] deliberately still calls this
            // with `down = true` on auto-repeat and suppresses only `send` (its repeatCount
            // guard), so an unguarded `bit == BTN_MISC1` would flap the mic for as long as the
            // button is held down.
            //
            // [Slot.hasMuteButton] is the other half, and it is not belt-and-braces: BTN_MISC1 is
            // the wire's misc/QAM bit, and `Sc2Device` puts a Steam Controller 2's QAM button on
            // it. Reading "any MISC1" as mute would mute the microphone on every QAM press.
            if (completesChord(wasHeld, bit, MIC_CHORD) ||
                (slot.hasMuteButton && completesChord(wasHeld, bit, Gamepad.BTN_MISC1))
            ) {
                onMicChord?.invoke()
            }
            if (completesChord(wasHeld, bit, STATS_CHORD)) onStatsChord?.invoke()
        } else {
            val owned = guideGesture && bit == Gamepad.BTN_BACK && consumeSelectRelease(slot)
            if (!owned && send && forwarding && !localOnly(slot, bit)) {
                NativeBridge.nativeSendGamepadButton(handle, bit, false, slot.index)
            }
            slot.held = slot.held and bit.inv()
            // A chord button lifted before the hold elapsed → cancel, unless another pad still
            // holds the full chord.
            if (bit and EXIT_CHORD != 0 && slots.values.none { it.held and EXIT_CHORD == EXIT_CHORD }) {
                disarmExit()
            }
        }
    }

    /** Start a pending Select's hold countdown ([GUIDE_HOLD_MS] → a synthetic guide, down until release). */
    private fun armGuide(slot: Slot) {
        val r = Runnable {
            slot.pendingGuide = null
            slot.selectAsGuide = true
            if (forwarding) {
                NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_GUIDE, true, slot.index)
            }
        }
        slot.pendingGuide = r
        mainHandler.postDelayed(r, GUIDE_HOLD_MS)
    }

    /**
     * A second button joined while Select was pending — it was a real Select after all; its
     * deferred down goes out before the caller sends the new button's, preserving chronology.
     */
    private fun flushPendingSelect(slot: Slot) {
        val r = slot.pendingGuide ?: return
        mainHandler.removeCallbacks(r)
        slot.pendingGuide = null
        if (forwarding) {
            NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_BACK, true, slot.index)
        }
    }

    /**
     * Select released with gesture state outstanding — true when the gesture owned the
     * release. A transformed hold lifts the synthetic guide; a pending tap delivers its
     * held-back press now, with the release [TAP_PRESS_MS] behind it (a back-to-back pair
     * can fold into nothing in the host's per-pad input fold).
     */
    private fun consumeSelectRelease(slot: Slot): Boolean {
        if (slot.selectAsGuide) {
            slot.selectAsGuide = false
            if (forwarding) {
                NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_GUIDE, false, slot.index)
            }
            return true
        }
        val r = slot.pendingGuide ?: return false
        mainHandler.removeCallbacks(r)
        slot.pendingGuide = null
        if (forwarding) {
            NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_BACK, true, slot.index)
            val up = Runnable {
                slot.pendingTapUp = null
                NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_BACK, false, slot.index)
            }
            slot.pendingTapUp = up
            mainHandler.postDelayed(up, TAP_PRESS_MS)
        }
        return true
    }

    /** Arm the exit-chord hold timer (once); on expiry, if the chord is still held, flush + leave. */
    private fun armExit() {
        if (pendingExit != null) return // already counting down
        val r = Runnable {
            pendingExit = null
            onExitArmed?.invoke(false) // countdown over — drop the hint whether or not we leave
            // Fire only if the chord survived the full hold on some pad.
            val held = slots.values.filter { it.held and EXIT_CHORD == EXIT_CHORD }
            if (held.isNotEmpty()) {
                // Release the held buttons + zero the axes on every triggering pad so nothing sticks
                // host-side once we leave, then signal the deliberate exit.
                for (s in held) releaseHeld(s)
                onExitChord?.invoke()
            }
        }
        pendingExit = r
        mainHandler.postDelayed(r, EXIT_HOLD_MS)
        onExitArmed?.invoke(true) // chord complete → show the "hold to quit" hint
    }

    /** Cancel a pending exit-chord hold timer. */
    private fun disarmExit() {
        val wasArmed = pendingExit != null
        pendingExit?.let { mainHandler.removeCallbacks(it) }
        pendingExit = null
        if (wasArmed) onExitArmed?.invoke(false) // released early — drop the hint
    }

    /**
     * One joystick MotionEvent — routed to the producing device's own [Gamepad.AxisMapper] (per-device
     * state). Returns true if consumed. Only a real gamepad drives a pad: a DualSense/DS4 motion-sensor
     * sibling node classifies as bare joystick (no GAMEPAD source class) and reports every pad axis as
     * 0, so [isForwardable] filters it out before it can open a slot or clobber axes.
     */
    fun onMotion(event: MotionEvent): Boolean {
        if (!event.isFromSource(InputDevice.SOURCE_JOYSTICK)) return false
        if (event.actionMasked != MotionEvent.ACTION_MOVE) return false
        val dev = event.device ?: return false
        if (!isForwardable(dev)) return false
        val slot = slotFor(dev) ?: return false
        if (forwarding) slot.mapper.onMotion(event)
        return true
    }

    /**
     * The controller currently mapped to wire pad [pad], for feedback routing; null if that index
     * holds no live slot (a pad that just unplugged — the update is then dropped) OR the slot is
     * an [ExternalPad] (its synthetic id resolves to no InputDevice, so rumble binds naturally
     * fall through to the capture link's own feedback path). Read from the feedback poll threads.
     */
    fun deviceForPad(pad: Int): InputDevice? {
        for ((deviceId, slot) in slots) {
            if (slot.index == pad) return InputDevice.getDevice(deviceId)
        }
        return null
    }

    /** Whether ANY live slot currently holds wire pad [pad]. Read from the phone-gyro thread. */
    fun padPresent(pad: Int): Boolean = slots.values.any { it.index == pad }

    /**
     * Whether wire sends are on at all — the forwarding preference AND the session's GAMEPAD
     * grant. For the writers that ride the pad planes from OUTSIDE this router (the phone-gyro
     * mirror), which must stand down with it. Read from the sensor thread.
     */
    fun sendsEnabled(): Boolean = forwarding

    /**
     * Whether wire pad [pad]'s motion already comes from the controller's OWN IMU — either a
     * capture-link slot ([ExternalPad] — USB DualSense / SC2; synthetic ids are negative
     * ([EXTERNAL_ID_BASE]), real [InputDevice] ids positive), or a real controller whose gyro
     * [PadSensors] is reading through the platform sensor framework (a Bluetooth DualSense /
     * Switch Pro / 8BitDo). The phone-gyro mirror stands down for both: two motion writers on one
     * wire pad would fight, and the pad's own IMU is the one attached to the player's hands.
     * Read from the phone-gyro thread (both tables are concurrent).
     */
    fun padHasOwnMotion(pad: Int): Boolean =
        slots.any { (id, slot) -> slot.index == pad && (id < 0 || id in sensorDevices) }

    /**
     * Declare (or withdraw) that real controller [deviceId] is sourcing its own rotation — see
     * [padHasOwnMotion]. Called by [PadSensors] as it registers and unregisters listeners, on the
     * main thread; read from the phone-gyro thread, hence the concurrent set. Keyed by device
     * rather than by pad index so a controller that changes wire index (a lower one freed up while
     * it was captured) carries the fact with it.
     */
    fun setDeviceHasSensorMotion(deviceId: Int, has: Boolean) {
        if (has) sensorDevices.add(deviceId) else sensorDevices.remove(deviceId)
        // This is the first moment we know a Bluetooth pad actually HAS a gyro — `openSlot` only
        // knows what kind it declared. So it is the honest place to raise the notice when that
        // gyro has nowhere to go, and the only one that cannot nag about a pad that never had one.
        if (has && forwarding && slots[deviceId]?.motionReaches == false) {
            onMotionUnreachable?.invoke()
        }
    }

    /**
     * One motion sample from real controller [deviceId]'s own sensors, on whatever wire index its
     * slot currently holds — [ExternalPad.motion] for pads the input stack still owns. Silently
     * drops when the slot is gone (unplugged, or claimed by a capture link between the sensor
     * callback and here) rather than writing to an index that may already belong to someone else.
     * Called from [PadSensors]' sensor thread.
     */
    fun deviceMotion(deviceId: Int, gyro: IntArray, accel: IntArray) {
        val slot = slots[deviceId] ?: return
        if (!forwarding) return
        // The same gate the USB capture path takes: a backend with no motion plane decodes every
        // sample and discards it, so sending is pure cost. Notified once per pad by
        // [setDeviceHasSensorMotion], which is where we first know the controller HAS a gyro to
        // lose — a pad without one must not produce a warning about motion.
        if (!slot.motionReaches) return
        NativeBridge.nativeSendPadMotion(
            handle, slot.index,
            gyro[0], gyro[1], gyro[2],
            accel[0], accel[1], accel[2],
        )
    }

    /** Snapshot of the REAL controllers currently forwarded, as deviceIds — the set [PadSensors]
     *  sweeps at start for the pads that were already connected when the session opened. */
    fun forwardedDevices(): List<Int> = slots.keys.filter { it >= 0 }

    /**
     * A capture-link pad occupying a wire slot without an Android [InputDevice] — the as-is Steam
     * Controller 2 passthrough (USB/BLE claimed directly, invisible to the input stack). Shares
     * the real slots' lifecycle: a stable lowest-free index, Arrival-before-input, held-state
     * flush + Remove on [close], and full participation in the emergency exit chord.
     */
    inner class ExternalPad internal constructor(
        private val syntheticId: Int,
        val index: Int,
        /**
         * Whether this pad's motion can reach the game at all, asked once at open (see
         * [NativeBridge.nativePadMotionReaches]). False means the host built this pad a backend
         * without a motion plane, so [motion] drops the sample here instead of paying to send one
         * the host will decode and discard — at a controller's full report rate, for the whole
         * session.
         */
        private val motionReaches: Boolean,
    ) {
        // Live lookup instead of a captured reference: after [close] (or a router release) the
        // slot is gone from the table and every entry point below degrades to a safe no-op.
        private val slot get() = slots[syntheticId]

        /** One button transition (a wire [Gamepad].BTN_* bit). On-change only — the caller diffs. */
        fun button(bit: Int, down: Boolean) {
            slot?.let { slotButton(it, bit, down, send = true) }
        }

        /** One axis update ([Gamepad].AXIS_*: stick i16 +y=up / trigger 0..255). On-change only. */
        fun axis(id: Int, value: Int) {
            if (slot != null && forwarding) NativeBridge.nativeSendGamepadAxis(handle, id, value, index)
        }

        /** One raw HID report, forwarded verbatim for the host's as-is virtual pad. */
        fun hidReport(buf: java.nio.ByteBuffer, len: Int) {
            if (slot != null && forwarding) NativeBridge.nativeSendPadHidReport(handle, index, buf, len)
        }

        /** One touchpad contact on the rich plane: [finger] 0/1, x/y normalized 0..65535 in
         *  SCREEN convention (+y down); `active = false` lifts the finger. On-change only. */
        fun touch(finger: Int, active: Boolean, x: Int, y: Int) {
            if (slot != null && forwarding) {
                NativeBridge.nativeSendPadTouch(handle, index, finger, active, x, y)
            }
        }

        /** One motion sample on the rich plane (gyro pitch/yaw/roll + accel, raw device i16
         *  units — the host passes them straight into the virtual pad's report). Per report. */
        fun motion(gyro: IntArray, accel: IntArray) {
            if (slot != null && forwarding && motionReaches) {
                NativeBridge.nativeSendPadMotion(
                    handle, index,
                    gyro[0], gyro[1], gyro[2],
                    accel[0], accel[1], accel[2],
                )
            }
        }

        /** Flush held state, signal the removal, and free the wire index. Idempotent. */
        fun close() = closeSlot(syntheticId)
    }

    /**
     * Open a slot for a capture-link pad, declaring [pref] as its kind; null when all 16 wire
     * indices are taken. Main thread (like the hot-plug callbacks).
     *
     * [hasGyro] says whether this link forwards motion on the RICH plane ([ExternalPad.motion]) —
     * true for the Sony pads, whose IMU is a headline feature, and false for the Steam Controller 2,
     * whose motion rides inside the opaque passthrough report that [ExternalPad.hidReport] carries
     * and which nothing here may second-guess. It gates only the notice: a pad that never sends
     * motion must not produce a warning about motion.
     */
    fun openExternal(pref: Int, hasGyro: Boolean = false): ExternalPad? {
        val index = lowestFreeIndex() ?: return null
        // Synthetic ids live below any real InputDevice id (those are positive), so they can't
        // collide and InputDevice.getDevice(id) resolves them to null for the feedback path.
        val syntheticId = EXTERNAL_ID_BASE - index
        if (forwarding) NativeBridge.nativeSendGamepadArrival(handle, pref, index)
        // Asked once, here, off the kind this pad just DECLARED — not off the session's resolved
        // backend, which under Automatic answers for whichever pad happened to be active at dial
        // time. Cheap enough to ask unconditionally; the answer holds for the pad's lifetime.
        val motionReaches = NativeBridge.nativePadMotionReaches(handle, pref)
        if (forwarding && hasGyro && !motionReaches) onMotionUnreachable?.invoke()
        // `DsDevice` raises BTN_MISC1 from the DualSense report's mute bit; `Sc2Device` raises the
        // same bit from the Steam Controller 2's QAM button, which must not touch the microphone.
        // The declared kind separates them (a DualShock 4 has no mute button either).
        val hasMute = pref == Gamepad.PREF_DUALSENSE || pref == Gamepad.PREF_DUALSENSEEDGE
        slots[syntheticId] = Slot(
            index,
            Gamepad.AxisMapper(handle, index),
            hasMuteButton = hasMute,
        )
        return ExternalPad(syntheticId, index, motionReaches)
    }

    /**
     * Close the slot (if any) for a physical controller a capture link just claimed. The claim
     * detaches the kernel driver, so the system's own removal callback would close it moments
     * later anyway — doing it at claim time makes the freed wire index deterministic for the
     * link's [ExternalPad] instead of racing the link's first report against that callback. Safe
     * to over-match (a same-VID/PID sibling that still exists as an InputDevice lazily reopens a
     * slot on its next input event). Main thread, like the hot-plug callbacks.
     */
    fun releaseDevice(deviceId: Int) = closeSlot(deviceId)

    /**
     * Flush + drop every slot and unregister the hot-plug listener. Call on session teardown, AFTER
     * the feedback poll threads are joined (they read [deviceForPad]).
     */
    fun release() {
        inputManager?.unregisterInputDeviceListener(listener)
        disarmExit() // drop any pending exit-chord timer so it can't fire after teardown
        // Snapshot the ids first — closeSlot mutates the map.
        for (id in slots.keys.toList()) closeSlot(id)
    }

    // ---- slots ----

    /** A real, non-virtual controller we forward — its source classes include GAMEPAD (excludes a pad's bare-joystick sensor node). */
    private fun isForwardable(dev: InputDevice): Boolean =
        !dev.isVirtual && dev.sources and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD

    /**
     * The slot for [dev], opening one (and declaring the pad) if this device is unseen; null when [dev]
     * isn't a forwardable controller or every wire index is taken. The [isForwardable] gate lives here —
     * the single lazy-open chokepoint both [onButton] and [onMotion] funnel through — so no entry point
     * can open a phantom slot for a virtual/non-gamepad source (the hot-plug listener and init loop
     * pre-filter and call [openSlot] directly).
     */
    private fun slotFor(dev: InputDevice?): Slot? {
        if (dev == null) return null
        slots[dev.id]?.let { return it }
        if (!isForwardable(dev)) return null
        return openSlot(dev)
    }

    /**
     * Open a slot for [dev] on the lowest free wire index, declaring its kind ([NativeBridge.nativeSendGamepadArrival])
     * before any input so the host builds a matching virtual device (mixed types across pads).
     * Idempotent; null when all 16 wire indices are already forwarded.
     */
    private fun openSlot(dev: InputDevice): Slot? {
        slots[dev.id]?.let { return it }
        val index = lowestFreeIndex() ?: return null // 16 pads already forwarded — drop this one
        // Automatic resolves the pad's type from its VID/PID; an explicit setting forces every pad
        // to that type (a single global choice — matches the handshake's session-default pref).
        val pref = if (setting == Gamepad.PREF_AUTO) Gamepad.prefFor(dev) else setting
        if (forwarding) NativeBridge.nativeSendGamepadArrival(handle, pref, index)
        // Asked here, off the kind this pad just DECLARED — not off the session's resolved backend,
        // which under Automatic answers for whichever pad happened to be active at dial time. Held
        // for the slot's life; the sensor path reads it on every sample.
        val map = Gamepad.padMap(dev)
        val slot = Slot(
            index,
            Gamepad.AxisMapper(handle, index, map),
            NativeBridge.nativePadMotionReaches(handle, pref),
            // The only route to BTN_MISC1 on this path is GENERIC_SONY's `0x13e` row, so the
            // report order IS the answer — and unlike `pref` it survives the user pinning every
            // pad to one type, which would otherwise cost a DualSense its mute button.
            hasMuteButton = map.buttons == Gamepad.PadButtons.GENERIC_SONY,
        )
        slots[dev.id] = slot
        // After the table holds the slot, so a listener that sends on this device the moment it is
        // told ([PadSensors]) finds an index to send on rather than dropping its first samples.
        onSlotOpened?.invoke(dev.id)
        return slot
    }

    /**
     * Flush a slot's held wire state (so nothing sticks host-side), signal the removal, and free its
     * index. Safe against an already-gone device — the flush emits wire events only, no device access.
     */
    private fun closeSlot(deviceId: Int) {
        val slot = slots.remove(deviceId) ?: return
        releaseHeld(slot)
        if (forwarding) NativeBridge.nativeSendGamepadRemove(handle, slot.index)
        // If this pad was mid-exit-chord, its removal may have left no pad holding it — drop the timer.
        if (slots.values.none { it.held and EXIT_CHORD == EXIT_CHORD }) disarmExit()
        // Release this controller's feedback bindings (close its lights session / cancel rumble).
        onSlotClosed?.invoke(deviceId)
    }

    /** Lift every held button + zero the axes/HAT dpad for [slot] (wire events only, all on its index). */
    private fun releaseHeld(slot: Slot) {
        // Gesture first: a pending (never-sent) Select just drops its timer; an owed tap
        // release goes out NOW (its down is already on the wire and the handle may not
        // outlive this slot); a transformed guide — which is not in `held` — is lifted.
        slot.pendingGuide?.let { mainHandler.removeCallbacks(it) }
        slot.pendingGuide = null
        slot.pendingTapUp?.let {
            mainHandler.removeCallbacks(it)
            slot.pendingTapUp = null
            if (forwarding) {
                NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_BACK, false, slot.index)
            }
        }
        if (slot.selectAsGuide) {
            slot.selectAsGuide = false
            if (forwarding) {
                NativeBridge.nativeSendGamepadButton(handle, Gamepad.BTN_GUIDE, false, slot.index)
            }
        }
        var bits = slot.held
        while (bits != 0) {
            val bit = bits and -bits // lowest set bit
            if (forwarding && !localOnly(slot, bit)) {
                NativeBridge.nativeSendGamepadButton(handle, bit, false, slot.index)
            }
            bits = bits and bit.inv()
        }
        slot.held = 0
        if (forwarding) slot.mapper.reset() // zero sticks/triggers + release the HAT dpad
    }

    /** Lowest wire index 0..[MAX_PADS) not held by a slot, or null when full — stable lowest-free keeps indices from shuffling on hot-plug. */
    private fun lowestFreeIndex(): Int? {
        val taken = slots.values.mapTo(HashSet()) { it.index }
        for (i in 0 until MAX_PADS) if (i !in taken) return i
        return null
    }

    // `internal` rather than private: the chord masks and [completesChord] are the only part of
    // this router a JVM unit test can reach — everything else needs an InputManager, a main Looper
    // and live InputDevices behind it — and until `GamepadChordTest` there was nothing pinning the
    // chords at all. Still invisible to :app, which is what private bought.
    internal companion object {
        /** Mirror of `punktfunk-core::input::MAX_PADS` — wire pad indices 0..15. */
        const val MAX_PADS = 16

        /** Emergency stream-exit chord: Select + Start + L1 + R1 held together (matches the legacy single-pad chord). */
        const val EXIT_CHORD = Gamepad.BTN_BACK or Gamepad.BTN_START or Gamepad.BTN_LB or Gamepad.BTN_RB

        /**
         * How long the exit chord must be held before the stream leaves — long enough that an
         * accidental brush of the four buttons doesn't quit, short enough to feel responsive (the
         * on-screen hint covers the gap). Roughly matches SDL/Apple `DISCONNECT_HOLD`.
         */
        const val EXIT_HOLD_MS = 1000L

        /**
         * Mic-mute chord: Select + Y. Y is deliberately NOT one of [EXIT_CHORD]'s buttons, so no
         * way of reaching the exit chord can pass through this one on the way (and vice versa) —
         * and Select is a menu button rather than a twitch action, which makes the pair unlikely
         * to occur inside real play.
         */
        const val MIC_CHORD = Gamepad.BTN_BACK or Gamepad.BTN_Y

        /**
         * Stats-overlay chord: Select + X, one verbosity tier per completion. X keeps both
         * properties [MIC_CHORD]'s Y has — it is none of [EXIT_CHORD]'s four buttons, so no way of
         * reaching the exit chord passes through this one on the way (and vice versa), and Select
         * is a menu button rather than a twitch action. Byte-for-byte the Apple client's
         * `GamepadCapture.statsChord`, which was modelled on [MIC_CHORD] in the first place and
         * leaves Y free for the mic chord to land there in turn — the two clients converge on one
         * pad vocabulary from both ends.
         */
        const val STATS_CHORD = Gamepad.BTN_BACK or Gamepad.BTN_X

        /**
         * Whether pressing [bit] on a pad that held [wasHeld] beforehand COMPLETED [chord]: a
         * genuine press (`wasHeld` lacks the bit, so an auto-repeat DOWN can't re-fire it) of a
         * chord member that leaves the whole chord held (`wasHeld or bit` is the slot's held set
         * the instant after the press). Any other button pressed while the chord is already down
         * fails the middle test, so a chord fires once per chord, not once per press — and lifting
         * any member re-arms it, since the next press of that member is a fresh completion.
         */
        internal fun completesChord(wasHeld: Int, bit: Int, chord: Int): Boolean =
            wasHeld and bit == 0 && bit and chord != 0 && (wasHeld or bit) and chord == chord

        /** Synthetic slot-key base for [ExternalPad]s — below every real (positive) InputDevice id. */
        const val EXTERNAL_ID_BASE = -1000

        /** pf-client-core's `GUIDE_HOLD`: hold Select alone this long → the host's guide goes down. */
        const val GUIDE_HOLD_MS = 350L

        /**
         * pf-client-core's `TAP_PRESS`: a held-back Select tap's release trails its press by
         * this much, so the pair can't coalesce into no press at all.
         */
        const val TAP_PRESS_MS = 50L
    }
}
