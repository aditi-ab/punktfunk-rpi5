package io.unom.punktfunk

import android.content.res.Configuration
import androidx.activity.compose.BackHandler
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource

// The gamepad-driven settings screen — the Android mirror of the Apple client's GamepadSettingsView:
// the couch-relevant subset of the touch settings restyled as a console page and fully navigable with
// a controller: up/down moves the focus bar, left/right steps the focused value, A cycles/toggles it,
// B closes. Both write the same SharedPreferences, so values round-trip with the touch settings.

private class GpRow(
    val id: String,
    val header: String?,
    val label: String,
    val value: String,
    val detail: String,
    val adjust: (Int) -> Boolean, // left/right; returns whether the value actually changed
    val activate: () -> Unit,     // A → cycle forward (wrapping) / flip
)

@Composable
fun GamepadSettingsScreen(
    initial: Settings,
    onChange: (Settings) -> Unit,
    onBack: () -> Unit,
    navActive: Boolean = true, // false while this screen is cross-fading out, so it drops the pad
) {
    var s by remember { mutableStateOf(initial) }
    fun update(next: Settings) { s = next; onChange(next) }

    val rows = buildSettingsRows(s, ::update)
    var focus by remember { mutableIntStateOf(0) }
    if (focus > rows.lastIndex) focus = rows.lastIndex
    val listState = rememberLazyListState()

    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE

    BackHandler(onBack = onBack)
    GamepadNavEffect2D(
        active = navActive,
        onDirection = { dir ->
            when (dir) {
                NavDir.UP -> if (focus > 0) focus--
                NavDir.DOWN -> if (focus < rows.lastIndex) focus++
                NavDir.LEFT -> rows.getOrNull(focus)?.adjust(-1)
                NavDir.RIGHT -> rows.getOrNull(focus)?.adjust(1)
            }
        },
        onActivate = { rows.getOrNull(focus)?.activate() },
    )
    // Keep the focused row on screen, but only SCROLL when it's actually off-screen — so entering the
    // screen (focus on the first row) leaves the "Settings" heading visible instead of jumping past it.
    // +1 accounts for the heading being item 0.
    LaunchedEffect(focus) {
        runCatching {
            val itemIndex = focus + 1
            val info = listState.layoutInfo
            val item = info.visibleItemsInfo.firstOrNull { it.index == itemIndex }
            val offScreen = item == null ||
                item.offset < info.viewportStartOffset ||
                item.offset + item.size > info.viewportEndOffset - 96 // keep clear of the floating legend
            if (offScreen) listState.animateScrollToItem(itemIndex)
        }
    }

    val hazeState = remember { HazeState() }

    Box(Modifier.fillMaxSize()) {
        // Everything scrolls — including the heading — so nothing is pinned. Vital in landscape,
        // where a fixed title + a fixed detail/legend strip ate most of the (short) height.
        Box(Modifier.fillMaxSize().hazeSource(hazeState)) {
            GamepadFormBackground(Modifier.fillMaxSize())
            LazyColumn(
                state = listState,
                modifier = Modifier.fillMaxSize().systemBarsPadding(),
                contentPadding = PaddingValues(start = 24.dp, end = 24.dp, top = 8.dp, bottom = 104.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
            item(key = "__title") {
                ConsoleHeader("Settings", horizontalInset = false)
            }
            itemsIndexed(rows, key = { _, r -> r.id }) { index, row ->
                SettingRowView(row, focused = index == focus, onClick = {
                    if (focus == index) row.activate() else focus = index
                })
            }
            }
        }

        // Floating frosted legend — a real backdrop blur of the rows scrolling behind it (no dedicated
        // strip). In landscape it ignores the safe area so it hugs the corner instead of the nav-bar inset.
        Box(
            Modifier
                .align(Alignment.BottomStart)
                .then(if (landscape) Modifier else Modifier.systemBarsPadding())
                .padding(ConsoleLegendInset),
        ) {
            GamepadHintBar(
                listOf(
                    GamepadHint('↔', Color(0xFF9A93C7), "Adjust"),
                    // Tappable too (touch escape hatch): Change cycles the focused row, Done leaves.
                    PadGlyph.hint('A', "Change") { rows.getOrNull(focus)?.activate() },
                    PadGlyph.hint('B', "Done", onClick = onBack),
                ),
                hazeState = hazeState,
            )
        }
    }
}

@Composable
private fun SettingRowView(row: GpRow, focused: Boolean, onClick: () -> Unit) {
    val scale by animateFloatAsState(if (focused) 1f else 0.98f, label = "rowScale")
    val shape = RoundedCornerShape(14.dp)
    Column {
        if (row.header != null) {
            Text(
                row.header.uppercase(),
                style = MaterialTheme.typography.labelMedium,
                color = Color.White.copy(alpha = 0.45f),
                letterSpacing = 1.4.sp,
                modifier = Modifier.padding(start = 16.dp, top = 14.dp, bottom = 4.dp),
            )
        }
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .graphicsLayer { scaleX = scale; scaleY = scale }
                .clip(shape)
                .background(if (focused) Color(0x336656F2) else Color(0x14FFFFFF))
                .border(1.dp, Color.White.copy(alpha = if (focused) 0.28f else 0.06f), shape)
                .clickable(
                    interactionSource = remember { MutableInteractionSource() },
                    indication = null,
                    onClick = onClick,
                )
                .padding(horizontal = 16.dp, vertical = 13.dp),
        ) {
            Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                Text(
                    row.label,
                    style = MaterialTheme.typography.bodyLarge,
                    fontWeight = FontWeight.SemiBold,
                    color = Color.White,
                    maxLines = 1,
                )
                Spacer(Modifier.weight(1f))
                if (focused) Text("‹ ", color = Color.White.copy(alpha = 0.6f))
                Text(
                    row.value,
                    style = MaterialTheme.typography.bodyMedium,
                    color = if (focused) Color.White else Color.White.copy(alpha = 0.6f),
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (focused) Text(" ›", color = Color.White.copy(alpha = 0.6f))
            }
            // The focused row carries its own one-line description — no dedicated (space-eating)
            // detail strip. It appears right where you're looking, and the row grows to fit.
            if (focused && row.detail.isNotBlank()) {
                Text(
                    row.detail,
                    style = MaterialTheme.typography.bodySmall,
                    color = Color.White.copy(alpha = 0.6f),
                    maxLines = 2,
                    modifier = Modifier.padding(top = 6.dp),
                )
            }
        }
    }
}

/** Build the console settings rows from the current [Settings], writing through [update]. */
private fun buildSettingsRows(s: Settings, update: (Settings) -> Unit): List<GpRow> {
    fun <T> choice(
        id: String, header: String?, label: String, detail: String,
        options: List<Pair<T, String>>, current: T, write: (T) -> Unit,
    ): GpRow {
        val idx = options.indexOfFirst { it.first == current }
        return GpRow(
            id, header, label,
            value = options.getOrNull(idx)?.second ?: "—",
            detail = detail,
            adjust = { delta ->
                if (idx < 0) {
                    options.firstOrNull()?.let { write(it.first) } != null
                } else {
                    val t = idx + delta
                    if (t in options.indices) { write(options[t].first); true } else false
                }
            },
            activate = {
                val i = if (idx < 0) 0 else (idx + 1) % options.size
                options.getOrNull(i)?.let { write(it.first) }
            },
        )
    }
    fun toggle(
        id: String, header: String?, label: String, detail: String,
        value: Boolean, write: (Boolean) -> Unit,
    ): GpRow = GpRow(
        id, header, label,
        value = if (value) "On" else "Off",
        detail = detail,
        adjust = { delta -> val target = delta > 0; if (value != target) { write(target); true } else false },
        activate = { write(!value) },
    )

    return listOf(
        choice(
            "resolution", "Stream", "Resolution",
            "The host creates a virtual display at exactly this size — no scaling.",
            RESOLUTION_OPTIONS.map { (w, h, lbl) -> (w to h) to lbl }, s.width to s.height,
        ) { (w, h) -> update(s.copy(width = w, height = h)) },
        choice(
            "refresh", null, "Refresh rate", "Frame rate the host renders and streams at.",
            REFRESH_OPTIONS, s.hz,
        ) { update(s.copy(hz = it)) },
        choice(
            "bitrate", null, "Bitrate",
            "Automatic uses the host's default. Run a speed test from the touch UI for an informed value.",
            BITRATE_OPTIONS, s.bitrateKbps,
        ) { update(s.copy(bitrateKbps = it)) },
        choice(
            "compositor", null, "Compositor",
            "Which compositor drives the virtual output — honored only if available on the host.",
            COMPOSITOR_OPTIONS.mapIndexed { i, lbl -> i to lbl }, s.compositor,
        ) { update(s.copy(compositor = it)) },

        choice(
            "codec", "Video", "Video codec",
            "A preference — the host falls back if it can't encode this one.",
            CODEC_OPTIONS, s.codec,
        ) { update(s.copy(codec = it)) },
        toggle(
            "hdr", null, "10-bit HDR",
            "HDR10 — engages when the host sends HDR content and this display supports it.",
            s.hdrEnabled,
        ) { update(s.copy(hdrEnabled = it)) },

        choice(
            "audio", "Audio", "Audio channels", "The speaker layout requested from the host.",
            AUDIO_CHANNEL_OPTIONS, s.audioChannels,
        ) { update(s.copy(audioChannels = it)) },
        toggle(
            "mic", null, "Microphone", "Send this device's microphone to the host's virtual mic.",
            s.micEnabled,
        ) { update(s.copy(micEnabled = it)) },

        choice(
            "padType", "Controller", "Controller type",
            "The virtual pad the host creates — Automatic matches this controller.",
            GAMEPAD_OPTIONS.mapIndexed { i, lbl -> i to lbl }, s.gamepad,
        ) { update(s.copy(gamepad = it)) },

        toggle(
            "hud", "Interface", "Statistics overlay",
            "Show FPS, throughput and latency while streaming.",
            s.statsHudEnabled,
        ) { update(s.copy(statsHudEnabled = it)) },
        toggle(
            "library", null, "Game library",
            "Browse a paired host's games with Y (experimental).",
            s.libraryEnabled,
        ) { update(s.copy(libraryEnabled = it)) },
        toggle(
            "gamepadUI", null, "Controller-optimized UI",
            "Turn off to use the touch interface even with a controller connected.",
            s.gamepadUiEnabled,
        ) { update(s.copy(gamepadUiEnabled = it)) },
    )
}
