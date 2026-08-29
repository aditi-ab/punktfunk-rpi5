package io.unom.punktfunk

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Insights
import androidx.compose.material.icons.filled.Keyboard
import androidx.compose.material.icons.filled.Logout
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material.icons.filled.MoreHoriz
import androidx.compose.material.icons.filled.Mouse
import androidx.compose.material.icons.filled.PanTool
import androidx.compose.material.icons.filled.PowerSettingsNew
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.TextFields
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import io.unom.punktfunk.kit.NativeBridge
import kotlinx.coroutines.delay
import kotlin.math.cos
import kotlin.math.roundToInt
import kotlin.math.sin

/*
 * The quick-action ring (design/touch-client-overlay.md §2): six round buttons on a circle under
 * the fingers plus a centre "More" that opens the sheet — the complete catalogue with values.
 * The two-finger twist drives the opening frame by frame ([RingState.turn]); the back gesture
 * opens it at the screen centre. Closed, it leaves the composition entirely (tenet 1).
 */

private val RING_RADIUS = 120.dp
private val SLOT_D = 56.dp
private val CENTRE_D = 64.dp
private const val IDLE_CLOSE_MS = 8_000L
private const val ARM_MS = 2_000L
private const val HINT_MS = 2_000L
/** Button k lags the previous one by this much of the twist, so the ring visibly unwinds. */
private const val SLOT_LAG = 0.06f

/** The ring's open/closed state and what it is showing. One per stream; the overlay reads it. */
class RingState {
    /** 0 closed … 1 open — driven by the twist until [committed]. */
    var progress by mutableStateOf(0f)
    var committed by mutableStateOf(false)
    var clockwise by mutableStateOf(true)
    /** Container px; the ring is centred here, clamped so it stays on screen. */
    var centre by mutableStateOf(Offset.Zero)
    var sheet by mutableStateOf(false)
    /** A destructive slot awaiting its second press (the slot id). */
    var armed by mutableStateOf<String?>(null)
    /** The label under the ring: a slot's name, why it is unavailable, or "press again". */
    var hint by mutableStateOf<String?>(null)
    var lastTouch by mutableLongStateOf(0L)
    private var twistArmed = false

    val visible: Boolean get() = committed || progress > 0f

    /** Returns true on the first turn of a twist — the moment the dial arms, worth one tick. */
    fun turn(p: Float, cw: Boolean, x: Float, y: Float): Boolean {
        if (committed) return false
        progress = p
        clockwise = cw
        centre = Offset(x, y)
        val first = !twistArmed
        twistArmed = true
        return first
    }

    fun commit() {
        committed = true
        progress = 1f
        touch()
    }

    /** The twist lifted short of commit, or wound back after one: the ring winds back in. */
    fun cancel() = close()

    fun openAt(c: Offset) {
        centre = c
        commit()
    }

    fun close() {
        committed = false
        progress = 0f
        sheet = false
        armed = null
        hint = null
        twistArmed = false
    }

    fun touch() {
        lastTouch = System.currentTimeMillis()
    }
}

/** What the ring can do this session — the shell's live state and commands behind each slot. */
class RingActions(
    val endStream: () -> Unit,
    val disconnectLinger: () -> Unit,
    val touchMode: () -> TouchMode,
    val cycleTouchMode: () -> Unit,
    val keyboardGranted: () -> Boolean,
    val keyboard: () -> Unit,
    val textSupported: Boolean,
    val sendText: (String) -> Unit,
    val stats: () -> StatsVerbosity,
    val cycleStats: () -> Unit,
    val micAvailable: () -> Boolean,
    val micMuted: () -> Boolean,
    val toggleMic: () -> Unit,
    val hostActions: () -> List<HostActions.Action>,
    val invokeHost: (HostActions.Action) -> Unit,
    val sendShortcut: (List<String>) -> Unit,
    /** `[w, h, hz]` as last requested (Android has no live read-back of the negotiated mode). */
    val currentMode: () -> IntArray,
    val requestMode: (Int, Int, Int) -> Unit,
)

/** One button as the ring draws it: glyph or keycap chip, its state, and why it is dimmed. */
private data class SlotSpec(
    val id: String,
    val label: String,
    val icon: ImageVector? = null,
    val chip: String? = null,
    val enabled: Boolean = true,
    val reason: String = "",
    /** Destructive: two presses. */
    val armed: Boolean = false,
    /** A toggle leaves the ring open so the new state is visible (D6). */
    val toggle: Boolean = false,
    val state: String = "",
)

private fun spec(slot: SlotId, cfg: OverlayConfig, a: RingActions): SlotSpec = when (slot) {
    SlotId.EndStream -> SlotSpec("end_stream", "End stream", Icons.Filled.Close, armed = true)
    SlotId.DisconnectLinger ->
        SlotSpec("disconnect_linger", "Disconnect, keep the game running", Icons.Filled.Logout)
    SlotId.TouchMode -> {
        val m = a.touchMode()
        SlotSpec(
            "touch_mode", "Touch mode",
            when (m) {
                TouchMode.TRACKPAD -> Icons.Filled.TouchApp
                TouchMode.POINTER -> Icons.Filled.Mouse
                TouchMode.TOUCH -> Icons.Filled.PanTool
            },
            toggle = true, state = m.name.lowercase().replaceFirstChar { it.uppercase() },
        )
    }
    SlotId.Keyboard -> SlotSpec(
        "keyboard", "Keyboard", Icons.Filled.Keyboard,
        enabled = a.keyboardGranted(), reason = "Keyboard input is not granted for this session",
    )
    SlotId.Stats -> SlotSpec(
        "stats", "Statistics", Icons.Filled.Insights, toggle = true, state = a.stats().label,
    )
    SlotId.Mic -> SlotSpec(
        "mic", "Microphone", if (a.micMuted()) Icons.Filled.MicOff else Icons.Filled.Mic,
        enabled = a.micAvailable(), reason = "No microphone is running this session",
        toggle = true, state = if (a.micMuted()) "Muted" else "On",
    )
    SlotId.Pad -> SlotSpec(
        "pad", "Virtual controller", Icons.Filled.SportsEsports,
        enabled = false, reason = "The virtual controller arrives in a later release",
    )
    SlotId.SendText -> SlotSpec(
        "send_text", "Send text", Icons.Filled.TextFields,
        enabled = a.textSupported && a.keyboardGranted(),
        reason = "This host does not take typed text",
    )
    is SlotId.Host -> {
        val act = a.hostActions().firstOrNull { it.id == slot.actionId }
        SlotSpec(
            "host:${slot.actionId}", act?.label ?: slot.actionId, Icons.Filled.PowerSettingsNew,
            enabled = act?.available == true,
            reason = act?.unavailableReason?.ifEmpty { null } ?: "This host does not offer it",
            armed = act?.danger ?: true,
        )
    }
    is SlotId.Shortcut -> {
        val s = cfg.shortcut(slot.shortcutId)
        SlotSpec(
            "shortcut:${slot.shortcutId}", s?.label?.ifEmpty { null } ?: chordChip(s?.keys.orEmpty()),
            chip = chordChip(s?.keys.orEmpty()),
            enabled = a.keyboardGranted() && s?.keys?.all { keyVk(it) != null } == true,
            reason = if (a.keyboardGranted()) "A key in this chord is unknown" else "Keyboard input is not granted for this session",
        )
    }
}

/**
 * The ring and its sheet. Sits above the gesture layer, so its buttons take the finger first;
 * a tap on the scrim outside closes it. Composed only while [RingState.visible].
 */
@Composable
fun RingOverlay(
    state: RingState,
    cfg: OverlayConfig,
    actions: RingActions,
    containerSize: IntSize,
    haptics: ConsoleHaptics,
    modifier: Modifier = Modifier,
) {
    if (!state.visible) return
    val density = LocalDensity.current
    val radiusPx = with(density) { RING_RADIUS.toPx() }
    val slotPx = with(density) { SLOT_D.toPx() }
    val marginPx = radiusPx + slotPx / 2 + with(density) { 16.dp.toPx() }
    // Clamped into the container so the whole ring is always on screen.
    val cx = state.centre.x.coerceIn(marginPx, (containerSize.width - marginPx).coerceAtLeast(marginPx))
    val cy = state.centre.y.coerceIn(marginPx, (containerSize.height - marginPx).coerceAtLeast(marginPx))

    // The twist drives the opening frame by frame; the commit settles with a spring; a close
    // shrinks it into the centre.
    val shown = remember { Animatable(0f) }
    LaunchedEffect(state.progress, state.committed) {
        when {
            state.committed -> shown.animateTo(1f, spring(dampingRatio = 0.55f, stiffness = Spring.StiffnessMediumLow))
            state.progress <= 0f -> shown.animateTo(0f, tween(200))
            else -> shown.snapTo(state.progress)
        }
    }
    // Idle: the exit disc's 8 s rule, for the same latency reason — unless the sheet is up.
    LaunchedEffect(state.committed, state.lastTouch, state.sheet) {
        if (state.committed && !state.sheet) {
            delay(IDLE_CLOSE_MS)
            state.close()
        }
    }
    // An armed slot and a hint both time out.
    LaunchedEffect(state.armed, state.hint, state.lastTouch) {
        if (state.armed != null || state.hint != null) {
            delay(ARM_MS.coerceAtLeast(HINT_MS))
            state.armed = null
            state.hint = null
        }
    }
    var textDialog by remember { mutableStateOf(false) }

    // The haptic vocabulary: a tap per press, a firm "no" on a dimmed button, a warning when a
    // destructive slot arms, and the confirm on the commit (StreamScreen fires that one).
    fun fire(s: SlotSpec, slot: SlotId) {
        state.touch()
        if (!s.enabled) {
            haptics.boundary()
            state.armed = null
            state.hint = s.reason
            return
        }
        if (s.armed && state.armed != s.id) {
            haptics.boundary()
            state.armed = s.id
            state.hint = "${s.label}? Tap again"
            return
        }
        haptics.tick()
        state.armed = null
        state.hint = null
        when (slot) {
            SlotId.EndStream -> { state.close(); actions.endStream() }
            SlotId.DisconnectLinger -> { state.close(); actions.disconnectLinger() }
            SlotId.TouchMode -> actions.cycleTouchMode()
            SlotId.Keyboard -> { state.close(); actions.keyboard() }
            SlotId.Stats -> actions.cycleStats()
            SlotId.Mic -> actions.toggleMic()
            SlotId.Pad -> {}
            SlotId.SendText -> textDialog = true
            is SlotId.Host -> {
                actions.hostActions().firstOrNull { it.id == slot.actionId }?.let { state.close(); actions.invokeHost(it) }
            }
            is SlotId.Shortcut -> {
                cfg.shortcut(slot.shortcutId)?.let { state.close(); actions.sendShortcut(it.keys) }
            }
        }
        if (s.toggle) state.hint = spec(slot, cfg, actions).let { "${it.label}: ${it.state}" }
    }

    Box(
        modifier
            .fillMaxSize()
            // The scrim: a tap outside the ring closes it, and nothing reaches the stream while
            // it is open. No backdrop blur — a blur over a video surface is a full-screen pass.
            .background(Color.Black.copy(alpha = 0.18f * shown.value))
            .clickable(
                interactionSource = remember { MutableInteractionSource() },
                indication = null,
            ) { if (state.sheet) state.sheet = false else state.close() },
    ) {
        val slotHalf = slotPx / 2
        cfg.ring.forEachIndexed { k, slot ->
            val q = ((shown.value - k * SLOT_LAG) / (1f - 5 * SLOT_LAG)).coerceIn(0f, 1f)
            if (q <= 0f) return@forEachIndexed
            // Slot k sits at 12, 2, 4… o'clock; it travels out along a short spiral that turns
            // the way the hand turns.
            val turn = if (state.clockwise) -40f else 40f
            val deg = -90f + 60f * k + (1f - q) * turn
            val rad = Math.toRadians(deg.toDouble())
            val x = cx + radiusPx * q * cos(rad).toFloat() - slotHalf
            val y = cy + radiusPx * q * sin(rad).toFloat() - slotHalf
            val s = slot?.let { spec(it, cfg, actions) }
            RingButton(
                spec = s,
                size = SLOT_D,
                scale = 0.6f + 0.4f * q,
                alpha = q,
                armed = s != null && state.armed == s.id,
                modifier = Modifier.offset { IntOffset(x.roundToInt(), y.roundToInt()) },
                onTap = { if (slot != null && s != null) fire(s, slot) },
            )
        }
        // The centre arrives last and opens the sheet.
        val cq = ((shown.value - 6 * SLOT_LAG) / (1f - 6 * SLOT_LAG)).coerceIn(0f, 1f)
        if (cq > 0f) {
            val centreHalf = with(density) { CENTRE_D.toPx() } / 2
            RingButton(
                spec = SlotSpec("more", "More", Icons.Filled.MoreHoriz),
                size = CENTRE_D,
                scale = 0.6f + 0.4f * cq,
                alpha = cq,
                armed = false,
                modifier = Modifier.offset { IntOffset((cx - centreHalf).roundToInt(), (cy - centreHalf).roundToInt()) },
                onTap = { state.touch(); haptics.tick(); state.sheet = true },
            )
        }
        state.hint?.let { hint ->
            val labelY = cy + radiusPx + slotPx
            Text(
                hint,
                modifier = Modifier
                    .align(Alignment.TopCenter)
                    .offset { IntOffset(0, labelY.roundToInt()) }
                    .background(Color.Black.copy(alpha = 0.55f), RoundedCornerShape(8.dp))
                    .padding(horizontal = 14.dp, vertical = 8.dp),
                color = Color.White,
                fontSize = 15.sp,
            )
        }
        if (state.sheet) {
            RingSheet(state, cfg, actions, haptics, Modifier.align(Alignment.BottomCenter))
        }
    }

    if (textDialog) {
        var text by remember { mutableStateOf("") }
        AlertDialog(
            onDismissRequest = { textDialog = false },
            title = { Text("Send text") },
            text = { OutlinedTextField(text, { text = it }, singleLine = true, modifier = Modifier.fillMaxWidth()) },
            confirmButton = {
                TextButton(onClick = { textDialog = false; state.close(); actions.sendText(text) }) { Text("Send") }
            },
            dismissButton = { TextButton(onClick = { textDialog = false }) { Text("Cancel") } },
        )
    }
}

/** One round translucent button — the in-stream pill family's surface, in a circle. */
@Composable
private fun RingButton(
    spec: SlotSpec?,
    size: androidx.compose.ui.unit.Dp,
    scale: Float,
    alpha: Float,
    armed: Boolean,
    modifier: Modifier,
    onTap: () -> Unit,
) {
    val tint = when {
        armed -> Color(0xFFFF5A5A)
        spec == null || !spec.enabled -> Color.White.copy(alpha = 0.35f)
        else -> Color.White
    }
    Box(
        modifier
            .size(size)
            .graphicsLayer { scaleX = scale; scaleY = scale; this.alpha = alpha }
            .clip(CircleShape)
            .background(Color.Black.copy(alpha = if (armed) 0.75f else 0.55f))
            .border(1.dp, Color.White.copy(alpha = if (armed) 0.6f else 0.18f), CircleShape)
            .clickable(enabled = spec != null, onClick = onTap)
            .semantics {
                contentDescription = spec?.label ?: "Empty slot"
                stateDescription = when {
                    armed -> "armed — press again"
                    spec?.enabled == false -> spec.reason
                    spec?.state?.isNotEmpty() == true -> spec.state
                    else -> ""
                }
            },
        contentAlignment = Alignment.Center,
    ) {
        when {
            spec?.chip != null -> Text(spec.chip, color = tint, fontSize = 11.sp, fontWeight = FontWeight.SemiBold)
            spec?.icon != null -> Icon(spec.icon, contentDescription = null, tint = tint, modifier = Modifier.size(size / 2))
        }
    }
}

/** Depth two: the complete catalogue in a fixed order (D2), as a scrollable bottom panel. */
@Composable
private fun RingSheet(
    state: RingState,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    modifier: Modifier,
) {
    val scroll = rememberScrollState()
    var textDialog by remember { mutableStateOf(false) }
    Column(
        modifier
            .fillMaxWidth(0.92f)
            .padding(bottom = 16.dp)
            .background(Color.Black.copy(alpha = 0.78f), RoundedCornerShape(16.dp))
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {}
            .padding(vertical = 8.dp)
            .verticalScroll(scroll),
    ) {
        @Composable
        fun row(label: String, value: String = "", enabled: Boolean = true, onTap: () -> Unit) {
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable { state.touch(); if (enabled) haptics.tick() else haptics.boundary(); onTap() }
                    .padding(horizontal = 20.dp, vertical = 12.dp)
                    .alpha(if (enabled) 1f else 0.45f),
            ) {
                Text(label, color = Color.White, fontSize = 15.sp, modifier = Modifier.weight(1f))
                if (value.isNotEmpty()) Text(value, color = Color.White.copy(alpha = 0.7f), fontSize = 15.sp)
            }
        }
        @Composable
        fun header(text: String) {
            Text(
                text, color = Color.White.copy(alpha = 0.6f), fontSize = 12.sp, fontWeight = FontWeight.SemiBold,
                modifier = Modifier.padding(start = 20.dp, top = 12.dp, bottom = 4.dp),
            )
        }
        header("Session")
        row("End stream", if (state.armed == "end_stream") "tap again" else "") {
            if (state.armed == "end_stream") { state.close(); actions.endStream() } else { haptics.boundary(); state.armed = "end_stream" }
        }
        row("Disconnect, keep the game running") { state.close(); actions.disconnectLinger() }

        header("Resolution")
        val mode = actions.currentMode()
        val native = mode.getOrElse(0) { 0 } to mode.getOrElse(1) { 0 }
        Row(Modifier.padding(horizontal = 16.dp, vertical = 4.dp)) {
            listOf("Native" to native, "1440p" to (2560 to 1440), "1080p" to (1920 to 1080), "720p" to (1280 to 720))
                .forEach { (name, wh) ->
                    Chip(name, selected = wh.first == mode.getOrElse(0) { 0 } && wh.second == mode.getOrElse(1) { 0 }) {
                        state.touch()
                        if (wh.first > 0) actions.requestMode(wh.first, wh.second, mode.getOrElse(2) { 60 })
                    }
                }
        }
        Row(Modifier.padding(horizontal = 16.dp, vertical = 4.dp)) {
            listOf("120 Hz" to 120, "60 Hz" to 60).forEach { (name, hz) ->
                Chip(name, selected = mode.getOrElse(2) { 0 } == hz) {
                    state.touch()
                    actions.requestMode(mode.getOrElse(0) { 0 }, mode.getOrElse(1) { 0 }, hz)
                }
            }
        }

        header("Input")
        val tm = spec(SlotId.TouchMode, cfg, actions)
        row(tm.label, tm.state) { actions.cycleTouchMode() }
        val kb = spec(SlotId.Keyboard, cfg, actions)
        row(kb.label, if (kb.enabled) "" else kb.reason, kb.enabled) { if (kb.enabled) { state.close(); actions.keyboard() } }
        val st = spec(SlotId.SendText, cfg, actions)
        row(st.label, if (st.enabled) "" else st.reason, st.enabled) { if (st.enabled) textDialog = true }
        val pad = spec(SlotId.Pad, cfg, actions)
        row(pad.label, pad.reason, false) {}

        header("View")
        row("Statistics", actions.stats().label) { actions.cycleStats() }

        header("Audio")
        val mic = spec(SlotId.Mic, cfg, actions)
        row(mic.label, if (mic.enabled) mic.state else mic.reason, mic.enabled) { if (mic.enabled) actions.toggleMic() }

        val hosts = actions.hostActions()
        if (hosts.isNotEmpty()) {
            header("Host")
            hosts.forEach { act ->
                val id = "host:${act.id}"
                row(
                    act.label,
                    when {
                        !act.available -> act.unavailableReason
                        state.armed == id -> "tap again"
                        else -> ""
                    },
                    act.available,
                ) {
                    if (!act.available) return@row
                    if (act.danger && state.armed != id) { haptics.boundary(); state.armed = id } else { state.close(); actions.invokeHost(act) }
                }
            }
        }
        if (cfg.shortcuts.isNotEmpty()) {
            header("Shortcuts")
            cfg.shortcuts.forEach { s ->
                val ok = actions.keyboardGranted() && s.keys.all { keyVk(it) != null }
                row(s.label.ifEmpty { chordChip(s.keys) }, chordChip(s.keys), ok) {
                    if (ok) { state.close(); actions.sendShortcut(s.keys) }
                }
            }
        }
        Spacer(Modifier.height(4.dp))
    }
    if (textDialog) {
        var text by remember { mutableStateOf("") }
        AlertDialog(
            onDismissRequest = { textDialog = false },
            title = { Text("Send text") },
            text = { OutlinedTextField(text, { text = it }, singleLine = true, modifier = Modifier.fillMaxWidth()) },
            confirmButton = {
                TextButton(onClick = { textDialog = false; state.close(); actions.sendText(text) }) { Text("Send") }
            },
            dismissButton = { TextButton(onClick = { textDialog = false }) { Text("Cancel") } },
        )
    }
}

@Composable
private fun Chip(label: String, selected: Boolean, onTap: () -> Unit) {
    Text(
        label,
        color = Color.White,
        fontSize = 13.sp,
        modifier = Modifier
            .padding(4.dp)
            .clip(RoundedCornerShape(16.dp))
            .background(Color.White.copy(alpha = if (selected) 0.28f else 0.10f))
            .clickable(onClick = onTap)
            .padding(horizontal = 12.dp, vertical = 6.dp),
    )
}

/** The chord as key events: modifiers down in order, the key, modifiers up in reverse. */
fun sendChord(handle: Long, keys: List<String>) {
    val vks = keys.mapNotNull { keyVk(it) }
    if (vks.size != keys.size || vks.isEmpty()) return
    vks.forEach { NativeBridge.nativeSendKey(handle, it, true, 0) }
    vks.asReversed().forEach { NativeBridge.nativeSendKey(handle, it, false, 0) }
}
