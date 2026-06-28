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
import kotlin.math.hypot
import kotlin.math.roundToInt

// Touch-gesture tuning (px / ms). TAP_SLOP: movement under this still counts as a tap, not a drag.
// TAP_DRAG_MS: a new touch within this long after a tap starts a left-button drag. SCROLL_DIV: px of
// two-finger pan per wheel notch (smaller = faster scroll).
private const val TAP_SLOP = 12f
private const val TAP_DRAG_MS = 250L
private const val SCROLL_DIV = 4f

// Trackpad-mode pointer ballistics (relative one-finger motion). POINTER_SENS: base finger-px →
// host-px gain (~1:1, never twitchy). The rest is mild acceleration so a flick crosses the screen
// while a slow drag stays precise: above ACCEL_SPEED_FLOOR px/ms the gain ramps by ACCEL_GAIN per
// px/ms, capped at ACCEL_MAX (so a fast swipe can't fling the cursor uncontrollably).
private const val POINTER_SENS = 1.3f
private const val ACCEL_GAIN = 0.6f
private const val ACCEL_SPEED_FLOOR = 0.3f
private const val ACCEL_MAX = 3.0f

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
    val initialSettings = remember { SettingsStore(context).load() }
    var stats by remember { mutableStateOf<DoubleArray?>(null) }
    var showStats by remember { mutableStateOf(initialSettings.statsHudEnabled) }
    // Touch model is fixed per session (re-keys the gesture handler below if it ever changes).
    val trackpad = initialSettings.trackpadMode
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
        // Touch → mouse. Two models, chosen by the Trackpad-mode setting:
        //  • trackpad (default): the cursor STAYS where it is on touch-down and moves by the finger's
        //    relative delta (MouseMove) with mild pointer acceleration — swipe to nudge, lift and
        //    re-swipe to walk it across, tap to click where it is. This is what makes the cursor
        //    reachable on a small screen.
        //  • direct (opt-out): the cursor jumps to the finger and follows it (MouseMoveAbs,
        //    host-normalized against the overlay size), the old "direct pointing" behaviour.
        // Both share the same gesture vocabulary: tap = left click; two-finger tap = right click;
        // two-finger drag = scroll; tap-then-press-and-drag = left-drag (text selection / moving
        // windows); three-finger tap = toggle the stats HUD.
        Box(
            Modifier.fillMaxSize().pointerInput(handle, trackpad) {
                var lastTapUp = 0L
                var lastTapX = 0f
                var lastTapY = 0f
                fun moveAbs(x: Float, y: Float) {
                    val sw = size.width
                    val sh = size.height
                    if (sw <= 0 || sh <= 0) return
                    NativeBridge.nativeSendPointerAbs(
                        handle,
                        x.coerceIn(0f, (sw - 1).toFloat()).roundToInt(),
                        y.coerceIn(0f, (sh - 1).toFloat()).roundToInt(),
                        sw,
                        sh,
                    )
                }
                awaitEachGesture {
                    val down = awaitFirstDown(requireUnconsumed = false)
                    val startX = down.position.x
                    val startY = down.position.y
                    // A touch landing just after a quick tap nearby = tap-and-drag: hold the left
                    // button for this whole gesture (laptop-trackpad convention).
                    val isDrag = down.uptimeMillis - lastTapUp < TAP_DRAG_MS &&
                        abs(startX - lastTapX) < TAP_SLOP && abs(startY - lastTapY) < TAP_SLOP
                    lastTapUp = 0L // consume the arming either way
                    // Direct mode jumps the cursor to the finger; trackpad mode leaves it put (the
                    // whole point — you nudge it with swipes instead).
                    if (!trackpad) moveAbs(startX, startY)
                    if (isDrag) NativeBridge.nativeSendPointerButton(handle, 1, true)

                    var moved = false
                    var maxFingers = 1
                    var scrolling = false
                    var prevCx = startX
                    var prevCy = startY
                    var upTime = down.uptimeMillis
                    // Trackpad relative-motion state: the tracked finger, its last position/time, and
                    // the sub-pixel remainder so a slow drag isn't lost to Int truncation.
                    var trackId = down.id
                    var prevX = startX
                    var prevY = startY
                    var prevT = down.uptimeMillis
                    var accX = 0f
                    var accY = 0f

                    while (true) {
                        val ev = awaitPointerEvent()
                        val pressed = ev.changes.filter { it.pressed }
                        if (pressed.isEmpty()) {
                            upTime = ev.changes.firstOrNull()?.uptimeMillis ?: upTime
                            break
                        }
                        if (pressed.size > maxFingers) maxFingers = pressed.size

                        if (pressed.size >= 2) {
                            // Two fingers → scroll by the centroid delta; never move the cursor.
                            val cx = (pressed.sumOf { it.position.x.toDouble() } / pressed.size).toFloat()
                            val cy = (pressed.sumOf { it.position.y.toDouble() } / pressed.size).toFloat()
                            if (!scrolling) {
                                scrolling = true
                                prevCx = cx
                                prevCy = cy
                            }
                            val sy = ((prevCy - cy) / SCROLL_DIV).toInt() // finger up → wheel up
                            val sx = ((cx - prevCx) / SCROLL_DIV).toInt()
                            if (sy != 0) {
                                NativeBridge.nativeSendScroll(handle, 0, sy * 120)
                                prevCy = cy
                                moved = true
                            }
                            if (sx != 0) {
                                NativeBridge.nativeSendScroll(handle, 1, sx * 120)
                                prevCx = cx
                                moved = true
                            }
                        } else if (!scrolling) {
                            // One finger (skipped once a gesture turned into a scroll, so dropping
                            // back to one finger doesn't jerk the cursor).
                            val p = pressed.firstOrNull { it.id == down.id } ?: pressed.first()
                            if (abs(p.position.x - startX) > TAP_SLOP ||
                                abs(p.position.y - startY) > TAP_SLOP
                            ) {
                                moved = true
                            }
                            if (trackpad) {
                                // Relative: move by the finger delta × (sensitivity × acceleration),
                                // carrying the sub-pixel remainder. Re-anchor (zero delta this frame)
                                // if the tracked finger changed, so lifting one of several fingers
                                // never jumps the cursor.
                                if (p.id != trackId) {
                                    trackId = p.id
                                    prevX = p.position.x
                                    prevY = p.position.y
                                    prevT = p.uptimeMillis
                                }
                                val dx = p.position.x - prevX
                                val dy = p.position.y - prevY
                                val dt = (p.uptimeMillis - prevT).coerceAtLeast(1L)
                                prevX = p.position.x
                                prevY = p.position.y
                                prevT = p.uptimeMillis
                                val speed = hypot(dx, dy) / dt // finger px per ms
                                val accel = (1f + ACCEL_GAIN * (speed - ACCEL_SPEED_FLOOR).coerceAtLeast(0f))
                                    .coerceAtMost(ACCEL_MAX)
                                accX += dx * POINTER_SENS * accel
                                accY += dy * POINTER_SENS * accel
                                val outX = accX.toInt() // truncates toward zero → remainder kept w/ sign
                                val outY = accY.toInt()
                                if (outX != 0 || outY != 0) {
                                    NativeBridge.nativeSendPointerMove(handle, outX, outY)
                                    accX -= outX
                                    accY -= outY
                                }
                            } else {
                                moveAbs(p.position.x, p.position.y) // direct: cursor follows the finger
                            }
                        }
                        ev.changes.forEach { it.consume() }
                    }

                    if (isDrag) {
                        NativeBridge.nativeSendPointerButton(handle, 1, false) // end the drag
                    } else if (!moved) {
                        when {
                            maxFingers >= 3 -> showStats = !showStats // in-stream HUD toggle
                            maxFingers == 2 -> { // two-finger tap → right click
                                NativeBridge.nativeSendPointerButton(handle, 3, true)
                                NativeBridge.nativeSendPointerButton(handle, 3, false)
                            }
                            else -> { // tap → left click (at the cursor's current spot), arm tap-drag
                                NativeBridge.nativeSendPointerButton(handle, 1, true)
                                NativeBridge.nativeSendPointerButton(handle, 1, false)
                                lastTapUp = upTime
                                lastTapX = startX
                                lastTapY = startY
                            }
                        }
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
internal fun StatsOverlay(s: DoubleArray, modifier: Modifier = Modifier) {
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
