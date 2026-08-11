package io.unom.punktfunk

import android.os.Build
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.size
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.DialogProperties
import io.unom.punktfunk.models.PendingTrust

// The prompts that say the SAME thing in both interfaces.
//
// Every one of these existed twice — a Material `AlertDialog` in ConnectDialogs.kt and a console
// glass card in GamepadDialogs.kt — with the two copies maintained by hand. Predictably they
// drifted, and always in the direction of the console losing something: "Pair with PIN…" lost its
// ellipsis, "if no prompt appears when you tap Allow" became "after Allow", and the speed test
// stopped telling console users which layer Apply would write to at all.
//
// What is shared here is the DESCRIPTION of a prompt — a title, a list of [DialogAction]s and a
// body — and what stays per-interface is only how that description is drawn. That split is the
// whole point: a copy change now lands in both places because there is only one place.
//
// ⚠ Deliberately NOT unified, and they belong apart: the PIN ceremony (a numeric keyboard field
// and an editable device name on touch; four D-pad digit slots on the console — different input
// models, not different skins), Add/Edit Host (a bottom sheet and a full screen with its own
// on-screen keyboard), and the host action list (an anchored dropdown vs a modal stack, and the
// touch one grows a row per profile).

/**
 * One prompt, drawn as whichever interface is running.
 *
 * [actions] is ordered PRIMARY FIRST — the console stacks them in that order with the cursor on
 * the first, and the touch renderer lifts that same first action into `confirmButton` and lays the
 * rest out beside it. One order, two idioms, no per-dialog bookkeeping.
 *
 * The two renderers cannot be one tree: an `AlertDialog` composes into its own platform window
 * while [ConsoleModal] is a plain Box in the calling tree — which is also why the console one
 * needs a `BackHandler` and the caller's `navActive` gate while the touch one needs neither.
 */
@Composable
fun PunktfunkDialog(
    gamepadUi: Boolean,
    title: String,
    onDismiss: () -> Unit,
    actions: List<DialogAction>,
    /**
     * False pins the prompt open against a stray tap outside it — for a dialog sitting over work
     * in flight, where a mis-tap would abandon it. Console-side there is no outside to tap, so
     * this only reaches the touch renderer.
     */
    dismissOnOutsideTap: Boolean = true,
    body: @Composable ColumnScope.() -> Unit,
) {
    if (gamepadUi) {
        GamepadDialog(title = title, onDismiss = onDismiss, actions = actions, body = body)
        return
    }
    val primary = actions.firstOrNull { it.primary } ?: actions.firstOrNull()
    val rest = actions.filter { it !== primary }
    AlertDialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(dismissOnClickOutside = dismissOnOutsideTap),
        title = { Text(title) },
        text = { Column(verticalArrangement = Arrangement.spacedBy(10.dp)) { body() } },
        confirmButton = {
            primary?.let { a ->
                TextButton(onClick = a.onClick, enabled = a.enabled) { Text(a.label) }
            }
        },
        dismissButton = {
            if (rest.isNotEmpty()) {
                Row {
                    rest.forEach { a ->
                        TextButton(onClick = a.onClick, enabled = a.enabled) { Text(a.label) }
                    }
                }
            }
        },
    )
}

/** A prompt's body paragraph, dimmed to sit under the title in either interface. */
@Composable
private fun PromptText(text: String, gamepadUi: Boolean) {
    val ink = LocalGamepadInk.current
    Text(
        text,
        style = MaterialTheme.typography.bodyMedium,
        color = if (gamepadUi) ink.fg(0.7f) else MaterialTheme.colorScheme.onSurfaceVariant,
    )
}

/** First connection to a host that advertised pair=optional: offer TOFU, but pitch PIN pairing. */
@Composable
fun TrustNewHostPrompt(
    gamepadUi: Boolean,
    pt: PendingTrust,
    onTrust: () -> Unit,
    onPairInstead: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        gamepadUi = gamepadUi,
        title = "Trust this host?",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Trust (TOFU)", primary = true, onClick = onTrust),
            DialogAction("Pair with PIN…", onClick = onPairInstead),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText("First connection to ${pt.host}:${pt.port}.", gamepadUi)
        pt.advertisedFp?.let { PromptText("Fingerprint ${it.take(16)}…", gamepadUi) }
        PromptText(
            "This host allows trust-on-first-use, but that can't tell an impostor from the real " +
                "host. Pairing with a PIN is stronger — it proves both sides.",
            gamepadUi,
        )
    }
}

/** The pinned fingerprint no longer matches — force re-pairing (never a silent re-trust). */
@Composable
fun FingerprintChangedPrompt(
    gamepadUi: Boolean,
    pt: PendingTrust,
    onRepair: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        gamepadUi = gamepadUi,
        title = "Host identity changed",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Re-pair", primary = true, onClick = onRepair),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText(
            "The pinned fingerprint for ${pt.host} no longer matches what it now advertises. " +
                "This can mean a host reinstall — or an impostor. Re-pair with the host's PIN to " +
                "continue.",
            gamepadUi,
        )
    }
}

/**
 * A fresh pair=required (or manual/unknown-policy) host: offer the two ways in. "Request access" is
 * the no-PIN path — connect and wait for the operator to click Approve in the host's console;
 * "Use a PIN…" switches to the SPAKE2 ceremony.
 */
@Composable
fun RequestAccessPrompt(
    gamepadUi: Boolean,
    pt: PendingTrust,
    onRequestAccess: () -> Unit,
    onUsePin: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        gamepadUi = gamepadUi,
        title = "Pairing required",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Request access", primary = true, onClick = onRequestAccess),
            DialogAction("Use a PIN…", onClick = onUsePin),
            DialogAction("Cancel", onClick = onDismiss),
        ),
    ) {
        PromptText("${pt.host}:${pt.port} requires pairing before it will stream.", gamepadUi)
        PromptText(
            "Request access and approve this device in the host's console (or web UI) — no PIN " +
                "needed. Or pair with the 4-digit PIN the host displays.",
            gamepadUi,
        )
    }
}

/**
 * The no-PIN "request access" wait: the connect is parked on the host until the operator approves
 * this device. Cancel returns the UI immediately — the caller trips the per-attempt flag so a late
 * approval is torn down silently (see ConnectScreen.requestAccess) and resumes discovery.
 *
 * Outside taps are ignored: a connect is parked on the host, and a stray tap beside the card is not
 * a decision to abandon it.
 */
@Composable
fun AwaitingApprovalPrompt(gamepadUi: Boolean, hostLabel: String, onCancel: () -> Unit) {
    val ink = LocalGamepadInk.current
    PunktfunkDialog(
        gamepadUi = gamepadUi,
        title = "Waiting for approval",
        onDismiss = onCancel,
        actions = listOf(DialogAction("Cancel", primary = true, onClick = onCancel)),
        dismissOnOutsideTap = false,
    ) {
        val deviceName = Build.MODEL ?: "this device"
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            CircularProgressIndicator(
                modifier = Modifier.size(20.dp),
                strokeWidth = 2.dp,
                color = if (gamepadUi) ink.fg else MaterialTheme.colorScheme.primary,
            )
            Text(
                "Approve this device on $hostLabel.",
                color = if (gamepadUi) ink.fg else MaterialTheme.colorScheme.onSurface,
            )
        }
        PromptText(
            "Open the host's console (or web UI) and approve “$deviceName”. It connects " +
                "automatically once you approve — no PIN needed.",
            gamepadUi,
        )
    }
}

/**
 * Android 17+ Local Network Protection rationale: ACCESS_LOCAL_NETWORK was denied, so discovery and
 * every connect are dead — offer the system prompt again and a settings deep link (a permanently-
 * denied request returns instantly without ever showing the prompt, so "Allow" alone isn't enough).
 */
@Composable
fun LocalNetworkPrompt(
    gamepadUi: Boolean,
    onAllow: () -> Unit,
    onSettings: () -> Unit,
    onDismiss: () -> Unit,
) {
    PunktfunkDialog(
        gamepadUi = gamepadUi,
        title = "Allow local network access",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Allow", primary = true, onClick = onAllow),
            DialogAction("Open settings", onClick = onSettings),
            DialogAction("Not now", onClick = onDismiss),
        ),
    ) {
        PromptText(
            "Android blocks Punktfunk from talking to devices on your network, so it can't find " +
                "or reach any host until you allow it.",
            gamepadUi,
        )
        PromptText(
            "If no prompt appears after you allow it, enable “Nearby devices” for Punktfunk in " +
                "system settings.",
            gamepadUi,
        )
    }
}

/**
 * The link measurement and what to do with the result. A TV box on a powerline adapter is exactly
 * the machine whose link is worth measuring, so this belongs on the couch surface too — and so
 * does [speedTestTargetNote], which the console used to omit, leaving a console user to guess
 * which layer Apply would write to.
 */
@Composable
fun SpeedTestPrompt(
    gamepadUi: Boolean,
    hostName: String,
    target: SpeedTestTarget,
    phase: SpeedTestPhase,
    onApply: (toProfile: Boolean) -> Unit,
    onDismiss: () -> Unit,
) {
    val ink = LocalGamepadInk.current
    val done = phase as? SpeedTestPhase.Done
    PunktfunkDialog(
        gamepadUi = gamepadUi,
        title = "Network speed test",
        onDismiss = onDismiss,
        // Measuring bursts traffic for two seconds; a tap outside must not abandon it midway.
        dismissOnOutsideTap = phase !is SpeedTestPhase.Measuring,
        actions = buildList {
            if (done != null) {
                add(
                    DialogAction(
                        when (target) {
                            SpeedTestTarget.Global -> "Apply"
                            is SpeedTestTarget.Profile -> "Apply to “${target.profile.name}”"
                            is SpeedTestTarget.Ask -> "Set in “${target.profile.name}”"
                        },
                        primary = true,
                    ) { onApply(true) },
                )
                if (target is SpeedTestTarget.Ask) {
                    add(DialogAction("Set as default") { onApply(false) })
                }
            }
            add(DialogAction("Close", primary = done == null, onClick = onDismiss))
        },
    ) {
        PromptText(hostName, gamepadUi)
        when (phase) {
            SpeedTestPhase.Connecting -> PromptText("Connecting…", gamepadUi)
            SpeedTestPhase.Measuring ->
                PromptText(
                    "Measuring — the host is bursting test traffic for two seconds.",
                    gamepadUi,
                )
            is SpeedTestPhase.Failed -> Text(
                phase.message,
                style = MaterialTheme.typography.bodyMedium,
                color = if (gamepadUi) ink.danger else MaterialTheme.colorScheme.error,
            )
            is SpeedTestPhase.Done -> {
                Text(
                    "%.0f Mbit/s measured · %.1f %% loss".format(phase.measuredMbps, phase.lossPct),
                    style = MaterialTheme.typography.bodyLarge,
                    fontWeight = FontWeight.SemiBold,
                    color = if (gamepadUi) ink.fg else MaterialTheme.colorScheme.onSurface,
                )
                PromptText(
                    "Recommended bitrate: %.0f Mbit/s".format(phase.recommendedMbps),
                    gamepadUi,
                )
                PromptText(speedTestTargetNote(target), gamepadUi)
            }
        }
    }
}

/** One line saying which layer an Apply will write to, and why that one. */
private fun speedTestTargetNote(target: SpeedTestTarget): String = when (target) {
    SpeedTestTarget.Global ->
        "This host uses the default settings, so the bitrate goes there."
    is SpeedTestTarget.Profile ->
        "This host streams with “${target.profile.name}”, which sets its own bitrate — " +
            "that override is what it actually reads."
    is SpeedTestTarget.Ask ->
        "This host streams with “${target.profile.name}”, which currently inherits the default " +
            "bitrate. Setting it in the profile affects only this host's profile; setting it as " +
            "the default affects everything that inherits it."
}
