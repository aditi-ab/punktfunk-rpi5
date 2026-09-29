package io.unom.punktfunk

import android.os.Build
import android.os.VibrationEffect
import android.os.Vibrator
import android.view.InputDevice
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import io.unom.punktfunk.kit.deviceBodyVibrator

/**
 * The console's menu feel: a tick as the cursor moves, a heavier thud when a press is refused at a
 * boundary, a pulse on confirm. The Apple client's `MenuHaptics` semantics and the desktop's
 * `menu_rumble(pulse)`, on whatever actuator this device actually has.
 *
 * Constructed by [rememberConsoleHaptics], which resolves the actuator in the order that matches
 * where the player's hands are: the DRIVING controller's own vibrator first, then the phone body
 * (which is what a clip-on pad without motors leaves you holding), and nothing at all on a TV — a
 * remote has no motor and a TV box has no body, so both resolve null and every call is a no-op.
 */
class ConsoleHaptics internal constructor(private val vibrator: Vibrator?) {
    /** The cursor moved one step. */
    fun tick() = play(7, 40)

    /** The press was refused — the list ended, or the value is already at its limit. */
    fun boundary() = play(18, 95)

    /** Something was confirmed, cycled, or flipped. */
    fun confirm() = play(12, 70)

    private fun play(durationMs: Long, amplitude: Int) {
        val v = vibrator ?: return
        runCatching {
            v.vibrate(
                if (v.hasAmplitudeControl()) {
                    VibrationEffect.createOneShot(durationMs, amplitude)
                } else {
                    VibrationEffect.createOneShot(durationMs, VibrationEffect.DEFAULT_AMPLITUDE)
                },
            )
        }
    }
}

/** A haptics object that renders nothing — previews, tests, and every device with no actuator. */
private val SilentHaptics = ConsoleHaptics(null)

/**
 * Resolve the console's haptics for whatever is driving the UI right now. Re-resolves when the
 * driving pad changes (a fresh controller may have motors where the last one had none), and honours
 * the system's own "touch feedback" switch — a user who turned haptics off meant it here too.
 */
@Composable
fun rememberConsoleHaptics(): ConsoleHaptics {
    val context = LocalContext.current
    val activity = context as? MainActivity ?: return SilentHaptics
    val padId = activity.lastPadDeviceId
    @Suppress("DEPRECATION") // still the canonical read for the user's touch-feedback switch
    val enabled = remember {
        runCatching {
            android.provider.Settings.System.getInt(
                context.contentResolver,
                android.provider.Settings.System.HAPTIC_FEEDBACK_ENABLED,
                1,
            ) != 0
        }.getOrDefault(true)
    }
    return remember(padId, enabled) {
        if (!enabled) SilentHaptics
        else ConsoleHaptics(padVibrator(padId) ?: deviceBodyVibrator(context))
    }
}

/** The vibrator of the controller with this device id, or null (no such device / no motors). */
private fun padVibrator(deviceId: Int): Vibrator? = runCatching {
    val dev = InputDevice.getDevice(deviceId) ?: return null
    val v = if (Build.VERSION.SDK_INT >= 31) {
        dev.vibratorManager.defaultVibrator
    } else {
        @Suppress("DEPRECATION")
        dev.vibrator
    }
    v?.takeIf { it.hasVibrator() }
}.getOrNull()
