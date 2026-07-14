package io.unom.punktfunk

import android.hardware.input.InputManager
import android.os.Build
import android.os.CombinedVibration
import android.os.Handler
import android.os.Looper
import android.os.VibrationEffect
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.kit.Gamepad
import kotlinx.coroutines.delay

/**
 * Connected-controllers debug view (Settings → Host → Connected controllers): everything the app
 * can see about attached input devices, plus a live input test. This exists for exactly the support
 * case where a pad "doesn't work" — adapters and BT-to-USB dongles often enumerate with a different
 * identity than the physical pad, or not as a gamepad at all, and punktfunk only forwards devices
 * Android classifies as gamepad/joystick. This screen makes that visible on the device itself.
 */
@Composable
fun ControllersScreen(gamepadSetting: Int, onBack: () -> Unit) {
    BackHandler(onBack = onBack)
    val context = LocalContext.current
    val activity = context as? MainActivity

    // Device list, re-read on every hot-plug event.
    var generation by remember { mutableIntStateOf(0) }
    val pads = remember(generation) { Gamepad.pads() }
    val others = remember(generation) {
        InputDevice.getDeviceIds()
            .toList()
            .mapNotNull { InputDevice.getDevice(it) }
            .filter { !it.isVirtual && !Gamepad.isPad(it) }
    }
    DisposableEffect(Unit) {
        val im = context.getSystemService(InputManager::class.java)
        val listener = object : InputManager.InputDeviceListener {
            override fun onInputDeviceAdded(deviceId: Int) { generation++ }
            override fun onInputDeviceRemoved(deviceId: Int) { generation++ }
            override fun onInputDeviceChanged(deviceId: Int) { generation++ }
        }
        im.registerInputDeviceListener(listener, Handler(Looper.getMainLooper()))
        onDispose { im.unregisterInputDeviceListener(listener) }
    }

    // Live input test. While `testing`, the MainActivity probes consume pad events (so they show up
    // here instead of driving focus navigation); holding B releases, since the pad can no longer
    // reach the Switch. Events are observed (not consumed) even when the test is off, so the
    // "last input" line works while browsing.
    var testing by remember { mutableStateOf(false) }
    val held = remember { mutableStateMapOf<Int, Boolean>() }
    val axes = remember { mutableStateMapOf<String, Float>() }
    var lastInput by remember { mutableStateOf<String?>(null) }
    var bHeld by remember { mutableStateOf(false) }

    DisposableEffect(Unit) {
        activity?.padKeyProbe = probe@{ event ->
            if (!Gamepad.isPad(event.device)) return@probe false
            when (event.action) {
                KeyEvent.ACTION_DOWN -> {
                    held[event.keyCode] = true
                    if (event.keyCode == KeyEvent.KEYCODE_BUTTON_B) bHeld = true
                }
                KeyEvent.ACTION_UP -> {
                    held[event.keyCode] = false
                    if (event.keyCode == KeyEvent.KEYCODE_BUTTON_B) bHeld = false
                }
            }
            lastInput = "${event.device?.name}: ${KeyEvent.keyCodeToString(event.keyCode)}"
            testing
        }
        activity?.padMotionProbe = probe@{ event ->
            if (!Gamepad.isPad(event.device)) return@probe false
            axes["LX"] = event.getAxisValue(MotionEvent.AXIS_X)
            axes["LY"] = event.getAxisValue(MotionEvent.AXIS_Y)
            axes["RX"] = event.getAxisValue(MotionEvent.AXIS_Z)
            axes["RY"] = event.getAxisValue(MotionEvent.AXIS_RZ)
            axes["LT"] = maxOf(
                event.getAxisValue(MotionEvent.AXIS_LTRIGGER),
                event.getAxisValue(MotionEvent.AXIS_BRAKE),
            )
            axes["RT"] = maxOf(
                event.getAxisValue(MotionEvent.AXIS_RTRIGGER),
                event.getAxisValue(MotionEvent.AXIS_GAS),
            )
            axes["HX"] = event.getAxisValue(MotionEvent.AXIS_HAT_X)
            axes["HY"] = event.getAxisValue(MotionEvent.AXIS_HAT_Y)
            testing
        }
        onDispose {
            activity?.padKeyProbe = null
            activity?.padMotionProbe = null
        }
    }
    // Hold-B-to-exit: with events consumed, the pad can't reach the Switch — a 1.2 s hold ends the
    // test instead (touch still works). A short tap cancels the effect before the delay fires.
    LaunchedEffect(bHeld) {
        if (bHeld && testing) {
            delay(1_200)
            testing = false
            held.clear()
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 24.dp),
        verticalArrangement = Arrangement.spacedBy(24.dp),
    ) {
        Text("Controllers", style = MaterialTheme.typography.headlineMedium)

        Group("Gamepads") {
            if (pads.isEmpty()) {
                Text(
                    "No controller detected. punktfunk can only forward devices Android " +
                        "classifies as a gamepad or joystick — a pad connected through an adapter " +
                        "or hub may show up under \"Other input devices\" below with the adapter's " +
                        "identity, or not at all.",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            // Every real controller is forwarded now (Automatic forwards them all, each on its own
            // wire pad index) — not just the first. A joystick-only device Android doesn't classify as
            // a gamepad still can't be forwarded (the host wants a gamepad), so gate the badge on it.
            pads.forEach { dev ->
                PadRow(dev, forwarded = isForwarded(dev), gamepadSetting = gamepadSetting)
            }
        }

        Group("Input test") {
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Test inputs", style = MaterialTheme.typography.bodyLarge)
                    Text(
                        if (testing) "Controller input stays on this screen — hold B to finish"
                        else "Show button presses and stick motion live",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Switch(checked = testing, onCheckedChange = { testing = it; if (!it) held.clear() })
            }
            if (testing) {
                ButtonGrid(held)
                AXIS_LABELS.forEach { label -> AxisBar(label, axes[label] ?: 0f) }
            }
            lastInput?.let {
                Text(
                    "Last input — $it",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }

        Group("Other input devices") {
            if (others.isEmpty()) {
                Text(
                    "None",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            others.forEach { dev ->
                Column {
                    Text(dev.name, style = MaterialTheme.typography.bodyMedium)
                    Text(
                        deviceDetail(dev),
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** One detected gamepad: identity, what it streams as, and a rumble test. */
@Composable
private fun PadRow(dev: InputDevice, forwarded: Boolean, gamepadSetting: Int) {
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(dev.name, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
                if (forwarded) {
                    // Android's own controller number (1-based; 0 = unassigned), shown so a multi-pad
                    // user can tell which physical pad is which. The stream's wire pad index is
                    // assigned separately (lowest-free per device) once streaming starts.
                    val number = dev.controllerNumber
                    Text(
                        if (number > 0) "forwarded · player $number" else "forwarded to host",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            Text(
                deviceDetail(dev),
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            val resolved = Gamepad.prefFor(dev)
            Text(
                if (gamepadSetting == Gamepad.PREF_AUTO) {
                    "Streams as: ${prefLabel(resolved)} (automatic)"
                } else {
                    "Streams as: ${prefLabel(gamepadSetting)} (set in Settings; " +
                        "automatic would pick ${prefLabel(resolved)})"
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            val canRumble = deviceHasVibrator(dev)
            if (canRumble) {
                OutlinedButton(onClick = { testRumble(dev) }) { Text("Test rumble") }
            } else {
                Text(
                    "No rumble motors reported — host rumble will be silent",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
    }
}

/** The forwarded buttons as chips that light up while held. */
@OptIn(ExperimentalLayoutApi::class)
@Composable
private fun ButtonGrid(held: Map<Int, Boolean>) {
    FlowRow(
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        verticalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        TEST_BUTTONS.forEach { (label, keyCode) ->
            val active = held[keyCode] == true
            Text(
                label,
                style = MaterialTheme.typography.labelMedium,
                color = if (active) MaterialTheme.colorScheme.onPrimary
                else MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier
                    .background(
                        if (active) MaterialTheme.colorScheme.primary
                        else MaterialTheme.colorScheme.surfaceVariant,
                        RoundedCornerShape(6.dp),
                    )
                    .padding(horizontal = 10.dp, vertical = 6.dp),
            )
        }
    }
}

/** A labelled live axis bar; sticks/HAT are −1..1 (centre = half), triggers 0..1. */
@Composable
private fun AxisBar(label: String, value: Float) {
    val progress = if (label == "LT" || label == "RT") value else (value + 1f) / 2f
    Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Text(label, style = MaterialTheme.typography.labelMedium, modifier = Modifier.width(32.dp))
        LinearProgressIndicator(
            progress = { progress.coerceIn(0f, 1f) },
            modifier = Modifier.weight(1f),
        )
        Text(
            "%+.2f".format(value),
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(start = 8.dp),
        )
    }
}

/** A titled section — same look as the Settings groups. */
@Composable
private fun Group(title: String, content: @Composable ColumnScope.() -> Unit) {
    Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
        Text(
            title,
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.padding(start = 4.dp),
        )
        Column(verticalArrangement = Arrangement.spacedBy(12.dp), content = content)
    }
}

/**
 * Whether this device is actually forwarded to the host — the same rule the stream's [GamepadRouter]
 * applies: a real, non-virtual controller whose source classes include GAMEPAD. A joystick-only node
 * (e.g. a DualSense motion-sensor sibling, or an adapter that enumerates as bare joystick) shows in
 * the list but isn't forwarded.
 */
private fun isForwarded(dev: InputDevice): Boolean =
    !dev.isVirtual && dev.sources and InputDevice.SOURCE_GAMEPAD == InputDevice.SOURCE_GAMEPAD

/** Whether the controller reports a rumble motor — via VibratorManager (API 31+) or the legacy Vibrator. */
private fun deviceHasVibrator(dev: InputDevice): Boolean =
    if (Build.VERSION.SDK_INT >= 31) {
        dev.vibratorManager.vibratorIds.isNotEmpty()
    } else {
        @Suppress("DEPRECATION")
        dev.vibrator.hasVibrator()
    }

private fun testRumble(dev: InputDevice) {
    runCatching {
        if (Build.VERSION.SDK_INT >= 31) {
            val vm = dev.vibratorManager
            if (vm.vibratorIds.isEmpty()) return
            vm.vibrate(CombinedVibration.createParallel(VibrationEffect.createOneShot(300, 200)))
        } else {
            @Suppress("DEPRECATION")
            val v = dev.vibrator
            if (!v.hasVibrator()) return
            v.vibrate(VibrationEffect.createOneShot(300, 200))
        }
    }
}

/** Identity line: VID:PID + the source classes Android assigned. */
private fun deviceDetail(dev: InputDevice): String =
    "%04X:%04X · %s".format(dev.vendorId, dev.productId, sourcesLabel(dev.sources))

private fun sourcesLabel(sources: Int): String {
    fun has(flag: Int) = sources and flag == flag
    val names = buildList {
        if (has(InputDevice.SOURCE_GAMEPAD)) add("gamepad")
        if (has(InputDevice.SOURCE_JOYSTICK)) add("joystick")
        if (has(InputDevice.SOURCE_DPAD)) add("dpad")
        if (has(InputDevice.SOURCE_KEYBOARD)) add("keyboard")
        if (has(InputDevice.SOURCE_MOUSE)) add("mouse")
        if (has(InputDevice.SOURCE_TOUCHSCREEN)) add("touchscreen")
        if (has(InputDevice.SOURCE_TOUCHPAD)) add("touchpad")
        if (has(InputDevice.SOURCE_STYLUS)) add("stylus")
        if (has(InputDevice.SOURCE_ROTARY_ENCODER)) add("rotary")
    }
    return if (names.isEmpty()) "sources 0x%08X".format(sources) else names.joinToString(" · ")
}

/** [Gamepad] PREF_* wire byte → user-facing label (mirrors GAMEPAD_OPTIONS, plus the Steam types). */
private fun prefLabel(pref: Int): String = when (pref) {
    Gamepad.PREF_XBOX360 -> "Xbox 360"
    Gamepad.PREF_DUALSENSE -> "DualSense"
    Gamepad.PREF_XBOXONE -> "Xbox One"
    Gamepad.PREF_DUALSHOCK4 -> "DualShock 4"
    Gamepad.PREF_STEAMCONTROLLER -> "Steam Controller"
    Gamepad.PREF_STEAMDECK -> "Steam Deck"
    Gamepad.PREF_DUALSENSEEDGE -> "DualSense Edge"
    Gamepad.PREF_SWITCHPRO -> "Switch Pro"
    else -> "Automatic"
}

/** Buttons shown in the test grid (label → Android keycode). */
private val TEST_BUTTONS = listOf(
    "A" to KeyEvent.KEYCODE_BUTTON_A,
    "B" to KeyEvent.KEYCODE_BUTTON_B,
    "X" to KeyEvent.KEYCODE_BUTTON_X,
    "Y" to KeyEvent.KEYCODE_BUTTON_Y,
    "LB" to KeyEvent.KEYCODE_BUTTON_L1,
    "RB" to KeyEvent.KEYCODE_BUTTON_R1,
    "L2" to KeyEvent.KEYCODE_BUTTON_L2,
    "R2" to KeyEvent.KEYCODE_BUTTON_R2,
    "LS" to KeyEvent.KEYCODE_BUTTON_THUMBL,
    "RS" to KeyEvent.KEYCODE_BUTTON_THUMBR,
    "Select" to KeyEvent.KEYCODE_BUTTON_SELECT,
    "Start" to KeyEvent.KEYCODE_BUTTON_START,
    "Guide" to KeyEvent.KEYCODE_BUTTON_MODE,
    "↑" to KeyEvent.KEYCODE_DPAD_UP,
    "↓" to KeyEvent.KEYCODE_DPAD_DOWN,
    "←" to KeyEvent.KEYCODE_DPAD_LEFT,
    "→" to KeyEvent.KEYCODE_DPAD_RIGHT,
)

/** Axis bars shown in the test view, in display order. */
private val AXIS_LABELS = listOf("LX", "LY", "RX", "RY", "LT", "RT", "HX", "HY")
