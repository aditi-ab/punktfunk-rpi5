package io.unom.punktfunk

import android.Manifest
import android.content.pm.PackageManager
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
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
import kotlin.math.abs
import kotlin.math.roundToInt

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

    // Live decode stats for the HUD. Poll once a second for the whole stream (cheap, and each call
    // drains+resets the native window so it never grows unbounded even while the overlay is hidden);
    // `showStats` only gates rendering. A 3-finger tap toggles it live; the default comes from Settings.
    var stats by remember { mutableStateOf<DoubleArray?>(null) }
    var showStats by remember { mutableStateOf(SettingsStore(context).load().statsHudEnabled) }
    LaunchedEffect(handle) {
        while (true) {
            delay(1000)
            stats = NativeBridge.nativeVideoStats(handle)
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
        // Touch virtual-trackpad overlay: 1-finger drag → relative mouse move; tap → left click;
        // 2-finger drag → scroll; 3-finger tap → toggle the stats HUD. (Physical-mouse pointer
        // capture comes in a later increment.)
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
                    } else if (!moved && maxFingers >= 3) {
                        showStats = !showStats // quick in-stream HUD toggle
                    }
                }
            },
        )
    }
}

/**
 * The live stats overlay — mirrors the Apple client's HUD. Reads the 10-double layout from
 * [NativeBridge.nativeVideoStats]:
 * `[fps, mbps, latP50Ms, latP95Ms, latValid, skew, w, h, hz, dropped]`.
 */
@Composable
private fun StatsOverlay(s: DoubleArray, modifier: Modifier = Modifier) {
    if (s.size < 10) return
    val w = s[6].toInt()
    val h = s[7].toInt()
    val hz = s[8].toInt()
    val latValid = s[4] != 0.0
    val skew = s[5] != 0.0
    val dropped = s[9].toLong()
    Column(
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.45f), RoundedCornerShape(6.dp))
            .padding(horizontal = 8.dp, vertical = 4.dp),
    ) {
        Text(
            "$w×$h@$hz   ${s[0].roundToInt()} fps   ${"%.1f".format(s[1])} Mb/s",
            color = Color.White,
            fontFamily = FontFamily.Monospace,
            fontSize = 12.sp,
        )
        if (latValid) {
            val tag = if (skew) "" else " (same-host)"
            Text(
                "capture→client ${"%.1f".format(s[2])}/${"%.1f".format(s[3])} ms p50/p95$tag",
                color = Color.White,
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
            )
        }
        if (dropped > 0) {
            Text(
                "dropped $dropped",
                color = Color(0xFFFFB0B0),
                fontFamily = FontFamily.Monospace,
                fontSize = 12.sp,
            )
        }
    }
}
