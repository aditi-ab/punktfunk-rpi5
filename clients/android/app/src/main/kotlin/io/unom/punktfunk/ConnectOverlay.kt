package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Bedtime
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.DialogProperties

/**
 * Which phase of the connect flow to draw — the pure view model [ConnectOverlay] resolves from the
 * live dial/wake state, so [ConnectTakeover] / [ConnectModal] can render (and be screenshot-tested)
 * statelessly.
 */
internal sealed interface ConnectPhase {
    val hostName: String

    /** The dial is in flight (shown the instant a host is picked). */
    data class Connecting(override val hostName: String) : ConnectPhase

    /** A sleeping host is being Wake-on-LAN'd and we're waiting for it to advertise again. */
    data class Waking(override val hostName: String, val seconds: Int, val connectsAfter: Boolean) : ConnectPhase

    /** The wake wait ran out — offer retry / cancel. */
    data class WakeTimedOut(override val hostName: String) : ConnectPhase
}

/** Per-phase copy, shared by the console takeover and the touch modal so both read identically. */
private data class ConnectCopy(
    val title: String,
    val subtitle: String,
    /** Monospace the subtitle so a ticking seconds counter doesn't jitter its width. */
    val monoSubtitle: Boolean,
    val cancelLabel: String,
)

private fun connectCopy(phase: ConnectPhase): ConnectCopy = when (phase) {
    is ConnectPhase.Connecting -> ConnectCopy(
        "Connecting to ${phase.hostName}", "Establishing a secure connection…", false, "Cancel",
    )
    is ConnectPhase.Waking -> ConnectCopy(
        "Waking ${phase.hostName}…",
        "Waiting for it to come online · ${phase.seconds}s",
        true,
        // A wake-only wait (no dial after) says "Stop Waiting"; a wake that will connect says "Cancel".
        if (!phase.connectsAfter) "Stop Waiting" else "Cancel",
    )
    is ConnectPhase.WakeTimedOut -> ConnectCopy(
        "${phase.hostName} didn't wake",
        "It may still be booting, or it's powered off / off this network.",
        false,
        "Cancel",
    )
}

/**
 * The unified "getting you connected" feedback — one flow for BOTH phases of reaching a host, so the
 * user gets feedback the instant they pick one and it flows seamlessly into a wake if the host turns
 * out to be asleep:
 *
 *  - **Connecting** ([connectingHostName] non-null): the dial is in flight. Shown immediately on tap,
 *    so a host that takes a beat to answer no longer looks like nothing happened.
 *  - **Waking** ([WakeController.waking] non-null): the dial failed on a sleeping host, so we're firing
 *    Wake-on-LAN and waiting for it to advertise again, escalating to a retry/cancel prompt on timeout.
 *
 * Presentation is mode-aware (mirrors the Apple client): in the **console / gamepad** UI it's a
 * full-screen aurora [ConnectTakeover] — the same signature backdrop the console home uses, driven by
 * the pad (B cancels, A retries once timed out) with a hint bar. In the **default touch** UI it's a
 * Material [ConnectModal] over the host grid, matching the app's other dialogs — the aurora takeover
 * looked out of place there.
 *
 * The two phases hand off within a single Compose frame (see ConnectScreen's `doConnectDirect` →
 * `waker.start` → redial), so nothing blinks between them.
 */
@Composable
fun ConnectOverlay(
    connectingHostName: String?,
    waker: WakeController,
    gamepadUi: Boolean,
    onCancelConnect: () -> Unit,
) {
    val waking = waker.waking
    // Waking takes precedence (it only exists after a dial has failed) so a stray overlap can't strand
    // the "Connecting…" phase over a wake in progress.
    val phase = when {
        waking != null && waking.timedOut -> ConnectPhase.WakeTimedOut(waking.hostName)
        waking != null -> ConnectPhase.Waking(waking.hostName, waking.seconds, waking.connectsAfter)
        connectingHostName != null -> ConnectPhase.Connecting(connectingHostName)
        else -> return
    }

    // System Back / pad B (remapped) cancels whatever's in flight — a plain dial or the wake wait.
    val cancel = { if (waking != null) waker.cancel() else onCancelConnect() }

    if (gamepadUi) {
        BackHandler { cancel() }
        // A retries once a wake has timed out; B falls through to the BackHandler above.
        GamepadNavEffect2D(
            active = true,
            onDirection = {},
            onActivate = { if (phase is ConnectPhase.WakeTimedOut) waker.retry() },
        )
        ConnectTakeover(phase = phase, onCancel = cancel, onRetry = { waker.retry() })
    } else {
        // The AlertDialog owns its own scrim + system-Back handling (routed to cancel).
        ConnectModal(phase = phase, onCancel = cancel, onRetry = { waker.retry() })
    }
}

/**
 * The default-UI presentation: a Material dialog over the host grid, matching the app's other touch
 * dialogs. A spinner (or the sleep glyph once timed out) sits above the title; the scrim is inert so a
 * stray tap can't drop a connect in flight — only the buttons or system Back cancel.
 */
@Composable
internal fun ConnectModal(
    phase: ConnectPhase,
    onCancel: () -> Unit,
    onRetry: () -> Unit,
) {
    val copy = connectCopy(phase)
    val timedOut = phase is ConnectPhase.WakeTimedOut
    AlertDialog(
        onDismissRequest = onCancel,
        properties = DialogProperties(dismissOnClickOutside = false),
        icon = {
            if (timedOut) {
                Icon(Icons.Filled.Bedtime, contentDescription = null)
            } else {
                CircularProgressIndicator(modifier = Modifier.size(28.dp), strokeWidth = 3.dp)
            }
        },
        title = { Text(copy.title, textAlign = TextAlign.Center) },
        text = {
            Text(
                copy.subtitle,
                textAlign = TextAlign.Center,
                fontFamily = if (copy.monoSubtitle) FontFamily.Monospace else FontFamily.Default,
            )
        },
        // No confirm action until the wake times out; then "Try Again" is the primary button.
        confirmButton = {
            if (timedOut) TextButton(onClick = onRetry) { Text("Try Again") }
        },
        dismissButton = {
            TextButton(onClick = onCancel) { Text(copy.cancelLabel) }
        },
    )
}

/**
 * The console / gamepad presentation: an opaque aurora backdrop with a centred spinner/title/subtitle
 * for [phase], plus a bottom hint bar spelling out the pad actions (B cancels, A retries once timed
 * out) — glyph-driven like every other console screen. onClick keeps the hints tappable too, so a
 * user without a working pad can still get out.
 */
@Composable
internal fun ConnectTakeover(
    phase: ConnectPhase,
    onCancel: () -> Unit,
    onRetry: () -> Unit,
) {
    val ink = LocalGamepadInk.current
    val copy = connectCopy(phase)
    val timedOut = phase is ConnectPhase.WakeTimedOut

    Box(
        Modifier
            .fillMaxSize()
            // Swallow taps so the screen behind can't be touched through the takeover.
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {},
        contentAlignment = Alignment.Center,
    ) {
        GamepadAuroraBackground(Modifier.fillMaxSize())
        Column(
            Modifier.padding(horizontal = 40.dp).widthIn(max = 460.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            if (timedOut) {
                Box(Modifier.size(120.dp), contentAlignment = Alignment.Center) {
                    Icon(
                        Icons.Filled.Bedtime,
                        contentDescription = null,
                        tint = ink.fg(0.9f),
                        modifier = Modifier.size(46.dp),
                    )
                }
            } else {
                PulsingSpinner()
            }
            Text(
                copy.title,
                color = ink.fg,
                fontWeight = FontWeight.Bold,
                fontSize = 24.sp,
                textAlign = TextAlign.Center,
            )
            Text(
                copy.subtitle,
                color = ink.fg(0.65f),
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                fontFamily = if (copy.monoSubtitle) FontFamily.Monospace else FontFamily.Default,
            )
        }
        val hints = buildList {
            add(PadGlyph.hint('B', copy.cancelLabel, onClick = onCancel))
            if (timedOut) add(PadGlyph.hint('A', "Try Again", onClick = onRetry))
        }
        GamepadHintBar(hints, Modifier.align(Alignment.BottomCenter).padding(bottom = 28.dp))
    }
}

/**
 * The connecting/waking indicator: a white progress ring inside two brand-violet halo rings that
 * expand and fade on a staggered loop — a small sign of life so the takeover reads as working, not
 * stalled.
 */
@Composable
private fun PulsingSpinner() {
    val ink = LocalGamepadInk.current
    val transition = rememberInfiniteTransition(label = "connectPulse")
    val pulse by transition.animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(tween(1600, easing = LinearEasing), RepeatMode.Restart),
        label = "pulse",
    )
    Box(Modifier.size(120.dp), contentAlignment = Alignment.Center) {
        Canvas(Modifier.fillMaxSize()) {
            val maxR = size.minDimension / 2f
            for (i in 0..1) {
                val p = (pulse + i * 0.5f) % 1f
                drawCircle(
                    color = ink.accent.copy(alpha = (1f - p) * 0.35f),
                    radius = maxR * (0.42f + p * 0.58f),
                    style = Stroke(width = 2.dp.toPx()),
                )
            }
        }
        CircularProgressIndicator(
            color = ink.fg,
            strokeWidth = 3.dp,
            modifier = Modifier.size(54.dp),
        )
    }
}
