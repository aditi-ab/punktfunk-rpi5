package io.unom.punktfunk

import android.os.Build
import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.models.PendingTrust
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

// Console-styled trust/pairing dialogs — the controller-navigable counterparts of the touch
// AlertDialogs in ConnectDialogs.kt, shown while the gamepad UI is active. A dark glass card over a
// scrim with focusable action buttons: D-pad left/right moves the focus, A activates it, B dismisses.

/** One dialog action button. */
class DialogAction(
    val label: String,
    val primary: Boolean = false,
    val enabled: Boolean = true,
    val onClick: () -> Unit,
)

/**
 * The shared console-dialog scaffold: scrim + glass card with a title, [body], and a row of focusable
 * [actions]. Owns its own controller nav (the presenting carousel drops its probes while a dialog is
 * up, via ConnectScreen's `navActive`). B → [onDismiss].
 */
@Composable
fun GamepadDialog(
    title: String,
    onDismiss: () -> Unit,
    actions: List<DialogAction>,
    body: @Composable ColumnScope.() -> Unit,
) {
    // Focus the primary action; buttons are stacked full-width, navigated up/down (fits long labels
    // like "Request access" without the cramped-row wrapping a horizontal layout caused).
    var focus by remember { mutableIntStateOf(actions.indexOfFirst { it.primary }.coerceAtLeast(0)) }
    BackHandler(onBack = onDismiss)
    GamepadNavEffect2D(
        active = true,
        onDirection = { dir ->
            when (dir) {
                NavDir.UP -> if (focus > 0) focus--
                NavDir.DOWN -> if (focus < actions.lastIndex) focus++
                else -> {}
            }
        },
        onActivate = { actions.getOrNull(focus)?.takeIf { it.enabled }?.onClick?.invoke() },
    )
    // Cap the card to most of the screen and let the BODY scroll — in a short landscape window the
    // title + body + buttons would otherwise overflow and compress/clip the bottom button.
    val maxCardHeight = (LocalConfiguration.current.screenHeightDp * 0.92f).dp
    Box(
        Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.62f)),
        contentAlignment = Alignment.Center,
    ) {
        Column(
            Modifier
                .padding(24.dp)
                .widthIn(max = 520.dp)
                .heightIn(max = maxCardHeight)
                .clip(RoundedCornerShape(24.dp))
                .background(Color(0xF01A1730))
                .border(1.dp, Color.White.copy(alpha = 0.12f), RoundedCornerShape(24.dp))
                .padding(28.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(title, style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold, color = Color.White)
            // The body scrolls; the title above and the buttons below stay pinned + always visible.
            Column(
                Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                body()
            }
            Spacer(Modifier.size(4.dp))
            actions.forEachIndexed { i, a ->
                DialogButton(a.label, focused = i == focus, primary = a.primary, enabled = a.enabled, onClick = a.onClick)
            }
        }
    }
}

@Composable
private fun DialogButton(label: String, focused: Boolean, primary: Boolean, enabled: Boolean, onClick: () -> Unit) {
    val scale by animateFloatAsState(if (focused) 1.02f else 1f, label = "btnScale")
    val shape = RoundedCornerShape(14.dp)
    val bg = when {
        focused -> Color(0xFF6656F2)
        primary -> Color(0x336656F2)
        else -> Color(0x14FFFFFF)
    }
    val fg = when {
        !enabled -> Color.White.copy(alpha = 0.35f)
        focused -> Color.White
        primary -> Color(0xFF8678F5)
        else -> Color.White.copy(alpha = 0.85f)
    }
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .graphicsLayer { scaleX = scale; scaleY = scale }
            .clip(shape)
            .background(bg)
            .border(1.dp, Color.White.copy(alpha = if (focused) 0.3f else 0.08f), shape)
            .clickable(
                enabled = enabled,
                interactionSource = remember { MutableInteractionSource() },
                indication = null,
                onClick = onClick,
            )
            .padding(horizontal = 20.dp, vertical = 13.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(label, style = MaterialTheme.typography.labelLarge, fontWeight = FontWeight.SemiBold, color = fg, maxLines = 1)
    }
}

/** Body text helper — a dimmed paragraph. */
@Composable
private fun DialogText(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = Color.White.copy(alpha = 0.7f))
}

/**
 * Console host options for a saved tile — Wake (offered only when offline + a MAC is known), Edit,
 * Forget. Reached by pressing Up on a focused saved host in the carousel; the console counterpart of
 * the touch host card's overflow menu.
 */
@Composable
fun GamepadHostOptionsDialog(
    hostName: String,
    canWake: Boolean,
    onWake: () -> Unit,
    onLibrary: (() -> Unit)?, // non-null when the game library is enabled → reachable without Y
    onEdit: () -> Unit,
    onForget: () -> Unit,
    onDismiss: () -> Unit,
) {
    GamepadDialog(
        title = hostName,
        onDismiss = onDismiss,
        actions = buildList {
            if (onLibrary != null) add(DialogAction("Library", primary = true, onClick = onLibrary))
            if (canWake) add(DialogAction("Wake host", onClick = onWake))
            add(DialogAction("Edit…", primary = onLibrary == null, onClick = onEdit))
            add(DialogAction("Forget", onClick = onForget))
            add(DialogAction("Cancel", onClick = onDismiss))
        },
    ) {
        DialogText("Manage this saved host.")
    }
}

@Composable
fun GamepadTrustNewDialog(pt: PendingTrust, onTrust: () -> Unit, onPairInstead: () -> Unit, onDismiss: () -> Unit) {
    GamepadDialog(
        title = "Trust this host?",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Cancel", onClick = onDismiss),
            DialogAction("Pair with PIN", onClick = onPairInstead),
            DialogAction("Trust (TOFU)", primary = true, onClick = onTrust),
        ),
    ) {
        DialogText("First connection to ${pt.host}:${pt.port}.")
        pt.advertisedFp?.let { DialogText("Fingerprint ${it.take(16)}…") }
        DialogText(
            "This host allows trust-on-first-use, but that can't tell an impostor from the real host. " +
                "Pairing with a PIN is stronger — it proves both sides.",
        )
    }
}

@Composable
fun GamepadFingerprintChangedDialog(pt: PendingTrust, onRepair: () -> Unit, onDismiss: () -> Unit) {
    GamepadDialog(
        title = "Host identity changed",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Cancel", onClick = onDismiss),
            DialogAction("Re-pair", primary = true, onClick = onRepair),
        ),
    ) {
        DialogText(
            "The pinned fingerprint for ${pt.host} no longer matches what it now advertises. This can " +
                "mean a host reinstall — or an impostor. Re-pair with the host's PIN to continue.",
        )
    }
}

@Composable
fun GamepadRequestAccessDialog(pt: PendingTrust, onRequestAccess: () -> Unit, onUsePin: () -> Unit, onDismiss: () -> Unit) {
    GamepadDialog(
        title = "Pairing required",
        onDismiss = onDismiss,
        actions = listOf(
            DialogAction("Cancel", onClick = onDismiss),
            DialogAction("Use a PIN", onClick = onUsePin),
            DialogAction("Request access", primary = true, onClick = onRequestAccess),
        ),
    ) {
        DialogText("${pt.host}:${pt.port} requires pairing before it will stream.")
        DialogText(
            "Request access and approve this device in the host's console (or web UI) — no PIN needed. " +
                "Or pair with the 4-digit PIN the host displays.",
        )
    }
}

@Composable
fun GamepadAwaitingApprovalDialog(hostLabel: String, onCancel: () -> Unit) {
    GamepadDialog(
        title = "Waiting for approval",
        onDismiss = onCancel,
        actions = listOf(DialogAction("Cancel", primary = true, onClick = onCancel)),
    ) {
        val deviceName = Build.MODEL ?: "this device"
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            CircularProgressIndicator(modifier = Modifier.size(20.dp), strokeWidth = 2.dp, color = Color.White)
            Text("Approve this device on $hostLabel.", color = Color.White)
        }
        DialogText(
            "Open the host's console (or web UI) and approve “$deviceName”. It connects automatically " +
                "once you approve — no PIN needed.",
        )
    }
}

/**
 * Console PIN pairing: four digit slots set with the D-pad (left/right selects a slot, up/down changes
 * 0–9), then Pair. Runs [NativeBridge.nativePair] off the UI thread; on success hands the verified
 * fingerprint to [onPaired]. No text keyboard needed — a PIN is four digits.
 */
@Composable
fun GamepadPairPinDialog(pt: PendingTrust, identity: ClientIdentity?, onPaired: (String) -> Unit, onDismiss: () -> Unit) {
    val scope = rememberCoroutineScope()
    val digits = remember(pt) { mutableStateListOf(0, 0, 0, 0) }
    var slot by remember(pt) { mutableIntStateOf(0) } // 0..3 = digit slots, 4 = Pair button
    var pairing by remember(pt) { mutableStateOf(false) }
    var err by remember(pt) { mutableStateOf<String?>(null) }
    val name = remember { Build.MODEL ?: "Android" }

    fun pair() {
        val id = identity ?: return
        pairing = true
        err = null
        val pin = digits.joinToString("")
        scope.launch {
            val fp = withContext(Dispatchers.IO) {
                NativeBridge.nativePair(pt.host, pt.port, id.certPem, id.privateKeyPem, pin, name)
            }
            pairing = false
            if (fp.isNotEmpty()) onPaired(fp) else err = "Pairing failed — wrong PIN, or the host isn't armed."
        }
    }

    BackHandler(onBack = { if (!pairing) onDismiss() })
    GamepadNavEffect2D(
        active = !pairing,
        onDirection = { dir ->
            when (dir) {
                NavDir.LEFT -> if (slot > 0) slot--
                NavDir.RIGHT -> if (slot < 4) slot++
                NavDir.UP -> if (slot < 4) digits[slot] = (digits[slot] + 1) % 10
                NavDir.DOWN -> if (slot < 4) digits[slot] = (digits[slot] + 9) % 10
            }
        },
        onActivate = { if (slot == 4 && identity != null) pair() },
    )

    val maxCardHeight = (LocalConfiguration.current.screenHeightDp * 0.92f).dp
    Box(Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.62f)), contentAlignment = Alignment.Center) {
        Column(
            Modifier.padding(24.dp).widthIn(max = 460.dp).heightIn(max = maxCardHeight)
                .clip(RoundedCornerShape(24.dp))
                .background(Color(0xF01A1730)).border(1.dp, Color.White.copy(alpha = 0.12f), RoundedCornerShape(24.dp))
                .verticalScroll(rememberScrollState())
                .padding(28.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            Text("Pair with PIN", style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold, color = Color.White)
            Text(
                "Enter the 4-digit PIN shown on the host — D-pad ↑↓ sets a digit, ←→ moves.",
                style = MaterialTheme.typography.bodyMedium, color = Color.White.copy(alpha = 0.7f), textAlign = TextAlign.Center,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                repeat(4) { i -> PinSlot(digits[i], focused = slot == i && !pairing) }
            }
            err?.let { Text(it, color = Color(0xFFE0736F), style = MaterialTheme.typography.bodyMedium) }
            DialogButton(
                label = if (pairing) "Pairing…" else "Pair",
                focused = slot == 4 && !pairing,
                primary = true,
                enabled = !pairing && identity != null,
                onClick = { if (identity != null) pair() },
            )
        }
    }
}

@Composable
private fun PinSlot(value: Int, focused: Boolean) {
    val shape = RoundedCornerShape(12.dp)
    Box(
        Modifier.size(54.dp, 66.dp).clip(shape)
            .background(if (focused) Color(0x336656F2) else Color(0x14FFFFFF))
            .border(if (focused) 2.dp else 1.dp, if (focused) Color(0xFF8678F5) else Color.White.copy(alpha = 0.1f), shape),
        contentAlignment = Alignment.Center,
    ) {
        Text(value.toString(), fontSize = 30.sp, fontWeight = FontWeight.Bold, color = Color.White, fontFamily = FontFamily.Monospace)
    }
}
