package io.unom.punktfunk

import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.ui.platform.LocalContext
import kotlin.math.abs
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive

// Controller navigation for the console carousels (host launcher + library coverflow). It taps the
// SAME MainActivity input probes the Controllers debug screen uses (padMotionProbe / padKeyProbe) so
// it sees the raw analog stick and consumes it BEFORE MainActivity's stick→D-pad focus synthesis —
// which is what made carousel scrolling feel wrong: that path is edge-only (no hold-to-repeat, so a
// held stick did nothing) and a flick could cross the threshold twice (double-move). Here the left
// stick drives discrete moves with hysteresis (fire once when it crosses HIGH; re-arm only after it
// falls back under LOW → a flick is exactly one move) and auto-repeat while held. The caller coalesces
// the moves against a target index so a fast repeat walks smoothly instead of overshooting.

private const val STICK_HIGH = 0.6f   // cross this to commit a move
private const val STICK_LOW = 0.3f    // fall back under this to re-arm (hysteresis)
private const val INITIAL_DELAY_MS = 420L // hold this long before the first auto-repeat
private const val REPEAT_MS = 150L        // then repeat this often while held

private class NavInputState {
    @Volatile var stickX = 0f
    @Volatile var stickY = 0f
    @Volatile var hatX = 0f
    @Volatile var hatY = 0f
    @Volatile var dpadX = 0
    @Volatile var dpadY = 0
    fun reset() { stickX = 0f; stickY = 0f; hatX = 0f; hatY = 0f; dpadX = 0; dpadY = 0 }
}

/** A committed navigation direction from the stick / D-pad / HAT. */
enum class NavDir { UP, DOWN, LEFT, RIGHT }

/**
 * Installs controller navigation for a console screen while [active]. [onMove] gets -1 (left) / +1
 * (right) for each committed step; [onActivate] is A / D-pad-center / Enter, [onTertiary] is X,
 * [onSecondary] is Y. B and the shoulders fall through to MainActivity (B → its BACK remap → the
 * screen's BackHandler). [active] is set false while a sheet/dialog is on top so the carousel stops
 * consuming the pad and the overlay can be navigated.
 */
@Composable
fun GamepadNavEffect(
    active: Boolean,
    onMove: (Int) -> Unit,
    onActivate: () -> Unit,
    onSecondary: () -> Unit = {},
    onTertiary: () -> Unit = {},
    // D-pad Up (the carousel is horizontal) → e.g. Settings, since a TV remote has no X face button.
    onUp: () -> Unit = {},
    onDown: () -> Unit = {},
    // Context/options menu — fired by the gamepad Select/View button OR a long-press of the select/OK
    // button (the Android-TV context-menu convention). A short OK press is [onActivate].
    onOptions: () -> Unit = {},
) {
    val activity = LocalContext.current as? MainActivity ?: return
    val state = remember { NavInputState() }
    // Menu feel, inherited by every console screen that navigates through here rather than wired
    // per screen: a tick as the cursor steps, a pulse on confirm. Renders on the driving pad's own
    // motors, the phone body if it has none, and nothing at all on a TV.
    val haptics by rememberUpdatedState(rememberConsoleHaptics())
    // The effects below are keyed on `active` only (they must NOT restart on every recomposition), so
    // they'd otherwise capture the FIRST callbacks — closing over a stale `tiles` (fewer hosts than are
    // discovered later, which clamped navigation to that old count). rememberUpdatedState keeps the
    // long-lived coroutine/probes pointed at the CURRENT callbacks.
    val currentOnMove by rememberUpdatedState(onMove)
    val currentOnActivate by rememberUpdatedState(onActivate)
    val currentOnSecondary by rememberUpdatedState(onSecondary)
    val currentOnTertiary by rememberUpdatedState(onTertiary)
    val currentOnUp by rememberUpdatedState(onUp)
    val currentOnDown by rememberUpdatedState(onDown)
    val currentOnOptions by rememberUpdatedState(onOptions)

    DisposableEffect(active) {
        // One entry on the MainActivity probe stack (see GamepadNavEffect2D), removed by identity on
        // dispose — a cross-fading-out screen must take only its OWN claim, never the incoming
        // screen's, and never the console shell's underneath.
        val motionProbe: (MotionEvent) -> Boolean = probe@{ ev ->
            if (ev.isFromSource(InputDevice.SOURCE_JOYSTICK) && ev.actionMasked == MotionEvent.ACTION_MOVE) {
                state.stickX = ev.getAxisValue(MotionEvent.AXIS_X)
                state.hatX = ev.getAxisValue(MotionEvent.AXIS_HAT_X)
                return@probe true // consume → MainActivity's stick→D-pad synthesis stays out of it
            }
            false
        }
        val keyProbe: (KeyEvent) -> Boolean = probe@{ ev ->
            val down = ev.action == KeyEvent.ACTION_DOWN
            val edge = down && ev.repeatCount == 0
            when (ev.keyCode) {
                KeyEvent.KEYCODE_DPAD_LEFT -> { state.dpadX = if (down) -1 else 0; true }
                KeyEvent.KEYCODE_DPAD_RIGHT -> { state.dpadX = if (down) 1 else 0; true }
                // TV remote (no face buttons): Up → Settings, Down → a saved host's Options.
                KeyEvent.KEYCODE_DPAD_UP -> { if (edge) currentOnUp(); true }
                KeyEvent.KEYCODE_DPAD_DOWN -> { if (edge) currentOnDown(); true }
                KeyEvent.KEYCODE_BUTTON_A, KeyEvent.KEYCODE_DPAD_CENTER,
                KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> {
                    if (edge) { haptics.confirm(); currentOnActivate() }
                    true
                }
                // The gamepad Select / View / Share button → context options (a remote uses Down).
                KeyEvent.KEYCODE_BUTTON_SELECT -> { if (edge) currentOnOptions(); true }
                KeyEvent.KEYCODE_BUTTON_X -> { if (edge) currentOnTertiary(); true }
                KeyEvent.KEYCODE_BUTTON_Y -> { if (edge) currentOnSecondary(); true }
                else -> false // B / shoulders / etc. → MainActivity handles (B remaps to BACK)
            }
        }
        val probes = if (active) MainActivity.PadProbes(keyProbe, motionProbe) else null
        probes?.let { activity.pushPadProbes(it) }
        onDispose {
            probes?.let { activity.removePadProbes(it) }
            state.reset()
        }
    }

    LaunchedEffect(active) {
        if (!active) return@LaunchedEffect
        var committed = 0     // the direction currently held (hysteresis + repeat authority)
        var fireAt = 0L       // uptime at/after which the next auto-repeat may fire
        while (isActive) {
            val now = SystemClock.uptimeMillis()
            val hat = if (state.hatX <= -0.5f) -1 else if (state.hatX >= 0.5f) 1 else 0
            val dir = when {
                state.dpadX != 0 -> state.dpadX
                hat != 0 -> hat
                else -> {
                    val x = state.stickX
                    when {
                        x >= STICK_HIGH -> 1
                        x <= -STICK_HIGH -> -1
                        abs(x) < STICK_LOW -> 0
                        else -> committed // inside the hysteresis band → hold the committed value
                    }
                }
            }
            when {
                dir == 0 -> committed = 0
                dir != committed -> {
                    haptics.tick(); currentOnMove(dir); committed = dir; fireAt = now + INITIAL_DELAY_MS
                }
                now >= fireAt -> { haptics.tick(); currentOnMove(dir); fireAt = now + REPEAT_MS }
            }
            delay(16)
        }
    }
}

/**
 * 2-D controller navigation for the console form screens (settings focus list, add-host, on-screen
 * keyboard). Same hysteresis + hold-to-repeat as [GamepadNavEffect] but on both axes — the dominant
 * stick axis (or the pressed D-pad/HAT) commits a [NavDir], and it re-arms only after the stick
 * returns near centre (so a flick is one step). [onActivate] is A / center, [onTertiary] is X,
 * [onSecondary] is Y, and [onShoulder] is L1 (-1) / R1 (+1) — a step SIDEWAYS out of the list, which
 * the settings screen uses for its section tabs. B is left to MainActivity's BACK remap → the
 * screen's BackHandler (so B "peels one layer": close the keyboard, then the screen).
 */
@Composable
fun GamepadNavEffect2D(
    active: Boolean,
    onDirection: (NavDir) -> Unit,
    onActivate: () -> Unit,
    onTertiary: () -> Unit = {},
    onSecondary: () -> Unit = {},
    onShoulder: (Int) -> Unit = {},
) {
    val activity = LocalContext.current as? MainActivity ?: return
    val state = remember { NavInputState() }
    // See [GamepadNavEffect] — the same menu feel, so a form screen and a carousel answer a press
    // identically.
    val haptics by rememberUpdatedState(rememberConsoleHaptics())
    val currentOnDirection by rememberUpdatedState(onDirection)
    val currentOnActivate by rememberUpdatedState(onActivate)
    val currentOnTertiary by rememberUpdatedState(onTertiary)
    val currentOnSecondary by rememberUpdatedState(onSecondary)
    val currentOnShoulder by rememberUpdatedState(onShoulder)

    DisposableEffect(active) {
        // One entry on the MainActivity probe stack, removed by identity on dispose — during a
        // cross-fade both the outgoing and incoming screen are briefly composed, and the outgoing's
        // teardown must take only its own claim. On the console this effect sits OVER the Skia
        // shell's probes: pushing (not overwriting) is what lets the shell's pad input resurface
        // the moment this screen pops, instead of dying with a nulled slot.
        val motionProbe: (MotionEvent) -> Boolean = probe@{ ev ->
            if (ev.isFromSource(InputDevice.SOURCE_JOYSTICK) && ev.actionMasked == MotionEvent.ACTION_MOVE) {
                state.stickX = ev.getAxisValue(MotionEvent.AXIS_X)
                state.stickY = ev.getAxisValue(MotionEvent.AXIS_Y)
                state.hatX = ev.getAxisValue(MotionEvent.AXIS_HAT_X)
                state.hatY = ev.getAxisValue(MotionEvent.AXIS_HAT_Y)
                return@probe true
            }
            false
        }
        val keyProbe: (KeyEvent) -> Boolean = probe@{ ev ->
            val down = ev.action == KeyEvent.ACTION_DOWN
            val edge = down && ev.repeatCount == 0
            when (ev.keyCode) {
                KeyEvent.KEYCODE_DPAD_LEFT -> { state.dpadX = if (down) -1 else 0; true }
                KeyEvent.KEYCODE_DPAD_RIGHT -> { state.dpadX = if (down) 1 else 0; true }
                KeyEvent.KEYCODE_DPAD_UP -> { state.dpadY = if (down) -1 else 0; true }
                KeyEvent.KEYCODE_DPAD_DOWN -> { state.dpadY = if (down) 1 else 0; true }
                KeyEvent.KEYCODE_BUTTON_A, KeyEvent.KEYCODE_DPAD_CENTER,
                KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> {
                    if (edge) { haptics.confirm(); currentOnActivate() }
                    true
                }
                KeyEvent.KEYCODE_BUTTON_X -> { if (edge) currentOnTertiary(); true }
                KeyEvent.KEYCODE_BUTTON_Y -> { if (edge) currentOnSecondary(); true }
                // Edge-only, no auto-repeat: a held shoulder shouldn't spin through the tabs.
                KeyEvent.KEYCODE_BUTTON_L1 -> { if (edge) { haptics.tick(); currentOnShoulder(-1) }; true }
                KeyEvent.KEYCODE_BUTTON_R1 -> { if (edge) { haptics.tick(); currentOnShoulder(1) }; true }
                else -> false // B → MainActivity (remapped to BACK → BackHandler)
            }
        }
        val probes = if (active) MainActivity.PadProbes(keyProbe, motionProbe) else null
        probes?.let { activity.pushPadProbes(it) }
        onDispose {
            probes?.let { activity.removePadProbes(it) }
            state.reset()
        }
    }

    LaunchedEffect(active) {
        if (!active) return@LaunchedEffect
        var committed: NavDir? = null
        var fireAt = 0L
        while (isActive) {
            val now = SystemClock.uptimeMillis()
            val raw = resolveDir(state)
            val nearCentre = state.dpadX == 0 && state.dpadY == 0 &&
                abs(state.hatX) < 0.5f && abs(state.hatY) < 0.5f &&
                abs(state.stickX) < STICK_LOW && abs(state.stickY) < STICK_LOW
            when {
                raw == null && nearCentre -> committed = null
                raw == null -> { /* in the hysteresis band → hold, don't fire */ }
                raw != committed -> {
                    haptics.tick(); currentOnDirection(raw); committed = raw; fireAt = now + INITIAL_DELAY_MS
                }
                now >= fireAt -> { haptics.tick(); currentOnDirection(raw); fireAt = now + REPEAT_MS }
            }
            delay(16)
        }
    }
}

/** The direction currently past the commit threshold (D-pad/HAT first, then the dominant stick axis). */
private fun resolveDir(s: NavInputState): NavDir? {
    if (s.dpadY < 0) return NavDir.UP
    if (s.dpadY > 0) return NavDir.DOWN
    if (s.dpadX < 0) return NavDir.LEFT
    if (s.dpadX > 0) return NavDir.RIGHT
    if (s.hatY <= -0.5f) return NavDir.UP
    if (s.hatY >= 0.5f) return NavDir.DOWN
    if (s.hatX <= -0.5f) return NavDir.LEFT
    if (s.hatX >= 0.5f) return NavDir.RIGHT
    // Horizontal wins an exact |x| == |y| diagonal tie (Y must be strictly greater to take the
    // vertical branch), matching the SDL core and Apple nav so a perfect 45° push resolves the
    // same on every client.
    return if (abs(s.stickY) > abs(s.stickX)) {
        when {
            s.stickY <= -STICK_HIGH -> NavDir.UP
            s.stickY >= STICK_HIGH -> NavDir.DOWN
            else -> null
        }
    } else {
        when {
            s.stickX <= -STICK_HIGH -> NavDir.LEFT
            s.stickX >= STICK_HIGH -> NavDir.RIGHT
            else -> null
        }
    }
}
