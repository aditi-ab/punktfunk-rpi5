package io.unom.punktfunk

import android.content.Context
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Apps
import androidx.compose.material.icons.filled.Insights
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.input.pointer.PointerInputScope
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/*
 * The companion panel (design/android-dual-screen.md §5): what the lower screen of a dual-screen
 * handheld shows while the picture has the upper one. Tabs flip between pages — the stats, the
 * quick actions, a trackpad, the virtual controller. One composable for both shapes: the lower
 * half of a hinge, and a second display's Presentation.
 */

internal enum class CompanionPage(val label: String, val icon: ImageVector) {
    STATS("Stats", Icons.Filled.Insights),
    ACTIONS("Actions", Icons.Filled.Apps),
    TRACKPAD("Trackpad", Icons.Filled.TouchApp),
    PAD("Controller", Icons.Filled.SportsEsports),
}

/** The pages a session offers: the trackpad needs the pointer grant, the controller a pad the host takes. */
internal fun companionPages(pointer: Boolean, pad: Boolean): List<CompanionPage> =
    CompanionPage.entries.filter { (it != CompanionPage.TRACKPAD || pointer) && (it != CompanionPage.PAD || pad) }

/**
 * The page the player last picked, kept across streams. The controller page is never kept:
 * showing it connects a pad, and a stream must not connect one on its own.
 */
internal object CompanionMemory {
    private const val PREFS = "punktfunk_companion"
    private const val PAGE = "page"

    private fun prefs(context: Context) =
        context.applicationContext.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    fun page(context: Context): CompanionPage {
        val name = prefs(context).getString(PAGE, null)
        return CompanionPage.entries.firstOrNull { it.name == name } ?: CompanionPage.STATS
    }

    fun keep(context: Context, page: CompanionPage) {
        if (page != CompanionPage.PAD) prefs(context).edit().putString(PAGE, page.name).apply()
    }
}

/**
 * The actions page, in the sheet's order; the host's actions and the shortcuts follow, the two
 * ways out close it. Statistics has a page of its own, and Send text needs a keyboard that the
 * second display's unfocusable window cannot take.
 */
private val ACTION_SLOTS = listOf(
    SlotId.Guide, SlotId.Qam, SlotId.Keyboard, SlotId.TouchMode, SlotId.PadMouse, SlotId.Pad,
    SlotId.Mic, SlotId.StreamMute,
)
private val EXIT_SLOTS = listOf(SlotId.DisconnectLinger, SlotId.EndStream)

private val SURFACE = Color.White.copy(alpha = 0.07f)
private val SURFACE_ON = Color.White.copy(alpha = 0.18f)

/**
 * The panel. [page] is one of [pages]; the controller's tab connects the virtual pad when none is
 * up, since picking it is the ask. [trackpad] is the gesture handler its page runs, [pad] the
 * virtual controller at its page's size.
 */
@Composable
internal fun CompanionPanel(
    pages: List<CompanionPage>,
    page: CompanionPage,
    onPage: (CompanionPage) -> Unit,
    stats: List<HudLine>,
    tier: StatsVerbosity,
    onTier: (StatsVerbosity) -> Unit,
    cfg: OverlayConfig,
    actions: RingActions,
    haptics: ConsoleHaptics,
    trackpad: suspend PointerInputScope.() -> Unit,
    pad: @Composable (IntSize) -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(modifier.fillMaxSize().background(Color.Black)) {
        Row(Modifier.fillMaxWidth().padding(8.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (p in pages) {
                Pill(p.label, p.icon, selected = p == page, modifier = Modifier.weight(1f)) {
                    haptics.tick()
                    if (p == CompanionPage.PAD && !actions.padShown()) actions.togglePad()
                    onPage(p)
                }
            }
        }
        Box(Modifier.fillMaxWidth().weight(1f)) {
            when (page) {
                CompanionPage.STATS -> StatsPage(stats, tier, onTier)
                CompanionPage.ACTIONS -> ActionsPage(cfg, actions, haptics)
                CompanionPage.TRACKPAD -> TrackpadPage(trackpad)
                CompanionPage.PAD -> PadPage(actions, pad)
            }
        }
    }
}

/** A tab or a choice on a rounded surface, lit while selected; a tab's icon sits over its label. */
@Composable
private fun Pill(
    label: String,
    icon: ImageVector?,
    selected: Boolean,
    modifier: Modifier = Modifier,
    role: Role = Role.Tab,
    onClick: () -> Unit,
) {
    val tint = Color.White.copy(alpha = if (selected) 1f else 0.6f)
    Column(
        modifier
            .clip(RoundedCornerShape(12.dp))
            .background(if (selected) SURFACE_ON else SURFACE)
            .selectable(selected = selected, role = role, onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 8.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        if (icon != null) Icon(icon, contentDescription = null, tint = tint, modifier = Modifier.size(22.dp))
        Text(label, color = tint, fontSize = 13.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}

/** The HUD's lines at arm's length: the tier to pick on top, larger type below. Never Off here. */
@Composable
private fun StatsPage(lines: List<HudLine>, tier: StatsVerbosity, onTier: (StatsVerbosity) -> Unit) {
    Column(Modifier.fillMaxSize().padding(horizontal = 16.dp)) {
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (t in listOf(StatsVerbosity.COMPACT, StatsVerbosity.NORMAL, StatsVerbosity.DETAILED)) {
                Pill(t.label, null, selected = t == tier) { onTier(t) }
            }
        }
        Spacer(Modifier.height(12.dp))
        Column(Modifier.verticalScroll(rememberScrollState())) {
            if (lines.isEmpty()) {
                Text("The numbers arrive within a second.", color = roleColor(2), fontSize = 15.sp)
            }
            for (line in lines) {
                Text(
                    line.text, color = roleColor(line.role), fontFamily = FontFamily.Monospace,
                    fontSize = 15.sp, lineHeight = 20.sp, letterSpacing = 0.sp,
                )
            }
            Spacer(Modifier.height(16.dp))
        }
    }
}

/** The ring's catalogue as tiles, fired through the ring's own rules: two presses to leave. */
@Composable
private fun ActionsPage(cfg: OverlayConfig, actions: RingActions, haptics: ConsoleHaptics) {
    val state = remember { RingState() }
    ExpireRingHint(state)
    // End game only while this device's launch is on the stream: a dimmed tile says nothing.
    val endGame = listOfNotNull(SlotId.EndGame.takeIf { actions.streamedGame() != null })
    val slots = ACTION_SLOTS + actions.hostActions().map { SlotId.Host(it.id) } +
        cfg.shortcuts.map { SlotId.Shortcut(it.id) } + endGame + EXIT_SLOTS
    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            GridCells.Adaptive(104.dp),
            contentPadding = PaddingValues(start = 12.dp, end = 12.dp, bottom = 56.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(slots, key = { it.id }) { slot ->
                val s = spec(slot, cfg, actions)
                ActionTile(s, armed = state.armed == s.id) { fireSlot(s, slot, state, cfg, actions, haptics) {} }
            }
        }
        state.hint?.let {
            Text(
                it,
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = 12.dp)
                    .background(Color(0xFF262626), RoundedCornerShape(8.dp))
                    .padding(horizontal = 14.dp, vertical = 8.dp),
                color = Color.White,
                fontSize = 15.sp,
            )
        }
    }
}

@Composable
private fun ActionTile(spec: SlotSpec, armed: Boolean, onTap: () -> Unit) {
    val tint = when {
        armed -> Color(0xFFFF5A5A)
        !spec.enabled -> Color.White.copy(alpha = 0.35f)
        else -> Color.White
    }
    Column(
        Modifier
            .fillMaxWidth()
            .heightIn(min = 88.dp)
            .clip(RoundedCornerShape(16.dp))
            .background(if (armed) SURFACE_ON else SURFACE)
            .clickable(onClick = onTap)
            .semantics {
                stateDescription = when {
                    armed -> "armed — press again"
                    !spec.enabled -> spec.reason
                    else -> spec.state
                }
            }
            .padding(10.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        when {
            spec.chip != null -> ChordKeycap(spec.chip, tint, 48.dp)
            spec.icon != null -> Icon(spec.icon, contentDescription = null, tint = tint, modifier = Modifier.size(28.dp))
        }
        Spacer(Modifier.height(6.dp))
        Text(
            spec.label, color = tint, fontSize = 13.sp, lineHeight = 16.sp, textAlign = TextAlign.Center,
            maxLines = 3, overflow = TextOverflow.Ellipsis,
        )
        if (spec.state.isNotEmpty()) Text(spec.state, color = tint.copy(alpha = 0.7f), fontSize = 12.sp)
    }
}

/** The whole page is a touchpad for the host pointer, whatever touch mode the picture uses. */
@Composable
private fun TrackpadPage(input: suspend PointerInputScope.() -> Unit) {
    Box(
        Modifier
            .fillMaxSize()
            .padding(start = 12.dp, end = 12.dp, bottom = 12.dp)
            .clip(RoundedCornerShape(16.dp))
            .background(SURFACE)
            .border(1.dp, Color.White.copy(alpha = 0.12f), RoundedCornerShape(16.dp))
            .pointerInput(Unit, input),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            "Tap to click · two fingers scroll · two-finger tap right-clicks",
            color = Color.White.copy(alpha = 0.45f), fontSize = 13.sp, textAlign = TextAlign.Center,
            modifier = Modifier.padding(24.dp),
        )
    }
}

/** The virtual controller at this page's size, or the way to bring it back once it was put away. */
@Composable
private fun PadPage(actions: RingActions, pad: @Composable (IntSize) -> Unit) {
    var size by remember { mutableStateOf(IntSize.Zero) }
    Box(Modifier.fillMaxSize().onSizeChanged { size = it }, contentAlignment = Alignment.Center) {
        if (actions.padShown()) {
            pad(size)
        } else {
            Pill("Show the controller", Icons.Filled.SportsEsports, selected = false, role = Role.Button) {
                actions.togglePad()
            }
        }
    }
}
