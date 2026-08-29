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
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.AudioEffect
import android.media.audiofx.NoiseSuppressor
import android.net.wifi.WifiManager
import android.os.Build
import android.text.InputType
import android.util.Log
import android.view.KeyEvent
import android.view.Surface
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
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
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
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.lifecycle.LifecycleOwner
import io.unom.punktfunk.kit.DeviceGyro
import io.unom.punktfunk.kit.DsCapture
import io.unom.punktfunk.kit.GamepadFeedback
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.security.IdentityLoad
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.kit.PadSensors
import io.unom.punktfunk.kit.Sc2Capture
import io.unom.punktfunk.kit.SessionAccess
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.models.ActiveSession
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * The immersive stream. Everything it reads about the session comes from [session] — the settings
 * the connect actually resolved (globals, or a profile's overrides on top of them) and the HOST's
 * clipboard decision — rather than from a fresh `SettingsStore` load, which could disagree with
 * the connect that produced this handle.
 */
@Composable
fun StreamScreen(session: ActiveSession, onSessionEnded: (SessionEndReason) -> Unit) {
    val handle = session.handle
    val initialSettings = session.settings
    val micEnabled = initialSettings.micEnabled
    val context = LocalContext.current
    val activity = context as? MainActivity
    // The View hosting this composition — the one that receives the stream's touch/pointer events
    // (the gesture Box below is a Compose node inside it), so it is where unbuffered dispatch is
    // requested.
    val composeView = androidx.compose.ui.platform.LocalView.current
    val window = activity?.window
    // The negotiated stream refresh, known from the handshake (0 = unknown / older native lib) —
    // drives the panel mode pin, the render-rate vote, and the presenter's latch grid.
    val streamHz = remember(handle) { NativeBridge.nativeVideoSize(handle)?.getOrNull(2) ?: 0 }
    val controller = remember(window) {
        window?.let { WindowCompat.getInsetsController(it, it.decorView) }
    }

    // The session's access level (the per-client grants of design/per-client-access.md), the
    // courtesy mirror of what the host enforces: seeded from the Welcome's advert here, kept live
    // by the 1 Hz poll below (the host's AccessUpdate messages fold latest-wins into the native
    // state). Full control + permanent — the only state an old host or an old native lib ever
    // reports — gates nothing and draws nothing: today's look, unchanged.
    val initialAccess = remember(handle) { NativeBridge.nativeAccessState(handle) }
    var accessGrants by remember(handle) {
        mutableStateOf(initialAccess?.getOrNull(0) ?: SessionAccess.ALL)
    }
    // Seconds until this session's access expires (0 = permanent), as last reported natively.
    var accessRemaining by remember(handle) {
        mutableStateOf(initialAccess?.getOrNull(1) ?: 0)
    }

    // Start mic only if the user enabled it AND granted RECORD_AUDIO (else the AAudio input fails).
    val micWanted = micEnabled && ContextCompat.checkSelfPermission(
        context,
        Manifest.permission.RECORD_AUDIO,
    ) == PackageManager.PERMISSION_GRANTED

    // The Java AEC/NS pair backstopping the native VoiceCommunication capture preset, hung off the
    // audio session id `nativeStartMic` returns. Attached in surfaceCreated (where the mic starts)
    // and released on every path that stops the mic — the surface teardown AND the final dispose —
    // so a surface recreate re-attaches to the fresh stream instead of leaking effect engines.
    // All three touch points run on the main thread; a plain list is race-free.
    val micEffects = remember { mutableListOf<AudioEffect>() }

    // In-stream mic mute. Per SESSION and never persisted (no setting backs it): a new stream
    // always starts unmuted. The authoritative flag lives on the native handle, which is why a mute
    // survives the mic stop/start a surface recreate performs — this state is the UI's mirror of
    // it, and survives the same recreate because the composition outlives the surface.
    var micMuted by remember(handle) { mutableStateOf(false) }
    // Whether a capture is actually RUNNING, not merely wanted — set from surfaceCreated on what
    // nativeMicActive reports. A device that refused every AAudio input rung gets no mute chord and
    // no chord line in the start banner, rather than an offer to mute a mic nobody is hearing.
    var micRunning by remember(handle) { mutableStateOf(false) }
    // Transient confirmation of a mic-chord toggle (null = nothing showing). With no standing mic
    // element on screen, this is mute's only feedback: a chord has no on-screen state of its own,
    // and "did that register?" is exactly the doubt to answer.
    var micHint by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(micHint) {
        if (micHint != null) {
            delay(1600)
            micHint = null
        }
    }
    // A captured pad has a gyro this session's virtual controller cannot carry (see
    // GamepadRouter.onMotionUnreachable). Shown briefly, then gone: the failure is otherwise
    // completely silent — the gyro simply does nothing, which from the couch is indistinguishable
    // from a broken sensor — and the fix is a setting, so the notice has to name it.
    var motionHint by remember { mutableStateOf(false) }
    LaunchedEffect(motionHint) {
        if (motionHint) {
            // Longer than the mic chord's 1.6 s: that one confirms something the user just did,
            // this one explains something they did not, in a sentence they have to read.
            delay(6000)
            motionHint = false
        }
    }
    // Whether this session has a controller — the start banner names pad chords only when there is
    // a pad to press them on. Seeded from the router the moment it is built (it opens a slot for
    // every already-connected controller) and latched true by a pad that arrives later; it never
    // goes back to false. A pad LEAVING inside the banner's six seconds is not worth the write:
    // teardown closes every slot, and poking Compose state from there is exactly what the nulled
    // callbacks in onDispose avoid. The latch is also what carries a pad through a USB capture
    // claiming it — its InputDevice slot closes and reopens as a capture-link one.
    var padPresent by remember(handle) { mutableStateOf(false) }
    // The start-of-stream banner: what this session's shortcuts ARE, said once. A stream takes the
    // whole screen and answers to none of the device's usual gestures, so it has to say how to get
    // back out — the desktop console draws the same pill for the same reason
    // (`pf-console-ui/src/skia_overlay.rs`, BANNER_S = 6 s with a BANNER_FADE_S = 0.6 s tail).
    // Two states because the fade and the removal are different moments: `bannerUp` composes the
    // pill at all, `bannerFading` runs its alpha down over the last 600 ms.
    var bannerUp by remember(handle) { mutableStateOf(true) }
    var bannerFading by remember(handle) { mutableStateOf(false) }
    val bannerAlpha by animateFloatAsState(
        targetValue = if (bannerFading) 0f else 1f,
        // Linear, like the desktop's (BANNER_S - age) / BANNER_FADE_S ramp — Compose's default
        // easing would hold near-opaque and then drop, which reads as a glitch rather than a fade.
        animationSpec = tween(600, easing = LinearEasing),
        label = "streamStartBanner",
    )
    LaunchedEffect(handle) {
        delay(5400) // 6 s − the 0.6 s tail: fully opaque until here, exactly as on the desktop
        bannerFading = true
        delay(600)
        bannerUp = false // stop composing it once it is invisible
    }
    // The one place mute is toggled — Compose state + the native flag, always together.
    val setMicMuted = { muted: Boolean ->
        micMuted = muted
        NativeBridge.nativeSetMicMuted(handle, muted)
    }

    // Push a grant mask into every gate that consults one — called at session start (once the
    // router/forwarders exist) and again whenever the poll sees the mask change (an AccessUpdate
    // revoked or restored something mid-session). A lambda, deliberately not a local fun — this
    // codebase has been burned by `::localFun` references in composable scopes. The gates it does
    // NOT reach (the Compose-side ones — the touch layer, the IME summon, the banner line, the
    // chip) key on `accessGrants` directly and re-run on the state write.
    val applyAccess: (Int) -> Unit = { grants ->
        activity?.streamAccess = grants
        activity?.gamepadRouter?.gamepadGranted = grants and SessionAccess.GAMEPAD != 0
        val pointerOk = grants and SessionAccess.POINTER != 0
        activity?.mouseForwarder?.let { m ->
            m.pointerGranted = pointerOk
            // A revocation must also let an existing grab go (and lift held buttons): a captured
            // mouse that moves nothing reads as a broken mouse, not a spectator session.
            if (!pointerOk) m.release()
        }
        activity?.remotePointer?.setGranted(pointerOk)
        // Mic revoked mid-session: stop the capture — the host detaches its end regardless, and
        // an open mic (with the platform's recording indicator lit) feeding a plane the host
        // drops would be the worst kind of lie. Not restarted on a re-grant: the host attaches
        // the mic service at session setup only, so a fresh session is the honest offer.
        if (grants and SessionAccess.MIC == 0 && micRunning) {
            releaseMicEffects(micEffects)
            NativeBridge.nativeStopMic(handle)
            micRunning = false
        }
    }

    // Live decode stats for the HUD. `statsOn` (verbosity != OFF) gates the whole native pipeline:
    // the per-frame sampling (nativeSetVideoStatsEnabled — a hidden HUD costs one atomic load per
    // frame) AND the 1 s poll loop, which only runs while the overlay is visible. Enabling resets
    // the native window, so re-showing never renders stale data. A 3-finger tap — or the Select + X
    // pad chord, which is the only route a TV or a passthrough-touch session has — cycles the
    // verbosity tier live (Off → Compact → Normal → Detailed → Off); the default comes from
    // Settings. The tier only changes how many lines `StatsOverlay` draws — switching between the
    // visible tiers keeps sampling running (the effect keys on `statsOn`, not the tier) so it never
    // blanks the numbers for a poll interval.
    var stats by remember { mutableStateOf<DoubleArray?>(null) }
    var decoderLabel by remember { mutableStateOf("") }
    var codecLabel by remember { mutableStateOf("") }
    // The panel's LIVE refresh rate, re-read each poll — the HUD flags a session whose panel sits
    // below the stream rate (an OEM governor that ignored both the mode pin and the surface hint).
    var panelHz by remember { mutableStateOf(0f) }
    var statsVerbosity by remember { mutableStateOf(initialSettings.statsVerbosity) }
    val statsOn = statsVerbosity != StatsVerbosity.OFF
    // Touch model is fixed per session (re-keys the gesture handler below if it ever changes).
    // Passthrough needs a host that injects touch; without the bit every contact would vanish, so
    // the session runs the trackpad model instead and `touchHint` below says so, once.
    val touchUnsupported = remember(handle) {
        initialSettings.touchMode == TouchMode.TOUCH && !NativeBridge.nativeHostSupportsTouch(handle)
    }
    // Live: the ring's Touch mode slot cycles it mid-stream (the gesture layer is keyed on it,
    // so a change applies from the next gesture — trap 2 in the design: never mid-gesture).
    var touchMode by remember(handle) {
        mutableStateOf(if (touchUnsupported) TouchMode.TRACKPAD else initialSettings.touchMode)
    }
    val hostAcceptsTouch = remember(handle) { NativeBridge.nativeHostSupportsTouch(handle) }
    // The quick-action ring (design/touch-client-overlay.md §2), declared ahead of the pad
    // router that opens and drives it.
    val ring = remember(handle) { RingState() }
    var containerSize by remember { mutableStateOf(IntSize.Zero) }
    val haptics = rememberConsoleHaptics()
    var touchHint by remember { mutableStateOf(touchUnsupported) }
    LaunchedEffect(touchHint) {
        if (touchHint) {
            delay(6000)
            touchHint = false
        }
    }
    // "Low-latency mode" master toggle, resolved once for the session. On (the default) enables the
    // fast pipeline — decoder ranking + vendor keys + async loop (native side), HDMI ALLM below,
    // game-tagged audio, and DSCP marking (applied earlier, at connect); off falls back to the
    // original synchronous decode pipeline as a per-device escape hatch.
    val lowLatencyMode = initialSettings.lowLatencyMode
    // TV form factor (leanback): the decoder actively switches the HDMI output mode to the stream
    // refresh; a phone/tablet gets the softer seamless frame-rate hint instead.
    val isTv = remember { context.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK) }
    // A screen with fingers on it — the start banner may only name the three-finger stats tap on a
    // device that can perform it. A TV box has no touchscreen at all, and its remote is not one.
    val hasTouch = remember {
        context.packageManager.hasSystemFeature(PackageManager.FEATURE_TOUCHSCREEN)
    }
    LaunchedEffect(handle, statsOn) {
        NativeBridge.nativeSetVideoStatsEnabled(handle, statsOn)
        if (statsOn) {
            // Codec is resolved at the handshake (Welcome) — fixed for the session, so read its
            // label once up front (before the first snapshot renders the video-feed line).
            if (codecLabel.isEmpty()) codecLabel = NativeBridge.nativeVideoCodecLabel(handle)
            while (true) {
                delay(1000)
                stats = NativeBridge.nativeVideoStats(handle)
                panelHz = runCatching { context.display }.getOrNull()?.refreshRate ?: 0f
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
        var lastAccessSeq = initialAccess?.getOrNull(2) ?: 0
        while (true) {
            delay(1000)
            // Access first, ended second: a session about to close on its expiry gets its final
            // countdown read, which is what lets the ended branch word that close honestly.
            NativeBridge.nativeAccessState(handle)?.let { st ->
                val grants = st.getOrNull(0) ?: SessionAccess.ALL
                val seq = st.getOrNull(2) ?: 0
                if (grants != accessGrants) {
                    accessGrants = grants
                    applyAccess(grants)
                }
                accessRemaining = st.getOrNull(1) ?: 0
                if (seq != lastAccessSeq) {
                    lastAccessSeq = seq
                    // A fresh AccessUpdate close to the deadline is the host's T−5 m / T−1 m
                    // courtesy warning — surface it. Grant edits (and a warning's grant echo)
                    // otherwise just move the chip; a toast per edit would be noise.
                    if (accessRemaining in 1..330) {
                        val mins = (accessRemaining + 30) / 60
                        Toast.makeText(
                            context,
                            if (mins <= 1) {
                                "Access expires in about a minute."
                            } else {
                                "Access expires in about $mins minutes."
                            },
                            Toast.LENGTH_LONG,
                        ).show()
                    }
                }
            }
            if (NativeBridge.nativeSessionEnded(handle)) {
                // WHY it ended decides what the user is told. This used to show the "host may be
                // asleep" line for EVERY ending — including a game the player had just quit and a
                // session the host ended on purpose — which reads as a failure report for
                // something nobody did wrong. Only a connection that actually died says that now.
                val reason = SessionEndReason.fromNative(NativeBridge.nativeEndReason(handle))
                when {
                    // The session died inside the access countdown's final stretch: that IS the
                    // typed expiry close (ACCESS_EXPIRED), worded with the shared rejection
                    // sentence rather than the generic host-ended silence. Recognized off the
                    // countdown because the generic end-reason byte predates the expiry code.
                    accessRemaining in 1..75 ->
                        Toast.makeText(
                            context,
                            "Your access to this host has expired.",
                            Toast.LENGTH_LONG,
                        ).show()
                    reason == SessionEndReason.LOST ->
                        Toast.makeText(
                            context,
                            "Connection lost — the host may be asleep. Wake it to reconnect.",
                            Toast.LENGTH_LONG,
                        ).show()
                    reason == SessionEndReason.HOST_ERROR ->
                        Toast.makeText(
                            context,
                            "The host ended the session with an error.",
                            Toast.LENGTH_LONG,
                        ).show()
                    // Deliberate endings — the player quit the game, the host was stopped, or we
                    // closed it. Leaving the stream IS the feedback; a toast would only add noise.
                    else -> {}
                }
                onSessionEnded(reason)
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

    // The video SurfaceView, hoisted for the same reason: the pointer paths built below map WINDOW
    // coordinates onto the picture, and with a letterboxed stream that rect is the video's, not the
    // panel's. Set when the view is created.
    var videoView by remember { mutableStateOf<SurfaceView?>(null) }

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
        // System bars: NOT hidden here — App.kt owns hide/show (one owner; the AnimatedContent
        // handoff broke per-screen ownership, see the `immersive` effect there).
        // The soft keyboard (three-finger swipe up → KeyCaptureView below) must OVERLAY the
        // stream, never pan/resize it — the video is a fixed-mode surface, not a document.
        // Scoped to the stream; the app's other screens keep the default for their text fields.
        val priorSoftInput = window?.attributes?.softInputMode
            ?: WindowManager.LayoutParams.SOFT_INPUT_ADJUST_UNSPECIFIED
        window?.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_ADJUST_NOTHING)
        // Draw under the display cutout, explicitly. Android 15's SDK-35 edge-to-edge enforcement
        // makes ALWAYS the immersive default, but pre-15 devices letterbox the notch as a dead
        // black bar unless asked — and the stream's own letterbox is black anyway, so the cutout
        // region can never show anything wrong. Captured + restored like the rest of the window
        // state so the menus keep their platform-default behaviour.
        val priorCutout = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.attributes?.layoutInDisplayCutoutMode
        } else {
            null
        }
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            window?.let { w ->
                w.attributes = w.attributes.apply {
                    layoutInDisplayCutoutMode =
                        WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_ALWAYS
                }
            }
        }
        // Lock to landscape while streaming — the host streams a landscape desktop, so pin the device
        // there (either landscape direction is fine) and stop it rotating to portrait mid-session. The
        // activity declares configChanges=orientation, so this re-lays out the surface in place without
        // recreating the activity (no stream restart). On TV (fixed landscape) it's a harmless no-op.
        // The prior request is captured and restored on the way out.
        //
        // COMPACT devices only (sw < 600 dp): on tablets/foldables/desktop windows the lock is a
        // large-display anti-pattern (Play flags it; Android 16+ ignores it there outright), and the
        // stream doesn't need it — the aspect-ratio letterbox renders correctly in any orientation,
        // the lock is purely a phone-ergonomics choice.
        val compactDevice = context.resources.configuration.smallestScreenWidthDp < 600
        val priorOrientation = activity?.requestedOrientation
        if (compactDevice) {
            activity?.requestedOrientation = ActivityInfo.SCREEN_ORIENTATION_SENSOR_LANDSCAPE
        }
        activity?.streamHandle = handle // route hardware keys to this session
        // Multi-controller router: a stable wire pad index per connected controller, per-device axis
        // state, Arrival/Remove on hot-plug, and feedback routed back by pad index. Forwards every
        // controller (Automatic). Built here, released on dispose.
        val router = GamepadRouter(
            context, handle, initialSettings.gamepad, initialSettings.gamepadForwarding,
            initialSettings.systemButtonsForward(), initialSettings.guideGestureEnabled(),
        )
        activity?.gamepadRouter = router
        // Every controller that was already connected got a slot in the router's constructor, so
        // this is the session's pad answer at t=0 — what the start banner's words are chosen from.
        padPresent = router.forwardedDevices().isNotEmpty()
        // Select+Start+L1+R1 chord leaves the stream — a deliberate quit (signal it so the host skips
        // the keep-alive linger), unlike a host-ended / backgrounded drop. The router debounces it
        // (must be held ~1.5 s) and fires onExitChord on its main-thread timer, so leave the stream
        // the same way the Back gesture does.
        activity?.requestStreamExit = { NativeBridge.nativeDisconnectQuit(handle); onSessionEnded(SessionEndReason.LOCAL) }
        router.onExitChord = { activity?.requestStreamExit?.invoke() }
        // Show a "hold to quit" hint the moment the chord completes (the router debounces the actual
        // exit); it clears when the buttons release early or the hold elapses. Runs on the main thread.
        router.onExitArmed = { armed -> exitArming = armed }
        // Select + Y toggles the mic — with no on-screen mute element, this chord is the whole of
        // the control. Ignored when no capture is running (there is nothing to mute, and a hint
        // saying "Microphone muted" over a mic nobody opened would be a lie).
        // A captured Sony pad whose motion this session cannot carry. Fires once per pad, at the
        // moment it is claimed, on the main thread.
        router.onMotionUnreachable = { motionHint = true }
        router.onMicChord = {
            if (micRunning) {
                val next = !micMuted
                setMicMuted(next)
                micHint = if (next) "Microphone muted" else "Microphone live"
            }
        }
        // Select + X steps the stats overlay one tier — the same live cycle the three-finger tap
        // performs, and the ONLY route to it on a TV or in a passthrough-touch session. Session-
        // local on purpose: this mirrors the tap exactly (`onCycleStats` below), and the settings
        // row calls it a live cycle — the stored default is what the next stream starts from.
        router.onStatsChord = { statsVerbosity = statsVerbosity.next() }
        // `Select+A` opens the ring at the screen centre; while it is up the pad belongs to it.
        router.onRingChord = {
            haptics.confirm()
            ring.openAt(Offset(containerSize.width / 2f, containerSize.height / 2f))
        }
        router.onRingNav = { ring.nav(it) }
        ring.onOpenChange = { open -> router.setRingOpen(open) }
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
            captureWanted = initialSettings.mouseMode == MouseMode.CAPTURE,
            // The picture's rect in window coordinates (see MouseForwarder.videoRect) — read live,
            // so it is right from the frame the SurfaceView is first laid out.
            videoRect = {
                videoView?.takeIf { it.width > 0 && it.height > 0 }?.let { v ->
                    val loc = IntArray(2)
                    v.getLocationInWindow(loc)
                    android.graphics.Rect(loc[0], loc[1], loc[0] + v.width, loc[1] + v.height)
                }
            },
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
                surfaceWidth = { videoView?.width?.takeIf { it > 0 } ?: decor?.width ?: 1920 },
                onActiveChanged = { on -> remotePointerOn = on },
                // The toggle TYPES — summoning also needs the KEYBOARD grant (hiding is free).
                onKeyboardToggle = {
                    keyCapture?.let { v ->
                        if (v.imeShown || accessGrants and SessionAccess.KEYBOARD != 0) {
                            v.setImeVisible(!v.imeShown)
                        }
                    }
                },
            )
        } else {
            null
        }
        activity?.remotePointer = remote
        // Everything the grant gates hang off now exists — apply the session's access level once
        // up front (the poll only re-applies on change, and a restricted session is restricted
        // from its first event, not from its first poll).
        applyAccess(accessGrants)
        // Shared clipboard (text v1): only when the user setting is on AND the session's access
        // includes the clipboard AND the host has a working clipboard service. Ungranted, the
        // host's policy resolution declines everything anyway (grants AND into it); not starting
        // the sync is the client-side mirror — no offers announced, no poll thread for a plane
        // that cannot move. Applied at session start only, like the host's own coordinator gate.
        val clip = if (session.clipboardSync &&
            accessGrants and SessionAccess.CLIPBOARD != 0 &&
            NativeBridge.nativeClipSupported(handle)
        ) {
            ClipboardSync(context, handle).also { it.start() }
        } else {
            null
        }
        // Pin the panel to the stream's refresh (exact / multiple) for the session. The decoder's
        // own ANativeWindow_setFrameRate hint still aligns vsync, but it is advisory — some OEM
        // refresh governors ignore it outright and would leave a 120 Hz session on a 60/90 Hz
        // panel. TV boxes skip the pin: the native side actively drives the HDMI mode there.
        if (isTv) {
            activity?.setConsoleHighRefreshRate(false) // the decoder's HDMI mode switch governs
        } else {
            activity?.setStreamDisplayMode(streamHz)
        }
        // Touch/pointer events are vsync-batched by default — up to a frame of input latency the
        // stream shouldn't pay. Unbuffered dispatch delivers them the moment the kernel does.
        // Undone by passing 0 on the way out (API 30+).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
            composeView.requestUnbufferedDispatch(android.view.InputDevice.SOURCE_CLASS_POINTER)
        }
        // Vote the app's RENDER rate up to the stream's (API 35+). The mode pin above governs the
        // panel, but the platform separately down-rates a quiet app's choreographer stream
        // (frame-rate categories: a non-animating UI reads as "normal" = 60) — observed on-glass
        // as 16.6 ms vsync callbacks on a 120 Hz panel, which would pace the presenter at half
        // rate. The native side also subdivides onto the panel grid, so this vote is the belt to
        // that braces. Reset to no-preference on the way out.
        if (Build.VERSION.SDK_INT >= 35 && streamHz > 0) {
            composeView.requestedFrameRate = streamHz.toFloat()
        }
        // Host→client feedback (rumble + DualSense lightbar/LEDs), routed to each controller by pad
        // index via the router; poll threads stopped + joined before the router is released and the
        // session closed. "Rumble on this phone" (opt-in) additionally mirrors controller 1's
        // rumble onto the device's own vibrator — for clip-on pads without rumble motors.
        val feedback = GamepadFeedback(
            handle,
            router,
            deviceVibrator = if (initialSettings.rumbleOnPhone) deviceBodyVibrator(context) else null,
        ).also { it.start() }
        // "Gyro from this phone" (opt-in): this device's IMU speaks for controller 1's motion
        // while wire pad 0 is a controller without a gyro of its own — the rumble mirror's
        // sibling, data flowing the other way. The mirror gates itself per sample (it stands
        // down whenever pad 0's controller has motion of its own — a capture link below, or a
        // pad whose own sensors PadSensors is reading), so it composes without coordination here.
        val phoneGyro = if (initialSettings.gyroOnPhone && initialSettings.gamepadForwarding) {
            DeviceGyro(context, handle, router).also { it.start() }
        } else {
            null
        }
        // A Bluetooth controller's OWN gyro, through the platform sensor framework (API 31+):
        // a BT DualSense / DS4 / Switch Pro / 8BitDo is an ordinary InputDevice, so none of the
        // capture links below ever sees it and its motion used to go nowhere at all. No separate
        // setting — this is the pad's own IMU doing what the pad is for, and unlike the USB
        // captures it claims nothing; forwarding being off is the only thing that silences it.
        val padSensors = if (initialSettings.gamepadForwarding) {
            PadSensors(router).also { it.start() }
        } else {
            null
        }
        // Free a disconnected controller's rumble/lights bindings promptly (else the open lights
        // session leaks until the session ends), and take its sensor listeners off with it — the
        // same callback also fires when a USB capture below CLAIMS the pad, which is what keeps
        // the claimed pad from being fed motion twice. The router owns hot-plug; the feedback owns
        // the binds. Assigned before the captures are constructed, so their claims land on it.
        router.onSlotClosed = { deviceId ->
            feedback.onDeviceRemoved(deviceId)
            padSensors?.onSlotClosed(deviceId)
        }
        // The other edge: a controller that arrives (or first speaks) mid-session gets its sensors
        // read too. The pads already connected were swept by PadSensors.start() above — both run
        // on the main thread with nothing between them, so no controller falls through the gap.
        router.onSlotOpened = { deviceId ->
            padSensors?.onSlotOpened(deviceId)
            // A pad that wakes up a second into the stream still deserves the chord banner — the
            // desktop rebuilds its banner text every frame for exactly this case.
            padPresent = true
        }
        // Steam Controller 2 as-is passthrough (opt-out): capture a wired/Puck USB pad — or an
        // already-paired BLE one — and forward its raw reports; the host mirrors a real
        // 28DE:1302 that its Steam drives directly, and Steam's rumble/settings writes come back
        // through feedback.onHidRaw onto the physical controller. Engages only when such a pad is
        // actually present; the wire slot is claimed lazily on its first state report.
        // The menu-time capture (UI navigation) must let go before the stream-mode capture can
        // claim the interfaces; it resumes in onDispose once the stream releases them.
        activity?.stopSc2MenuNav()
        val sc2 = if (initialSettings.sc2Capture && initialSettings.gamepadForwarding) {
            Sc2Capture(context, router)
        } else {
            null
        }
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
                // No USB pad: fall back to a bonded BLE one. The Bluetooth-permission gate lives
                // inside pairedBleAddress() (it answers null, and says why, when the grant is
                // missing) rather than being restated here — the grant itself is asked for where
                // a user can act on it, in the console UI and the Controllers screen.
                else -> {
                    sc2.pairedBleAddress()?.let { addr ->
                        Log.i("punktfunk", "SC2: no USB pad — using the paired BLE controller $addr")
                        sc2.startBle(addr)
                    }
                }
            }
        }
        // Sony pad capture (DualSense / Edge / DualShock 4, opt-out): claim a USB-connected
        // pad's HID interface and drive it directly — rumble without a kernel force-feedback
        // driver, plus adaptive triggers, lightbar, player LEDs and gyro/touchpad, none of which
        // the InputDevice path can render (no platform API for any of them). Uncaptured (toggle
        // off / permission denied / Bluetooth) the pad stays on the ordinary InputDevice path —
        // the automatic fallback. Host feedback routes back through feedback.sink; the claim
        // frees the pad's InputDevice slot itself (see DsCapture.startUsb), so the wire index
        // hands over deterministically.
        val ds = if (initialSettings.dsCapture && initialSettings.gamepadForwarding) {
            DsCapture(context, router)
        } else {
            null
        }
        var dsUsbReceiver: BroadcastReceiver? = null
        if (ds != null) {
            feedback.sink = ds
            // Tier-A pad audio: render the host's 0xD1 streams on the pad's own 4-channel USB
            // audio device. Bound here rather than inside DsCapture because the session handle
            // lives at this layer; DsCapture decides WHEN (it knows the wire index and the link
            // lifetime), this decides WHETHER.
            if (initialSettings.padHaptics || initialSettings.padSpeaker) {
                ds.padAudio = object : DsCapture.PadAudioHook {
                    override fun start(pad: Int, fd: Int) {
                        val ok = NativeBridge.nativeStartPadAudio(
                            handle,
                            pad,
                            fd,
                            initialSettings.padHaptics,
                            initialSettings.padSpeaker,
                        )
                        Log.i("punktfunk", "pad audio on pad $pad: ${if (ok) "started" else "unavailable"}")
                    }

                    // Returns only once the render thread is joined — DsCapture calls this before
                    // closing the connection whose descriptor that thread borrows.
                    override fun stop(pad: Int) = NativeBridge.nativeStopPadAudio(handle, pad)
                }
            }
            val usbManager = context.getSystemService(Context.USB_SERVICE) as UsbManager
            val usbDev = ds.findUsbDevice()
            when {
                usbDev != null && usbManager.hasPermission(usbDev) -> ds.startUsb(usbDev)
                usbDev != null -> {
                    // One-time system dialog; capture engages on grant (Android remembers the
                    // grant for as long as the device stays attached).
                    val action = "io.unom.punktfunk.DS_USB_PERMISSION"
                    val receiver = object : BroadcastReceiver() {
                        override fun onReceive(c: Context?, intent: Intent?) {
                            if (intent?.action != action) return
                            val ok = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)
                            if (ok) ds.startUsb(usbDev) else Log.i("punktfunk", "Sony pad USB permission denied")
                        }
                    }
                    dsUsbReceiver = receiver
                    ContextCompat.registerReceiver(
                        context, receiver, IntentFilter(action), ContextCompat.RECEIVER_NOT_EXPORTED,
                    )
                    usbManager.requestPermission(
                        usbDev,
                        PendingIntent.getBroadcast(
                            context, 2, // requestCode 2 — 0/1 are the SC2 stream/menu grants
                            Intent(action).setPackage(context.packageName),
                            // MUTABLE: the USB stack appends the grant extras to this intent.
                            PendingIntent.FLAG_MUTABLE,
                        ),
                    )
                }
            }
        }
        onDispose {
            closed.set(true) // from here the handle gets freed; surfaceDestroyed must not touch it
            clip?.stop() // stop + join the clipboard poll thread BEFORE the handle is freed
            feedback.onHidRaw = null
            feedback.sink = null
            feedback.stop() // stop + join the poll threads BEFORE the router is released / handle freed
            phoneGyro?.stop() // join the sensor thread + park pad 0's rotation at zero, same ordering rule
            // After the mirror, so it cannot resume writing pad 0 in the gap when a pad's own
            // sensors let go of it; before the router is released, so the parks still find slots.
            padSensors?.stop()
            sc2UsbReceiver?.let { runCatching { context.unregisterReceiver(it) } }
            sc2?.stop() // release the USB/BLE link + free the wire slot (host tears the pad down)
            dsUsbReceiver?.let { runCatching { context.unregisterReceiver(it) } }
            ds?.stop() // rumble-stop on the physical pad + release the USB link + free the wire slot
            router.onExitArmed = null // don't poke Compose state from release()'s disarm while tearing down
            router.onMicChord = null // same: no mute toggle on buttons released during teardown
            router.onStatsChord = null // same: no tier cycle on buttons released during teardown
            router.onRingChord = null
            router.onRingNav = null
            ring.onOpenChange = null
            router.onMotionUnreachable = null // same: no notice raised by a slot closing at teardown
            router.release() // flush every slot (nothing sticks host-side) + drop the hot-plug listener
            activity?.gamepadRouter = null
            // Mouse/remote-pointer teardown: lift held buttons, drop the grab, restore the cursor.
            mouse.release()
            activity?.mouseForwarder = null
            remote?.release()
            activity?.remotePointer = null
            decor?.pointerIcon = priorPointerIcon
            activity?.streamHandle = 0L
            activity?.streamAccess = SessionAccess.ALL // grants are per session, like the handle
            activity?.requestStreamExit = null
            // Back in the menus: the SC2 (if present) resumes driving the console UI.
            activity?.startSc2MenuNav()
            activity?.setConsoleHighRefreshRate(true) // back to the console UI's max refresh
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                composeView.requestUnbufferedDispatch(0) // back to ordinary batched dispatch
            }
            if (Build.VERSION.SDK_INT >= 35) {
                composeView.requestedFrameRate = View.REQUESTED_FRAME_RATE_CATEGORY_DEFAULT
            }
            controller?.hide(WindowInsetsCompat.Type.ime()) // drop any keyboard left showing
            window?.setSoftInputMode(priorSoftInput)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && priorCutout != null) {
                window?.let { w ->
                    w.attributes = w.attributes.apply { layoutInDisplayCutoutMode = priorCutout }
                }
            }
            window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            if (lowLatencyMode && Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                window?.setPreferMinimalPostProcessing(false)
            }
            wifiLocks.forEach { runCatching { if (it.isHeld) it.release() } }
            // Release the landscape lock so the rest of the app follows the device/system again.
            activity?.requestedOrientation =
                priorOrientation ?: ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED
            // Leaving the stream: stop the mic + audio + decode threads and tear down the session.
            releaseMicEffects(micEffects)
            NativeBridge.nativeStopMic(handle)
            NativeBridge.nativeStopAudio(handle)
            NativeBridge.nativeStopVideo(handle)
            NativeBridge.nativeClose(handle)
        }
    }

    // The quick-action ring (design/touch-client-overlay.md §2). Back opens it at the screen
    // centre instead of ending the session — an edge swipe mid-game used to tear the session down
    // with no confirmation (§5.3). "End stream" is a slot inside, behind a two-press arm.
    BackHandler {
        when {
            ring.sheet -> ring.sheet = false
            ring.committed -> ring.close()
            else -> ring.openAt(Offset(containerSize.width / 2f, containerSize.height / 2f))
        }
    }
    // Host actions are PRE-FETCHED on the session tick, never fetched when the ring opens: two
    // of these buttons shut a machine down, and buttons that appear under a moving finger are a
    // hazard. Empty toward an older host, an unreachable one, or without the record.
    var hostActions by remember(handle) { mutableStateOf<List<HostActions.Action>>(emptyList()) }
    val hostRecord = remember(session.hostId) {
        session.hostId?.let { id -> KnownHostStore(context).all().firstOrNull { it.id == id } }
    }
    LaunchedEffect(handle) {
        val kh = hostRecord ?: return@LaunchedEffect
        if (kh.fpHex.isEmpty()) return@LaunchedEffect
        val identity = withContext(Dispatchers.IO) {
            (IdentityStore(context).load() as? IdentityLoad.Ok)?.identity
        } ?: return@LaunchedEffect
        while (true) {
            hostActions = withContext(Dispatchers.IO) {
                HostActions.list(identity, kh.address, kh.effectiveMgmtPort, kh.fpHex)
            }
            delay(300_000)
        }
    }
    // The live session mode: `nativeVideoSize` follows an accepted mode switch, but its ack lands
    // off the composition, so a request writes the asked-for mode here at once and re-reads the
    // truth shortly after (a rejection shows through then).
    var requestedMode by remember(handle) {
        mutableStateOf(NativeBridge.nativeVideoSize(handle)?.takeIf { it.size >= 2 } ?: intArrayOf(0, 0, 60))
    }
    val scope = rememberCoroutineScope()

    // Leaving the app (Home, task switch, screen off) MUST end the session. Android does not
    // suspend a process for going to background, so without this the native worker kept running and
    // its QUIC connection kept answering the host's keep-alives — the user was long gone but the
    // host still saw a live client and held the session (and its display + encoder) open until the
    // OS eventually reclaimed the process, which on a TV box is effectively never.
    //
    // Route it through `onSessionEnded()` so the composable's `onDispose` above runs the one real
    // teardown path. Deliberately NOT a `nativeDisconnectQuit`: backgrounding isn't a user "quit",
    // so the host should linger the display and make coming straight back a fast reconnect.
    DisposableEffect(handle) {
        val lifecycle = (context as? LifecycleOwner)?.lifecycle
        val obs = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_STOP) {
                onSessionEnded(SessionEndReason.LOCAL)
            }
        }
        lifecycle?.addObserver(obs)
        onDispose { lifecycle?.removeObserver(obs) }
    }

    // Auto-engage pointer capture at stream start (setting on + a mouse actually present).
    // Delayed a beat: the grab needs window focus and the capture view attached.
    LaunchedEffect(handle) {
        delay(400)
        activity?.mouseForwarder?.engageFromStart()
    }

    // Fit the picture to the stream's own aspect, letterboxing the rest in black. MediaCodec scales
    // whatever it decodes to fill the Surface it renders into, so a 16:9 stream on a 20:9 panel came
    // out stretched — the surface has to carry the aspect, because nothing downstream of it can.
    // The mode is the negotiated one (known from the handshake, before the first frame); 0/absent —
    // an older native lib — falls back to filling, i.e. exactly the previous behaviour.
    val videoAspect = remember(handle) {
        val size = NativeBridge.nativeVideoSize(handle)
        val w = size?.getOrNull(0) ?: 0
        val h = size?.getOrNull(1) ?: 0
        if (w > 0 && h > 0) w.toFloat() / h.toFloat() else 0f
    }
    Box(modifier = Modifier.fillMaxSize().background(Color.Black).onSizeChanged { containerSize = it }) {
        // The picture is aspect-fitted; the gesture layer below spans the WHOLE container and maps
        // every absolute contact — direct-pointer touch, passthrough, the pen lane — into this same
        // fit through `videoFitRect`, so a swipe that starts on a letterbox bar still registers and
        // a contact on a bar lands on the nearest picture edge.
        val videoFit = if (videoAspect > 0f) {
            Modifier.align(Alignment.Center).aspectRatio(videoAspect)
        } else {
            Modifier.fillMaxSize()
        }
        AndroidView(
            modifier = videoFit,
            factory = { ctx ->
                SurfaceView(ctx).apply {
                    videoView = this
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
                                initialSettings.presentPriorityWire(),
                                initialSettings.smoothBuffer,
                                // The panel's own refresh — from the mode TABLE (streamPanelFps),
                                // because display.refreshRate reports a per-uid override, not the
                                // panel. Fallback: the (possibly lying) live rate.
                                activity?.streamPanelFps(streamHz)?.takeIf { it > 0 }
                                    ?: (runCatching { context.display }.getOrNull()?.refreshRate ?: 0f)
                                        .roundToInt(),
                                // The SurfaceView's on-screen pixel size — the coordinate space the
                                // ASurfaceControl layer composites in (the aspect-fitted video rect,
                                // not the window's rotated buffer geometry). 0 if not laid out yet;
                                // native falls back to the window buffer size.
                                this@apply.width,
                                this@apply.height,
                            )
                            NativeBridge.nativeStartAudio(handle, lowLatencyMode, isTv)
                            // The MIC grant is read live (a surface recreate re-runs this, and
                            // the mask may have changed since the last one): without it no
                            // capture opens — the host never attached this session to its mic
                            // service, so the platform's recording indicator would announce a
                            // mic nobody can hear.
                            if (micWanted && accessGrants and SessionAccess.MIC != 0) {
                                val sessionId =
                                    NativeBridge.nativeStartMic(handle, initialSettings.echoCancel)
                                if (initialSettings.echoCancel) {
                                    attachMicEffects(sessionId, micEffects)
                                }
                                // Did a capture actually open? That — not the setting — is what
                                // puts the mute control on screen. A restart after a surface
                                // recreate comes back already muted if the user muted: the flag
                                // lives on the session handle, so nothing has to be re-applied.
                                micRunning = NativeBridge.nativeMicActive(handle)
                            }
                        }

                        override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
                            // The view's CURRENT pixel size, for the ASurfaceControl layer's
                            // destination rect. It is reported here and not only at
                            // surfaceCreated because the view grows a frame or two after the
                            // stream screen appears — hiding the system bars and switching on
                            // cutout drawing both resize it, and neither recreates the surface.
                            // A layer left on the start-up rect paints the picture small, in the
                            // top-left corner. The view's own size, not the buffer geometry in
                            // `width`/`height`: the layer composites in the view's space.
                            NativeBridge.nativeVideoSurfaceSize(
                                handle, this@apply.width, this@apply.height,
                            )
                            // Re-assert the frame-rate vote: a buffer-geometry change can reset
                            // the surface's frame-rate setting on some OEM builds, silently
                            // dropping the 120 Hz pin mid-stream. Mirrors the native hint's
                            // policy (FIXED_SOURCE; ALWAYS only on the TV low-latency path —
                            // phones stay seamless so a re-hint can never force a mode flicker).
                            if (streamHz > 0) runCatching {
                                holder.surface.setFrameRate(
                                    streamHz.toFloat(),
                                    Surface.FRAME_RATE_COMPATIBILITY_FIXED_SOURCE,
                                    if (isTv && lowLatencyMode) {
                                        Surface.CHANGE_FRAME_RATE_ALWAYS
                                    } else {
                                        Surface.CHANGE_FRAME_RATE_ONLY_IF_SEAMLESS
                                    },
                                )
                            }
                        }

                        override fun surfaceDestroyed(holder: SurfaceHolder) {
                            // Surface gone (backgrounding, or on the way out). Stop the threads that
                            // render to it — but only while the session is still open. Once
                            // DisposableEffect has closed it, the handle is freed; dereferencing it
                            // here is the use-after-free that crashed on back-navigation.
                            if (!closed.get()) {
                                releaseMicEffects(micEffects)
                                NativeBridge.nativeStopMic(handle)
                                // No capture, no control — but the MUTE state is deliberately left
                                // standing (native keeps it on the handle), so the restart in
                                // surfaceCreated brings the user's choice back with it.
                                micRunning = false
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
                StatsOverlay(
                    it, statsVerbosity, decoderLabel, codecLabel, session.profileName,
                    panelHz,
                    Modifier.align(Alignment.TopStart).padding(12.dp),
                )
            }
        }
        // The Access chip — what this session is allowed to do, said in the preset vocabulary
        // ("Controller only · 1 h 58 m left"), standing for the whole stream. Full control with
        // no expiry — every session against an old host, and most against a new one — shows
        // NOTHING: the chip exists for the sessions where input silently not landing needs an
        // explanation, not as new chrome on everyone's stream. TopEnd, in the shared pill family
        // (TopStart is the HUD's, TopCentre the transient cues', BottomCentre the banner's).
        val accessChip = when {
            accessGrants and SessionAccess.ALL == SessionAccess.ALL && accessRemaining == 0 -> null
            accessRemaining > 0 ->
                "${SessionAccess.label(accessGrants)} · " +
                    "${SessionAccess.remainingLabel(accessRemaining)} left"
            else -> SessionAccess.label(accessGrants)
        }
        if (accessChip != null) {
            AccessChip(accessChip, Modifier.align(Alignment.TopEnd).padding(12.dp))
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
        // The start banner (desktop parity), naming ONLY the shortcuts this session actually has:
        // pad chords when a controller is here, the Back gesture and the three-finger tap when it
        // is not. Recomputed rather than captured, because both inputs change under it — a pad can
        // wake mid-banner, and `micRunning` only settles once the capture has actually opened.
        // Above the video and below the gesture layer: it teaches touches, it must never eat one.
        //
        // Bottom-centre is the desktop's placement and the only edge left — TopStart is the HUD,
        // TopEnd the Access chip, TopCentre the three transient cues — but MotionUnreachableHint
        // already owns it, and both of these can be up at t≈0. The banner YIELDS rather than
        // stacking or sliding off-centre: the notice reports something broken about THIS session
        // and names the setting that fixes it, while the banner repeats shortcuts that will be
        // there next stream too. Two pills sharing an edge for six seconds would cost the reader
        // both.
        if (bannerUp && !motionHint && !touchHint) {
            StreamStartBanner(
                text = buildList {
                    if (padPresent) {
                        add("Hold Select + Start + L1 + R1 to leave")
                        // Only while a capture is actually running: the chord itself no-ops
                        // without one, and offering a mute for a mic nobody has is the lie the
                        // whole control exists to avoid.
                        if (micRunning) add("Select + Y mic")
                        add("Select + X stats")
                    } else {
                        // No pad: Back is the deliberate exit (gesture, key, or a TV remote's
                        // button — all land on the same BackHandler).
                        add("Back leaves the stream")
                        // The tap lives in the pointer touch models only — passthrough gives every
                        // finger to the host verbatim — and needs a screen to put three fingers on,
                        // plus the POINTER grant (without it the gesture layer is not installed).
                        if (hasTouch && touchMode != TouchMode.TOUCH &&
                            accessGrants and SessionAccess.POINTER != 0
                        ) {
                            add("three-finger tap for stats")
                        }
                    }
                }.joinToString(" · "),
                alpha = bannerAlpha,
                modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp),
            )
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
        // The POINTER grant gates the whole touch/stylus capture layer — "don't capture what
        // can't land": ungranted, no gesture handler is installed at all (and no pen lane opens),
        // rather than fingers being read into events the host will drop. Keyed on the grant so an
        // AccessUpdate flipping it mid-session swaps the layer live.
        val pointerOk = accessGrants and SessionAccess.POINTER != 0
        val stylus = remember(handle, pointerOk) {
            if (pointerOk && NativeBridge.nativeHostSupportsPen(handle)) StylusStream(handle) else null
        }
        if (stylus != null) {
            LaunchedEffect(stylus) { stylus.heartbeatLoop() }
        }
        Box(
            Modifier.fillMaxSize().pointerInput(handle, touchMode, pointerOk) {
                when {
                    !pointerOk -> {} // no capture — the Access chip is what says why
                    touchMode == TouchMode.TOUCH -> streamTouchPassthrough(handle, stylus, videoAspect)
                    else -> streamTouchInput(
                        handle,
                        stylus,
                        videoAspect,
                        trackpad = touchMode == TouchMode.TRACKPAD,
                        invertScroll = initialSettings.invertScroll,
                        onCycleStats = { statsVerbosity = statsVerbosity.next() },
                        // The summon rides the pointer gesture but TYPES — so it also needs the
                        // KEYBOARD grant (dismissing is always allowed).
                        onKeyboard = { show ->
                            if (!show || accessGrants and SessionAccess.KEYBOARD != 0) {
                                keyCapture?.setImeVisible(show)
                            }
                        },
                        // The two-finger twist turns the quick-action ring, frame by frame.
                        onDial = { ev ->
                            when (ev) {
                                is DialEvent.Turn ->
                                    if (ring.turn(ev.progress, ev.clockwise, ev.x, ev.y)) haptics.tick()
                                DialEvent.Commit -> { ring.commit(); haptics.confirm() }
                                DialEvent.Cancel -> ring.cancel()
                            }
                        },
                    )
                }
            },
        )
        // No standing mic element here: the in-stream mute control is deliberately absent until the
        // on-screen overlay UI lands and can carry it as one of its controls. Mute itself is intact
        // — the Select + Y chord toggles it, and the hint below is what confirms the toggle.
        // Chord confirmation (gamepad/TV) — mute has no standing indicator, so this is the whole
        // of its feedback: a toggle that showed nothing at all would be indistinguishable from one
        // that never registered.
        // The ring, above the gesture layer so its buttons take the finger first. Composed only
        // while open: a closed overlay costs nothing (tenet 1).
        RingOverlay(
            state = ring,
            cfg = remember(initialSettings.overlayActions) { OverlayConfig.parse(initialSettings.overlayActions) },
            actions = RingActions(
                endStream = { NativeBridge.nativeDisconnectQuit(handle); onSessionEnded(SessionEndReason.LOCAL) },
                disconnectLinger = { onSessionEnded(SessionEndReason.LOCAL) },
                touchMode = { touchMode },
                cycleTouchMode = {
                    // Passthrough is skipped toward a host that drops contacts (§5.4).
                    val order = if (hostAcceptsTouch) TouchMode.entries else listOf(TouchMode.TRACKPAD, TouchMode.POINTER)
                    touchMode = order[(order.indexOf(touchMode) + 1) % order.size]
                },
                keyboardGranted = { accessGrants and SessionAccess.KEYBOARD != 0 },
                keyboard = { keyCapture?.setImeVisible(true) },
                textSupported = NativeBridge.nativeTextInputSupported(handle),
                sendText = { NativeBridge.nativeSendText(handle, it) },
                stats = { statsVerbosity },
                cycleStats = { statsVerbosity = statsVerbosity.next() },
                micAvailable = { micRunning },
                micMuted = { micMuted },
                toggleMic = { setMicMuted(!micMuted) },
                hostActions = { hostActions },
                invokeHost = { act ->
                    hostRecord?.let { kh ->
                        scope.launch(Dispatchers.IO) {
                            (IdentityStore(context).load() as? IdentityLoad.Ok)?.identity?.let { id ->
                                HostActions.invoke(id, kh.address, kh.effectiveMgmtPort, kh.fpHex, kh.name, act.id, act.label)
                            }
                        }
                    }
                },
                sendShortcut = { sendChord(handle, it) },
                currentMode = { requestedMode },
                requestMode = { w, h, hz ->
                    if (NativeBridge.nativeRequestMode(handle, w, h, hz)) {
                        requestedMode = intArrayOf(w, h, hz)
                        scope.launch {
                            delay(500)
                            NativeBridge.nativeVideoSize(handle)?.takeIf { it.size >= 3 }?.let { requestedMode = it }
                        }
                    }
                },
            ),
            containerSize = containerSize,
            haptics = haptics,
        )
        micHint?.let { MicChordHint(it, Modifier.align(Alignment.TopCenter).padding(top = 16.dp)) }
        // Bottom, not top: this can coincide with a mic-chord confirmation or the exit cue, and a
        // notice landing on top of one of those would cost the user both.
        if (motionHint) {
            MotionUnreachableHint(Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp))
        } else if (touchHint) {
            TouchFallbackHint(Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp))
        }
    }
}

/**
 * "This host doesn't accept touch" — shown briefly when the Touch (passthrough) model meets a host
 * whose injector drops contacts (no `HOST_CAP2_TOUCH`). The session runs the trackpad model
 * instead; without this line the user would see their setting silently ignored.
 */
@Composable
private fun TouchFallbackHint(modifier: Modifier = Modifier) {
    Text(
        "This host doesn't accept touch — using the trackpad model",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * Attach the Java echo-canceller + noise-suppressor pair to the mic stream's audio session — the
 * backstop for HALs whose VoiceCommunication capture path doesn't cancel on its own (the native
 * side already opened the stream under that preset). [sessionId] `<= 0` means native allocated no
 * session (echo cancellation off, or the preset fell back to the plain open), so there is nothing
 * to hang an effect on. Created effects land in [into] for [releaseMicEffects]; `create()`
 * returning null (unsupported / claimed) is quietly nothing — the HAL preset still does its part.
 * Needs no extra permission: the effect APIs attach to our own recording session.
 */
private fun attachMicEffects(sessionId: Int, into: MutableList<AudioEffect>) {
    if (sessionId <= 0) return
    if (AcousticEchoCanceler.isAvailable()) {
        AcousticEchoCanceler.create(sessionId)?.let { it.setEnabled(true); into.add(it) }
    }
    if (NoiseSuppressor.isAvailable()) {
        NoiseSuppressor.create(sessionId)?.let { it.setEnabled(true); into.add(it) }
    }
}

/** Release every attached mic effect engine. Idempotent — the list is cleared, and both stop
 * paths (surface teardown, final dispose) may call it in either order. */
private fun releaseMicEffects(effects: MutableList<AudioEffect>) {
    effects.forEach { runCatching { it.release() } }
    effects.clear()
}

/**
 * Transient confirmation that the mic chord (Select + Y) registered. Nothing else on screen says
 * *muted* or *un*muted, so this pill carries both — "did that press do anything?" is the whole
 * doubt a chord with no button under the finger creates. Same pill vocabulary as the other
 * in-stream cues; the caller clears it after a beat.
 */
@Composable
private fun MicChordHint(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
}

/**
 * The standing Access chip — the session's access level in the preset vocabulary, with the live
 * countdown when the grant expires ("Controller only · 1 h 58 m left"). Same pill family as the
 * other in-stream overlays, sized down a step because it stands for the whole session rather than
 * flashing a moment's confirmation. Only composed when there is something to say: a full-control
 * permanent session — today's normal — shows nothing at all.
 */
@Composable
private fun AccessChip(text: String, modifier: Modifier = Modifier) {
    Text(
        text,
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 10.dp, vertical = 5.dp),
        color = Color.White,
        fontSize = 12.sp,
    )
}

/**
 * "This pad's gyro can't reach the game" — shown briefly when a captured controller with motion
 * meets a session whose virtual pad has no motion plane (the X-Box classes have no gyro in their
 * HID contract, so every sample would be decoded and dropped host-side).
 *
 * It names the setting because that is the whole point: without it the player has a gyro that
 * silently does nothing and no way to tell that from a broken sensor. Not a control — the setting
 * applies from the next session, so offering to change it here would promise something this stream
 * cannot deliver. [GamepadRouter.onMotionUnreachable] raises it.
 */
@Composable
private fun MotionUnreachableHint(modifier: Modifier = Modifier) {
    Text(
        "Motion won't reach this session — set Controller type to DualSense",
        modifier = modifier
            .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
            .padding(horizontal = 14.dp, vertical = 8.dp),
        color = Color.White,
        fontSize = 15.sp,
    )
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
 * The start-of-stream banner: the shortcuts this session actually has, in the same pill as every
 * other in-stream cue, shown once and then gone. The desktop console draws the identical thing
 * bottom-centre (`pf-console-ui/src/skia_overlay.rs` — six seconds with a 0.6 s fade), because a
 * stream owns the whole screen and answers to none of the device's usual gestures: without a line
 * saying how to get back out, the only discoverable exit is force-quitting the app.
 *
 * [text] and [alpha] are the caller's. Only it knows what this session HAS — a pad, a mic, a
 * touchscreen — and only it owns the timer, which is precisely what a screenshot wants to skip.
 * Purely visual: it sits below the gesture layer, takes no touches and is never clickable. Internal
 * so the screenshot scene can shoot the real pill instead of a copy of it that drifts.
 */
@Composable
internal fun StreamStartBanner(text: String, alpha: Float, modifier: Modifier = Modifier) {
    Text(
        text,
        // Alpha FIRST: the fade has to take the pill's backdrop with it, and everything after this
        // in the chain draws inside the layer it opens.
        modifier = modifier
            .alpha(alpha)
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
