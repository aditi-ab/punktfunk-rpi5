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
import androidx.compose.material.icons.filled.Bedtime
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
import androidx.compose.material.icons.filled.RestartAlt
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
import androidx.compose.runtime.mutableIntStateOf
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
import io.unom.punktfunk.kit.RingNav
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
    /** The pad's highlight: a slot 0…5, or 6 for the centre (the initial one — `Select+A`
     *  then A opens the sheet in two presses). `null` until a pad moves it. */
    var highlight by mutableStateOf<Int?>(null)
    /** The sheet row the pad is on. */
    var sheetCursor by mutableIntStateOf(0)
    /** The mode at first open — the Resolution row's "Native". */
    var nativeMode: IntArray? = null
    /** A pad press awaiting the overlay ([RingOverlay] consumes it); [navSeq] makes each one an event. */
    var pendingNav by mutableStateOf<RingNav?>(null)
    var navSeq by mutableIntStateOf(0)
    /** Open/close edges, for the shell: the pad router masks itself while the ring is up. */
    var onOpenChange: ((Boolean) -> Unit)? = null

    fun nav(n: RingNav) {
        pendingNav = n
        navSeq++
    }

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
        val was = committed
        committed = true
        progress = 1f
        touch()
        if (!was) onOpenChange?.invoke(true)
    }

    /** The twist lifted short of commit, or wound back after one: the ring winds back in. */
    fun cancel() = close()

    fun openAt(c: Offset) {
        centre = c
        commit()
    }

    fun close() {
        val was = committed
        committed = false
        progress = 0f
        sheet = false
        armed = null
        hint = null
        highlight = null
        twistArmed = false
        if (was) onOpenChange?.invoke(false)
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
        // Three power actions, three glyphs — the same icon on all three made them one button.
        val icon = when (slot.actionId) {
            "power.sleep" -> Icons.Filled.Bedtime
            "power.reboot" -> Icons.Filled.RestartAlt
            else -> Icons.Filled.PowerSettingsNew
        }
        SlotSpec(
            "host:${slot.actionId}", act?.label ?: slot.actionId, icon,
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
    LaunchedEffect(Unit) { if (state.nativeMode == null) state.nativeMode = actions.currentMode() }
    val rows = if (state.sheet) sheetRows(state, cfg, actions, haptics) { textDialog = true } else emptyList()

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

    // The pad (design §2.6): Right steps the highlight clockwise, Left anticlockwise, Up jumps
    // to 12 o'clock, Down to 6, Y returns it to the centre; A fires the highlight (the centre
    // opens the sheet), B closes. In the sheet, Up/Down walk the rows, Left/Right adjust one.
    LaunchedEffect(state.navSeq) {
        val n = state.pendingNav ?: return@LaunchedEffect
        state.pendingNav = null
        state.touch()
        if (state.sheet) {
            when (n) {
                RingNav.UP -> { state.sheetCursor = (state.sheetCursor - 1).coerceAtLeast(0); haptics.tick() }
                RingNav.DOWN -> { state.sheetCursor = (state.sheetCursor + 1).coerceAtMost(rows.lastIndex.coerceAtLeast(0)); haptics.tick() }
                RingNav.LEFT -> rows.getOrNull(state.sheetCursor)?.onAdjust?.let { it(-1); haptics.tick() } ?: haptics.boundary()
                RingNav.RIGHT -> rows.getOrNull(state.sheetCursor)?.onAdjust?.let { it(1); haptics.tick() } ?: haptics.boundary()
                RingNav.CONFIRM -> rows.getOrNull(state.sheetCursor)?.let { if (it.enabled) haptics.tick() else haptics.boundary(); it.onTap() }
                RingNav.BACK -> { state.sheet = false; haptics.tick() }
                RingNav.CENTRE -> {}
            }
            return@LaunchedEffect
        }
        val h = state.highlight ?: 6
        when (n) {
            RingNav.RIGHT -> { state.highlight = if (h >= 6) 0 else (h + 1) % 6; haptics.tick() }
            RingNav.LEFT -> { state.highlight = if (h >= 6) 5 else (h + 5) % 6; haptics.tick() }
            RingNav.UP -> { state.highlight = 0; haptics.tick() }
            RingNav.DOWN -> { state.highlight = 3; haptics.tick() }
            RingNav.CENTRE -> { state.highlight = 6; haptics.tick() }
            RingNav.CONFIRM -> if (h >= 6) { haptics.tick(); state.sheetCursor = 0; state.sheet = true } else {
                cfg.ring[h]?.let { fire(spec(it, cfg, actions), it) } ?: haptics.boundary()
            }
            RingNav.BACK -> state.close()
        }
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
                highlighted = state.highlight == k,
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
                highlighted = state.highlight == 6,
                modifier = Modifier.offset { IntOffset((cx - centreHalf).roundToInt(), (cy - centreHalf).roundToInt()) },
                onTap = { state.touch(); haptics.tick(); state.sheet = true },
            )
        }
        // The label under the ring: a hint, else the highlighted slot's name.
        val label = state.hint ?: state.highlight?.let { h ->
            if (h == 6) "More" else cfg.ring[h]?.let { spec(it, cfg, actions).label }
        }
        label?.let { hint ->
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
            RingSheet(state, rows, haptics, Modifier.align(Alignment.BottomCenter))
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
    highlighted: Boolean = false,
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
            .border(
                if (highlighted) 2.dp else 1.dp,
                Color.White.copy(alpha = if (highlighted) 0.8f else if (armed) 0.6f else 0.18f),
                CircleShape,
            )
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
            // A chord as a stacked keycap: modifiers small on top, the key large under them —
            // one legend line ran past the disc's edge.
            spec?.chip != null -> {
                val parts = spec.chip.split("+")
                Column(horizontalAlignment = Alignment.CenterHorizontally, modifier = Modifier.width(size - 12.dp)) {
                    if (parts.size > 1) {
                        Text(
                            parts.dropLast(1).joinToString("+"),
                            color = tint, fontSize = 8.sp, fontWeight = FontWeight.SemiBold,
                            maxLines = 1, softWrap = false,
                        )
                    }
                    Text(
                        parts.last(),
                        color = tint, fontSize = if (parts.last().length > 3) 10.sp else 13.sp,
                        fontWeight = FontWeight.SemiBold, maxLines = 1, softWrap = false,
                    )
                }
            }
            spec?.icon != null -> Icon(spec.icon, contentDescription = null, tint = tint, modifier = Modifier.size(size / 2))
        }
    }
}

/** One row of the sheet as data, so a finger and a pad drive the same list. */
private data class SheetRowSpec(
    val header: String? = null,
    val label: String,
    val value: String = "",
    val enabled: Boolean = true,
    /** Left/Right on a pad: cycle a value (the resolution rows); a tap cycles forward. */
    val onAdjust: ((Int) -> Unit)? = null,
    val onTap: () -> Unit,
)

private val RES_PRESETS = listOf("1440p" to (2560 to 1440), "1080p" to (1920 to 1080), "720p" to (1280 to 720))
private val HZ_PRESETS = listOf(120, 60)

/** Depth two: the complete catalogue in a fixed order (D2). */
private fun sheetRows(
    state: RingState,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    requestText: () -> Unit,
): List<SheetRowSpec> {
    val rows = mutableListOf<SheetRowSpec>()
    val mode = actions.currentMode()
    val native = state.nativeMode ?: mode
    val (w, h, hz) = Triple(mode.getOrElse(0) { 0 }, mode.getOrElse(1) { 0 }, mode.getOrElse(2) { 60 })
    val (nw, nh, nhz) = Triple(native.getOrElse(0) { 0 }, native.getOrElse(1) { 0 }, native.getOrElse(2) { 60 })
    val resLabel = if (w == nw && h == nh) "Native ($w×$h)" else RES_PRESETS.firstOrNull { it.second == (w to h) }?.first ?: "$w×$h"
    fun adjustRes(dir: Int) {
        val options = listOf(nw to nh) + RES_PRESETS.map { it.second }
        val i = options.indexOf(w to h).coerceAtLeast(0)
        val n = options.size
        val next = options[((i + dir) % n + n) % n]
        actions.requestMode(next.first, next.second, hz)
    }
    fun adjustHz(dir: Int) {
        val options = (listOf(nhz) + HZ_PRESETS).distinct()
        val i = options.indexOf(hz).coerceAtLeast(0)
        val n = options.size
        actions.requestMode(w, h, options[((i + dir) % n + n) % n])
    }
    rows += SheetRowSpec("Session", "End stream", if (state.armed == "end_stream") "tap again" else "") {
        if (state.armed == "end_stream") { state.close(); actions.endStream() } else { haptics.boundary(); state.armed = "end_stream" }
    }
    rows += SheetRowSpec(null, "Disconnect, keep the game running") { state.close(); actions.disconnectLinger() }
    rows += SheetRowSpec("Resolution", "Resolution", resLabel, onAdjust = ::adjustRes) { adjustRes(1) }
    rows += SheetRowSpec(null, "Refresh", "$hz Hz", onAdjust = ::adjustHz) { adjustHz(1) }
    val tm = spec(SlotId.TouchMode, cfg, actions)
    rows += SheetRowSpec("Input", tm.label, tm.state) { actions.cycleTouchMode() }
    val kb = spec(SlotId.Keyboard, cfg, actions)
    rows += SheetRowSpec(null, kb.label, if (kb.enabled) "" else kb.reason, kb.enabled) { if (kb.enabled) { state.close(); actions.keyboard() } }
    val st = spec(SlotId.SendText, cfg, actions)
    rows += SheetRowSpec(null, st.label, if (st.enabled) "" else st.reason, st.enabled) { if (st.enabled) requestText() }
    val pad = spec(SlotId.Pad, cfg, actions)
    rows += SheetRowSpec(null, pad.label, pad.reason, false) {}
    rows += SheetRowSpec("View", "Statistics", actions.stats().label) { actions.cycleStats() }
    val mic = spec(SlotId.Mic, cfg, actions)
    rows += SheetRowSpec("Audio", mic.label, if (mic.enabled) mic.state else mic.reason, mic.enabled) { if (mic.enabled) actions.toggleMic() }
    actions.hostActions().forEachIndexed { i, act ->
        val id = "host:${act.id}"
        rows += SheetRowSpec(
            if (i == 0) "Host" else null, act.label,
            when {
                !act.available -> act.unavailableReason
                state.armed == id -> "tap again"
                else -> ""
            },
            act.available,
        ) {
            if (!act.available) return@SheetRowSpec
            if (act.danger && state.armed != id) { haptics.boundary(); state.armed = id } else { state.close(); actions.invokeHost(act) }
        }
    }
    cfg.shortcuts.forEachIndexed { i, s ->
        val ok = actions.keyboardGranted() && s.keys.all { keyVk(it) != null }
        rows += SheetRowSpec(if (i == 0) "Shortcuts" else null, s.label.ifEmpty { chordChip(s.keys) }, chordChip(s.keys), ok) {
            if (ok) { state.close(); actions.sendShortcut(s.keys) }
        }
    }
    return rows
}

/** The sheet as a scrollable bottom panel; the pad's cursor row is tinted. */
@Composable
private fun RingSheet(state: RingState, rows: List<SheetRowSpec>, haptics: ConsoleHaptics, modifier: Modifier) {
    val scroll = rememberScrollState()
    Column(
        modifier
            .fillMaxWidth(0.92f)
            .padding(bottom = 16.dp)
            .background(Color.Black.copy(alpha = 0.78f), RoundedCornerShape(16.dp))
            .clickable(interactionSource = remember { MutableInteractionSource() }, indication = null) {}
            .padding(vertical = 8.dp)
            .verticalScroll(scroll),
    ) {
        rows.forEachIndexed { i, r ->
            r.header?.let { text ->
                Text(
                    text, color = Color.White.copy(alpha = 0.6f), fontSize = 12.sp, fontWeight = FontWeight.SemiBold,
                    modifier = Modifier.padding(start = 20.dp, top = 12.dp, bottom = 4.dp),
                )
            }
            Row(
                Modifier
                    .fillMaxWidth()
                    .background(if (state.sheetCursor == i) Color.White.copy(alpha = 0.12f) else Color.Transparent)
                    .clickable {
                        state.touch()
                        state.sheetCursor = i
                        if (r.enabled) haptics.tick() else haptics.boundary()
                        r.onTap()
                    }
                    .padding(horizontal = 20.dp, vertical = 12.dp)
                    .alpha(if (r.enabled) 1f else 0.45f),
            ) {
                Text(r.label, color = Color.White, fontSize = 15.sp, modifier = Modifier.weight(1f))
                if (r.value.isNotEmpty()) Text(r.value, color = Color.White.copy(alpha = 0.7f), fontSize = 15.sp)
            }
        }
        Spacer(Modifier.height(4.dp))
    }
}

/** The chord as key events: modifiers down in order, the key, modifiers up in reverse. */
fun sendChord(handle: Long, keys: List<String>) {
    val vks = keys.mapNotNull { keyVk(it) }
    if (vks.size != keys.size || vks.isEmpty()) return
    vks.forEach { NativeBridge.nativeSendKey(handle, it, true, 0) }
    vks.asReversed().forEach { NativeBridge.nativeSendKey(handle, it, false, 0) }
}
