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
class GamepadRouter(context: Context, private val handle: Long, private val setting: Int) {

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
     * Start + L1 + R1) on any one pad ARMS a [EXIT_HOLD_MS] hold timer rather than leaving instantly;
     * [onExitChord] fires only if the chord is still held at expiry (a brief accidental brush is
     * ignored), matching `DISCONNECT_HOLD` on the SDL/Apple clients. Any controller can leave.
     */
    fun onButton(event: KeyEvent, bit: Int) {
        val slot = slotFor(event.device) ?: return
        when (event.action) {
            KeyEvent.ACTION_DOWN -> {
                // repeatCount guard: don't re-send a held button as auto-repeat.
                if (event.repeatCount == 0) NativeBridge.nativeSendGamepadButton(handle, bit, true, slot.index)
                slot.held = slot.held or bit
                // Full chord now held on this pad → start the hold countdown (idempotent while held).
                if (slot.held and EXIT_CHORD == EXIT_CHORD) armExit()
            }
            KeyEvent.ACTION_UP -> {
                NativeBridge.nativeSendGamepadButton(handle, bit, false, slot.index)
                slot.held = slot.held and bit.inv()
                // A chord button lifted before the hold elapsed → cancel, unless another pad still
                // holds the full chord.
                if (bit and EXIT_CHORD != 0 && slots.values.none { it.held and EXIT_CHORD == EXIT_CHORD }) {
                    disarmExit()
                }
            }
        }
    }

    /** Arm the exit-chord hold timer (once); on expiry, if the chord is still held, flush + leave. */
    private fun armExit() {
        if (pendingExit != null) return // already counting down
        val r = Runnable {
            pendingExit = null
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
    }

    /** Cancel a pending exit-chord hold timer. */
    private fun disarmExit() {
        pendingExit?.let { mainHandler.removeCallbacks(it) }
        pendingExit = null
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
        slot.mapper.onMotion(event)
        return true
    }

    /**
     * The controller currently mapped to wire pad [pad], for feedback routing; null if that index
     * holds no live slot (a pad that just unplugged — the update is then dropped). Read from the
     * feedback poll threads.
     */
    fun deviceForPad(pad: Int): InputDevice? {
        for ((deviceId, slot) in slots) {
            if (slot.index == pad) return InputDevice.getDevice(deviceId)
        }
        return null
    }

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
        NativeBridge.nativeSendGamepadArrival(handle, pref, index)
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
        NativeBridge.nativeSendGamepadRemove(handle, slot.index)
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
            NativeBridge.nativeSendGamepadButton(handle, bit, false, slot.index)
            bits = bits and bit.inv()
        }
        slot.held = 0
        slot.mapper.reset() // zero sticks/triggers + release the HAT dpad
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

        /** How long the exit chord must be held before the stream leaves — matches SDL/Apple `DISCONNECT_HOLD`. */
        const val EXIT_HOLD_MS = 1500L
    }
}
