package io.unom.punktfunk

import android.os.Bundle
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.Keymap
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.abs
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

class MainActivity : ComponentActivity() {
    /**
     * The active stream session handle (0 = not streaming). Set by [StreamScreen] while it's shown.
     * `dispatchKeyEvent` is the earliest, most reliable key hook — above Compose's focus system —
     * so hardware keys are forwarded to the host regardless of which view holds focus.
     */
    var streamHandle: Long = 0L

    /** Joystick-axis state mapper for the active session (built/reset by StreamScreen). */
    var axisMapper: Gamepad.AxisMapper? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent {
            MaterialTheme(colorScheme = darkColorScheme()) {
                Surface(modifier = Modifier.fillMaxSize()) { App() }
            }
        }
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        val handle = streamHandle
        if (handle != 0L) {
            // Gamepad buttons (incl. DPAD only when truly from a gamepad — else KEYCODE_DPAD_* are
            // keyboard arrows and belong to the VK path below).
            if (event.isFromSource(InputDevice.SOURCE_GAMEPAD)) {
                val bit = Gamepad.buttonBit(event.keyCode)
                if (bit != 0) {
                    when (event.action) {
                        // repeatCount guard: don't re-send a held button as auto-repeat.
                        KeyEvent.ACTION_DOWN ->
                            if (event.repeatCount == 0) NativeBridge.nativeSendGamepadButton(handle, bit, true)
                        KeyEvent.ACTION_UP -> NativeBridge.nativeSendGamepadButton(handle, bit, false)
                    }
                    return true // consumed
                }
            }
            when (event.keyCode) {
                // Leave these to the system even while streaming.
                KeyEvent.KEYCODE_BACK, // → BackHandler leaves the stream
                KeyEvent.KEYCODE_VOLUME_UP,
                KeyEvent.KEYCODE_VOLUME_DOWN,
                KeyEvent.KEYCODE_VOLUME_MUTE,
                KeyEvent.KEYCODE_POWER -> {}
                else -> {
                    val down = when (event.action) {
                        KeyEvent.ACTION_DOWN -> true
                        KeyEvent.ACTION_UP -> false
                        else -> return super.dispatchKeyEvent(event)
                    }
                    val vk = Keymap.toVk(event.keyCode)
                    if (vk != 0) {
                        NativeBridge.nativeSendKey(handle, vk, down, 0)
                        return true // consumed — don't let the system also act on it
                    }
                }
            }
        }
        return super.dispatchKeyEvent(event)
    }

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (streamHandle != 0L && axisMapper?.onMotion(event) == true) return true
        return super.dispatchGenericMotionEvent(event)
    }
}

/** Scaffold mode requested from the host (WxH@Hz). TODO: derive from the display. */
private val REQUEST_MODE = Triple(1280, 720, 60)

private sealed interface Screen {
    data object Connect : Screen
    data class Stream(val handle: Long) : Screen
}

@Composable
private fun App() {
    var screen by remember { mutableStateOf<Screen>(Screen.Connect) }
    when (val s = screen) {
        Screen.Connect -> ConnectScreen(onConnected = { handle -> screen = Screen.Stream(handle) })
        is Screen.Stream -> StreamScreen(s.handle, onDisconnect = { screen = Screen.Connect })
    }
}

@Composable
private fun ConnectScreen(onConnected: (Long) -> Unit) {
    val scope = rememberCoroutineScope()
    var host by remember { mutableStateOf("") }
    var port by remember { mutableStateOf("9777") }
    var connecting by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }
    val abi = remember { runCatching { NativeBridge.abiVersion() }.getOrDefault(-1) }
    val (w, h, hz) = REQUEST_MODE

    Column(
        modifier = Modifier.fillMaxSize().padding(24.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Text("punktfunk", style = MaterialTheme.typography.headlineMedium)
        Text("Android client", style = MaterialTheme.typography.bodyMedium)
        Spacer(Modifier.height(24.dp))
        OutlinedTextField(
            value = host,
            onValueChange = { host = it },
            label = { Text("Host") },
            singleLine = true,
        )
        Spacer(Modifier.height(8.dp))
        OutlinedTextField(
            value = port,
            onValueChange = { v -> port = v.filter { it.isDigit() }.take(5) },
            label = { Text("Port") },
            singleLine = true,
            keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        )
        Spacer(Modifier.height(16.dp))
        Button(
            enabled = !connecting && host.isNotBlank() && port.isNotBlank(),
            onClick = {
                connecting = true
                status = "Connecting to $host:$port…"
                scope.launch {
                    val handle = withContext(Dispatchers.IO) {
                        NativeBridge.nativeConnect(host.trim(), port.toInt(), w, h, hz)
                    }
                    connecting = false
                    if (handle != 0L) {
                        onConnected(handle)
                    } else {
                        status = "Connection failed — check host/port and logcat"
                    }
                }
            },
        ) { Text(if (connecting) "Connecting…" else "Connect  ($w×$h@$hz)") }
        status?.let {
            Spacer(Modifier.height(12.dp))
            Text(it, style = MaterialTheme.typography.bodySmall)
        }
        Spacer(Modifier.height(24.dp))
        Text("core ABI v$abi", style = MaterialTheme.typography.labelSmall)
    }
}

@Composable
private fun StreamScreen(handle: Long, onDisconnect: () -> Unit) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val window = activity?.window

    DisposableEffect(handle) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        activity?.streamHandle = handle // route hardware keys to this session
        activity?.axisMapper = Gamepad.AxisMapper(handle) // route joystick axes
        onDispose {
            activity?.axisMapper?.reset() // release-all so nothing sticks on the host
            activity?.axisMapper = null
            activity?.streamHandle = 0L
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            // Leaving the stream: stop the audio + decode threads and tear down the session.
            NativeBridge.nativeStopAudio(handle)
            NativeBridge.nativeStopVideo(handle)
            NativeBridge.nativeClose(handle)
        }
    }

    BackHandler { onDisconnect() }

    Box(modifier = Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                SurfaceView(ctx).apply {
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(holder: SurfaceHolder) {
                            NativeBridge.nativeStartVideo(handle, holder.surface)
                            NativeBridge.nativeStartAudio(handle)
                        }

                        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            NativeBridge.nativeStopAudio(handle)
                            NativeBridge.nativeStopVideo(handle)
                        }
                    })
                }
            },
        )
        // Touch virtual-trackpad overlay: 1-finger drag → relative mouse move; tap → left click;
        // 2-finger drag → scroll. (Physical-mouse pointer capture comes in a later increment.)
        Box(
            Modifier.fillMaxSize().pointerInput(handle) {
                awaitEachGesture {
                    val first = awaitFirstDown(requireUnconsumed = false)
                    var moved = false
                    var maxFingers = 1
                    while (true) {
                        val ev = awaitPointerEvent()
                        val fingers = ev.changes.count { it.pressed }
                        if (fingers == 0) break
                        if (fingers > maxFingers) maxFingers = fingers
                        val primary = ev.changes.firstOrNull { it.id == first.id } ?: ev.changes.first()
                        val d = primary.positionChange()
                        if (abs(d.x) > 0.5f || abs(d.y) > 0.5f) {
                            moved = true
                            if (fingers >= 2) {
                                // screen +y down → wire +up, so negate y. Coarse divisor; tune live.
                                val sy = (-d.y / 4f).toInt()
                                val sx = (d.x / 4f).toInt()
                                if (sy != 0) NativeBridge.nativeSendScroll(handle, 0, sy * 120)
                                if (sx != 0) NativeBridge.nativeSendScroll(handle, 1, sx * 120)
                            } else {
                                NativeBridge.nativeSendPointerMove(handle, d.x.toInt(), d.y.toInt())
                            }
                        }
                        ev.changes.forEach { it.consume() }
                    }
                    if (!moved && maxFingers == 1) {
                        NativeBridge.nativeSendPointerButton(handle, 1, true)
                        NativeBridge.nativeSendPointerButton(handle, 1, false)
                    }
                }
            },
        )
    }
}
