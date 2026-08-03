package io.unom.punktfunk.kit

import android.content.Context
import android.hardware.input.InputManager
import android.os.Handler
import android.os.Looper
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
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
 * threads, so the slot table is a [ConcurrentHashMap].
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
     * deliberate: the exit and mic chords are read off the same slots, and a couch that lost its
     * quit shortcut because a forwarding preference was off would be the worse bug. Nothing is
     * claimed by keeping a slot — the Android input stack shares controllers — unlike the USB
     * capture links, which `StreamScreen` does not start at all while this is off.
     */
    private val forwarding: Boolean = true,
) {

    /** One forwarded controller: its stable wire pad index, per-device axis state, and held buttons. */
    private class Slot(val index: Int, val mapper: Gamepad.AxisMapper) {
        /** Forwarded button bits currently held (Gamepad.BTN_*) — for release-on-close + chord detection. */
        var held = 0
    }

    /** deviceId → slot. Concurrent: the feedback poll threads read it via [deviceForPad]. */
    private val slots = ConcurrentHashMap<Int, Slot>()

    /**
     * Invoked (main thread) with the deviceId whenever a slot closes — hot-unplug or session teardown.
     * `StreamScreen` wires this to `GamepadFeedback.onDeviceRemoved` so a disconnected pad's rumble /
     * lights bindings are released promptly instead of leaking until the feedback threads stop.
     */
    var onSlotClosed: ((deviceId: Int) -> Unit)? = null

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
     * a pad — the couch equivalent of the stream's on-screen mute button, which a gamepad user
     * cannot reach. `StreamScreen` wires it to the mute toggle. Unlike the exit chord this fires
     * immediately: muting is the kind of thing you want to have already happened, and the on-screen
     * indicator makes an accidental toggle self-evident. The buttons still go to the host — the
     * chord adds a meaning to them rather than swallowing them, exactly as the exit chord does.
     */
    var onMicChord: (() -> Unit)? = null

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
     * One button transition on [slot] — the shared body behind [onButton] and an [ExternalPad]'s
     * transitions: forward the wire event, track held state, arm/disarm the exit chord, and fire
     * the mic-mute chord ([MIC_CHORD]).
     */
    private fun slotButton(slot: Slot, bit: Int, down: Boolean, send: Boolean) {
        if (down) {
            if (send && forwarding) {
                NativeBridge.nativeSendGamepadButton(handle, bit, true, slot.index)
            }
            val wasHeld = slot.held
            slot.held = slot.held or bit
            // Full chord now held on this pad → start the hold countdown (idempotent while held).
            if (slot.held and EXIT_CHORD == EXIT_CHORD) armExit()
            // Mic mute, edge-triggered on the button that COMPLETES the chord: a genuine press
            // (`wasHeld` lacks the bit, so an auto-repeat DOWN can't re-fire it) of a chord member
            // that leaves the whole chord held. Any other button pressed while Select + Y are down
            // fails the middle test, so the toggle happens once per chord, not once per press.
            if (wasHeld and bit == 0 && bit and MIC_CHORD != 0 && slot.held and MIC_CHORD == MIC_CHORD) {
                onMicChord?.invoke()
            }
        } else {
            if (send && forwarding) {
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

    /**
     * A capture-link pad occupying a wire slot without an Android [InputDevice] — the as-is Steam
     * Controller 2 passthrough (USB/BLE claimed directly, invisible to the input stack). Shares
     * the real slots' lifecycle: a stable lowest-free index, Arrival-before-input, held-state
     * flush + Remove on [close], and full participation in the emergency exit chord.
     */
    inner class ExternalPad internal constructor(private val syntheticId: Int, val index: Int) {
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
            if (slot != null && forwarding) {
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
     */
    fun openExternal(pref: Int): ExternalPad? {
        val index = lowestFreeIndex() ?: return null
        // Synthetic ids live below any real InputDevice id (those are positive), so they can't
        // collide and InputDevice.getDevice(id) resolves them to null for the feedback path.
        val syntheticId = EXTERNAL_ID_BASE - index
        if (forwarding) NativeBridge.nativeSendGamepadArrival(handle, pref, index)
        slots[syntheticId] = Slot(index, Gamepad.AxisMapper(handle, index))
        return ExternalPad(syntheticId, index)
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
        val slot = Slot(index, Gamepad.AxisMapper(handle, index))
        slots[dev.id] = slot
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
        var bits = slot.held
        while (bits != 0) {
            val bit = bits and -bits // lowest set bit
            if (forwarding) NativeBridge.nativeSendGamepadButton(handle, bit, false, slot.index)
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

    private companion object {
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

        /** Synthetic slot-key base for [ExternalPad]s — below every real (positive) InputDevice id. */
        const val EXTERNAL_ID_BASE = -1000
    }
}
