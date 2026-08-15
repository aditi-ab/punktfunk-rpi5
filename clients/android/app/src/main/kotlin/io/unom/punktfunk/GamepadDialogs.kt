package io.unom.punktfunk

import androidx.activity.compose.BackHandler
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.ExperimentalFoundationApi
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
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.relocation.BringIntoViewRequester
import androidx.compose.foundation.relocation.bringIntoViewRequester
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
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
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.KnownHost
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
    val ink = LocalGamepadInk.current
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
    // Cap the card to most of the screen and let body + BUTTONS scroll together — in a short
    // landscape window a 5-action stack (host options) exceeds the card even with an empty body, and
    // a pinned actions column can only compress/clip its last button. Only the title stays pinned;
    // the focused button pulls itself into view (see DialogButton), so D-pad navigation always shows
    // the current action even when the stack scrolls.
    val maxCardHeight = (LocalConfiguration.current.screenHeightDp * 0.92f).dp
    ConsoleModal {
        Column(
            Modifier
                .padding(24.dp)
                .widthIn(max = 520.dp)
                .heightIn(max = maxCardHeight)
                .consoleCard()
                .padding(28.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(title, style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold, color = ink.fg)
            Column(
                Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                body()
                Spacer(Modifier.size(4.dp))
                actions.forEachIndexed { i, a ->
                    DialogButton(a.label, focused = i == focus, primary = a.primary, enabled = a.enabled, onClick = a.onClick)
                }
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun DialogButton(label: String, focused: Boolean, primary: Boolean, enabled: Boolean, onClick: () -> Unit) {
    val ink = LocalGamepadInk.current
    val scale by animateFloatAsState(
        if (focused) 1.02f else 1f,
        spring(dampingRatio = 0.7f, stiffness = Spring.StiffnessMediumLow),
        label = "btnScale",
    )
    // The action stack lives inside the dialog's scroll region: when D-pad focus moves to a button
    // that's scrolled out of a short window, pull it into view (no-op when already visible).
    val intoView = remember { BringIntoViewRequester() }
    LaunchedEffect(focused) { if (focused) intoView.bringIntoView() }
    val focus by animateFloatAsState(
        if (focused) 1f else 0f,
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "btnFocus",
    )
    // Focus sweeps up/down the stack — cross-fade the fills so it glides instead of snapping.
    val bg by animateColorAsState(
        when {
            focused -> ink.accent
            primary -> ink.accent(0.20f)
            else -> ink.glass
        },
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "btnBg",
    )
    val fg by animateColorAsState(
        when {
            !enabled -> ink.fg(0.35f)
            // On the accent, not on the field — a pale palette's accent decides this, not the ink.
            focused -> ink.onAccent
            primary -> ink.accent
            else -> ink.fg(0.85f)
        },
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "btnFg",
    )
    val borderColor by animateColorAsState(
        ink.fg(if (focused) 0.3f else 0.08f),
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "btnBorder",
    )
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .bringIntoViewRequester(intoView)
            .consoleGlass(ConsoleShape.Row, ConsoleFocusVisuals(scale, bg, borderColor, focus))
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
    val ink = LocalGamepadInk.current
    Text(text, style = MaterialTheme.typography.bodyMedium, color = ink.fg(0.7f))
}

/**
 * Console host options for a saved tile — Wake (offered only when offline + a MAC is known), Copy
 * link, Edit, Forget. Reached by pressing Up on a focused saved host in the carousel; the console
 * counterpart of the touch host card's overflow menu.
 */
@Composable
fun GamepadHostOptionsDialog(
    hostName: String,
    canWake: Boolean,
    onWake: () -> Unit,
    onLibrary: (() -> Unit)?, // non-null when the game library is enabled → reachable without Y
    onEdit: () -> Unit,
    onForget: () -> Unit,
    /**
     * Copy this tile's `punktfunk://` link. Offered on a pinned tile too — unlike the host's other
     * actions it says nothing about the host, it hands out the shortcut this very tile already is
     * (profile included), which is exactly what a pin is for.
     */
    onCopyLink: () -> Unit,
    onDismiss: () -> Unit,
    onSpeedTest: (() -> Unit)? = null,
    /**
     * Non-null when this is a PINNED host+profile tile, whose only action is to unpin. A pin is a
     * shortcut, not a second host — offering the host's destructive actions on it would blur
     * exactly that, and the touch grid withholds them for the same reason.
     */
    onUnpin: (() -> Unit)? = null,
    profileName: String? = null,
) {
    GamepadDialog(
        title = if (profileName != null) "$hostName · $profileName" else hostName,
        onDismiss = onDismiss,
        actions = buildList {
            if (onUnpin != null) {
                add(DialogAction("Unpin card", primary = true, onClick = onUnpin))
                add(DialogAction("Copy link", onClick = onCopyLink))
                add(DialogAction("Cancel", onClick = onDismiss))
                return@buildList
            }
            if (onLibrary != null) add(DialogAction("Library", primary = true, onClick = onLibrary))
            if (canWake) add(DialogAction("Wake host", onClick = onWake))
            if (onSpeedTest != null) add(DialogAction("Network speed test", onClick = onSpeedTest))
            add(DialogAction("Copy link", onClick = onCopyLink))
            add(DialogAction("Edit…", primary = onLibrary == null, onClick = onEdit))
            add(DialogAction("Forget", onClick = onForget))
            add(DialogAction("Cancel", onClick = onDismiss))
        },
    ) {
        DialogText(
            if (onUnpin != null) {
                "This card is a shortcut to this host with one profile. Unpinning it changes " +
                    "nothing about the host or the profile."
            } else {
                "Manage this saved host."
            },
        )
    }
}

/**
 * The pin-to-hosts picker the settings screen's Profiles section opens — the Android mirror of the
 * desktop console's PinHostsScreen (design §5.2a): one toggle row per SAVED host, D-pad up/down
 * moves, A flips the focused pin, left/right unpins/pins (the settings-toggle semantics), B closes.
 * A toggle is presentation only: it edits the host's pinned cards through the same store write the
 * carousel's unpin uses, never the profile itself and never the host's default binding.
 *
 * Pin state is read live from [pinned] (backed by the host records), so what a switch shows is
 * always what the store holds — the row can't disagree with the carousel it feeds.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun GamepadPinHostsDialog(
    profileName: String,
    hosts: List<KnownHost>,
    pinned: (KnownHost) -> Boolean,
    onToggle: (KnownHost) -> Unit,
    onDismiss: () -> Unit,
) {
    val ink = LocalGamepadInk.current
    // 0..hosts.lastIndex = host rows, hosts.size = the Done button (with no hosts, index 0 IS
    // Done, so it starts focused).
    var focus by remember { mutableIntStateOf(0) }
    BackHandler(onBack = onDismiss)
    GamepadNavEffect2D(
        active = true,
        onDirection = { dir ->
            when (dir) {
                NavDir.UP -> if (focus > 0) focus--
                NavDir.DOWN -> if (focus < hosts.size) focus++
                // Directional = state-targeted (left → unpinned, right → pinned), so holding a
                // direction can't oscillate; asking for the state it's already in is a no-op.
                NavDir.LEFT -> hosts.getOrNull(focus)?.let { if (pinned(it)) onToggle(it) }
                NavDir.RIGHT -> hosts.getOrNull(focus)?.let { if (!pinned(it)) onToggle(it) }
            }
        },
        onActivate = {
            val kh = hosts.getOrNull(focus)
            if (kh != null) onToggle(kh) else onDismiss()
        },
    )
    val maxCardHeight = (LocalConfiguration.current.screenHeightDp * 0.92f).dp
    ConsoleModal {
        Column(
            Modifier
                .padding(24.dp)
                .widthIn(max = 520.dp)
                .heightIn(max = maxCardHeight)
                .consoleCard()
                .padding(28.dp),
            verticalArrangement = Arrangement.spacedBy(14.dp),
        ) {
            Text(
                "Pin “$profileName”",
                style = MaterialTheme.typography.headlineSmall,
                fontWeight = FontWeight.Bold,
                color = ink.fg,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            Column(
                Modifier.weight(1f, fill = false).verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                if (hosts.isEmpty()) {
                    DialogText("No saved hosts yet — pair with a host first, then pin this profile to it.")
                } else {
                    DialogText("A pinned profile appears as its own card on the host — one press connects with it.")
                    hosts.forEachIndexed { i, kh ->
                        PinHostRow(
                            label = kh.name,
                            on = pinned(kh),
                            focused = i == focus,
                            onClick = { onToggle(kh) },
                        )
                    }
                }
                Spacer(Modifier.size(4.dp))
                DialogButton(
                    "Done",
                    focused = focus == hosts.size,
                    primary = true,
                    enabled = true,
                    onClick = onDismiss,
                )
            }
        }
    }
}

/** One host's pin toggle: name + a [ConsoleSwitch], with the shared console focus visuals. */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun PinHostRow(label: String, on: Boolean, focused: Boolean, onClick: () -> Unit) {
    val ink = LocalGamepadInk.current
    val visuals = animateConsoleFocus(active = focused)
    // Inside the dialog's scroll region, like DialogButton: a focused row scrolled out of a short
    // landscape window pulls itself into view.
    val intoView = remember { BringIntoViewRequester() }
    LaunchedEffect(focused) { if (focused) intoView.bringIntoView() }
    Row(
        Modifier
            .fillMaxWidth()
            .bringIntoViewRequester(intoView)
            .consoleGlass(ConsoleShape.Row, visuals)
            .clickable(
                interactionSource = remember { MutableInteractionSource() },
                indication = null,
                onClick = onClick,
            )
            .padding(horizontal = 16.dp, vertical = 13.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            label,
            style = MaterialTheme.typography.bodyLarge,
            fontWeight = FontWeight.SemiBold,
            color = ink.fg,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        Spacer(Modifier.weight(1f))
        ConsoleSwitch(on = on, focused = focused)
    }
}

/**
 * Console PIN pairing: four digit slots set with the D-pad (left/right selects a slot, up/down changes
 * 0–9), then Pair. Runs [NativeBridge.nativePair] off the UI thread; on success hands the verified
 * fingerprint to [onPaired]. No text keyboard needed — a PIN is four digits.
 */
@Composable
fun GamepadPairPinDialog(pt: PendingTrust, identity: ClientIdentity?, onPaired: (String) -> Unit, onDismiss: () -> Unit) {
    val ink = LocalGamepadInk.current
    val scope = rememberCoroutineScope()
    val digits = remember(pt) { mutableStateListOf(0, 0, 0, 0) }
    var slot by remember(pt) { mutableIntStateOf(0) } // 0..3 = digit slots, 4 = Pair button
    var pairing by remember(pt) { mutableStateOf(false) }
    var err by remember(pt) { mutableStateOf<String?>(null) }
    val context = LocalContext.current
    val name = remember(context) { deviceName(context) }

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
            if (fp.isNotEmpty()) {
                onPaired(fp)
            } else {
                // Cause-specific: wrong PIN vs not-armed vs unreachable.
                err = ConnectErrors.pairMessage(NativeBridge.nativeTakeLastError())
            }
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
    ConsoleModal {
        Column(
            Modifier.padding(24.dp).widthIn(max = 460.dp).heightIn(max = maxCardHeight)
                .consoleCard()
                .verticalScroll(rememberScrollState())
                .padding(28.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            Text("Pair with PIN", style = MaterialTheme.typography.headlineSmall, fontWeight = FontWeight.Bold, color = ink.fg)
            Text(
                "Enter the 4-digit PIN shown on the host — D-pad ↑↓ sets a digit, ←→ moves.",
                style = MaterialTheme.typography.bodyMedium, color = ink.fg(0.7f), textAlign = TextAlign.Center,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                repeat(4) { i -> PinSlot(digits[i], focused = slot == i && !pairing) }
            }
            err?.let { Text(it, color = ink.danger, style = MaterialTheme.typography.bodyMedium) }
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
    val ink = LocalGamepadInk.current
    val shape = RoundedCornerShape(12.dp)
    Box(
        Modifier.size(54.dp, 66.dp).clip(shape)
            .background(if (focused) ink.accent(0.20f) else ink.glass)
            .border(if (focused) 2.dp else 1.dp, if (focused) ink.accent else ink.fg(0.1f), shape),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            value.toString(),
            fontSize = 30.sp,
            fontWeight = FontWeight.Bold,
            color = ink.fg,
            fontFamily = FontFamily.Monospace,
        )
    }
}
