package io.unom.punktfunk

import android.Manifest
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadFeedback
import io.unom.punktfunk.kit.NativeBridge
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.delay

@Composable
fun StreamScreen(handle: Long, micEnabled: Boolean, onDisconnect: () -> Unit) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val window = activity?.window
    val controller = remember(window) {
        window?.let { WindowCompat.getInsetsController(it, it.decorView) }
    }

    // Start mic only if the user enabled it AND granted RECORD_AUDIO (else the AAudio input fails).
    val micWanted = micEnabled && ContextCompat.checkSelfPermission(
        context,
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    // Live decode stats for the HUD. `showStats` gates the whole pipeline: the native per-frame
    // sampling (nativeSetVideoStatsEnabled — hidden HUD costs one atomic load per frame) AND the
    // 1 s poll loop, which only runs while the overlay is visible. Enabling resets the native
    // window, so re-showing never renders stale data. A 3-finger tap toggles it live; the default
    // comes from Settings.
    val initialSettings = remember { SettingsStore(context).load() }
    var stats by remember { mutableStateOf<DoubleArray?>(null) }
    var showStats by remember { mutableStateOf(initialSettings.statsHudEnabled) }
    // Touch model is fixed per session (re-keys the gesture handler below if it ever changes).
    val trackpad = initialSettings.trackpadMode
    LaunchedEffect(handle, showStats) {
        NativeBridge.nativeSetVideoStatsEnabled(handle, showStats)
        if (showStats) {
            while (true) {
                delay(1000)
                stats = NativeBridge.nativeVideoStats(handle)
            }
        } else {
            stats = null // drop the last snapshot so a re-show never flashes stale numbers
        }
    }

    // One-shot teardown guard. Both the SurfaceView callback and DisposableEffect tear down on the
    // way out, but `nativeClose` frees the handle — so once it's closed, NO path may touch the handle
    // again (use-after-free → SIGSEGV: the consistent back-while-streaming crash). Both run on the
    // main thread, so a plain flag is race-free; AtomicBoolean just makes the intent explicit.
    val closed = remember { AtomicBoolean(false) }

    DisposableEffect(handle) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        controller?.let {
            it.systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
            it.hide(WindowInsetsCompat.Type.systemBars())
        }
        // Lock to landscape while streaming — the host streams a landscape desktop, so pin the device
        // there (either landscape direction is fine) and stop it rotating to portrait mid-session. The
        // activity declares configChanges=orientation, so this re-lays out the surface in place without
        // recreating the activity (no stream restart). On TV (fixed landscape) it's a harmless no-op.
        // The prior request is captured and restored on the way out.
        val priorOrientation = activity?.requestedOrientation
        activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        activity?.streamHandle = handle // route hardware keys to this session
        activity?.axisMapper = Gamepad.AxisMapper(handle) // route joystick axes
        // Host→client feedback (rumble + DualSense lightbar/LEDs); poll threads stopped before close.
        val feedback = GamepadFeedback(handle).also { it.start() }
        onDispose {
            closed.set(true) // from here the handle gets freed; surfaceDestroyed must not touch it
            feedback.stop() // stop + join the poll threads BEFORE nativeClose frees the handle
            activity?.axisMapper?.reset() // release-all so nothing sticks on the host
            activity?.axisMapper = null
            activity?.streamHandle = 0L
            controller?.show(WindowInsetsCompat.Type.systemBars())
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            // Release the landscape lock so the rest of the app follows the device/system again.
            activity?.requestedOrientation =
                priorOrientation ?: ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
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
                            // Surface gone (backgrounding, or on the way out). Stop the threads that
                            // render to it — but only while the session is still open. Once
                            // DisposableEffect has closed it, the handle is freed; dereferencing it
                            // here is the use-after-free that crashed on back-navigation.
                            if (!closed.get()) {
                                NativeBridge.nativeStopMic(handle)
                                NativeBridge.nativeStopAudio(handle)
                                NativeBridge.nativeStopVideo(handle)
                            }
                        }
                    })
                }
            },
        )
        // Live stats HUD (FPS / throughput / capture→client latency), drawn over the video but
        // BEFORE the transparent gesture layer below, so it shows through and never eats touches.
        if (showStats) {
            stats?.let { StatsOverlay(it, Modifier.align(Alignment.TopStart).padding(12.dp)) }
        }
        // Touch → mouse (trackpad vs. direct pointing + the shared gesture vocabulary — see
        // streamTouchInput in TouchInput.kt).
        Box(
            Modifier.fillMaxSize().pointerInput(handle, trackpad) {
                streamTouchInput(handle, trackpad, onToggleStats = { showStats = !showStats })
            },
        )
    }
}
