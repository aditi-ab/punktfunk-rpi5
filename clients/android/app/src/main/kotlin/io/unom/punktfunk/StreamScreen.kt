package io.unom.punktfunk

import android.Manifest
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.hardware.usb.UsbManager
import android.net.wifi.WifiManager
import android.os.Build
import android.text.InputType
import android.util.Log
import android.view.KeyEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.WindowManager
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
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
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import io.unom.punktfunk.kit.GamepadFeedback
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.Sc2Capture
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

    // Live decode stats for the HUD. `statsOn` (verbosity != OFF) gates the whole native pipeline:
    // the per-frame sampling (nativeSetVideoStatsEnabled — a hidden HUD costs one atomic load per
    // frame) AND the 1 s poll loop, which only runs while the overlay is visible. Enabling resets
    // the native window, so re-showing never renders stale data. A 3-finger tap cycles the
    // verbosity tier live (Off → Compact → Normal → Detailed → Off); the default comes from
    // Settings. The tier only changes how many lines `StatsOverlay` draws — switching between the
    // visible tiers keeps sampling running (the effect keys on `statsOn`, not the tier) so it never
    // blanks the numbers for a poll interval.
    val initialSettings = remember { SettingsStore(context).load() }
    var stats by remember { mutableStateOf<DoubleArray?>(null) }
    var decoderLabel by remember { mutableStateOf("") }
    var codecLabel by remember { mutableStateOf("") }
    var statsVerbosity by remember { mutableStateOf(initialSettings.statsVerbosity) }
    val statsOn = statsVerbosity != StatsVerbosity.OFF
    // Touch model is fixed per session (re-keys the gesture handler below if it ever changes).
    val touchMode = initialSettings.touchMode
    // "Low-latency mode" master toggle, resolved once for the session. On (the default) enables the
    // fast pipeline — decoder ranking + vendor keys + async loop (native side), HDMI ALLM below,
    // game-tagged audio, and DSCP marking (applied earlier, at connect); off falls back to the
    // original synchronous decode pipeline as a per-device escape hatch.
    val lowLatencyMode = initialSettings.lowLatencyMode
    // TV form factor (leanback): the decoder actively switches the HDMI output mode to the stream
    // refresh; a phone/tablet gets the softer seamless frame-rate hint instead.
    val isTv = remember { context.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK) }
    LaunchedEffect(handle, statsOn) {
        NativeBridge.nativeSetVideoStatsEnabled(handle, statsOn)
        if (statsOn) {
            // Codec is resolved at the handshake (Welcome) — fixed for the session, so read its
            // label once up front (before the first snapshot renders the video-feed line).
            if (codecLabel.isEmpty()) codecLabel = NativeBridge.nativeVideoCodecLabel(handle)
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

    // True while the gamepad exit chord (Select+Start+L1+R1) is held and counting down — drives the
    // "hold to quit" hint overlay. Set from the router's onExitArmed (main thread).
    var exitArming by remember { mutableStateOf(false) }

    // True while the TV remote is acting as a pointer (hold SELECT toggles) — drives the mode hint.
    var remotePointerOn by remember { mutableStateOf(false) }

    // Focus anchor the soft keyboard is summoned onto AND the pointer-capture grab target (a grab
    // needs a focusable view; captured-pointer events land on it). Declared before the effect
    // below so the capture callbacks can reach the view once it exists.
    var keyCapture by remember { mutableStateOf<KeyCaptureView?>(null) }

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
        // The soft keyboard (three-finger swipe up → KeyCaptureView below) must OVERLAY the
        // stream, never pan/resize it — the video is a fixed-mode surface, not a document.
        // Scoped to the stream; the app's other screens keep the default for their text fields.
        val priorSoftInput = window?.attributes?.softInputMode
            ?: WindowManager.LayoutParams.SOFT_INPUT_ADJUST_UNSPECIFIED
        window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
        // Lock to landscape while streaming — the host streams a landscape desktop, so pin the device
        // there (either landscape direction is fine) and stop it rotating to portrait mid-session. The
        // activity declares configChanges=orientation, so this re-lays out the surface in place without
        // recreating the activity (no stream restart). On TV (fixed landscape) it's a harmless no-op.
        // The prior request is captured and restored on the way out.
        val priorOrientation = activity?.requestedOrientation
        activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        activity?.streamHandle = handle // route hardware keys to this session
        // Multi-controller router: a stable wire pad index per connected controller, per-device axis
        // state, Arrival/Remove on hot-plug, and feedback routed back by pad index. Forwards every
        // controller (Automatic). Built here, released on dispose.
        val router = GamepadRouter(context, handle, initialSettings.gamepad)
        activity?.gamepadRouter = router
        // Select+Start+L1+R1 chord leaves the stream — a deliberate quit (signal it so the host skips
        // the keep-alive linger), unlike a host-ended / backgrounded drop. The router debounces it
        // (must be held ~1.5 s) and fires onExitChord on its main-thread timer, so leave the stream
        // the same way the Back gesture does.
        activity?.requestStreamExit = { NativeBridge.nativeDisconnectQuit(handle); onDisconnect() }
        router.onExitChord = { activity?.requestStreamExit?.invoke() }
        // Show a "hold to quit" hint the moment the chord completes (the router debounces the actual
        // exit); it clears when the buttons release early or the hold elapses. Runs on the main thread.
        router.onExitArmed = { armed -> exitArming = armed }
        // Physical mouse: uncaptured hover/click/wheel forwards as absolute pointing; captured
        // (setting or the Ctrl+Alt+Shift+Q chord) raw deltas forward as relative mouse-look.
        // The local cursor is hidden over the stream — the host's own cursor, composited into
        // the video, is the one the user sees (twin of the desktop clients' hidden cursor).
        val decor = window?.decorView
        val priorPointerIcon = decor?.pointerIcon
        decor?.pointerIcon = android.view.PointerIcon.getSystemIcon(
            context,
            android.view.PointerIcon.TYPE_NULL,
        )
        val mouse = MouseForwarder(
            handle,
            invertScroll = initialSettings.invertScroll,
            captureWanted = initialSettings.pointerCapture,
            surfaceSize = { (decor?.width ?: 0) to (decor?.height ?: 0) },
        )
        mouse.onRequestCapture = {
            // The grab needs the (focusable) capture view: focus it, then ask. Posted so a
            // request racing view attach/focus settles on the next frame.
            keyCapture?.let { v ->
                v.post {
                    v.requestFocus()
                    v.requestPointerCapture()
                }
            }
        }
        mouse.onReleaseCapture = { keyCapture?.releasePointerCapture() }
        activity?.mouseForwarder = mouse
        // TV remote-as-pointer: hold SELECT ≈ 0.8 s to toggle; the D-pad then glides the host
        // cursor (see RemotePointer). TV only — a phone's remote-less keys stay on the VK path.
        val remote = if (isTv) {
            RemotePointer(
                handle,
                surfaceWidth = { decor?.width ?: 1920 },
                onActiveChanged = { on -> remotePointerOn = on },
                onKeyboardToggle = { keyCapture?.let { it.setImeVisible(!it.imeShown) } },
            )
        } else {
            null
        }
        activity?.remotePointer = remote
        // Shared clipboard (text v1): only when the user setting is on AND the host has a
        // working clipboard service. Protocol-level opt-in + the poll thread live in the sync.
        val clip = if (initialSettings.clipboardSync && NativeBridge.nativeClipSupported(handle)) {
            ClipboardSync(context, handle).also { it.start() }
        } else {
            null
        }
        activity?.setConsoleHighRefreshRate(false) // let the decoder's setFrameRate pick the panel rate
        // Host→client feedback (rumble + DualSense lightbar/LEDs), routed to each controller by pad
        // index via the router; poll threads stopped + joined before the router is released and the
        // session closed. "Rumble on this phone" (opt-in) additionally mirrors controller 1's
        // rumble onto the device's own vibrator — for clip-on pads without rumble motors.
        val feedback = GamepadFeedback(
            handle,
            router,
            deviceVibrator = if (initialSettings.rumbleOnPhone) deviceBodyVibrator(context) else null,
        ).also { it.start() }
        // Free a disconnected controller's rumble/lights bindings promptly (else the open lights
        // session leaks until the session ends). The router owns hot-plug; the feedback owns the binds.
        router.onSlotClosed = feedback::onDeviceRemoved
        // Steam Controller 2 as-is passthrough (opt-out): capture a wired/Puck USB pad — or an
        // already-paired BLE one — and forward its raw reports; the host mirrors a real
        // 28DE:1302 that its Steam drives directly, and Steam's rumble/settings writes come back
        // through feedback.onHidRaw onto the physical controller. Engages only when such a pad is
        // actually present; the wire slot is claimed lazily on its first state report.
        // The menu-time capture (UI navigation) must let go before the stream-mode capture can
        // claim the interfaces; it resumes in onDispose once the stream releases them.
        activity?.stopSc2MenuNav()
        val sc2 = if (initialSettings.sc2Capture) Sc2Capture(context, router) else null
        var sc2UsbReceiver: BroadcastReceiver? = null
        if (sc2 != null) {
            feedback.onHidRaw = sc2::onHidRaw
            val usbManager = context.getSystemService(Context.USB_SERVICE) as UsbManager
            val usbDev = sc2.findUsbDevice()
            when {
                usbDev != null && usbManager.hasPermission(usbDev) -> sc2.startUsb(usbDev)
                usbDev != null -> {
                    // One-time system dialog; capture engages on grant (Android remembers the
                    // grant for as long as the device stays attached).
                    val action = "io.unom.punktfunk.SC2_USB_PERMISSION"
                    val receiver = object : BroadcastReceiver() {
                        override fun onReceive(c: Context?, intent: Intent?) {
                            if (intent?.action != action) return
                            val ok = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
                            if (ok) sc2.startUsb(usbDev) else Log.i("punktfunk", "SC2 USB permission denied")
                        }
                    }
                    sc2UsbReceiver = receiver
                    ContextCompat.registerReceiver(
                        context, receiver, IntentFilter(action), ContextCompat.RECEIVER_NOT_EXPORTED,
                    )
                    usbManager.requestPermission(
                        usbDev,
                        PendingIntent.getBroadcast(
                            context, 0,
                            Intent(action).setPackage(context.packageName),
                            // MUTABLE: the USB stack appends the grant extras to this intent.
                            PendingIntent.FLAG_MUTABLE,
                        ),
                    )
                }
                ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_CONNECT) ==
                    PackageManager.PERMISSION_GRANTED -> {
                    sc2.pairedBleAddress()?.let { addr ->
                        Log.i("punktfunk", "SC2: no USB pad — using the paired BLE controller $addr")
                        sc2.startBle(addr)
                    }
                }
            }
        }
        onDispose {
            closed.set(true) // from here the handle gets freed; surfaceDestroyed must not touch it
            clip?.stop() // stop + join the clipboard poll thread BEFORE the handle is freed
            feedback.onHidRaw = null
            feedback.stop() // stop + join the poll threads BEFORE the router is released / handle freed
            sc2UsbReceiver?.let { runCatching { context.unregisterReceiver(it) } }
            sc2?.stop() // release the USB/BLE link + free the wire slot (host tears the pad down)
            router.onExitArmed = null // don't poke Compose state from release()'s disarm while tearing down
            router.release() // flush every slot (nothing sticks host-side) + drop the hot-plug listener
            activity?.gamepadRouter = null
            // Mouse/remote-pointer teardown: lift held buttons, drop the grab, restore the cursor.
            mouse.release()
            activity?.mouseForwarder = null
            remote?.release()
            activity?.remotePointer = null
            decor?.pointerIcon = priorPointerIcon
            activity?.streamHandle = 0L
            activity?.requestStreamExit = null
            // Back in the menus: the SC2 (if present) resumes driving the console UI.
            activity?.startSc2MenuNav()
            activity?.setConsoleHighRefreshRate(true) // back to the console UI's max refresh
            controller?.hide(WindowInsetsCompat.Type.ime()) // drop any keyboard left showing
            window?.setSoftInputMode(priorSoftInput)
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

    // Auto-engage pointer capture at stream start (setting on + a mouse actually present).
    // Delayed a beat: the grab needs window focus and the capture view attached.
    LaunchedEffect(handle) {
        delay(400)
        activity?.mouseForwarder?.engageFromStart()
    }

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
        if (statsOn) {
            stats?.let {
                StatsOverlay(it, statsVerbosity, decoderLabel, codecLabel, Modifier.align(Alignment.TopStart).padding(12.dp))
            }
        }
        // "Hold to quit" hint while the gamepad exit chord is armed — the exit debounces on a ~1 s
        // hold, so without this cue a couch user reads the (deliberately no-longer-instant) chord as
        // broken. Purely visual; it sits above the video and below the gesture layer.
        if (exitArming) {
            ExitChordHint(Modifier.align(Alignment.TopCenter).padding(top = 16.dp))
        }
        // Remote-pointer mode hint — the remote's keys are remapped while it's on, so say so.
        if (remotePointerOn) {
            RemotePointerHint(Modifier.align(Alignment.TopCenter).padding(top = 16.dp))
        }
        // Invisible 1-px focus anchor for the host-typing soft keyboard (three-finger swipe up
        // in the mouse modes) AND the pointer-capture grab target — it never draws or takes
        // touches, it just owns IME focus and receives captured-pointer events.
        AndroidView(
            modifier = Modifier.size(1.dp),
            factory = { ctx ->
                KeyCaptureView(ctx).also { v ->
                    keyCapture = v
                    // Real IME text path when the host types committed text (see KeyCaptureView).
                    v.textHandle =
                        if (NativeBridge.nativeTextInputSupported(handle)) handle else 0L
                    v.setOnCapturedPointerListener { _, ev ->
                        (ctx as? MainActivity)?.mouseForwarder?.onCapturedPointer(ev) ?: false
                    }
                }
            },
        )
        // Touch input per the Settings model: trackpad/direct-pointer mouse (the shared gesture
        // vocabulary) or real multi-touch passthrough — see TouchInput.kt. Passthrough gets no
        // keyboard gesture: its fingers belong to the host verbatim (a swipe there may BE a
        // host-OS gesture), so intercepting three fingers would corrupt real multi-touch.
        // Stylus lane (design/pen-tablet-input.md §7): against a HOST_CAP_PEN host a stylus
        // splits out of BOTH touch models onto the pen plane; its heartbeat coroutine keeps a
        // stationary held stroke alive (and its cancellation lifts everything on teardown).
        val stylus = remember(handle) {
            if (NativeBridge.nativeHostSupportsPen(handle)) StylusStream(handle) else null
        }
        if (stylus != null) {
            LaunchedEffect(stylus) { stylus.heartbeatLoop() }
        }
        Box(
            Modifier.fillMaxSize().pointerInput(handle, touchMode) {
                when (touchMode) {
                    TouchMode.TOUCH -> streamTouchPassthrough(handle, stylus)
                    else -> streamTouchInput(
                        handle,
                        stylus,
                        trackpad = touchMode == TouchMode.TRACKPAD,
                        invertScroll = initialSettings.invertScroll,
                        onCycleStats = { statsVerbosity = statsVerbosity.next() },
                        onKeyboard = { show -> keyCapture?.setImeVisible(show) },
                    )
                }
            },
        )
    }
}

/**
 * The "hold to quit" cue shown while the gamepad exit chord (Select + Start + L1 + R1) is held. The
 * chord no longer quits on a quick press — the router debounces it on a ~1 s hold — so this confirms
 * the press registered and tells the user to keep holding. Purely visual; [GamepadRouter.onExitArmed]
 * toggles its visibility.
 */
@Composable
private fun ExitChordHint(modifier: Modifier = Modifier) {
    Text(
        "Hold to quit…",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * The remote-pointer mode cue: while active the remote's keys are remapped (D-pad glides the host
 * cursor, SELECT clicks), so the overlay both confirms the toggle and teaches the vocabulary.
 */
@Composable
private fun RemotePointerHint(modifier: Modifier = Modifier) {
    Text(
        "Remote pointer — SELECT click · play/pause right-click · hold SELECT to exit",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * Invisible focus anchor for typing on the host: the three-finger swipe summons the device IME
 * onto this view. Two IME models, picked by the host's capabilities:
 *  * **Text path** ([textHandle] set — the host advertised `HOST_CAP_TEXT_INPUT`): a real
 *    editable [HostTextConnection], so the IME gives autocorrect, gesture typing, non-Latin
 *    composition and emoji, all mirrored to the host as committed text + diffs.
 *  * **Fallback** (older host): `TYPE_NULL` puts the IME in "dumb keyboard" mode — raw
 *    [KeyEvent]s flow through `MainActivity.dispatchKeyEvent` → `Keymap.toVk` → the host, the
 *    exact path a hardware keyboard takes (with the IME-shift wrap documented there).
 *
 * Doubles as the pointer-capture grab target: a grab needs a focusable view, and captured-pointer
 * events are delivered to it (routed to [MouseForwarder.onCapturedPointer] via the listener the
 * stream screen installs).
 */
private class KeyCaptureView(context: Context) : View(context) {
    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    /** The session handle when the host types committed text; `0` = VK-only fallback. */
    var textHandle: Long = 0L

    /** Whether [setImeVisible] last showed the IME — for toggle-style callers (remote pointer). */
    var imeShown = false
        private set

    override fun onCheckIsTextEditor(): Boolean = imeShown

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection? {
        // Only an editor while the user has SUMMONED the keyboard (gesture / remote toggle).
        // This view holds focus for the whole stream (it's the capture anchor), and with an
        // always-live editable connection the IME counts input as active on it — TV IMEs then
        // pop their UI the moment a PHYSICAL keyboard key arrives. With no connection, hardware
        // typing stays on the raw dispatchKeyEvent → Keymap → wire path and no keyboard appears.
        if (!imeShown) return null
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_ENTER_ACTION
        return if (textHandle != 0L) {
            outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
                InputType.TYPE_TEXT_FLAG_AUTO_CORRECT or InputType.TYPE_TEXT_FLAG_MULTI_LINE
            HostTextConnection(this, textHandle)
        } else {
            outAttrs.inputType = InputType.TYPE_NULL
            BaseInputConnection(this, false)
        }
    }

    fun setImeVisible(show: Boolean) {
        val imm = context.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager
            ?: return
        imeShown = show
        if (show) {
            requestFocus()
            // The view may already be focused from a null-connection state — restart so the
            // framework re-queries onCreateInputConnection with the gate now open.
            imm.restartInput(this)
            imm.showSoftInput(this, 0)
        } else {
            imm.hideSoftInputFromWindow(windowToken, 0)
            imm.restartInput(this) // gate closed — drop the editable connection
        }
    }

    /**
     * BACK while the summoned keyboard is up: the IME consumes it pre-IME to dismiss itself, so
     * [setImeVisible] never hears about it — sync the gate here or a stale `imeShown` leaves the
     * editable connection live and physical typing re-pops the keyboard.
     */
    override fun onKeyPreIme(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK && imeShown && event.action == KeyEvent.ACTION_UP) {
            imeShown = false
            (context.getSystemService(Context.INPUT_METHOD_SERVICE) as? InputMethodManager)
                ?.restartInput(this)
        }
        return super.onKeyPreIme(keyCode, event)
    }
}

/**
 * IME → host text bridge (the `HOST_CAP_TEXT_INPUT` path): a real **editable** connection, so
 * the IME runs its full machinery (autocorrect, gesture typing, non-Latin composition), mirrored
 * to the host as it happens. The one piece of host-side state tracked is *what the host currently
 * shows of the active composition* ([sentComposition]): composing updates send a common-prefix
 * diff (backspaces + the new suffix) so corrections materialize live on the host; a commit
 * settles it. [setComposingRegion] adopts already-committed text as the active composition
 * (autocorrect-revert / backspace-into-word flows), so the next update diffs against it instead
 * of retyping. Newlines become Enter taps; [deleteSurroundingText] becomes Backspace/Delete taps.
 *
 * Known approximation: diff lengths are counted in Unicode scalars, assuming one host Backspace
 * deletes one scalar — true for the composition text IMEs actually produce (emoji and other
 * multi-unit graphemes commit directly rather than composing).
 */
private class HostTextConnection(
    view: KeyCaptureView,
    private val handle: Long,
) : BaseInputConnection(view, true) {
    /** What the host currently shows of the active composition ("" = none). */
    private var sentComposition = ""

    override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
        retype(text.toString())
        sentComposition = ""
        val ok = super.commitText(text, newCursorPosition)
        trimEditable()
        return ok
    }

    override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
        retype(text.toString())
        return super.setComposingText(text, newCursorPosition)
    }

    override fun finishComposingText(): Boolean {
        // The composition text stands as committed — the host already shows it verbatim.
        sentComposition = ""
        return super.finishComposingText()
    }

    override fun setComposingRegion(start: Int, end: Int): Boolean {
        val e = editable
        if (e != null) {
            val a = start.coerceIn(0, e.length)
            val b = end.coerceIn(0, e.length)
            sentComposition = e.subSequence(minOf(a, b), maxOf(a, b)).toString()
        }
        return super.setComposingRegion(start, end)
    }

    override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
        repeat(beforeLength.coerceIn(0, MAX_TAPS)) { tapVk(VK_BACK) }
        repeat(afterLength.coerceIn(0, MAX_TAPS)) { tapVk(VK_DELETE) }
        return super.deleteSurroundingText(beforeLength, afterLength)
    }

    override fun performEditorAction(actionCode: Int): Boolean {
        tapVk(VK_RETURN)
        return true
    }

    /** Replace the host's view of the composition with [text] via a common-prefix diff. */
    private fun retype(text: String) {
        var common = sentComposition.commonPrefixWith(text)
        // Never split a surrogate pair mid-diff — back off to the pair boundary.
        if (common.isNotEmpty() && common.last().isHighSurrogate()) {
            common = common.dropLast(1)
        }
        val stale = sentComposition.substring(common.length)
        repeat(stale.codePointCount(0, stale.length).coerceAtMost(MAX_TAPS)) { tapVk(VK_BACK) }
        sendText(text.substring(common.length))
        sentComposition = text
    }

    /** Forward literal text, turning newlines into Enter taps (control chars never ride text). */
    private fun sendText(s: String) {
        var chunk = StringBuilder()
        for (ch in s) {
            if (ch == '\n') {
                if (chunk.isNotEmpty()) {
                    NativeBridge.nativeSendText(handle, chunk.toString())
                    chunk = StringBuilder()
                }
                tapVk(VK_RETURN)
            } else {
                chunk.append(ch)
            }
        }
        if (chunk.isNotEmpty()) NativeBridge.nativeSendText(handle, chunk.toString())
    }

    private fun tapVk(vk: Int) {
        NativeBridge.nativeSendKey(handle, vk, true, 0)
        NativeBridge.nativeSendKey(handle, vk, false, 0)
    }

    /** Bound the mirror buffer: once nothing is composing, old text serves no purpose. */
    private fun trimEditable() {
        val e = editable ?: return
        if (getComposingSpanStart(e) == -1 && e.length > 4000) e.clear()
    }

    private companion object {
        const val VK_BACK = 0x08
        const val VK_RETURN = 0x0D
        const val VK_DELETE = 0x2E
        const val MAX_TAPS = 256
    }
}
