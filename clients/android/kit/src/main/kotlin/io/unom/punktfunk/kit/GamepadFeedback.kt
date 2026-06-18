package io.unom.punktfunk.kit

import android.graphics.Color
import android.hardware.lights.Light
import android.hardware.lights.LightState
import android.hardware.lights.LightsManager
import android.hardware.lights.LightsRequest
import android.os.Build
import android.os.CombinedVibration
import android.os.VibrationEffect
import android.os.VibratorManager
import android.util.Log
import android.view.InputDevice
import java.nio.ByteBuffer

/**
 * Host→client gamepad feedback for one session (single-pad model — pad 0 only). Two daemon poll
 * threads drain the blocking native pulls and render in Kotlin: rumble → the controller's
 * `VibratorManager`; HID-output → lightbar / player-LED via `LightsManager` (API 33+); adaptive
 * triggers are parse-validated and logged (Android has no public adaptive-trigger API).
 *
 * Mirrors `nativeStartAudio`'s lifecycle: [start]/[stop] driven by the StreamScreen. [stop] flips a
 * flag; the ~100 ms native pull timeout lets the threads exit, then they're joined (bounded) — and
 * this MUST run before `nativeClose` frees the session handle.
 *
 * The active pad is resolved from the connected input devices (first gamepad/joystick). With none
 * connected (emulator) rumble/lights become logged no-ops — exactly the verification path; the
 * `Log.i` receipt lines fire regardless of rendering hardware.
 */
class GamepadFeedback(private val handle: Long) {
    private companion object {
        const val TAG = "pf.feedback"
        const val TAG_LED: Byte = 0x01
        const val TAG_PLAYER_LEDS: Byte = 0x02
        const val TAG_TRIGGER: Byte = 0x03
    }

    @Volatile private var running = false
    private var rumbleThread: Thread? = null
    private var hidoutThread: Thread? = null

    private var vm: VibratorManager? = null
    private var vibratorIds: IntArray = IntArray(0)
    private var amplitudeControlled = false

    private var lightsSession: LightsManager.LightsSession? = null
    private var rgbLight: Light? = null
    private var playerLight: Light? = null

    fun start() {
        val dev = resolvePad()
        bindRumble(dev)
        if (Build.VERSION.SDK_INT >= 33) {
            bindLights(dev)
        } else {
            Log.i(TAG, "lights need API 33 (have ${Build.VERSION.SDK_INT}) — lightbar/playerLed no-op")
        }

        running = true
        rumbleThread = Thread({
            while (running) {
                val ev = NativeBridge.nativeNextRumble(handle)
                if (ev < 0L) continue // timeout / closed
                renderRumble(((ev ushr 16) and 0xFFFF).toInt(), (ev and 0xFFFF).toInt())
            }
        }, "pf-rumble").apply { isDaemon = true; start() }

        hidoutThread = Thread({
            val buf = ByteBuffer.allocateDirect(64)
            while (running) {
                val n = NativeBridge.nativeNextHidout(handle, buf)
                if (n < 0) continue // timeout / closed
                dispatchHidout(buf, n)
            }
        }, "pf-hidout").apply { isDaemon = true; start() }
    }

    /** Idempotent. Stops + joins the poll threads (must complete before the session handle is freed). */
    fun stop() {
        running = false
        rumbleThread?.interrupt()
        hidoutThread?.interrupt()
        runCatching { vm?.cancel() } // drop any held rumble immediately
        runCatching { rumbleThread?.join(200) }
        runCatching { hidoutThread?.join(200) }
        rumbleThread = null
        hidoutThread = null
        runCatching { lightsSession?.close() }
        lightsSession = null
        rgbLight = null
        playerLight = null
        vm = null
        vibratorIds = IntArray(0)
    }

    /** First connected gamepad/joystick InputDevice, or null (→ logged no-op on the emulator). */
    private fun resolvePad(): InputDevice? {
        for (id in InputDevice.getDeviceIds()) {
            val d = InputDevice.getDevice(id) ?: continue
            val s = d.sources
            if (s and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD ||
                s and InputDevice.SOURCE_JOYSTICK == InputDevice.SOURCE_JOYSTICK
            ) {
                return d
            }
        }
        return null
    }

    // ---- Rumble ----

    private fun bindRumble(dev: InputDevice?) {
        if (dev == null) {
            Log.i(TAG, "rumble: no controller connected — rumble no-op (emulator path)")
            return
        }
        val m = dev.vibratorManager
        val ids = m.vibratorIds
        if (ids.isEmpty()) {
            Log.i(TAG, "rumble: controller '${dev.name}' has no vibrators — rumble no-op")
            return
        }
        vm = m
        vibratorIds = ids
        amplitudeControlled = ids.all { m.getVibrator(it).hasAmplitudeControl() }
        Log.i(TAG, "rumble: bound ${ids.size} vibrators amplitudeControl=$amplitudeControlled")
    }

    /** low = heavy/left motor, high = light/right motor; both 0..0xFFFF (the host's u16 amplitudes). */
    private fun renderRumble(low: Int, high: Int) {
        Log.i(TAG, "rumble low=$low high=$high") // verification line — BEFORE any no-op return
        val m = vm ?: return
        val lo = toAmplitude(low)
        val hi = toAmplitude(high)
        if (lo == 0 && hi == 0) {
            m.cancel() // (0,0) = stop
            return
        }
        val combo = CombinedVibration.startParallel()
        if (amplitudeControlled && vibratorIds.size >= 2) {
            // ids[0] = light/right, ids[1] = heavy/left (XInput/Moonlight convention).
            if (hi != 0) combo.addVibrator(vibratorIds[0], oneShot(hi))
            if (lo != 0) combo.addVibrator(vibratorIds[1], oneShot(lo))
        } else {
            // Single motor or no amplitude control: blend both into one effect.
            val a = (lo * 0.8 + hi * 0.33).toInt().coerceIn(1, 255)
            for (id in vibratorIds) combo.addVibrator(id, oneShot(a))
        }
        runCatching { m.vibrate(combo.combine()) }
    }

    // 0..0xFFFF → 1..255 (high byte); a nonzero motor never collapses to 0.
    private fun toAmplitude(v16: Int): Int {
        val a = (v16 ushr 8) and 0xFF
        return if (v16 != 0 && a == 0) 1 else a
    }

    // Long one-shot held until the next packet (the host re-sends ~periodically); cancel on zero.
    private fun oneShot(amp: Int): VibrationEffect = VibrationEffect.createOneShot(60_000L, amp)

    // ---- HID output ----

    private fun dispatchHidout(buf: ByteBuffer, n: Int) {
        buf.rewind()
        when (buf.get()) { // kind tag
            TAG_LED -> {
                val r = buf.get().toInt() and 0xFF
                val g = buf.get().toInt() and 0xFF
                val b = buf.get().toInt() and 0xFF
                Log.i(TAG, "hidout Led r=$r g=$g b=$b") // verification line
                if (Build.VERSION.SDK_INT >= 33) setLightbar(Color.rgb(r, g, b))
            }
            TAG_PLAYER_LEDS -> {
                val bits = buf.get().toInt() and 0x1F
                val player = playerIndexForBits(bits)
                Log.i(TAG, "hidout PlayerLeds bits=$bits player=$player") // verification line
                if (Build.VERSION.SDK_INT >= 33) setPlayerId(player)
            }
            TAG_TRIGGER -> {
                val which = buf.get().toInt() and 0xFF // 0 = L2, 1 = R2
                val effLen = n - 2
                val mode = if (effLen > 0) buf.get().toInt() and 0xFF else 0
                // No public adaptive-trigger API on Android — parse-validate the mode + log only.
                Log.i(
                    TAG,
                    "hidout Trigger which=$which effLen=$effLen mode=0x%02x (adaptive triggers unsupported on Android)".format(mode),
                )
            }
            else -> Log.d(TAG, "hidout: unknown kind, dropped")
        }
    }

    /** hid-playstation 5-LED pattern → player index 1..4 (0 = off); falls back to a bit count. */
    private fun playerIndexForBits(bits: Int): Int = when (bits and 0x1F) {
        0b00000 -> 0
        0b00100 -> 1
        0b01010 -> 2
        0b10101 -> 3
        0b11011 -> 4
        else -> Integer.bitCount(bits and 0x1F).coerceIn(1, 4)
    }

    private fun bindLights(dev: InputDevice?) {
        if (dev == null) {
            Log.i(TAG, "lights: no controller connected — lightbar/playerLed no-op (emulator path)")
            return
        }
        val lm = dev.lightsManager
        for (l in lm.lights) {
            if (rgbLight == null && l.hasRgbControl()) rgbLight = l
            if (playerLight == null && l.type == Light.LIGHT_TYPE_PLAYER_ID) playerLight = l
        }
        if (rgbLight == null && playerLight == null) {
            Log.i(TAG, "lights: controller '${dev.name}' exposes no controllable lights — no-op")
            return
        }
        lightsSession = lm.openSession()
        Log.i(TAG, "lights: bound rgb=${rgbLight != null} playerLed=${playerLight != null}")
    }

    private fun setLightbar(argb: Int) {
        val s = lightsSession ?: return
        val l = rgbLight ?: return
        runCatching {
            s.requestLights(LightsRequest.Builder().addLight(l, LightState.Builder().setColor(argb).build()).build())
        }
    }

    private fun setPlayerId(player: Int) {
        val s = lightsSession ?: return
        val l = playerLight ?: return
        runCatching {
            s.requestLights(LightsRequest.Builder().addLight(l, LightState.Builder().setPlayerId(player).build()).build())
        }
    }
}
