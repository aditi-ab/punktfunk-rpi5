package io.unom.punktfunk

import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Bundle
import android.view.InputDevice
import android.view.KeyCharacterMap
import android.view.KeyEvent
import android.view.MotionEvent
import android.widget.Toast
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.systemBars
import androidx.compose.material3.Surface
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.GamepadRouter
import io.unom.punktfunk.kit.Keymap
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.security.KnownHostStore

/** Broadcast action for the menu-time SC2 USB-permission grant (see [MainActivity.startSc2MenuNav]). */
private const val SC2_MENU_PERMISSION = "io.unom.punktfunk.SC2_MENU_USB_PERMISSION"

/**
 * Keeps ONE window-insets reader alive for as long as the app's UI exists — the fix for the menus
 * coming back from a stream laid out against the WRONG safe area.
 *
 * Compose attaches its `OnApplyWindowInsets` and `WindowInsetsAnimation` callbacks when the first
 * composable reads an inset, and removes them again when the last reader goes away
 * (`WindowInsetsHolder.increment/decrementAccessors`). [StreamScreen] reads no insets at all — it's
 * a bare full-screen surface — so a stream drops the reader count to zero for its whole duration.
 *
 * That alone is survivable; what isn't is a session that ends while the app is BACKGROUNDED, which
 * is the common case (leaving the app ends the session — see StreamScreen's ON_STOP observer). The
 * whole window restore — `show(systemBars())`, releasing the landscape lock — then runs on a stopped
 * activity, and the corrected insets that follow arrive while Compose has no listener attached. When
 * the menus recompose, `incrementAccessors` re-attaches and asks for a fresh pass, but a stopped
 * window produces no dispatch, and on resume nothing has *changed* any more, so none ever comes.
 * Compose keeps serving what it last saw: the landscape, bars-hidden values.
 *
 * That's exactly what the reporter's phone showed (on-glass 2026-07-29, verified by dump): the
 * platform reported `bars=[0,162,0,72] cutout=[0,162,0,0]` for the window while the layout was still
 * using the landscape immersive set — cutout `left=162` (Material3 lays out against
 * `systemBars.union(displayCutout)`), bars all zero. Content shoved right by the landscape cutout,
 * nothing kept clear of the status bar or the gesture pill, and no rotation or IME animation could
 * shake it loose. A/B'd over eight runs of the real teardown sequence: 3 of 4 wrong without this,
 * 4 of 4 correct with it.
 *
 * Reading an inset here holds the count above zero for the activity's whole life, so the listeners
 * survive the stream and every dispatch lands. It subscribes to no inset VALUE (only the holder
 * object), so it triggers no recomposition — the cost is one DisposableEffect.
 */
@Composable
private fun HoldWindowInsetsListeners() {
    // The read itself IS the registration (the accessor is scoped to this composable, which never
    // leaves the composition); `remember` is only what keeps it from being a value nobody uses.
    remember(WindowInsets.systemBars) {}
}

class MainActivity : ComponentActivity() {
    /**
     * The active stream session handle (0 = not streaming). Set by [StreamScreen] while it's shown.
     * `dispatchKeyEvent` is the earliest, most reliable key hook — above Compose's focus system —
     * so hardware keys are forwarded to the host regardless of which view holds focus.
     */
    var streamHandle: Long = 0L

    /**
     * Multi-controller router for the active session (built/released by StreamScreen): assigns each
     * connected pad a stable wire index, threads it onto every event, declares/removes pads on
     * hot-plug, and routes rumble/HID feedback back by pad index. Null while not streaming.
     */
    var gamepadRouter: GamepadRouter? = null

    /**
     * Input observers for the Controllers debug screen (set while it is shown, like [streamHandle]).
     * Called for every key/motion event while not streaming; a `true` return consumes the event —
     * the screen's "test inputs" mode uses that to keep pad input from also driving focus navigation.
     */
    var padKeyProbe: ((KeyEvent) -> Boolean)? = null
    var padMotionProbe: ((MotionEvent) -> Boolean)? = null

    /**
     * Physical-mouse forwarder for the active session (built/released by StreamScreen, like
     * [gamepadRouter]): uncaptured hover/click/wheel forwards as absolute cursor input, captured
     * ([android.view.View.requestPointerCapture]) raw deltas as relative mouse-look. The dispatch
     * overrides below route every SOURCE_MOUSE event here while streaming. Null while not streaming.
     */
    var mouseForwarder: MouseForwarder? = null

    /**
     * TV remote-as-pointer for the active session (StreamScreen builds it on TV devices only):
     * hold SELECT to toggle, then the D-pad glides the host cursor. Consulted first for
     * non-gamepad keys while streaming. Null while not streaming or not a TV.
     */
    var remotePointer: RemotePointer? = null

    /**
     * Set by [StreamScreen] to its disconnect action. The emergency-exit chord (below) invokes it so a
     * couch user with no keyboard/Back can always leave a stream.
     */
    var requestStreamExit: (() -> Unit)? = null

    /**
     * Whether the last console input came from a real gamepad (face buttons / stick) vs. a TV D-pad
     * remote (which has no A/B/X/Y). The console UI reads this to show glyphs the user recognises — pad
     * face buttons, or a select glyph + arrows for a remote. Compose observes it (a snapshot state).
     * Defaults to the remote glyphs on a TV (its D-pad remote is the typical first input, and often the
     * only one) and to gamepad glyphs everywhere else (the console UI on a phone/tablet only activates
     * via a real controller, so a TV-remote glyph would be a wrong first impression there) — set from
     * [onCreate] once a [Context] is available, then kept live by real input.
     */
    var lastPadIsGamepad by mutableStateOf(true)
        private set

    /**
     * The glyph family of the controller driving the console UI (Xbox letters / PlayStation shapes /
     * Nintendo monochrome) — seeded from the first connected pad, then kept live by real input the
     * same way [lastPadIsGamepad] is. Compose observes it (a snapshot state); the hint bar picks its
     * button glyphs from it so a DualSense user isn't shown Xbox lettering.
     */
    var lastPadStyle by mutableStateOf(Gamepad.PadStyle.GENERIC)
        private set

    /**
     * A `punktfunk://` URL waiting to be routed — set from the VIEW intent that started (or
     * re-entered) this activity, cleared by whoever handles it. Compose observes it.
     *
     * Read in BOTH [onCreate] and [onNewIntent] on purpose: `launchMode` is `standard`, so a second
     * link usually arrives as a fresh activity instance (onCreate) and only sometimes as a new
     * intent on this one (a caller that set `FLAG_ACTIVITY_SINGLE_TOP`). A link arriving while an
     * earlier one is still unhandled replaces it — the user's latest intent is the live one.
     */
    var pendingDeepLink by mutableStateOf<String?>(null)

    /** The panel's highest-refresh display mode (0 = unknown/unsupported), resolved once at startup. */
    private var highRefreshModeId = 0

    /**
     * Menu-time Steam Controller 2 capture (UI mode — no router): a captured SC2 never produces
     * ordinary gamepad events (lizard mode is kb/mouse; the claim removes even those), so this
     * drives the console UI directly from the parsed reports via [sc2NavKey]. Runs while the app
     * is foreground and NOT streaming; StreamScreen pauses it around its own stream-mode capture.
     * [sc2MenuActive] is observed by the console-UI gate ([rememberControllerConnected]) and the
     * Controllers screen.
     */
    private var sc2Menu: io.unom.punktfunk.kit.Sc2Capture? = null
    var sc2MenuActive by mutableStateOf(false)
        private set
    private var sc2Receiver: BroadcastReceiver? = null
    private var sc2PermissionAsked = false

    /**
     * Compose focus hook for the SC2's synthetic D-pad (set by [onCreate]'s composition). A
     * synthetic KeyEvent dispatched from OUTSIDE the real input pipeline never reaches
     * ViewRootImpl's focus-navigation stage — the one that grants initial focus for a real
     * pad's first D-pad press — so on a phone in touch mode it lands on a focus-less window
     * and does nothing (first on-glass run: only B worked, since it bypasses key events
     * entirely). `FocusManager.moveFocus` is the public API for exactly this.
     */
    private var sc2MoveFocus: ((androidx.compose.ui.focus.FocusDirection) -> Boolean)? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A URL may never preempt a live session (design/client-deep-links.md §3.2). With
        // `launchMode = standard` a link normally arrives as a NEW activity instance in a new task
        // — the streaming one gets backgrounded, and backgrounding ends a session — so the refusal
        // has to happen HERE, before this instance is resumed, not inside the composition (which
        // only ever sees the rare `onNewIntent` case). Finishing now leaves the streaming task in
        // front, untouched.
        val live = liveStream
        if (live != null && deepLinkFrom(intent) != null) {
            // Pointing at the host already being streamed is the one exception, and its right
            // answer is to do nothing: the intent has already brought the app forward, which is
            // what "focus it" means here.
            if (!targetsHost(intent, live)) {
                Toast.makeText(
                    this,
                    "Already streaming — end this session first.",
                    Toast.LENGTH_LONG,
                ).show()
            }
            finish()
            return
        }
        pendingDeepLink = deepLinkFrom(intent)
        lastPadIsGamepad = !isTvDevice(this)
        lastPadStyle = Gamepad.styleFor(Gamepad.firstPad())
        resolveHighRefreshMode()
        setConsoleHighRefreshRate(true) // the console UI wants max refresh; streaming manages its own
        // Dark, transparent system bars regardless of the system theme — our UI is always dark, so
        // the status/nav bars blend with our surface and get light icons. (The no-arg edge-to-edge
        // picks the *system* light/dark, which left a black status bar over our dark background.)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
            navigationBarStyle = SystemBarStyle.dark(android.graphics.Color.TRANSPARENT),
        )
        // Dev escape hatch (mirrors the Apple client's PUNKTFUNK_FORCE_GAMEPAD_UI): force the console
        // UI without a physical pad — `adb shell am start -n io.unom.punktfunk/.MainActivity --ez
        // pf_force_gamepad_ui true`. Never set in normal use; real activation is a connected pad / TV.
        val forceGamepadUi = intent?.getBooleanExtra("pf_force_gamepad_ui", false) ?: false
        // SC2 hot-plug + the menu-time USB-permission grant both (re)start the menu capture.
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(c: Context?, intent: Intent?) {
                when (intent?.action) {
                    UsbManager.ACTION_USB_DEVICE_ATTACHED -> {
                        sc2PermissionAsked = false // a fresh attach may ask once again
                        startSc2MenuNav()
                    }
                    SC2_MENU_PERMISSION -> {
                        if (intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)) {
                            startSc2MenuNav()
                        }
                    }
                }
            }
        }
        sc2Receiver = receiver
        val filter = IntentFilter().apply {
            addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED)
            addAction(SC2_MENU_PERMISSION)
        }
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(receiver, filter, Context.RECEIVER_NOT_EXPORTED)
        } else {
            @Suppress("UnspecifiedRegisterReceiverFlag")
            registerReceiver(receiver, filter)
        }
        setContent {
            PunktfunkTheme {
                HoldWindowInsetsListeners()
                // Focus hook for the SC2's synthetic navigation (see [sc2MoveFocus]). `Next` is
                // the bootstrap: directional moves need an already-focused node, while one-
                // dimensional traversal assigns initial focus when there is none.
                val focusManager = androidx.compose.ui.platform.LocalFocusManager.current
                androidx.compose.runtime.DisposableEffect(Unit) {
                    sc2MoveFocus = { dir ->
                        focusManager.moveFocus(dir) ||
                            focusManager.moveFocus(androidx.compose.ui.focus.FocusDirection.Next)
                    }
                    onDispose { sc2MoveFocus = null }
                }
                Surface(modifier = Modifier.fillMaxSize()) { App(forceGamepadUi = forceGamepadUi) }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        // Keep `getIntent()` truthful for anything that reads it later (the gamepad-UI dev flag).
        setIntent(intent)
        deepLinkFrom(intent)?.let { pendingDeepLink = it }
    }

    /**
     * The `punktfunk://` URL of a VIEW intent, or null. Only VIEW: the launcher's MAIN intent
     * carries no data, and nothing else may inject a URL into the router.
     */
    private fun deepLinkFrom(intent: Intent?): String? =
        intent?.takeIf { it.action == Intent.ACTION_VIEW }?.data?.toString()

    override fun onResume() {
        super.onResume()
        startSc2MenuNav()
    }

    override fun onPause() {
        // Release the claim while backgrounded so the OS (and other apps) get the pad back.
        stopSc2MenuNav()
        super.onPause()
    }

    override fun onDestroy() {
        sc2Receiver?.let { runCatching { unregisterReceiver(it) } }
        sc2Receiver = null
        stopSc2MenuNav()
        super.onDestroy()
    }

    /**
     * Engage the menu-time SC2 capture if possible: setting on, not streaming, and a wired/Puck
     * pad attached (asking for USB permission at most once per attach — [forceAsk] re-arms the
     * dialog, for the Controllers screen's explicit grant button) — else an already-paired BLE
     * controller when BLUETOOTH_CONNECT is granted. Safe to call repeatedly.
     */
    fun startSc2MenuNav(forceAsk: Boolean = false) {
        if (forceAsk) sc2PermissionAsked = false
        if (streamHandle != 0L) return // StreamScreen owns the pad while streaming
        if (sc2Menu?.isActive == true) return
        if (!SettingsStore(this).load().sc2Capture) return
        val cap = sc2Menu ?: io.unom.punktfunk.kit.Sc2Capture(this).also { c ->
            c.onUiKey = { key, down -> runOnUiThread { sc2NavKey(key, down) } }
            c.onActiveChanged = { on -> runOnUiThread { sc2MenuActive = on } }
            sc2Menu = c
        }
        val usbManager = getSystemService(Context.USB_SERVICE) as UsbManager
        val dev = cap.findUsbDevice()
        when {
            dev != null && usbManager.hasPermission(dev) -> cap.startUsb(dev)
            dev != null && !sc2PermissionAsked -> {
                sc2PermissionAsked = true
                usbManager.requestPermission(
                    dev,
                    PendingIntent.getBroadcast(
                        this, 1,
                        Intent(SC2_MENU_PERMISSION).setPackage(packageName),
                        // MUTABLE: the USB stack appends the grant extras to this intent.
                        PendingIntent.FLAG_MUTABLE,
                    ),
                )
            }
            dev == null && checkSelfPermission(android.Manifest.permission.BLUETOOTH_CONNECT) ==
                PackageManager.PERMISSION_GRANTED -> {
                cap.pairedBleAddress()?.let { cap.startBle(it) }
            }
        }
    }

    /** Release the menu-time SC2 capture (backgrounded / stream taking over). Idempotent. */
    fun stopSc2MenuNav() {
        sc2Menu?.stop()
        sc2MenuActive = false
    }

    /**
     * One SC2 navigation key transition from the menu-time capture (main thread) — routed the
     * same way [dispatchKeyEvent]'s not-streaming branch routes a real pad's buttons: B backs,
     * A activates the focused element, everything else (D-pad, shoulders, Start/Select) goes to
     * the framework's focus navigation. Also claims the console-UI glyphs for the pad.
     */
    private fun sc2NavKey(keyCode: Int, down: Boolean) {
        if (streamHandle != 0L) return // raced a stream start — the wire path owns input now
        lastPadIsGamepad = true
        lastPadStyle = Gamepad.PadStyle.XBOX // Valve pads carry A/B/X/Y in Xbox positions
        val action = if (down) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP
        // The console UI navigates through padKeyProbe (GamepadNavEffect's held-state + repeat
        // machinery — A/X/Y/D-pad/Select), NOT the focus system: synthesized events must be
        // offered there first, exactly like real ones in dispatchKeyEvent (tester-diagnosed:
        // routing everything via super.dispatchKeyEvent bypassed the probe, so only B — which
        // never rides key events — did anything). The probes gate on keycode only, so a
        // synthetic KeyEvent satisfies them.
        padKeyProbe?.let { if (it(KeyEvent(action, keyCode))) return }
        when (keyCode) {
            // B → back, on release (same edge the real-pad path uses).
            KeyEvent.KEYCODE_BUTTON_B -> if (!down) onBackPressedDispatcher.onBackPressed()
            // A → activate the focused element (the focus system understands DPAD_CENTER; the
            // Compose node focused via the moveFocus hook receives it once the ComposeView
            // holds view-focus).
            KeyEvent.KEYCODE_BUTTON_A ->
                super.dispatchKeyEvent(KeyEvent(action, KeyEvent.KEYCODE_DPAD_CENTER))
            // D-pad → Compose's own focus API (a synthetic DPAD KeyEvent can't grant initial
            // focus — see [sc2MoveFocus]); one move per press edge.
            KeyEvent.KEYCODE_DPAD_UP -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Up)
            KeyEvent.KEYCODE_DPAD_DOWN -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Down)
            KeyEvent.KEYCODE_DPAD_LEFT -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Left)
            KeyEvent.KEYCODE_DPAD_RIGHT -> if (down) moveSc2Focus(androidx.compose.ui.focus.FocusDirection.Right)
            else -> super.dispatchKeyEvent(KeyEvent(action, keyCode))
        }
    }

    private fun moveSc2Focus(dir: androidx.compose.ui.focus.FocusDirection) {
        val hook = sc2MoveFocus
        if (hook == null || !hook(dir)) {
            // No composition hook (shouldn't happen) — fall back to the raw key dispatch.
            super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, dirToKey(dir)))
            super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_UP, dirToKey(dir)))
        }
    }

    private fun dirToKey(dir: androidx.compose.ui.focus.FocusDirection): Int = when (dir) {
        androidx.compose.ui.focus.FocusDirection.Up -> KeyEvent.KEYCODE_DPAD_UP
        androidx.compose.ui.focus.FocusDirection.Down -> KeyEvent.KEYCODE_DPAD_DOWN
        androidx.compose.ui.focus.FocusDirection.Left -> KeyEvent.KEYCODE_DPAD_LEFT
        else -> KeyEvent.KEYCODE_DPAD_RIGHT
    }

    /** Resolve the panel's highest-refresh mode (same resolution) once, for [setConsoleHighRefreshRate]. */
    private fun resolveHighRefreshMode() {
        @Suppress("DEPRECATION")
        val disp = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) display else windowManager.defaultDisplay
        highRefreshModeId = disp?.supportedModes?.maxWithOrNull(
            compareBy({ it.refreshRate }, { it.physicalWidth * it.physicalHeight }),
        )?.modeId ?: 0
    }

    /**
     * Opt the CONSOLE UI into the panel's highest refresh mode. Some OEMs (Nothing OS among them) pin
     * third-party apps to 60Hz unless they explicitly ask for more, which halves the smoothness of the
     * UI's scrolling/animation on a 120/144Hz panel. [StreamScreen] turns this OFF while streaming so
     * its own `ANativeWindow_setFrameRate` (matched to the video) governs the panel instead.
     */
    fun setConsoleHighRefreshRate(high: Boolean) {
        if (highRefreshModeId == 0) return
        window.attributes = window.attributes.apply {
            preferredDisplayModeId = if (high) highRefreshModeId else 0
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
                    // The router forwards the bit on this device's own wire pad index and tracks held
                    // state per pad. The emergency-exit chord (Select + Start + L1 + R1) is handled
                    // inside the router: holding it briefly (~1 s, with an on-screen hint) fires
                    // router.onExitChord (wired in StreamScreen), so a couch user with no keyboard/Back
                    // can still leave — but an accidental brush of the four buttons no longer quits.
                    gamepadRouter?.onButton(event, bit)
                    return true // consumed
                }
            }
            // A mouse's side buttons, when they arrive key-shaped, are X1/X2 — not navigation.
            // Resolved before the remote-pointer hook so pointer mode can't eat them as its own
            // BACK. See [mouseSideButton] for how a mouse's BACK is told from a remote's.
            mouseSideButton(event)?.let { back ->
                when (event.action) {
                    KeyEvent.ACTION_DOWN ->
                        if (event.repeatCount == 0) mouseForwarder?.sideButtonKey(back, true)
                    KeyEvent.ACTION_UP -> mouseForwarder?.sideButtonKey(back, false)
                }
                return true
            }
            // TV remote-as-pointer sees non-gamepad keys first (SELECT long-press toggles it;
            // while active it owns the D-pad/SELECT/PLAY-PAUSE/BACK).
            if (!event.isFromSource(InputDevice.SOURCE_GAMEPAD)) {
                remotePointer?.let { if (it.onKey(event)) return true }
            }
            // Ctrl+Alt+Shift+Q — the cross-client pointer-capture toggle chord. Swallow both
            // edges of the Q (the modifiers already went over the wire, exactly like desktop).
            if (event.keyCode == KeyEvent.KEYCODE_Q &&
                event.isCtrlPressed && event.isAltPressed && event.isShiftPressed
            ) {
                if (event.action == KeyEvent.ACTION_DOWN && event.repeatCount == 0) {
                    mouseForwarder?.toggleCapture()
                }
                return true
            }
            when (event.keyCode) {
                // Whatever [mouseSideButton] didn't claim. A view-level FALLBACK BACK appears when
                // a BUTTON_* press goes unconsumed, and an air-mouse remote stamps its own BACK
                // SOURCE_MOUSE; both are duplicates of something already handled, and letting
                // either through doubles as Android navigation and yanks the user out of the
                // stream. A remote/keyboard BACK is never mouse-sourced, so it still falls through
                // to the BackHandler and exits.
                KeyEvent.KEYCODE_BACK, KeyEvent.KEYCODE_FORWARD ->
                    if (event.isFromSource(InputDevice.SOURCE_MOUSE) ||
                        event.flags and KeyEvent.FLAG_FALLBACK != 0
                    ) {
                        return true
                    }
                // Leave these to the system even while streaming.
                // (BACK above → BackHandler leaves the stream.)
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
                    // Full-event overload: evdev scancode first (positional under ANY selected
                    // physical-keyboard layout), keycode fallback — see Keymap docs.
                    val vk = Keymap.toVk(event)
                    if (vk != 0) {
                        // Soft-keyboard events (the IME's virtual device — the stream's
                        // KeyCaptureView path) carry Shift only as META state, where a real
                        // keyboard sends discrete Shift transitions — so mirror the meta bit as
                        // a VK_LSHIFT wrap or every IME capital/symbol lands unshifted on the
                        // host. Never applied to hardware events: their Shift already went over
                        // the wire, and a synthetic release here would un-hold a physical Shift
                        // the user is still pressing.
                        val imeShift = event.deviceId == KeyCharacterMap.VIRTUAL_KEYBOARD &&
                            event.isShiftPressed && vk != 0xA0 && vk != 0xA1
                        if (down && imeShift) NativeBridge.nativeSendKey(handle, 0xA0, true, 0)
                        NativeBridge.nativeSendKey(handle, vk, down, 0)
                        if (!down && imeShift) NativeBridge.nativeSendKey(handle, 0xA0, false, 0)
                        return true // consumed — don't let the system also act on it
                    }
                }
            }
        } else {
            // Note which input the console UI is being driven by, so its glyphs match (a TV remote's
            // D-pad is not from SOURCE_GAMEPAD; a pad's face buttons / D-pad are) — and, for a real
            // pad, WHICH pad family, so the glyphs wear its lettering/shapes.
            if (event.action == KeyEvent.ACTION_DOWN && isConsoleNavKey(event.keyCode)) {
                lastPadIsGamepad = event.isFromSource(InputDevice.SOURCE_GAMEPAD)
                if (lastPadIsGamepad) lastPadStyle = Gamepad.styleFor(event.device)
            }
            // The Controllers debug screen sees pad events before the navigation remap below.
            padKeyProbe?.let { if (it(event)) return true }
            if (event.isFromSource(InputDevice.SOURCE_GAMEPAD)) {
                // Not streaming: a game controller drives the Compose UI (TV + phone). Map the face
                // buttons to the navigation the focus system / back stack understand; D-pad *keys*
                // already move focus on their own, so they fall through to super untouched.
                when (event.keyCode) {
                    // B → back. Drive the OnBackPressedDispatcher directly rather than synthesising a
                    // BACK KeyEvent: a synthetic event isn't "tracking", so the framework's default
                    // onKeyUp(BACK) never calls onBackPressed() and Compose BackHandlers wouldn't fire.
                    KeyEvent.KEYCODE_BUTTON_B -> {
                        if (event.action == KeyEvent.ACTION_UP) onBackPressedDispatcher.onBackPressed()
                        return true
                    }
                    // A → activate the focused element (the focus system understands DPAD_CENTER).
                    KeyEvent.KEYCODE_BUTTON_A ->
                        return super.dispatchKeyEvent(KeyEvent(event.action, KeyEvent.KEYCODE_DPAD_CENTER))
                }
            }
        }
        return super.dispatchKeyEvent(event)
    }

    /**
     * `true` (back) / `false` (forward) when this key event is a MOUSE side button, null when it is
     * anything else — including a remote's or keyboard's BACK, which must keep exiting the stream.
     *
     * A mouse that carries its side buttons on the HID consumer page (AC Back / AC Forward) reaches
     * us only as `KEYCODE_BACK`/`KEYCODE_FORWARD`, with no `BUTTON_BACK`/`BUTTON_FORWARD` motion
     * edge behind it — on those, the motion path alone leaves the side buttons dead. The event may
     * even be stamped SOURCE_KEYBOARD rather than SOURCE_MOUSE, because the consumer-page collection
     * is a separate sub-device, so the DEVICE is what we ask: it has to be able to be a mouse.
     *
     * A D-pad-capable device is excluded even when it also reports a pointer: that is an air-mouse
     * remote, whose BACK is the couch user's way out of the stream and must stay navigation.
     * FLAG_FALLBACK events are excluded too — those are a duplicate the framework raises after an
     * unconsumed BUTTON_* press, i.e. one the motion path already forwarded.
     */
    private fun mouseSideButton(event: KeyEvent): Boolean? {
        val back = when (event.keyCode) {
            KeyEvent.KEYCODE_BACK -> true
            KeyEvent.KEYCODE_FORWARD -> false
            else -> return null
        }
        if (event.flags and KeyEvent.FLAG_FALLBACK != 0) return null
        val device = event.device ?: return null
        if (!device.supportsSource(InputDevice.SOURCE_MOUSE)) return null
        if (device.supportsSource(InputDevice.SOURCE_DPAD)) return null
        return back
    }

    /** Last D-pad direction synthesised from a stick/HAT — edge detection (one focus move per push). */
    private var lastNavDir = 0

    override fun dispatchGenericMotionEvent(event: MotionEvent): Boolean {
        if (streamHandle != 0L) {
            if (gamepadRouter?.onMotion(event) == true) return true
            // Physical mouse (uncaptured): hover motion, wheel, button edges.
            if (event.isFromSource(InputDevice.SOURCE_MOUSE)) {
                mouseForwarder?.let { if (it.onGenericMotion(event)) return true }
            }
            return super.dispatchGenericMotionEvent(event)
        }
        // The Controllers debug screen sees pad motion before the stick→D-pad synthesis below.
        padMotionProbe?.let { if (it(event)) return true }
        // Not streaming: turn the gamepad HAT / left stick into discrete D-pad focus moves, so a
        // controller navigates the menus even when its D-pad reports as axes (not key events) and
        // for stick-based navigation. Edge-detected so a held direction moves focus exactly once.
        if (event.isFromSource(InputDevice.SOURCE_JOYSTICK) ||
            event.isFromSource(InputDevice.SOURCE_GAMEPAD)
        ) {
            val x = event.getAxisValue(MotionEvent.AXIS_HAT_X)
                .let { if (it != 0f) it else event.getAxisValue(MotionEvent.AXIS_X) }
            val y = event.getAxisValue(MotionEvent.AXIS_HAT_Y)
                .let { if (it != 0f) it else event.getAxisValue(MotionEvent.AXIS_Y) }
            val dir = when {
                x <= -0.5f -> KeyEvent.KEYCODE_DPAD_LEFT
                x >= 0.5f -> KeyEvent.KEYCODE_DPAD_RIGHT
                y <= -0.5f -> KeyEvent.KEYCODE_DPAD_UP
                y >= 0.5f -> KeyEvent.KEYCODE_DPAD_DOWN
                else -> 0
            }
            if (dir != lastNavDir) {
                lastNavDir = dir
                if (dir != 0) {
                    lastPadIsGamepad = true // a stick/HAT push can only come from a real gamepad
                    lastPadStyle = Gamepad.styleFor(event.device)
                    super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_DOWN, dir))
                    super.dispatchKeyEvent(KeyEvent(KeyEvent.ACTION_UP, dir))
                    return true
                }
            } else if (dir != 0) {
                return true // already moved for this push; swallow until the stick returns to centre
            }
        }
        return super.dispatchGenericMotionEvent(event)
    }

    /**
     * Mouse clicks/drags ride the TOUCH stream (the pointer is "down"). While streaming they
     * belong to the mouse forwarder, never to the Compose touch-gesture layer — a physical
     * mouse click must be a real click at the cursor, not a synthesized trackpad tap.
     */
    override fun dispatchTouchEvent(ev: MotionEvent): Boolean {
        if (streamHandle != 0L && ev.isFromSource(InputDevice.SOURCE_MOUSE)) {
            mouseForwarder?.let { if (it.onTouchEvent(ev)) return true }
        }
        return super.dispatchTouchEvent(ev)
    }

    /** The OS is the source of truth for pointer capture (it releases on focus loss). */
    override fun onPointerCaptureChanged(hasCapture: Boolean) {
        super.onPointerCaptureChanged(hasCapture)
        mouseForwarder?.onCaptureChanged(hasCapture)
    }

    /** Keys that drive the console UI — D-pad + face buttons; used to classify the last input source. */
    private fun isConsoleNavKey(kc: Int): Boolean = when (kc) {
        KeyEvent.KEYCODE_DPAD_UP, KeyEvent.KEYCODE_DPAD_DOWN, KeyEvent.KEYCODE_DPAD_LEFT,
        KeyEvent.KEYCODE_DPAD_RIGHT, KeyEvent.KEYCODE_DPAD_CENTER, KeyEvent.KEYCODE_ENTER,
        -> true
        else -> KeyEvent.isGamepadButton(kc)
    }

    /** Does [intent]'s link resolve to the host [live] is already streaming? */
    private fun targetsHost(intent: Intent?, live: LiveStream): Boolean {
        val url = deepLinkFrom(intent) ?: return false
        val parsed = DeepLinks.parse(url) as? DeepLinkResult.Parsed ?: return false
        val target = DeepLinks.resolveHost(parsed.link, KnownHostStore(this).all())
        return target is HostResolution.Known && target.host.id == live.hostId
    }

    /** The host a live stream is on — see [liveStream]. */
    data class LiveStream(val hostId: String?)

    companion object {
        /**
         * The live stream, PROCESS-wide (null = not streaming), published by the composition that
         * owns it.
         *
         * Deliberately not per-instance state: `launchMode` is `standard`, so a `punktfunk://`
         * link arrives as a second activity instance that knows nothing about the first — and the
         * one thing it must know is that a session is already running. Static state is what
         * crosses that gap; the process dying resets it, which is also correct.
         */
        @Volatile
        var liveStream: LiveStream? = null
    }
}
