package io.unom.punktfunk.console

import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbManager
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.app.ActivityCompat
import io.unom.punktfunk.ConsoleLicensesScreen
import io.unom.punktfunk.DS_USB_PERMISSION_ACTION
import io.unom.punktfunk.MainActivity
import io.unom.punktfunk.Settings
import io.unom.punktfunk.SettingsStore
import io.unom.punktfunk.kit.DsDevice
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.LibraryReturn
import io.unom.punktfunk.kit.Sc2BleLink
import io.unom.punktfunk.rememberConsoleHaptics
import io.unom.punktfunk.testRumble
import kotlin.math.roundToInt

/**
 * The gamepad/console UI drawn by the Skia shell (`crates/pf-console-ui`), hosted on a
 * `SurfaceView` this composable owns and driven through [SkiaConsole]. Same call shape as the
 * Compose `GamepadShell` it replaces (`App.kt` picks one by [SkiaConsole.wanted]).
 *
 * What lives here is only what needs a composition: the surface lifecycle, the safe-area insets,
 * the pad probes (raw pad → the shared menu synthesizer, over JNI), the system Back, the
 * platform-native sub-screen the console can open (Licences — Compose, drawn over the surface;
 * Connected controllers is the console's own Skia screen now), and the two intents the app hands
 * over on the way in (a deep link, "come back to this shelf").
 */
@Composable
fun SkiaConsoleShell(
    settings: Settings,
    onSettingsChange: (Settings) -> Unit,
    onConnected: (ActiveSession) -> Unit,
    deepLink: String? = null,
    onDeepLinkHandled: () -> Unit = {},
    reopenLibrary: LibraryReturn? = null,
    onReopenLibraryHandled: () -> Unit = {},
) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val handle = remember { SkiaConsole.ensure(context, settings) }
    val haptics = rememberConsoleHaptics()
    // A platform-native screen the console opened over itself (design D7): the console's own
    // input is held while it is up, and Back closes it.
    var platformScreen by remember { mutableStateOf<String?>(null) }

    val currentOnConnected by rememberUpdatedState(onConnected)
    val currentOnSettingsChange by rememberUpdatedState(onSettingsChange)
    DisposableEffect(handle) {
        SkiaConsole.attach(
            onConnected = { currentOnConnected(it) },
            onSettingsChange = { currentOnSettingsChange(it) },
            onQuit = { activity?.moveTaskToBack(true) },
            onPlatformScreen = { platformScreen = it },
            onPadAction = { action, key -> padAction(activity, action, key) },
            onPulse = { pulse ->
                when (pulse) {
                    "move" -> haptics.tick()
                    "confirm" -> haptics.confirm()
                    "boundary" -> haptics.boundary()
                }
            },
        )
        onDispose { SkiaConsole.detach() }
    }

    // Settings edited elsewhere (the touch UI shares the store) reach the shell on its next read.
    LaunchedEffect(settings) { SkiaConsole.settingsChanged(settings) }

    // "Come back to the shelf this game was launched from" — consumed once, on entry.
    LaunchedEffect(reopenLibrary) {
        val (id, pinId) = reopenLibrary ?: return@LaunchedEffect
        SkiaConsole.openLibrary(id, pinId)
        onReopenLibraryHandled()
    }
    // A `punktfunk://` link on the way in: consumed once; the bridge dials a known-and-pinned
    // host and refuses (with a notice) anything that would need a trust decision.
    LaunchedEffect(deepLink) {
        val url = deepLink ?: return@LaunchedEffect
        onDeepLinkHandled()
        SkiaConsole.handleDeepLink(url)
    }

    // The console owns the whole panel while it fronts the app, exactly like the stream: the
    // status bar and the gesture bar are hidden (a swipe shows them transiently). This is both the
    // space win AND the safe-area fix — hidden bars report zero insets, so the scroll clips that
    // used to end at the visible gesture-bar line now run to the panel edge. Only the display
    // cutout stays a real inset. The hide/show itself lives in App.kt (one owner; a per-screen
    // `onDispose { show }` fired after the stream's hide during the AnimatedContent cross-fade).

    // The safe area, in surface pixels: system bars ∪ display cutout — the NP3's landscape punch
    // is a SIDE inset, and the console's chrome must stay clear of it (its backdrop need not).
    // With the bars hidden above, this is normally just the cutout.
    val density = LocalDensity.current
    val ld = LocalLayoutDirection.current
    val insets = WindowInsets.systemBars.union(WindowInsets.displayCutout)
    val left = insets.getLeft(density, ld).toFloat()
    val top = insets.getTop(density).toFloat()
    val right = insets.getRight(density, ld).toFloat()
    val bottom = insets.getBottom(density).toFloat()
    // Design-unit scale: TVs take the couch formula (0 = the shell decides: a 4K panel is 2.7×,
    // the same 800-unit field as a Deck); a phone or tablet in the hand gets a density FLOOR
    // under that formula, so type never shrinks below what the touch UI draws at the same
    // density (design D5 — a bare height/800 on a 460 dpi phone lands ~26 % smaller than a Deck).
    // The 0.75 is the on-glass tuning knob — raised from 0.6 after a 460 dpi phone (Nothing
    // Phone) still read a step too small in the hand: the floor is what sets the phone scale
    // (the couch term only wins on tablets and TVs), so this is a phones-only bump.
    val tv = remember { io.unom.punktfunk.isTvDevice(context) }
    // The SurfaceView's own laid-out size, fed back by `onSizeChanged` below — deliberately not
    // `displayMetrics`. The reduced buffer's aspect ratio has to match the RECT it is scaled into
    // or the compositor stretches the whole interface, and while those two normally agree,
    // `displayMetrics` has a long history of disagreeing with a view's real size by a system bar
    // depending on the version and on who is currently hiding what. "Normally agree" is not
    // something to hang picture geometry on. Zero until the first layout, which is exactly what
    // `render` wants: the surface comes up at its natural size and is re-fixed a frame later.
    var viewW by remember { mutableStateOf(0) }
    var viewH by remember { mutableStateOf(0) }
    // "Reduce interface resolution" (`Settings.reduceUiResolution`): cap the console's BUFFER at
    // 1920 on its long edge and let the compositor scale it up to the panel. 1 means "draw at the
    // panel's own resolution" — the setting is off, or the display is already at or under 1080p
    // and there is nothing to give back.
    //
    // ONE factor on both axes, so the aspect ratio survives exactly and no layout can stretch.
    // Everything else in this function that speaks in SURFACE pixels multiplies by it — the insets
    // and design-unit scale just below, the pointer coordinates further down — because
    // `setFixedSize` shrinks the buffer WITHOUT shrinking the view: a mouse still reports its
    // position in view pixels, and handing those straight to a half-size surface would land the
    // cursor at twice its true offset.
    val render = if (!settings.reduceUiResolution) 1f else {
        val long = maxOf(viewW, viewH)
        if (long > 1920) 1920f / long else 1f
    }
    // The pointer listeners below are installed in `factory`, which runs ONCE — capturing `render`
    // directly would freeze them at its first-composition value (1, before the first layout has
    // reported a size), and a mouse would keep reporting view pixels into a half-size surface for
    // the rest of the session. Same reason `platformUp` is held this way.
    val currentRender by rememberUpdatedState(render)
    val dm = context.resources.displayMetrics
    val scale = if (tv) 0f else {
        val couch = minOf(dm.widthPixels, dm.heightPixels) / 800f
        // `render` too: the design-unit scale is in SURFACE pixels, so shrinking the buffer without
        // shrinking this would draw the type larger on screen than the same phone draws it today.
        maxOf(couch, density.density * 0.75f).coerceIn(0.75f, 3f) * render
    }
    LaunchedEffect(handle, left, top, right, bottom, scale, render) {
        if (handle != 0L) {
            NativeBridge.nativeConsoleSetViewport(
                handle,
                left * render,
                top * render,
                right * render,
                bottom * render,
                scale,
            )
        }
    }

    // The pad, raw, before MainActivity's B→Back and stick→D-pad synthesis: face buttons and the
    // stick/HAT become one MenuSample the shared synthesizer turns into menu events; a TV remote's
    // D-pad keys (not SOURCE_GAMEPAD) go in as discrete events; hardware keys as `Key`s.
    val padState = remember { PadState() }
    val platformUp by rememberUpdatedState(platformScreen != null)
    DisposableEffect(handle, activity) {
        if (activity == null || handle == 0L) return@DisposableEffect onDispose {}
        val keyProbe: (KeyEvent) -> Boolean = probe@{ ev ->
            if (platformUp) return@probe false
            val down = ev.action == KeyEvent.ACTION_DOWN
            if (ev.action != KeyEvent.ACTION_DOWN && ev.action != KeyEvent.ACTION_UP) return@probe false
            val fromPad = ev.isFromSource(InputDevice.SOURCE_GAMEPAD)
            if (fromPad) {
                // The CORRECTED keycode: a pad Android has no key layout for delivers its buttons
                // under other buttons' names, so read raw this console answered ✕ with whatever
                // sat in BUTTON_A's scancode slot. Same resolution the stream uses — the console
                // and the game must not disagree about which button a user pressed.
                val code = Gamepad.padKeyCode(ev)
                val bit = when (code) {
                    KeyEvent.KEYCODE_BUTTON_A -> 0
                    KeyEvent.KEYCODE_BUTTON_B -> 1
                    KeyEvent.KEYCODE_BUTTON_X -> 2
                    KeyEvent.KEYCODE_BUTTON_Y -> 3
                    KeyEvent.KEYCODE_BUTTON_L1 -> 4
                    KeyEvent.KEYCODE_BUTTON_R1 -> 5
                    else -> -1
                }
                if (bit >= 0) {
                    padState.button(bit, down)
                    padState.push(handle)
                    // MainActivity already noted the driving pad (lastPadDeviceId) before this
                    // probe ran; refresh the chip when the pad behind the buttons changes.
                    if (padState.deviceId != ev.deviceId) {
                        padState.deviceId = ev.deviceId
                        SkiaConsole.padsChanged(ev.device)
                    }
                    return@probe true
                }
                val dbit = when (code) {
                    KeyEvent.KEYCODE_DPAD_UP -> 0
                    KeyEvent.KEYCODE_DPAD_DOWN -> 1
                    KeyEvent.KEYCODE_DPAD_LEFT -> 2
                    KeyEvent.KEYCODE_DPAD_RIGHT -> 3
                    else -> -1
                }
                if (dbit >= 0) {
                    padState.dpad(dbit, down)
                    padState.push(handle)
                    return@probe true
                }
                if (code == KeyEvent.KEYCODE_BUTTON_SELECT && down && ev.repeatCount == 0) {
                    NativeBridge.nativeConsoleMenu(handle, 0) // ▲ opens the tile's options on Home
                    return@probe true
                }
                return@probe false
            }
            // A remote / keyboard. D-pad keys and DPAD_CENTER as discrete events with the
            // framework's own repeat; the rest as console keys; printable text while editing.
            if (!down) {
                return@probe when (ev.keyCode) {
                    KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN, KeyEvent.KEYCODE_DPAD_LEFT,
                    KeyEvent.KEYCODE_DPAD_RIGHT, KeyEvent.KEYCODE_DPAD_CENTER, KeyEvent.KEYCODE_ENTER,
                    KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_ESCAPE, KeyEvent.KEYCODE_TAB, KeyEvent.KEYCODE_SPACE,
                    KeyEvent.KEYCODE_DEL, KeyEvent.KEYCODE_PAGE_UP, KeyEvent.KEYCODE_PAGE_DOWN -> true
                    else -> false
                }
            }
            val repeat = ev.repeatCount > 0
            when (ev.keyCode) {
                KeyEvent.KEYCODE_DPAD_UP -> NativeBridge.nativeConsoleMenu(handle, 0)
                KeyEvent.KEYCODE_DPAD_DOWN -> NativeBridge.nativeConsoleMenu(handle, 1)
                KeyEvent.KEYCODE_DPAD_LEFT -> NativeBridge.nativeConsoleMenu(handle, 2)
                KeyEvent.KEYCODE_DPAD_RIGHT -> NativeBridge.nativeConsoleMenu(handle, 3)
                KeyEvent.KEYCODE_DPAD_CENTER -> if (!repeat) NativeBridge.nativeConsoleMenu(handle, 4)
                KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> NativeBridge.nativeConsoleKey(handle, 4, ev.isShiftPressed, repeat)
                KeyEvent.KEYCODE_SPACE -> NativeBridge.nativeConsoleKey(handle, 5, ev.isShiftPressed, repeat)
                KeyEvent.KEYCODE_ESCAPE -> NativeBridge.nativeConsoleKey(handle, 6, ev.isShiftPressed, repeat)
                KeyEvent.KEYCODE_BACK -> if (!repeat) NativeBridge.nativeConsoleMenu(handle, 5)
                KeyEvent.KEYCODE_DEL -> NativeBridge.nativeConsoleKey(handle, 7, ev.isShiftPressed, repeat)
                KeyEvent.KEYCODE_PAGE_UP -> NativeBridge.nativeConsoleKey(handle, 8, ev.isShiftPressed, repeat)
                KeyEvent.KEYCODE_PAGE_DOWN -> NativeBridge.nativeConsoleKey(handle, 9, ev.isShiftPressed, repeat)
                KeyEvent.KEYCODE_TAB -> NativeBridge.nativeConsoleKey(handle, 10, ev.isShiftPressed, repeat)
                else -> {
                    val ch = ev.unicodeChar
                    if (ch != 0 && !ev.isCtrlPressed && !ev.isAltPressed && ch >= 0x20) {
                        NativeBridge.nativeConsoleText(handle, String(Character.toChars(ch)))
                    } else {
                        return@probe false
                    }
                }
            }
            true
        }
        val motionProbe: (MotionEvent) -> Boolean = probe@{ ev ->
            if (platformUp) return@probe false
            if (!ev.isFromSource(InputDevice.SOURCE_JOYSTICK) && !ev.isFromSource(InputDevice.SOURCE_GAMEPAD)) {
                return@probe false
            }
            val lx = ev.getAxisValue(MotionEvent.AXIS_X)
            val ly = ev.getAxisValue(MotionEvent.AXIS_Y)
            val hx = ev.getAxisValue(MotionEvent.AXIS_HAT_X)
            val hy = ev.getAxisValue(MotionEvent.AXIS_HAT_Y)
            padState.stick(lx, ly)
            padState.hat(hx, hy)
            padState.push(handle)
            true
        }
        val probes = MainActivity.PadProbes(keyProbe, motionProbe)
        activity.pushPadProbes(probes)
        SkiaConsole.padsChanged(Gamepad.firstPad())
        onDispose {
            // Remove OUR claim only — a platform screen pushed over us keeps its own, and when it
            // pops, this one resurfaces (the stack is what fixed the pad dying after Controllers).
            activity.removePadProbes(probes)
            padState.reset()
            if (handle != 0L) padState.push(handle)
        }
    }

    // The system Back (gesture or key) is the console's B; at its root the shell raises Quit.
    BackHandler(enabled = platformScreen == null) {
        if (handle != 0L) NativeBridge.nativeConsoleMenu(handle, 5)
    }

    Box(Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier
                .fillMaxSize()
                .onSizeChanged { viewW = it.width; viewH = it.height },
            factory = { ctx ->
                SurfaceView(ctx).apply {
                    // The console draws opaque, edge to edge; Compose overlays sit above it.
                    setZOrderMediaOverlay(false)
                    isFocusable = false
                    isFocusableInTouchMode = false
                    holder.addCallback(object : SurfaceHolder.Callback {
                        override fun surfaceCreated(h: SurfaceHolder) {
                            if (handle != 0L) NativeBridge.nativeConsoleSurfaceCreated(handle, h.surface)
                        }
                        override fun surfaceChanged(h: SurfaceHolder, format: Int, width: Int, height: Int) {
                            if (handle != 0L) NativeBridge.nativeConsoleSurfaceChanged(handle)
                        }
                        override fun surfaceDestroyed(h: SurfaceHolder) {
                            if (handle != 0L) NativeBridge.nativeConsoleSurfaceDestroyed(handle)
                        }
                    })
                    // Touch → the console's pointer (surface pixels): the escape hatch when no
                    // pad is attached, and the natural way to press a legend hint on a phone.
                    // A finger's down is kind 6 (the shell defers it so a swipe scrolls); a
                    // mouse — which Android delivers through this same listener — keeps kind 1
                    // and acts on the press, as a mouse should.
                    setOnTouchListener { v, ev ->
                        if (handle == 0L) return@setOnTouchListener false
                        val kind = when (ev.actionMasked) {
                            MotionEvent.ACTION_DOWN ->
                                if (ev.getToolType(0) == MotionEvent.TOOL_TYPE_MOUSE) 1 else 6
                            MotionEvent.ACTION_MOVE -> 0
                            MotionEvent.ACTION_UP -> 2
                            MotionEvent.ACTION_CANCEL -> 5
                            else -> return@setOnTouchListener false
                        }
                        // View pixels → SURFACE pixels (see `render` above).
                        NativeBridge.nativeConsolePointer(handle, kind, ev.x * currentRender, ev.y * currentRender, 0f)
                        if (ev.actionMasked == MotionEvent.ACTION_UP) v.performClick()
                        true
                    }
                    setOnGenericMotionListener { _, ev ->
                        if (handle != 0L && ev.actionMasked == MotionEvent.ACTION_SCROLL &&
                            ev.isFromSource(InputDevice.SOURCE_CLASS_POINTER)
                        ) {
                            NativeBridge.nativeConsolePointer(handle, 4, ev.x * currentRender, ev.y * currentRender, ev.getAxisValue(MotionEvent.AXIS_VSCROLL))
                            true
                        } else false
                    }
                    importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
                }
            },
            // Applied here rather than in `factory` so flipping the setting takes effect without
            // leaving the console: `setFixedSize` re-creates the buffer and the render thread
            // re-wraps it through the ordinary surfaceChanged path. `setSizeFromLayout` is the
            // documented way back to "the view's own size" when the setting goes off again.
            update = { view ->
                if (render < 1f) {
                    view.holder.setFixedSize(
                        (viewW * render).roundToInt().coerceAtLeast(1),
                        (viewH * render).roundToInt().coerceAtLeast(1),
                    )
                } else {
                    view.holder.setSizeFromLayout()
                }
            },
        )
        when (platformScreen) {
            "licenses" -> ConsoleLicensesScreen(onBack = { platformScreen = null }, navActive = true)
        }
    }
}

/**
 * A `ConsoleCmd::PadAction` from the console's Connected-controllers screen — the handful of
 * things only the platform can do: a rumble pulse on the real [InputDevice], the USB/Bluetooth
 * grant dialogs, the DualSense pad-audio self test. The touch Controllers screen keeps its own
 * buttons for the same actions; both routes end in the same helpers ([testRumble], the grant
 * intents, `nativePadAudioSelfTest`), so the support answer cannot drift between interfaces.
 * Runs on the main thread (the command drain lives there); results ride [SkiaConsole.notice].
 */
private fun padAction(activity: MainActivity?, action: String, padKey: String) {
    if (activity == null) return
    val settings = SettingsStore(activity).load()
    val usb = activity.getSystemService(Context.USB_SERVICE) as UsbManager
    when (action) {
        "rumble" ->
            Gamepad.pads()
                .firstOrNull { "${it.vendorId}:${it.productId}:${it.name}" == padKey }
                ?.let(::testRumble)
        "sc2_bluetooth" -> when {
            !settings.sc2Capture ->
                SkiaConsole.notice("Enable \"Steam Controller 2 passthrough\" in Settings first.")
            Sc2BleLink.permissionGranted(activity) ->
                SkiaConsole.notice("Bluetooth access is already granted.")
            // The system dialog pauses the activity; onResume re-probes and engages the capture,
            // the same way the menu-time auto-ask completes.
            else -> Sc2BleLink.CONNECT_PERMISSION?.let {
                ActivityCompat.requestPermissions(activity, arrayOf(it), 5)
            }
        }
        "sc2_usb" ->
            if (!settings.sc2Capture) {
                SkiaConsole.notice("Enable \"Steam Controller 2 passthrough\" in Settings first.")
            } else {
                // Asks for the USB grant when one is missing and engages the capture on it.
                activity.startSc2MenuNav(forceAsk = true)
            }
        "ds_usb" -> {
            val dev = usb.deviceList.values.firstOrNull {
                it.vendorId == DsDevice.VID_SONY && it.productId in DsDevice.USB_PIDS
            }
            when {
                !settings.dsCapture ->
                    SkiaConsole.notice(
                        "Enable \"DualSense / DualShock passthrough (USB)\" in Settings first.",
                    )
                dev == null -> SkiaConsole.notice("No wired DualSense or DualShock 4 detected.")
                usb.hasPermission(dev) -> SkiaConsole.notice("USB access is already granted.")
                else -> usb.requestPermission(
                    dev,
                    PendingIntent.getBroadcast(
                        activity, 3, // requestCode 3 — shared with the touch card's button
                        Intent(DS_USB_PERMISSION_ACTION).setPackage(activity.packageName),
                        // MUTABLE: the USB stack appends the grant extras to this intent.
                        PendingIntent.FLAG_MUTABLE,
                    ),
                )
            }
        }
        "ds_haptics" -> {
            val dev = usb.deviceList.values.firstOrNull {
                it.vendorId == DsDevice.VID_SONY && it.productId in DsDevice.USB_PIDS
            }
            when {
                dev == null -> SkiaConsole.notice("No wired DualSense detected.")
                DsDevice.modelFor(dev.productId) == DsDevice.Model.DUALSHOCK4 ->
                    SkiaConsole.notice("The DualShock 4 has no haptics audio device.")
                !usb.hasPermission(dev) -> SkiaConsole.notice("Grant USB access first.")
                else -> Thread({
                    // Its OWN connection: the renderer's descriptor must never be shared with
                    // another transfer engine, and that applies to this test as much as to the
                    // real path (same rule as the touch card's test).
                    val conn = runCatching { usb.openDevice(dev) }.getOrNull()
                    val fd = conn?.fileDescriptor ?: -1
                    val r = if (fd >= 0) NativeBridge.nativePadAudioSelfTest(fd, 3, 60) else -1
                    conn?.close()
                    SkiaConsole.notice(
                        when {
                            r > 0 -> "Haptics test passed — $r frames to the pad."
                            r == -1 ->
                                "Could not open the pad's audio interface. Some kernels " +
                                    "refuse it; the pad still works normally."
                            r == -2 -> "The audio stream stopped part-way."
                            else -> "The stream opened but no audio reached the pad."
                        },
                    )
                }, "pf-pad-selftest-console").start()
            }
        }
    }
}

/** The raw pad as one `MenuSample`, pushed whenever any part of it changes. */
private class PadState {
    var deviceId = -1
    private var buttons = 0
    private var dpad = 0
    private var lx = 0
    private var ly = 0
    private var last: IntArray? = null

    fun button(bit: Int, down: Boolean) {
        buttons = if (down) buttons or (1 shl bit) else buttons and (1 shl bit).inv()
    }

    fun dpad(bit: Int, down: Boolean) {
        dpad = if (down) dpad or (1 shl bit) else dpad and (1 shl bit).inv()
    }

    fun stick(x: Float, y: Float) {
        lx = (x.coerceIn(-1f, 1f) * 32767f).roundToInt()
        ly = (y.coerceIn(-1f, 1f) * 32767f).roundToInt()
    }

    /** The HAT is the D-pad on most pads' motion path (±1 per axis). */
    fun hat(x: Float, y: Float) {
        dpad(2, x <= -0.5f); dpad(3, x >= 0.5f); dpad(0, y <= -0.5f); dpad(1, y >= 0.5f)
    }

    fun reset() {
        buttons = 0; dpad = 0; lx = 0; ly = 0
    }

    fun push(handle: Long) {
        val now = intArrayOf(buttons, lx, ly, dpad)
        if (last?.contentEquals(now) == true) return
        last = now
        NativeBridge.nativeConsolePadSample(handle, buttons, lx, ly, dpad)
    }
}
