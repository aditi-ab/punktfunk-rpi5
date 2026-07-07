package io.unom.punktfunk

import android.Manifest
import android.content.Context
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import android.widget.Toast
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
import io.unom.punktfunk.kit.VideoDecoders
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
    var decoderLabel by remember { mutableStateOf("") }
    var showStats by remember { mutableStateOf(initialSettings.statsHudEnabled) }
    // Touch model is fixed per session (re-keys the gesture handler below if it ever changes).
    val touchMode = initialSettings.touchMode
    // "Low-latency mode (experimental)" master toggle, resolved once for the session. Off (the
    // default) runs the original decode pipeline; on enables the aggressive stack — decoder
    // ranking + vendor keys + async loop (native side), HDMI ALLM below, game-tagged audio, and
    // DSCP marking (applied earlier, at connect).
    val lowLatencyMode = initialSettings.lowLatencyMode
    // TV form factor (leanback): the decoder actively switches the HDMI output mode to the stream
    // refresh; a phone/tablet gets the softer seamless frame-rate hint instead.
    val isTv = remember { context.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK) }
    LaunchedEffect(handle, showStats) {
        NativeBridge.nativeSetVideoStatsEnabled(handle, showStats)
        if (showStats) {
            while (true) {
                delay(1000)
                stats = NativeBridge.nativeVideoStats(handle)
                // The decoder is fixed for the session; fetch its label once it's resolved.
                if (decoderLabel.isEmpty()) decoderLabel = NativeBridge.nativeVideoDecoderLabel(handle)
            }
        } else {
            stats = null // drop the last snapshot so a re-show never flashes stale numbers
        }
    }

    // Host-gone watchdog. When the host suspends/sleeps (or crashes, or drops off the network) it
    // stops answering the QUIC keep-alive and the connection idle-times out (~8 s) — no more frames
    // arrive and the decoder would otherwise sit frozen on its last decoded frame until the user
    // manually backed out. Poll the native session-liveness flag (one atomic load, independent of the
    // stats HUD) and, the moment the session is dead, drop back to the menu so the user can
    // Wake-on-LAN the host instead of being stranded on a frozen picture. Mirrors the Apple client's
    // onSessionEnd → sessionEnded() → disconnect(). The 1 s cadence + the ~8 s idle timeout is a
    // deliberately generous window: the keep-alive holds a merely-quiet connection (a static desktop)
    // open, so this fires only on a genuinely dead peer, never a false positive. Keyed on `handle`, so
    // it stops the moment we navigate away (the handle is only freed later, in onDispose).
    LaunchedEffect(handle) {
        while (true) {
            delay(1000)
            if (NativeBridge.nativeSessionEnded(handle)) {
                Toast.makeText(
                    context,
                    "Connection lost — the host may be asleep. Wake it to reconnect.",
                    Toast.LENGTH_LONG,
                ).show()
                onDisconnect()
                return@LaunchedEffect
            }
        }
    }

    // One-shot teardown guard. Both the SurfaceView callback and DisposableEffect tear down on the
    // way out, but `nativeClose` frees the handle — so once it's closed, NO path may touch the handle
    // again (use-after-free → SIGSEGV: the consistent back-while-streaming crash). Both run on the
    // main thread, so a plain flag is race-free; AtomicBoolean just makes the intent explicit.
    val closed = remember { AtomicBoolean(false) }

    // Wi-Fi locks held for the stream's duration — BOTH of them, unconditionally (Moonlight does
    // the same). Without an effective lock, Wi-Fi power save batches downlink delivery into
    // beacon-interval clumps: hundreds of ms of latency mush, sawtoothing bitrate, and periodic
    // whole-frame loss when the AP's power-save buffer overflows (all observed live on a phone).
    //  - FULL_LOW_LATENCY (API 29+) is the only lock that actually disables power save on modern
    //    Android; it needs the app foreground + screen on, which a stream always is.
    //  - FULL_HIGH_PERF covers older releases — it is deprecated AND a documented no-op on recent
    //    Android, which is exactly why it can't be the only lock (a lesson learned: holding just
    //    HIGH_PERF left power save fully active on Android 13+).
    // acquire() ENFORCES the WAKE_LOCK permission (manifest) — and a failed acquire MUST be loud:
    // a silent runCatching hid the missing permission for weeks (dumpsys wifi showed
    // low_latency_active_time_ms=0 across every "locked" stream). Non-reference-counted: one
    // explicit acquire/release each.
    val wifiLocks = remember(handle) {
        val wm = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
            ?: return@remember emptyList<WifiManager.WifiLock>()
        buildList {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                wm.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "punktfunk:stream-ll")
                    ?.let(::add)
            }
            @Suppress("DEPRECATION")
            wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "punktfunk:stream-hp")
                ?.let(::add)
        }.onEach { it.setReferenceCounted(false) }
    }

    DisposableEffect(handle) {
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        wifiLocks.forEach { lock ->
            runCatching { lock.acquire() }.onFailure { e ->
                Log.w("punktfunk", "WifiLock acquire failed — power save stays ON: $lock", e)
            }
        }
        // HDMI Auto Low-Latency Mode: ask the display to drop its post-processing (game mode) —
        // the biggest panel-side latency win on the TV boxes. No-op where ALLM isn't supported. API
        // 30+. Part of the experimental low-latency stack.
        if (lowLatencyMode && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.setPreferMinimalPostProcessing(true)
        }
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
        // Select+Start+L1+R1 chord leaves the stream — a deliberate quit (signal it so the host skips
        // the keep-alive linger), unlike a host-ended / backgrounded drop.
        activity?.requestStreamExit = { NativeBridge.nativeDisconnectQuit(handle); onDisconnect() }
        activity?.setConsoleHighRefreshRate(false) // let the decoder's setFrameRate pick the panel rate
        // Host→client feedback (rumble + DualSense lightbar/LEDs); poll threads stopped before close.
        val feedback = GamepadFeedback(handle).also { it.start() }
        onDispose {
            closed.set(true) // from here the handle gets freed; surfaceDestroyed must not touch it
            feedback.stop() // stop + join the poll threads BEFORE nativeClose frees the handle
            activity?.axisMapper?.reset() // release-all so nothing sticks on the host
            activity?.axisMapper = null
            activity?.streamHandle = 0L
            activity?.requestStreamExit = null
            activity?.setConsoleHighRefreshRate(true) // back to the console UI's max refresh
            controller?.show(WindowInsetsCompat.Type.systemBars())
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            if (lowLatencyMode && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                window?.setPreferMinimalPostProcessing(false)
            }
            wifiLocks.forEach { runCatching { if (it.isHeld) it.release() } }
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

    // Back gesture = a deliberate exit → signal the quit so the host tears down now (no linger).
    BackHandler { NativeBridge.nativeDisconnectQuit(handle); onDisconnect() }

    Box(modifier = Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { ctx ->
                SurfaceView(ctx).apply {
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(holder: SurfaceHolder) {
                            // Low-latency mode: rank MediaCodecList decoders for the negotiated
                            // MIME (framework-only API) and hand the chosen one to Rust, which
                            // creates it by name and applies the per-SoC vendor low-latency keys.
                            // Off ⇒ no ranking: the platform resolves its default decoder for the
                            // MIME, exactly as before the overhaul.
                            val mime = NativeBridge.nativeVideoMime(handle)
                            val choice = if (lowLatencyMode) VideoDecoders.pickDecoder(mime) else null
                            NativeBridge.nativeStartVideo(
                                handle,
                                holder.surface,
                                choice?.name ?: "",
                                lowLatencyMode,
                                choice?.lowLatencyFeature ?: false,
                                isTv,
                            )
                            NativeBridge.nativeStartAudio(handle, lowLatencyMode)
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
            stats?.let { StatsOverlay(it, decoderLabel, Modifier.align(Alignment.TopStart).padding(12.dp)) }
        }
        // Touch input per the Settings model: trackpad/direct-pointer mouse (the shared gesture
        // vocabulary) or real multi-touch passthrough — see TouchInput.kt.
        Box(
            Modifier.fillMaxSize().pointerInput(handle, touchMode) {
                when (touchMode) {
                    TouchMode.TOUCH -> streamTouchPassthrough(handle)
                    else -> streamTouchInput(
                        handle,
                        trackpad = touchMode == TouchMode.TRACKPAD,
                        onToggleStats = { showStats = !showStats },
                    )
                }
            },
        )
    }
}
