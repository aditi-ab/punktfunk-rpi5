package io.unom.punktfunk.console

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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.viewinterop.AndroidView
import io.unom.punktfunk.ConsoleControllersScreen
import io.unom.punktfunk.ConsoleLicensesScreen
import io.unom.punktfunk.MainActivity
import io.unom.punktfunk.Settings
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.LibraryReturn
import io.unom.punktfunk.rememberConsoleHaptics
import kotlin.math.roundToInt

/**
 * The gamepad/console UI drawn by the Skia shell (`crates/pf-console-ui`), hosted on a
 * `SurfaceView` this composable owns and driven through [SkiaConsole]. Same call shape as the
 * Compose `GamepadShell` it replaces (`App.kt` picks one by [SkiaConsole.wanted]).
 *
 * What lives here is only what needs a composition: the surface lifecycle, the safe-area insets,
 * the pad probes (raw pad → the shared menu synthesizer, over JNI), the system Back, the
 * platform-native sub-screens the console can open (Controllers, Licences — Compose, drawn over the
 * surface), and the two intents the app hands over on the way in (a deep link, "come back to this
 * shelf").
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

    // The safe area, in surface pixels: system bars ∪ display cutout — the NP3's landscape punch
    // is a SIDE inset, and the console's chrome must stay clear of it (its backdrop need not).
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
    // The 0.6 is the on-glass tuning knob.
    val tv = remember { io.unom.punktfunk.isTvDevice(context) }
    val scale = if (tv) 0f else {
        val dm = context.resources.displayMetrics
        val couch = minOf(dm.widthPixels, dm.heightPixels) / 800f
        maxOf(couch, density.density * 0.6f).coerceIn(0.75f, 3f)
    }
    LaunchedEffect(handle, left, top, right, bottom, scale) {
        if (handle != 0L) NativeBridge.nativeConsoleSetViewport(handle, left, top, right, bottom, scale)
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
                val bit = when (ev.keyCode) {
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
                val dbit = when (ev.keyCode) {
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
                if (ev.keyCode == KeyEvent.KEYCODE_BUTTON_SELECT && down && ev.repeatCount == 0) {
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
            modifier = Modifier.fillMaxSize(),
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
                    setOnTouchListener { v, ev ->
                        if (handle == 0L) return@setOnTouchListener false
                        val kind = when (ev.actionMasked) {
                            MotionEvent.ACTION_DOWN -> 1
                            MotionEvent.ACTION_MOVE -> 0
                            MotionEvent.ACTION_UP -> 2
                            MotionEvent.ACTION_CANCEL -> 5
                            else -> return@setOnTouchListener false
                        }
                        NativeBridge.nativeConsolePointer(handle, kind, ev.x, ev.y, 0f)
                        if (ev.actionMasked == MotionEvent.ACTION_UP) v.performClick()
                        true
                    }
                    setOnGenericMotionListener { _, ev ->
                        if (handle != 0L && ev.actionMasked == MotionEvent.ACTION_SCROLL &&
                            ev.isFromSource(InputDevice.SOURCE_CLASS_POINTER)
                        ) {
                            NativeBridge.nativeConsolePointer(handle, 4, ev.x, ev.y, ev.getAxisValue(MotionEvent.AXIS_VSCROLL))
                            true
                        } else false
                    }
                    importantForAccessibility = View.IMPORTANT_FOR_ACCESSIBILITY_NO
                }
            },
        )
        when (platformScreen) {
            "controllers" -> ConsoleControllersScreen(
                gamepadSetting = settings.gamepad,
                onBack = { platformScreen = null },
                navActive = true,
            )
            "licenses" -> ConsoleLicensesScreen(onBack = { platformScreen = null }, navActive = true)
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
