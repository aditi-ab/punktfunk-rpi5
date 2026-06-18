package io.unom.punktfunk

import android.Manifest
import android.content.pm.PackageManager
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadFeedback
import io.unom.punktfunk.kit.NativeBridge
import kotlin.math.abs

@Composable
fun StreamScreen(handle: Long, micEnabled: Boolean, onDisconnect: () -> Unit) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val window = activity?.window
    // Start mic only if the user enabled it AND granted RECORD_AUDIO (else the AAudio input fails).
    val micWanted = micEnabled && ContextCompat.checkSelfPermission(
        context,
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    DisposableEffect(handle) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        activity?.streamHandle = handle // route hardware keys to this session
        activity?.axisMapper = Gamepad.AxisMapper(handle) // route joystick axes
        // Host→client feedback (rumble + DualSense lightbar/LEDs); poll threads stopped before close.
        val feedback = GamepadFeedback(handle).also { it.start() }
        onDispose {
            feedback.stop() // stop + join the poll threads BEFORE nativeClose frees the handle
            activity?.axisMapper?.reset() // release-all so nothing sticks on the host
            activity?.axisMapper = null
            activity?.streamHandle = 0L
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            // Leaving the stream: stop the mic + audio + decode threads and tear down the session.
            NativeBridge.nativeStopMic(handle)
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
                            if (micWanted) NativeBridge.nativeStartMic(handle)
                        }

                        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            NativeBridge.nativeStopMic(handle)
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
