package io.unom.punktfunk

import android.os.Bundle
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.ui.Modifier
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.Keymap
import io.unom.punktfunk.kit.NativeBridge

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
        // Dark, transparent system bars regardless of the system theme — our UI is always dark, so
        // the status/nav bars blend with our surface and get light icons. (The no-arg edge-to-edge
        // picks the *system* light/dark, which left a black status bar over our dark background.)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
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
