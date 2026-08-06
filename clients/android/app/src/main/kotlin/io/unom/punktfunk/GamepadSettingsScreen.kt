package io.unom.punktfunk

import android.content.res.Configuration
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
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
import androidx.compose.runtime.mutableStateMapOf
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
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.KnownHostStore

// The gamepad-driven settings screen — the Android mirror of the Apple client's GamepadSettingsView:
// the couch-relevant subset of the touch settings restyled as a console page and fully navigable with
// a controller: up/down moves the focus bar, left/right steps the focused value, A cycles/toggles it,
// L1/R1 change SECTION, B closes. Both write the same SharedPreferences, so values round-trip with
// the touch settings.
//
// The rows are split across SECTION TABS ([GpTab]) — a shoulder press on a pad, a tap on a phone.
// They used to be one long scroll with inline `Group · Subgroup` headers, which on a TV meant
// walking past Display and Audio to reach the controller settings. The tab names match the desktop
// console's and the Apple client's, so a setting is found under the same word wherever you look.

/**
 * The settings screen's sections. Order IS the strip order and the L1/R1 cycle order; the names
 * match `pf-console-ui`'s `TABS` and the Apple client's `GpSettingsTab`.
 */
enum class GpTab(val title: String) {
    STREAM("Stream"),
    VIDEO("Video"),
    AUDIO("Audio"),
    CONTROLLER("Controller"),
    INTERFACE("Interface"),
    PROFILES("Profiles"),
}

internal class GpRow(
    val id: String,
    val tab: GpTab,
    /**
     * A sub-heading above this row, for the few tabs that hold more than one group. Most rows have
     * none: the tab pill already names the section, and repeating it would be a second label
     * saying the same word.
     */
    val header: String?,
    val label: String,
    val value: String,
    val detail: String,
    val adjust: (Int) -> Boolean, // left/right; returns whether the value actually changed
    val activate: () -> Unit,     // A → cycle forward (wrapping) / flip
    val toggled: Boolean? = null, // non-null = a toggle row, drawn as a ConsoleSwitch (not text)
    val adjustable: Boolean = true, // false = the row navigates/acts instead of stepping — no chevrons
    val enabled: Boolean = true,    // dimmed + inert when false (still focusable, for its detail)
)

/**
 * The row at [index], or null when it is dimmed. The single place the "disabled ⇒ inert" half of
 * [GpRow.enabled] is enforced, so the three input paths (pad left/right, A, and a tap on the
 * already-focused row) cannot drift apart — before this, `enabled` dimmed the label and nothing
 * else, and every dimmed row still stepped its setting.
 */
internal fun liveRow(rows: List<GpRow>, index: Int): GpRow? =
    rows.getOrNull(index)?.takeIf { it.enabled }

@Composable
fun GamepadSettingsScreen(
    initial: Settings,
    onChange: (Settings) -> Unit,
    onBack: () -> Unit,
    navActive: Boolean = true, // false while this screen is cross-fading out, so it drops the pad
) {
    var s by remember { mutableStateOf(initial) }
    fun update(next: Settings) { s = next; onChange(next) }

    val context = LocalContext.current
    // Gates the "Rumble on this phone" row — a TV box has no body vibrator to mirror onto.
    val hasBodyVibrator = remember { deviceBodyVibrator(context) != null }
    // Gates the AV1 codec row the same way the touch settings do (see `codecOptionsFor`).
    val av1Capable = remember { io.unom.punktfunk.kit.VideoDecoders.pickDecoder("video/av01") != null }

    // The Profiles section's stores, constructed here the way ConnectScreen constructs its own.
    // The catalog is read once per screen entry: this screen can't create or edit profiles
    // (design §5.4 — the touch interface does), so the list is stable for its lifetime. The saved
    // hosts DO change under it — every pin toggle writes one — so they live in state and refresh
    // on each toggle, keeping the "Pinned to N hosts" counts honest.
    val knownHostStore = remember { KnownHostStore(context) }
    val profileStore = remember { ProfileStore(context) }
    val profiles = remember { profileStore.all() }
    var savedHosts by remember { mutableStateOf(knownHostStore.all()) }
    // The profile whose pin-to-hosts picker is up, or null. While it's showing, it owns the pad
    // (this screen's nav gates on it, the ConnectScreen-dialog pattern).
    var pinProfile by remember { mutableStateOf<StreamProfile?>(null) }

    // Toggle a host+profile pin — the same store write ConnectScreen's togglePin does. Presentation
    // only: pin appends at the end (card order), unpin removes, and the host's default binding
    // (profileId) is never touched.
    fun togglePin(kh: KnownHost, profile: StreamProfile) {
        val pins = if (profile.id in kh.pinnedProfileIds) {
            kh.pinnedProfileIds - profile.id
        } else {
            kh.pinnedProfileIds + profile.id
        }
        knownHostStore.save(kh.copy(pinnedProfileIds = pins))
        savedHosts = knownHostStore.all()
    }

    // On a TV "the touch interface" is confusing advice (no touch to reach it with) — the honest
    // path there is this screen's own Controller-optimized UI toggle, which swaps in the standard
    // interface remote-navigably. The strings branch on it.
    val tv = remember { isTvDevice(context) }
    val allRows = buildSettingsRows(s, hasBodyVibrator, av1Capable, ::update) +
        buildProfileRows(profiles, savedHosts, tv) { pinProfile = it }
    // Which section is showing, and where each one's focus was when it was last left — a detour
    // into another tab shouldn't lose your place.
    var tab by remember { mutableStateOf(GpTab.STREAM) }
    // True while the STRIP holds the cursor rather than the list. Up from the first row moves
    // here and Down goes back — the only route to the sections on a D-pad remote, which has no
    // shoulder buttons at all (and is exactly what a TV box ships with).
    var tabFocused by remember { mutableStateOf(false) }
    val tabFocus = remember { mutableStateMapOf<GpTab, Int>() }
    val rows = allRows.filter { it.tab == tab }
    var focus by remember { mutableIntStateOf(0) }
    if (focus > rows.lastIndex) focus = rows.lastIndex.coerceAtLeast(0)

    // L1/R1 — one section along, wrapping (the strip is a ring, like A's value cycle).
    fun selectTab(next: GpTab) {
        if (next == tab) return
        tabFocus[tab] = focus
        tab = next
        // Clamp: a tab's length follows the hardware and the catalog, so a remembered index can
        // outlive the row it pointed at.
        focus = (tabFocus[next] ?: 0)
            .coerceIn(0, (allRows.count { it.tab == next } - 1).coerceAtLeast(0))
    }
    fun stepTab(delta: Int) {
        val all = GpTab.entries
        selectTab(all[((all.indexOf(tab) + delta) % all.size + all.size) % all.size])
    }
    // The direction the focused value last stepped (+1 forward / -1 back) — drives which way the
    // value text slides in its AnimatedContent, so the motion matches the button press.
    var adjustDir by remember { mutableIntStateOf(1) }
    val listState = rememberLazyListState()

    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE

    BackHandler(onBack = onBack)
    GamepadNavEffect2D(
        // The pin picker owns the pad while it's up (its own nav + BackHandler), so this screen
        // drops its probes — the pattern ConnectScreen's dialogs use.
        active = navActive && pinProfile == null,
        onDirection = { dir ->
            when (dir) {
                NavDir.UP -> if (focus > 0) focus-- else tabFocused = true
                NavDir.DOWN -> if (tabFocused) tabFocused = false else if (focus < rows.lastIndex) focus++
                // On the strip, left/right walks sections; on a row it steps the value. A disabled
                // row is INERT, not just dim — the step is refused instead of writing a setting
                // that has nothing to act on (see `liveRow`).
                NavDir.LEFT ->
                    if (tabFocused) stepTab(-1) else { adjustDir = -1; liveRow(rows, focus)?.adjust(-1) }
                NavDir.RIGHT ->
                    if (tabFocused) stepTab(1) else { adjustDir = 1; liveRow(rows, focus)?.adjust(1) }
            }
        },
        // A on the strip drops into the section you picked, which is what "confirm" means there.
        onActivate = {
            if (tabFocused) tabFocused = false else { adjustDir = 1; liveRow(rows, focus)?.activate() }
        },
        // The shoulders work from either place — a real pad never has to visit the strip.
        onShoulder = { delta -> stepTab(delta) },
    )
    // Keep the focused row on screen, but only SCROLL when it's actually off-screen — so entering the
    // screen (focus on the first row) leaves the "Settings" heading visible instead of jumping past it.
    // +1 accounts for the heading being item 0.
    LaunchedEffect(focus, tab) {
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
            Column(Modifier.fillMaxSize().systemBarsPadding()) {
            // The strip is PINNED while the rows scroll under it: it is this screen's primary
            // navigation now, and a switcher you have to scroll back up to find isn't one. The
            // title stays in the scrolling list (landscape has no height to spare, and the
            // selected pill already says which section you are in).
            ConsoleTabStrip(
                titles = GpTab.entries.map { it.title },
                selected = GpTab.entries.indexOf(tab),
                onSelect = { tabFocused = false; selectTab(GpTab.entries[it]) },
                modifier = Modifier.fillMaxWidth().padding(top = 8.dp, bottom = 2.dp),
                focused = tabFocused,
            )
            LazyColumn(
                state = listState,
                modifier = Modifier.fillMaxSize(),
                contentPadding = PaddingValues(start = 24.dp, end = 24.dp, top = 8.dp, bottom = 104.dp),
                verticalArrangement = Arrangement.spacedBy(6.dp),
            ) {
            item(key = "__title") {
                // "Default settings", not "Settings": this screen edits the base layer only. The
                // console honours a host's profile but doesn't edit profiles (design §5.4), so a
                // bare "Settings" would quietly imply it changes whatever that host streams with.
                ConsoleHeader("Default settings", horizontalInset = false)
            }
            itemsIndexed(rows, key = { _, r -> r.id }) { index, row ->
                SettingRowView(
                    row,
                    focused = index == focus && !tabFocused,
                    adjustDir = adjustDir,
                    onClick = {
                        // Same inertness as the pad path above — tapping a dimmed row focuses it
                        // (so its detail explains itself) but never flips it.
                        tabFocused = false
                        if (focus != index) focus = index
                        else if (row.enabled) { adjustDir = 1; row.activate() }
                    },
                )
            }
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
            // The legend follows the focused row (the desktop console's hints() does the same):
            // a profile row doesn't adjust, it opens the pin picker, and the "No profiles yet"
            // placeholder does nothing at all — advertising ↔/A on those would be a lie.
            val focused = rows.getOrNull(focus)
            // The shoulders always change section, so that cell leads on every row. Tappable too,
            // like the others — a user without a working pad can still reach every tab.
            // Advertise the shoulders only where they EXIST: a TV remote has none (its route is Up
            // into the strip) and a touch user taps a pill, so on those the cell would be both a
            // lie and the reason a 360 dp legend runs out of room. Defaults to the pad case off an
            // Activity (preview/tests), like GamepadHintBar's own glyph choice.
            val padIsGamepad = (LocalContext.current as? MainActivity)?.lastPadIsGamepad ?: true
            val sections = listOfNotNull(
                GamepadHint('⇄', Color(0xFF9A93C7), "Section", onClick = { stepTab(1) })
                    .takeIf { padIsGamepad },
            )
            GamepadHintBar(
                if (tabFocused) listOf(
                    GamepadHint('↔', Color(0xFF9A93C7), "Section"),
                    PadGlyph.hint('A', "Open") { tabFocused = false },
                    PadGlyph.hint('B', "Done", onClick = onBack),
                ) else sections + when {
                    focused != null && !focused.enabled -> listOf(
                        PadGlyph.hint('B', "Done", onClick = onBack),
                    )
                    focused != null && !focused.adjustable -> listOf(
                        PadGlyph.hint('A', "Pin to hosts") { focused.activate() },
                        PadGlyph.hint('B', "Done", onClick = onBack),
                    )
                    else -> listOf(
                        GamepadHint('↔', Color(0xFF9A93C7), "Adjust"),
                        // Tappable too (touch escape hatch): Change cycles the focused row, Done leaves.
                        PadGlyph.hint('A', "Change") { rows.getOrNull(focus)?.activate() },
                        PadGlyph.hint('B', "Done", onClick = onBack),
                    )
                },
                hazeState = hazeState,
            )
        }

        // The pin-to-hosts picker for the activated profile row — the console counterpart of the
        // touch UI's per-profile pin toggles in the host edit sheet.
        pinProfile?.let { p ->
            GamepadPinHostsDialog(
                profileName = p.name,
                hosts = savedHosts,
                pinned = { kh -> p.id in kh.pinnedProfileIds },
                onToggle = { kh -> togglePin(kh, p) },
                onDismiss = { pinProfile = null },
            )
        }
    }
}

@Composable
private fun SettingRowView(row: GpRow, focused: Boolean, adjustDir: Int, onClick: () -> Unit) {
    val ink = LocalGamepadInk.current
    val visuals = animateConsoleFocus(active = focused)
    val shape = RoundedCornerShape(14.dp)
    // The chevrons keep their layout slot and only fade, so the value never jumps sideways when
    // focus arrives; the value colour cross-fades with them. A non-adjustable row (a profile row
    // navigates, the empty-catalog placeholder does nothing) never shows them at all.
    val chevronAlpha by animateFloatAsState(
        if (focused && row.adjustable) 0.6f else 0f,
        tween(160),
        label = "chevrons",
    )
    val valueColor by animateColorAsState(
        ink.fg(if (focused) 1f else 0.6f),
        tween(160),
        label = "valueColor",
    )
    Column {
        if (row.header != null) {
            Text(
                row.header.uppercase(),
                style = MaterialTheme.typography.labelMedium,
                color = ink.fg(0.45f),
                letterSpacing = 1.4.sp,
                modifier = Modifier.padding(start = 16.dp, top = 14.dp, bottom = 4.dp),
            )
        }
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .graphicsLayer { scaleX = visuals.scale; scaleY = visuals.scale }
                .clip(shape)
                .background(visuals.background)
                .border(1.dp, visuals.border, shape)
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
                    // A disabled row (the "No profiles yet" placeholder) dims but stays focusable,
                    // so its detail line can still explain what would go here.
                    color = ink.fg(if (row.enabled) 1f else 0.45f),
                    maxLines = 1,
                )
                Spacer(Modifier.weight(1f))
                if (row.toggled != null) {
                    // A toggle is a switch, not text — the sliding knob + tinting track IS the value.
                    ConsoleSwitch(on = row.toggled, focused = focused)
                } else {
                    Text("‹ ", color = ink.fg, modifier = Modifier.graphicsLayer { alpha = chevronAlpha })
                    // The value slides in the direction it was stepped and its width animates, so
                    // cycling a choice reads as motion through a list rather than a text swap.
                    AnimatedContent(
                        targetState = row.value,
                        transitionSpec = {
                            val dir = adjustDir
                            (slideInHorizontally(tween(180)) { w -> w / 2 * dir } + fadeIn(tween(180))) togetherWith
                                (slideOutHorizontally(tween(140)) { w -> -w / 2 * dir } + fadeOut(tween(100))) using
                                SizeTransform(clip = false)
                        },
                        label = "value",
                    ) { value ->
                        Text(
                            value,
                            style = MaterialTheme.typography.bodyMedium,
                            color = valueColor,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                    Text(" ›", color = ink.fg, modifier = Modifier.graphicsLayer { alpha = chevronAlpha })
                }
            }
            // The focused row carries its own one-line description — no dedicated (space-eating)
            // detail strip. It unfolds right where you're looking, and the row grows to fit.
            AnimatedVisibility(
                visible = focused && row.detail.isNotBlank(),
                enter = fadeIn(tween(180, delayMillis = 60)) + expandVertically(tween(180)),
                exit = fadeOut(tween(90)) + shrinkVertically(tween(150)),
            ) {
                Text(
                    row.detail,
                    style = MaterialTheme.typography.bodySmall,
                    color = ink.fg(0.6f),
                    maxLines = 2,
                    modifier = Modifier.padding(top = 6.dp),
                )
            }
        }
    }
}

/** Build the console settings rows from the current [Settings], writing through [update].
 * [hasBodyVibrator] gates the "Rumble on this phone" row (absent on TVs); [av1Capable] gates the
 * AV1 codec entry (see `codecOptionsFor`). Every row declares its [GpTab]; the screen shows one
 * tab at a time. */
internal fun buildSettingsRows(
    s: Settings,
    hasBodyVibrator: Boolean,
    av1Capable: Boolean,
    update: (Settings) -> Unit,
): List<GpRow> {
    fun <T> choice(
        id: String, tab: GpTab, header: String?, label: String, detail: String,
        options: List<Pair<T, String>>, current: T, enabled: Boolean = true, write: (T) -> Unit,
    ): GpRow {
        val idx = options.indexOfFirst { it.first == current }
        return GpRow(
            id, tab, header, label,
            value = options.getOrNull(idx)?.second ?: "—",
            detail = detail,
            enabled = enabled,
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
        id: String, tab: GpTab, header: String?, label: String, detail: String,
        value: Boolean, enabled: Boolean = true, write: (Boolean) -> Unit,
    ): GpRow = GpRow(
        id, tab, header, label,
        value = if (value) "On" else "Off",
        detail = detail,
        enabled = enabled,
        adjust = { delta -> val target = delta > 0; if (value != target) { write(target); true } else false },
        activate = { write(!value) },
        toggled = value,
    )

    // Grouped by the cross-client tab map (Stream / Video / Audio / Controller / Interface /
    // Profiles), so a setting sits under the same word whichever client you found it on. The ROWS
    // stay the couch-relevant subset: a pad can't drive a touch-input picker, and adding one for
    // the sake of symmetry would be parity in name only.
    return listOf(
        choice(
            "resolution", GpTab.STREAM, null, "Resolution",
            "The host creates a virtual display at exactly this size — no scaling. " +
                "Custom sizes are typed in the touch settings.",
            // A custom size (typed in the touch settings) leads the list so it stays visible and
            // selectable here instead of being silently snapped to Native — a pad can keep a
            // custom size, it just can't type one.
            (if (s.isCustomResolution()) {
                listOf((s.width to s.height) to "Custom · ${s.width} × ${s.height}")
            } else {
                emptyList()
            }) + RESOLUTION_OPTIONS.map { (w, h, lbl) -> (w to h) to lbl },
            s.width to s.height,
        ) { (w, h) -> update(s.copy(width = w, height = h)) },
        choice(
            "refresh", GpTab.STREAM, null, "Refresh rate",
            "Frame rate the host renders and streams at.",
            REFRESH_OPTIONS, s.hz,
        ) { update(s.copy(hz = it)) },
        choice(
            "bitrate", GpTab.STREAM, null, "Bitrate",
            "Automatic uses the host's default. A host's options (Up on its tile) can measure the " +
                "link and set an informed value.",
            BITRATE_OPTIONS, s.bitrateKbps,
        ) { update(s.copy(bitrateKbps = it)) },
        choice(
            "compositor", GpTab.STREAM, "Host output", "Compositor",
            "Which compositor drives the virtual output — honored only if available on the host.",
            COMPOSITOR_OPTIONS.mapIndexed { i, lbl -> i to lbl }, s.compositor,
        ) { update(s.copy(compositor = it)) },

        choice(
            "codec", GpTab.VIDEO, null, "Video codec",
            "A preference — the host falls back if it can't encode this one.",
            codecOptionsFor(s.codec, av1Capable), s.codec,
        ) { update(s.copy(codec = it)) },
        toggle(
            "hdr", GpTab.VIDEO, null, "10-bit HDR",
            "HDR10 — engages when the host sends HDR content and this display supports it.",
            s.hdrEnabled,
        ) { update(s.copy(hdrEnabled = it)) },
        toggle(
            "lowLatency", GpTab.VIDEO, "Decoding", "Low-latency mode",
            "The fast pipeline (async decode + system tuning). On by default — turn off to fall back if the stream stutters or glitches.",
            s.lowLatencyMode,
        ) { update(s.copy(lowLatencyMode = it)) },

        choice(
            "audio", GpTab.AUDIO, null, "Audio channels",
            "The speaker layout requested from the host.",
            AUDIO_CHANNEL_OPTIONS, s.audioChannels,
        ) { update(s.copy(audioChannels = it)) },
        toggle(
            "mic", GpTab.AUDIO, null, "Microphone",
            "Send this device's microphone to the host's virtual mic.",
            s.micEnabled,
        ) { update(s.copy(micEnabled = it)) },
        toggle(
            "echoCancel", GpTab.AUDIO, null, "Echo cancellation",
            "Filter the stream's own audio out of the mic pickup. Applies while the microphone is on.",
            s.echoCancel,
        ) { update(s.copy(echoCancel = it)) },

        toggle(
            "padForward", GpTab.CONTROLLER, null, "Forward controllers",
            "Send this device's controllers to the host. Turn it off when your controller " +
                "already reaches the host another way — USB passthrough such as VirtualHere — " +
                "so games don't see two of them.",
            s.gamepadForwarding,
        ) { update(s.copy(gamepadForwarding = it)) },
        // Everything below the master switch follows it — dim and inert while nothing is being
        // forwarded, the same relationship the touch settings draw with `enabled =`. This screen
        // had the capability (`GpRow.enabled`) and used it only for the profiles placeholder, so
        // the pad rows kept stepping settings that had nothing to act on.
        choice(
            "padType", GpTab.CONTROLLER, null, "Controller type",
            "The virtual pad the host creates — Automatic matches this controller.",
            GAMEPAD_OPTIONS, s.gamepad, enabled = s.gamepadForwarding,
        ) { update(s.copy(gamepad = it)) },
        choice(
            "systemButtons", GpTab.CONTROLLER, null, "Guide button",
            "Where the guide (Xbox/PS) and share presses go while streaming — Automatic " +
                "sends them to the host whenever this device delivers them.",
            SYSTEM_BUTTON_OPTIONS, s.systemButtons, enabled = s.gamepadForwarding,
        ) { update(s.copy(systemButtons = it)) },
        choice(
            "guideGesture", GpTab.CONTROLLER, null, "Hold Select for guide",
            "Hold Select alone to press the host's guide button — keep holding for a " +
                "Gaming-Mode host's quick-access menu. A Select tap still goes through.",
            GUIDE_GESTURE_OPTIONS, s.guideGesture, enabled = s.gamepadForwarding,
        ) { update(s.copy(guideGesture = it)) },
    ) + listOfNotNull(
        if (hasBodyVibrator) {
            toggle(
                "phoneRumble", GpTab.CONTROLLER, null, "Rumble on this phone",
                "Also play controller 1's rumble on this phone's own vibration motor — " +
                    "for clip-on pads without rumble motors.",
                s.rumbleOnPhone,
            ) { update(s.copy(rumbleOnPhone = it)) }
        } else {
            null
        },
    ) + listOf(
        // NOT gated on the vibrator (the bug A2 fixed in the touch settings): an SC2 capture has
        // nothing to do with this device's motor, and a TV box is where it matters most.
        toggle(
            "sc2", GpTab.CONTROLLER, "Passthrough", "Steam Controller 2 passthrough",
            "Capture a Steam Controller 2 (wired, Puck dongle, or paired Bluetooth) and stream " +
                "it as-is — Steam on the host drives it like the physical pad.",
            s.sc2Capture, enabled = s.gamepadForwarding,
        ) { update(s.copy(sc2Capture = it)) },
        // The SC2 row's twin, and missing here until now: the touch settings have carried both
        // side by side, so a couch user on a TV box — where there IS no touch interface to fall
        // back to — could turn on SC2 passthrough but not the Sony one. Same no-vibrator-gate
        // reasoning: this capture renders feedback on the CONTROLLER's motors, not this device's.
        toggle(
            "dsCapture", GpTab.CONTROLLER, null, "DualSense / DualShock passthrough (USB)",
            "Drive a USB-connected Sony pad directly — rumble on any phone, plus adaptive " +
                "triggers, lightbar and gyro.",
            s.dsCapture, enabled = s.gamepadForwarding,
        ) { update(s.copy(dsCapture = it)) },

        // The palette leads Interface: it is the one row whose effect you can see while you step
        // it (the backdrop behind this very list recolours), so it wants to be the first thing
        // found in the section.
        choice(
            "palette", GpTab.INTERFACE, null, "Background",
            "The colour family this backdrop drifts through — it changes as you step, so pick by " +
                "looking. Appearance only.",
            GamepadPalette.ALL.map { it.id to it.name },
            GamepadPalette.named(s.uiPalette).id,
        ) { update(s.copy(uiPalette = it)) },
        choice(
            "hud", GpTab.INTERFACE, null, "Statistics overlay",
            "How much the overlay shows: Compact (one line) → Normal → Detailed (full HUD). " +
                "A 3-finger tap cycles the tiers live.",
            STATS_VERBOSITY_OPTIONS, s.statsVerbosity,
        ) { update(s.copy(statsVerbosity = it)) },
        toggle(
            "autoWake", GpTab.INTERFACE, null, "Auto-wake on connect",
            "Wake a saved host with Wake-on-LAN when it isn't seen on the network, then connect.",
            s.autoWakeEnabled,
        ) { update(s.copy(autoWakeEnabled = it)) },
        toggle(
            "library", GpTab.INTERFACE, null, "Game library",
            "Browse a paired host's games with Y (experimental).",
            s.libraryEnabled,
        ) { update(s.copy(libraryEnabled = it)) },
        toggle(
            "gamepadUI", GpTab.INTERFACE, null, "Controller-optimized UI",
            "Turn off to use the touch interface even with a controller connected.",
            s.gamepadUiEnabled,
        ) { update(s.copy(gamepadUiEnabled = it)) },
    )
}


/**
 * The trailing Profiles section — the Android mirror of the desktop console's (design §5.2a, §5.4):
 * one row per catalog profile, valued with how many saved hosts pin it, activating into the
 * pin-to-hosts picker. Read-only beyond pinning: profiles are created and edited in the standard
 * interface, so an empty catalog shows one dimmed placeholder explaining where they come from
 * instead of a dead-looking empty tab. On a TV that phrasing changes: "touch interface" points
 * nowhere useful on a touchless device, so the strings name the actual route — the
 * Controller-optimized UI toggle a few rows up, which swaps the standard interface in
 * (d-pad-navigable; the profile editor lives there on every device, unlike tvOS where none exists).
 */
private fun buildProfileRows(
    profiles: List<StreamProfile>,
    savedHosts: List<KnownHost>,
    tv: Boolean,
    openPinPicker: (StreamProfile) -> Unit,
): List<GpRow> {
    val createHint = if (tv) {
        "To create or edit profiles on this device, turn off Controller-optimized UI above " +
            "and use the standard interface."
    } else {
        "Profiles are created and edited in the touch interface."
    }
    if (profiles.isEmpty()) {
        return listOf(
            GpRow(
                id = "noProfiles",
                tab = GpTab.PROFILES,
                header = null,
                label = "No profiles yet",
                value = "",
                detail = "Profiles bundle stream settings for different uses — pinned ones become " +
                    "one-press connect cards here. " + createHint,
                adjust = { false },
                activate = {},
                adjustable = false,
                enabled = false,
            ),
        )
    }
    return profiles.map { p ->
        // Counted straight off the host records, so it agrees with what the carousel renders.
        val pins = savedHosts.count { p.id in it.pinnedProfileIds }
        GpRow(
            id = "profile:${p.id}",
            tab = GpTab.PROFILES,
            header = null,
            label = p.name,
            value = when (pins) {
                0 -> "Not pinned"
                1 -> "Pinned to 1 host"
                else -> "Pinned to $pins hosts"
            },
            detail = "Pin this profile to a host and it appears as its own card — one press " +
                "connects with it. " + createHint,
            adjust = { false },
            activate = { openPinPicker(p) },
            adjustable = false,
        )
    }
}
