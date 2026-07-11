package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Bedtime
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * Which phase of the connect takeover to draw — the pure view model [ConnectOverlay] resolves from the
 * live dial/wake state, so [ConnectTakeover] can render (and be screenshot-tested) statelessly.
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

/**
 * The unified full-screen "getting you connected" takeover — one look for BOTH phases of reaching a
 * host, so the user gets feedback the instant they pick one and it flows seamlessly into a wake if the
 * host turns out to be asleep:
 *
 *  - **Connecting** ([connectingHostName] non-null): the dial is in flight. Shown immediately on tap,
 *    so a host that takes a beat to answer no longer looks like nothing happened.
 *  - **Waking** ([WakeController.waking] non-null): the dial failed on a sleeping host, so we're firing
 *    Wake-on-LAN and waiting for it to advertise again (the old standalone `WakeOverlay`'s job),
 *    escalating to a retry/cancel prompt on timeout.
 *
 * The two phases hand off within a single Compose frame (see ConnectScreen's `doConnectDirect` →
 * `waker.start` → redial), so the overlay never blinks between them. It replaces both the touch grid's
 * tiny inline "Connecting…" row and the old centred "Waking…" card with one opaque, aurora-backed
 * screen — the same signature backdrop the console UI uses, so it reads as a deliberate takeover.
 *
 * Rendered over BOTH the touch grid and the console home; it swallows input to the screen behind it,
 * and in console mode the pad drives it (B cancels via the BackHandler, A retries once a wake has timed
 * out) with a hint bar spelling that out, while the touch buttons work for a pointer either way.
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
    BackHandler { cancel() }
    if (gamepadUi) {
        // A retries once a wake has timed out; B falls through to the BackHandler above.
        GamepadNavEffect2D(
            active = true,
            onDirection = {},
            onActivate = { if (phase is ConnectPhase.WakeTimedOut) waker.retry() },
        )
    }

    ConnectTakeover(phase = phase, gamepadUi = gamepadUi, onCancel = cancel, onRetry = { waker.retry() })
}

/**
 * The stateless view of the connect takeover: an opaque aurora backdrop with a centred spinner/title/
 * subtitle for [phase], plus its actions (touch pills, or a console hint bar in [gamepadUi] mode).
 */
@Composable
internal fun ConnectTakeover(
    phase: ConnectPhase,
    gamepadUi: Boolean,
    onCancel: () -> Unit,
    onRetry: () -> Unit,
) {
    val timedOut = phase is ConnectPhase.WakeTimedOut
    // Copy per phase. Titles lead with the verb + host (parallel across phases); the waking counter
    // gets a monospace subtitle so its width doesn't jitter as the seconds tick.
    val title: String
    val subtitle: String
    val monoSubtitle: Boolean
    when (phase) {
        is ConnectPhase.Connecting -> {
            title = "Connecting to ${phase.hostName}"
            subtitle = "Establishing a secure connection…"
            monoSubtitle = false
        }
        is ConnectPhase.Waking -> {
            title = "Waking ${phase.hostName}…"
            subtitle = "Waiting for it to come online · ${phase.seconds}s"
            monoSubtitle = true
        }
        is ConnectPhase.WakeTimedOut -> {
            title = "${phase.hostName} didn't wake"
            subtitle = "It may still be booting, or it's powered off / off this network."
            monoSubtitle = false
        }
    }
    // A wake-only wait (no dial after) says "Stop Waiting"; every other phase is a plain "Cancel".
    val cancelLabel = if (phase is ConnectPhase.Waking && !phase.connectsAfter) "Stop Waiting" else "Cancel"

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
                        tint = Color.White.copy(alpha = 0.9f),
                        modifier = Modifier.size(46.dp),
                    )
                }
            } else {
                PulsingSpinner()
            }
            Text(
                title,
                color = Color.White,
                fontWeight = FontWeight.Bold,
                fontSize = 24.sp,
                textAlign = TextAlign.Center,
            )
            Text(
                subtitle,
                color = Color.White.copy(alpha = 0.65f),
                fontSize = 14.sp,
                textAlign = TextAlign.Center,
                fontFamily = if (monoSubtitle) FontFamily.Monospace else FontFamily.Default,
            )
            // Touch: real tappable pills. Console: the bottom hint bar owns the actions instead, so the
            // look stays glyph-driven like every other console screen.
            if (!gamepadUi) {
                Row(
                    horizontalArrangement = Arrangement.spacedBy(12.dp),
                    modifier = Modifier.padding(top = 8.dp),
                ) {
                    OverlayButton(cancelLabel, primary = false, onClick = onCancel)
                    if (timedOut) OverlayButton("Try Again", primary = true, onClick = onRetry)
                }
            }
        }
        if (gamepadUi) {
            // B cancels, A retries (only once timed out). onClick keeps the cells tappable too, so a
            // user without a working pad can still get out.
            val hints = buildList {
                add(PadGlyph.hint('B', cancelLabel, onClick = onCancel))
                if (timedOut) add(PadGlyph.hint('A', "Try Again", onClick = onRetry))
            }
            GamepadHintBar(hints, Modifier.align(Alignment.BottomCenter).padding(bottom = 28.dp))
        }
    }
}

/**
 * The connecting/waking indicator: a white progress ring inside two brand-violet halo rings that
 * expand and fade on a staggered loop — a small sign of life so the takeover reads as working, not
 * stalled.
 */
@Composable
private fun PulsingSpinner() {
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
                    color = Color(0xFF8678F5).copy(alpha = (1f - p) * 0.35f),
                    radius = maxR * (0.42f + p * 0.58f),
                    style = Stroke(width = 2.dp.toPx()),
                )
            }
        }
        CircularProgressIndicator(
            color = Color.White,
            strokeWidth = 3.dp,
            modifier = Modifier.size(54.dp),
        )
    }
}

/** A pill action button styled for the dark aurora — filled violet when [primary], else glassy. */
@Composable
private fun OverlayButton(label: String, primary: Boolean, onClick: () -> Unit) {
    val shape = RoundedCornerShape(14.dp)
    Box(
        Modifier
            .clip(shape)
            .background(if (primary) Color(0xFF6656F2) else Color(0x1FFFFFFF))
            .border(1.dp, Color.White.copy(alpha = if (primary) 0f else 0.18f), shape)
            .clickable(onClick = onClick)
            .padding(horizontal = 26.dp, vertical = 13.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.labelLarge,
            fontWeight = FontWeight.SemiBold,
            color = Color.White,
        )
    }
}
