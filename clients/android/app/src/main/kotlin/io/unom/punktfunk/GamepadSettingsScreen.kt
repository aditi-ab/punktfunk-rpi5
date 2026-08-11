package io.unom.punktfunk

import android.content.res.Configuration
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.SizeTransform
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.snap
import androidx.compose.animation.core.spring
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.displayCutout
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.union
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ChevronLeft
import androidx.compose.material.icons.filled.ChevronRight
import androidx.compose.material3.Icon
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
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.disabled
import androidx.compose.ui.semantics.hideFromAccessibility
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.toggleableState
import androidx.compose.ui.state.ToggleableState
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import io.unom.punktfunk.kit.DeviceGyro
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
    /**
     * What A does on a non-adjustable row, for the legend. It was the literal "Pin to hosts" in the
     * hint bar back when a profile row was the only kind of row that acted rather than stepped; a
     * row that opens the Controllers view was then advertised as pinning something.
     */
    val actionHint: String = "Open",
    /**
     * A choice row's full option list + where [value] sits in it — what the [ConsoleOptionBand]
     * drum turns through. Null (with [selectedIndex] -1) on everything that is not a stepped
     * choice: toggles are a switch, and the flat rows keep the quiet text slip.
     */
    val options: List<String>? = null,
    val selectedIndex: Int = -1,
)

/**
 * The row at [index], or null when it is dimmed. The single place the "disabled ⇒ inert" half of
 * [GpRow.enabled] is enforced, so the three input paths (pad left/right, A, and a tap on the
 * already-focused row) cannot drift apart — before this, `enabled` dimmed the label and nothing
 * else, and every dimmed row still stepped its setting.
 */
internal fun liveRow(rows: List<GpRow>, index: Int): GpRow? =
    rows.getOrNull(index)?.takeIf { it.enabled }

/**
 * Where the cursor was when a row opened a SUB-SCREEN. The shell holds it across the trip (this
 * screen's own state does not outlive it) and hands it back, so Back from the Controllers view lands
 * on the row that opened it rather than on the first row of the first section.
 *
 * The row is remembered by ID, not by index: a tab's length follows the hardware and the profile
 * catalog, and a remembered index is the stale-pointer bug the tab-switch clamp already exists for.
 */
data class GpSettingsPlace(val tab: GpTab, val rowId: String)

@Composable
fun GamepadSettingsScreen(
    initial: Settings,
    onChange: (Settings) -> Unit,
    onBack: () -> Unit,
    navActive: Boolean = true, // false while this screen is cross-fading out, so it drops the pad
    /** Open the connected-controllers view / the open-source notices — the shell pushes them. */
    onOpenControllers: () -> Unit = {},
    onOpenLicenses: () -> Unit = {},
    /** Where a return from one of those lands; null = a fresh entry, which starts at the top. */
    resume: GpSettingsPlace? = null,
    onPlace: (GpSettingsPlace) -> Unit = {},
) {
    var s by remember { mutableStateOf(initial) }
    fun update(next: Settings) { s = next; onChange(next) }

    val context = LocalContext.current
    // Gates the "Rumble on this phone" row — a TV box has no body vibrator to mirror onto.
    val hasBodyVibrator = remember { deviceBodyVibrator(context) != null }
    // Gates "Gyro from this phone" the same way — a TV box has no gyroscope to mirror from.
    val hasGyroscope = remember { DeviceGyro.available(context) }
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
    // The installed version, for the About row — the console is the ONLY interface on a TV box, so
    // the identity the touch About page states has to be reachable from here too.
    val appVersion = remember {
        runCatching {
            @Suppress("DEPRECATION")
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull().orEmpty()
    }
    val allRows = buildSettingsRows(
        s, hasBodyVibrator, hasGyroscope, av1Capable,
        appVersion = appVersion,
        openControllers = onOpenControllers,
        openLicenses = onOpenLicenses,
        update = ::update,
    ) + buildProfileRows(profiles, savedHosts, tv) { pinProfile = it }
    // Which section is showing, and where each one's focus was when it was last left — a detour
    // into another tab shouldn't lose your place.
    var tab by remember { mutableStateOf(resume?.tab ?: GpTab.STREAM) }
    // True while the STRIP holds the cursor rather than the list. Up from the first row moves
    // here and Down goes back — the only route to the sections on a D-pad remote, which has no
    // shoulder buttons at all (and is exactly what a TV box ships with).
    var tabFocused by remember { mutableStateOf(false) }
    val tabFocus = remember { mutableStateMapOf<GpTab, Int>() }
    val rows = allRows.filter { it.tab == tab }
    // Entry focus: the row a sub-screen was opened from, if we are coming back from one. Resolved
    // ONCE, against the first row list — after that the cursor belongs to this screen.
    var focus by remember {
        mutableIntStateOf(rows.indexOfFirst { it.id == resume?.rowId }.coerceAtLeast(0))
    }
    if (focus > rows.lastIndex) focus = rows.lastIndex.coerceAtLeast(0)

    // Which way the section last moved (+1 forward / -1 back) — the row list slides in from that
    // side, so stepping sections reads as travelling along a strip rather than teleporting.
    var tabDir by remember { mutableIntStateOf(1) }

    // L1/R1 — one section along, wrapping (the strip is a ring, like A's value cycle).
    fun selectTab(next: GpTab) {
        if (next == tab) return
        tabFocus[tab] = focus
        tabDir = if (next.ordinal > tab.ordinal) 1 else -1
        tab = next
        // Clamp: a tab's length follows the hardware and the catalog, so a remembered index can
        // outlive the row it pointed at.
        focus = (tabFocus[next] ?: 0)
            .coerceIn(0, (allRows.count { it.tab == next } - 1).coerceAtLeast(0))
    }
    fun stepTab(delta: Int) {
        val all = GpTab.entries
        val next = all[((all.indexOf(tab) + delta) % all.size + all.size) % all.size]
        selectTab(next)
        // A wrap (last → first) is still a step in the direction you pressed, whatever the ordinals
        // say — selectTab's ordinal compare would read it backwards.
        tabDir = delta
    }
    // The direction the focused value last stepped (+1 forward / -1 back) — drives which way the
    // value text slides in its AnimatedContent, so the motion matches the button press.
    var adjustDir by remember { mutableIntStateOf(1) }
    // Bumped on every ACCEPTED step of the focused row (the chevron ticks) and every REFUSED one
    // (the value gives a little and springs back). A press always gets an answer, even "no".
    var stepToken by remember { mutableIntStateOf(0) }
    var refusalToken by remember { mutableIntStateOf(0) }
    val haptics = rememberConsoleHaptics()

    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE

    // Act on a row, publishing where the cursor was FIRST. A row that opens a sub-screen unmounts
    // this one on the spot, so the place has to be out of here before its `activate` runs; every
    // activation route (pad, tap, the legend's own A cell) goes through this one door so none of
    // them can be the one that forgets.
    fun activate(row: GpRow) {
        onPlace(GpSettingsPlace(tab, row.id))
        adjustDir = 1
        row.activate()
    }

    // Step the focused row's value, answering a refusal rather than swallowing it.
    fun step(delta: Int) {
        adjustDir = delta
        val row = liveRow(rows, focus)
        if (row != null && row.adjust(delta)) {
            stepToken++
        } else {
            refusalToken++
            haptics.boundary()
        }
    }

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
                NavDir.LEFT -> if (tabFocused) stepTab(-1) else step(-1)
                NavDir.RIGHT -> if (tabFocused) stepTab(1) else step(1)
            }
        },
        // A on the strip drops into the section you picked, which is what "confirm" means there.
        onActivate = {
            if (tabFocused) tabFocused = false else liveRow(rows, focus)?.let { activate(it) }
        },
        // The shoulders work from either place — a real pad never has to visit the strip.
        onShoulder = { delta -> stepTab(delta) },
    )
    val animated = animationsEnabled()
    val hazeState = remember { HazeState() }
    val ink = LocalGamepadInk.current
    // The list runs to the PHYSICAL bottom edge (see the column's insets below), so the legend
    // zone's clearance has to carry the bottom bar inset itself. Landscape's zone is only the
    // pill — its detail lives in the side pane — so it clears less.
    val bottomInset = with(LocalDensity.current) {
        WindowInsets.systemBars.getBottom(this).toDp()
    }
    val legendClearance = (if (landscape) 92.dp else ConsoleLegendClearance) + bottomInset
    val legendClearancePx = with(LocalDensity.current) { legendClearance.roundToPx() }
    // The drum's fixed stage: a portrait phone is the one place the full width starves the row's
    // label, so it alone narrows it — the Apple band makes the same single exception. 132, not the
    // 156 of the first cut: at 156 the LABELS truncated ("Resoluti…"), and a clipped label loses
    // meaning where a drum value only loses its tail into the edge fade.
    val bandWidth = if (landscape) 220.dp else 132.dp

    /**
     * One section's rows as a scrolling pane. A composable local rather than inline because the
     * tab transition composes TWO of these at once (incoming and outgoing), and each needs its own
     * [LazyListState] — Compose refuses one state attached to two lists, which is why the previous
     * cut animated a single list's contents and read as the same fade in every direction.
     */
    val tabPane: @Composable (GpTab, Modifier) -> Unit = { paneTab, paneModifier ->
        val paneRows = if (paneTab == tab) rows else allRows.filter { it.tab == paneTab }
        val paneFocus = if (paneTab == tab) focus else (tabFocus[paneTab] ?: 0)
        // Seeded at the restored cursor, so re-entering a section lands where it was left without
        // a visible catch-up scroll on the first frame.
        val paneListState = rememberLazyListState(
            initialFirstVisibleItemIndex = paneFocus.coerceIn(0, paneRows.lastIndex.coerceAtLeast(0)),
        )
        // Keep the focused row on screen, but only SCROLL when it's actually off-screen. Only the
        // LIVE pane tracks the cursor; the outgoing one is a photograph on its way out.
        if (paneTab == tab) {
            LaunchedEffect(focus) {
                runCatching {
                    val info = paneListState.layoutInfo
                    val item = info.visibleItemsInfo.firstOrNull { it.index == focus }
                    val offScreen = item == null ||
                        item.offset < info.viewportStartOffset ||
                        // The SAME clearance the list pads its bottom with, rather than a literal
                        // that has to be remembered when the legend zone grows.
                        item.offset + item.size > info.viewportEndOffset - legendClearancePx
                    if (offScreen) paneListState.animateScrollToItem(focus)
                }
            }
        }
        LazyColumn(
            state = paneListState,
            // Capped at the Apple client's 620 row width: a landscape phone is WIDER than it is
            // useful, and a settings row stretched across 900 dp reads as a ribbon, not a control.
            // Start-aligned (not centred) so the rows and the side detail pane split the screen
            // rather than both crowding the middle.
            modifier = paneModifier.widthIn(max = 620.dp + ConsoleEdgeInset * 2),
            contentPadding = PaddingValues(
                start = ConsoleEdgeInset,
                end = ConsoleEdgeInset,
                top = 8.dp,
                // Clears the whole floating legend ZONE, bottom bar included — the list itself
                // runs to the screen edge now.
                bottom = legendClearance,
            ),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            itemsIndexed(paneRows, key = { _, r -> r.id }) { index, row ->
                val rowFocused = paneTab == tab && index == focus && !tabFocused
                SettingRowView(
                    row,
                    focused = rowFocused,
                    adjustDir = adjustDir,
                    // Only the focused row can be stepped, so only it needs to answer one.
                    stepToken = if (rowFocused) stepToken else 0,
                    refusalToken = if (rowFocused) refusalToken else 0,
                    bandWidth = bandWidth,
                    onClick = {
                        // Same inertness as the pad path above — tapping a dimmed row focuses it
                        // (so its detail explains itself) but never flips it.
                        tabFocused = false
                        if (focus != index) focus = index
                        else if (row.enabled) activate(row)
                    },
                )
            }
        }
    }

    /** The section switcher with its directional content — shared by both orientations below. */
    val tabbedContent: @Composable (Modifier) -> Unit = { contentModifier ->
        AnimatedContent(
            targetState = tab,
            modifier = contentModifier,
            transitionSpec = {
                if (!animated) {
                    fadeIn(tween(ConsoleMotion.REDUCED_MS)) togetherWith
                        fadeOut(tween(ConsoleMotion.REDUCED_MS))
                } else {
                    // DIRECTION-driven, with a real exit: the incoming section slides in from the
                    // side the press pointed at while the outgoing leaves the other way — paging
                    // along a strip. The previous cut slid a single list's contents 24 dp under an
                    // 85 % fade, which read as the same crossfade whichever shoulder was pressed.
                    val dir = tabDir
                    (
                        slideInHorizontally(ConsoleMotion.ease(ConsoleMotion.TAB_MS)) { it / 6 * dir } +
                            fadeIn(ConsoleMotion.ease(ConsoleMotion.TAB_MS))
                        ) togetherWith (
                        slideOutHorizontally(ConsoleMotion.ease(ConsoleMotion.TAB_MS)) { -it / 6 * dir } +
                            fadeOut(ConsoleMotion.ease(ConsoleMotion.TAB_MS))
                        )
                }
            },
            label = "settingsTab",
        ) { t ->
            tabPane(t, Modifier.fillMaxHeight())
        }
    }

    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(hazeState)) {
            // The backdrop stays full-bleed — it is ambience. The CHROME (strip, rows' start
            // edge) takes the safe area on the sides and top only: the LIST deliberately runs to
            // the physical bottom of the screen, with the bottom inset folded into its
            // contentPadding, so scrolled rows glide off the edge instead of being guillotined at
            // an invisible inset line 24 px above it (third on-glass verdict).
            GamepadFormBackground(Modifier.fillMaxSize())
            Column(
                Modifier
                    .fillMaxSize()
                    .windowInsetsPadding(
                        WindowInsets.systemBars.union(WindowInsets.displayCutout)
                            .only(WindowInsetsSides.Horizontal + WindowInsetsSides.Top),
                    ),
            ) {
                // The strip is PINNED while the rows scroll under it: it is this screen's primary
                // navigation, and a switcher you have to scroll back up to find isn't one.
                Row(
                    Modifier.fillMaxWidth().padding(top = 8.dp, bottom = 2.dp),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    ConsoleTabStrip(
                        titles = GpTab.entries.map { it.title },
                        selected = GpTab.entries.indexOf(tab),
                        onSelect = { tabFocused = false; selectTab(GpTab.entries[it]) },
                        modifier = Modifier.weight(1f),
                        focused = tabFocused,
                    )
                    // The base-layer marker, where a full "Default settings" heading used to eat a
                    // headline row on EVERY tab. The honesty it carried stays: this screen edits
                    // the defaults only — the console honours a host's profile but doesn't edit
                    // profiles (design §5.4) — and this quiet chip at the strip's end says so
                    // without a second heading repeating the tab pill's own word.
                    Text(
                        "Defaults",
                        style = MaterialTheme.typography.labelMedium,
                        color = ink.fg(0.45f),
                        maxLines = 1,
                        modifier = Modifier.padding(start = 10.dp, end = ConsoleEdgeInset),
                    )
                }
                if (landscape) {
                    Row(Modifier.fillMaxSize()) {
                        tabbedContent(Modifier.weight(0.6f))
                        // The focused row's description, in the width a wide phone wastes — beside
                        // the rows instead of floating over the list's tail (the portrait band).
                        // Presentation only: the row already merges this text into its own
                        // announcement, so the pane is hidden from a screen reader like the band.
                        val focusedRow = rows.getOrNull(focus)
                        AnimatedContent(
                            targetState = if (tabFocused) null else focusedRow,
                            transitionSpec = {
                                fadeIn(ConsoleMotion.ease(ConsoleMotion.FOCUS_MS)) togetherWith
                                    fadeOut(ConsoleMotion.ease(ConsoleMotion.FOCUS_MS))
                            },
                            modifier = Modifier
                                .weight(0.4f)
                                .semantics { hideFromAccessibility() },
                            label = "sideDetail",
                        ) { r ->
                            Column(
                                Modifier
                                    .fillMaxHeight()
                                    .padding(start = 6.dp, end = ConsoleEdgeInset, top = 22.dp),
                            ) {
                                if (r != null && r.detail.isNotBlank()) {
                                    Text(
                                        r.label,
                                        style = MaterialTheme.typography.titleMedium,
                                        fontWeight = FontWeight.SemiBold,
                                        color = ink.fg(0.85f),
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                    Text(
                                        r.detail,
                                        style = MaterialTheme.typography.bodyMedium,
                                        color = ink.fg(0.6f),
                                        modifier = Modifier.padding(top = 6.dp),
                                    )
                                }
                            }
                        }
                    }
                } else {
                    tabbedContent(Modifier.fillMaxSize())
                }
            }
        }

        // The floating legend ZONE: the focused row's description above, the controls pill below,
        // both frosted over whatever scrolls behind them. It is an OVERLAY, so nothing in it can
        // ever displace the list — which is the whole reason the detail moved here out of the row.
        // In landscape it ignores the system bars so it hugs the corner instead of the nav-bar
        // inset, but it still takes the display cutout (reverse-landscape parks the punch here).
        Box(
            Modifier
                .align(Alignment.BottomStart)
                .consoleLegendInsets(landscape)
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
                GamepadHint('⇄', PadGlyph.Arrow, "Section", onClick = { stepTab(1) })
                    .takeIf { padIsGamepad },
            )
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                // Portrait only: landscape's description lives in the side pane, where the wide
                // aspect has room for it — a band AND a pane would say the same thing twice.
                if (!landscape) {
                    ConsoleDetailBand(
                        // On the strip there is no row to describe, and the pills already name the
                        // sections — a stale row's description there would describe the wrong thing.
                        text = if (tabFocused) "" else focused?.detail.orEmpty(),
                        key = if (tabFocused) "__strip" else focused?.id,
                        hazeState = hazeState,
                    )
                }
                GamepadHintBar(
                    if (tabFocused) listOf(
                        GamepadHint('↔', PadGlyph.Arrow, "Section"),
                        PadGlyph.hint('A', "Open") { tabFocused = false },
                        PadGlyph.hint('B', "Done", onClick = onBack),
                    ) else sections + when {
                        focused != null && !focused.enabled -> listOf(
                            PadGlyph.hint('B', "Done", onClick = onBack),
                        )
                        // What A does here follows the ROW: it opens the pin picker on a profile,
                        // the connected-controllers view on that one, the notices on the About row.
                        // It was the literal "Pin to hosts" while profiles were the only such rows.
                        focused != null && !focused.adjustable -> listOf(
                            PadGlyph.hint('A', focused.actionHint) { activate(focused) },
                            PadGlyph.hint('B', "Done", onClick = onBack),
                        )
                        else -> listOf(
                            GamepadHint('↔', PadGlyph.Arrow, "Adjust"),
                            // Tappable too (touch hatch): Change cycles the focused row, Done leaves.
                            PadGlyph.hint('A', "Change") {
                                rows.getOrNull(focus)?.let { activate(it) }
                            },
                            PadGlyph.hint('B', "Done", onClick = onBack),
                        )
                    },
                    hazeState = hazeState,
                )
            }
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

/**
 * One settings row. Its geometry NEVER changes with focus — that is the whole design of it.
 *
 * It used to unfold its description in place, which meant every D-pad step shrank one row and grew
 * another, shifting the entire list under the cursor and moving the keep-focus-visible scroll's
 * target out from under it mid-animation. The description now lives in the screen's floating
 * [ConsoleDetailBand], which is an overlay and cannot displace anything. Focus changes colour,
 * lift and bloom here; it does not change size.
 *
 * The value gets the same treatment sideways: a fixed minimum slot, end-aligned, with the size
 * transform snapped so the slot's WIDTH never animates. Stepping a choice used to widen and narrow
 * that slot on every press, walking the ‹ chevron back and forth. Tabular figures finish the job —
 * without them `1920 × 1080 → 2560 × 1440` changes width on the digits alone.
 */
@Composable
private fun SettingRowView(
    row: GpRow,
    focused: Boolean,
    adjustDir: Int,
    stepToken: Int,
    refusalToken: Int,
    /** The option drum's fixed stage — sized by the SCREEN (orientation decides how much a row can spare). */
    bandWidth: Dp,
    onClick: () -> Unit,
) {
    val ink = LocalGamepadInk.current
    val visuals = animateConsoleFocus(active = focused)
    // The chevrons keep their layout slot and only fade, so the value never jumps sideways when
    // focus arrives; the value colour cross-fades with them. A non-adjustable row (a profile row
    // navigates, the empty-catalog placeholder does nothing) never shows them at all.
    val chevronAlpha by animateFloatAsState(
        if (focused && row.adjustable) 0.6f else 0f,
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "chevrons",
    )
    val valueColor by animateColorAsState(
        ink.fg(if (focused) 1f else 0.6f),
        ConsoleMotion.ease(ConsoleMotion.FOCUS_MS),
        label = "valueColor",
    )
    // A press always gets an answer. Accepted: the chevron on the pressed side ticks outward and
    // springs back. Refused: the whole value slot gives 4 dp toward the press and springs back —
    // the "door is locked" motion, so a limit reads as a limit instead of as a dropped input.
    val chevronKick = remember { Animatable(0f) }
    LaunchedEffect(stepToken) {
        if (stepToken == 0) return@LaunchedEffect
        chevronKick.snapTo(2f * adjustDir)
        chevronKick.animateTo(0f, spring(dampingRatio = 0.45f, stiffness = 900f))
    }
    val refusal = remember { Animatable(0f) }
    LaunchedEffect(refusalToken) {
        if (refusalToken == 0) return@LaunchedEffect
        refusal.animateTo(
            ConsoleMotion.REFUSAL_NUDGE.value * adjustDir,
            ConsoleMotion.ease(ConsoleMotion.REFUSAL_MS / 2),
        )
        refusal.animateTo(0f, spring(dampingRatio = 0.5f, stiffness = Spring.StiffnessMedium))
    }
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
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .consoleGlass(ConsoleShape.Row, visuals)
                .clickable(
                    interactionSource = remember { MutableInteractionSource() },
                    indication = null,
                    onClick = onClick,
                )
                // ONE announcement per row, not five leaves read in layout order. Three things had
                // to be gathered to make it true:
                //  * the VALUE of a toggle row existed nowhere in the tree — the switch replaces
                //    the value text (see below), so `row.value` ("On"/"Off") was drawn by nothing;
                //  * the DESCRIPTION lives in the floating band at the far bottom of the screen,
                //    which is the right place to LOOK and the wrong place to be read — so it is
                //    merged here, where the row it explains is;
                //  * `enabled` was a colour and nothing else.
                // A row therefore announces "Refresh rate, 120 Hz, Frame rate the host renders and
                // streams at" — which is what the screen already means, said once.
                .semantics(mergeDescendants = true) {
                    role = if (row.toggled != null) Role.Switch else Role.Button
                    contentDescription = listOfNotNull(
                        row.label,
                        row.value.takeIf { it.isNotBlank() },
                        row.detail.takeIf { it.isNotBlank() },
                    ).joinToString(", ")
                    row.toggled?.let {
                        toggleableState = if (it) ToggleableState.On else ToggleableState.Off
                    }
                    if (!row.enabled) disabled()
                }
                .padding(horizontal = 16.dp, vertical = 13.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                row.label,
                style = MaterialTheme.typography.bodyLarge,
                fontWeight = FontWeight.SemiBold,
                // A disabled row (the "No profiles yet" placeholder) dims but stays focusable, so
                // the detail band can still explain what would go here.
                color = ink.fg(if (row.enabled) 1f else 0.45f),
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                // Takes the slack rather than a Spacer doing it, so a long label ellipsizes into
                // the room it actually has instead of shoving the value slot off the row.
                modifier = Modifier.weight(1f),
            )
            Spacer(Modifier.width(8.dp))
            Row(
                modifier = Modifier.offset { IntOffset(refusal.value.dp.roundToPx(), 0) },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                if (row.toggled != null) {
                    // A toggle is a switch, not text — the sliding knob + tinting track IS the value.
                    ConsoleSwitch(on = row.toggled, focused = focused)
                } else {
                    Icon(
                        Icons.Filled.ChevronLeft,
                        // Decoration: it says "this value steps", which the row's Switch/Button
                        // role already says. Left in the tree it is read out on every focused row.
                        contentDescription = null,
                        tint = ink.fg,
                        modifier = Modifier
                            .size(18.dp)
                            .semantics { hideFromAccessibility() }
                            .graphicsLayer { alpha = chevronAlpha }
                            .offset { IntOffset(minOf(chevronKick.value, 0f).dp.roundToPx(), 0) },
                    )
                    if (row.options != null && row.selectedIndex in row.options.indices) {
                        // The drum — see ConsoleOptionBand. Its width is FIXED by the row, so a
                        // step can never reflow the chevrons, and the tabular-figures concern
                        // dissolves with it: nothing about the row's layout depends on the label.
                        ConsoleOptionBand(
                            options = row.options,
                            selection = row.selectedIndex,
                            focused = focused,
                            width = bandWidth,
                        )
                    } else {
                        // The flat rows (profile pin counts, the empty-catalog placeholder) keep
                        // the quiet slip: the changed string slides in following the motion.
                        AnimatedContent(
                            targetState = row.value,
                            transitionSpec = {
                                val dir = adjustDir
                                (
                                    slideInHorizontally(
                                        ConsoleMotion.ease(ConsoleMotion.VALUE_MS),
                                    ) { w -> w / 2 * dir } +
                                        fadeIn(ConsoleMotion.ease(ConsoleMotion.VALUE_MS))
                                    ) togetherWith (
                                    slideOutHorizontally(
                                        ConsoleMotion.ease(ConsoleMotion.VALUE_OUT_MS),
                                    ) { w -> -w / 2 * dir } +
                                        fadeOut(ConsoleMotion.ease(100))
                                    ) using SizeTransform(clip = false) { _, _ -> snap() }
                            },
                            label = "value",
                        ) { value ->
                            Text(
                                value,
                                style = MaterialTheme.typography.bodyMedium,
                                color = valueColor,
                                textAlign = TextAlign.End,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                            )
                        }
                    }
                    Icon(
                        Icons.Filled.ChevronRight,
                        contentDescription = null,
                        tint = ink.fg,
                        modifier = Modifier
                            .size(18.dp)
                            .semantics { hideFromAccessibility() }
                            .graphicsLayer { alpha = chevronAlpha }
                            .offset { IntOffset(maxOf(chevronKick.value, 0f).dp.roundToPx(), 0) },
                    )
                }
            }
        }
    }
}

/** Build the console settings rows from the current [Settings], writing through [update].
 * [hasBodyVibrator] gates the "Rumble on this phone" row and [hasGyroscope] the "Gyro from this
 * phone" row (both absent on TVs); [av1Capable] gates the AV1 codec entry (see
 * `codecOptionsFor`). [appVersion] is the installed version the About row states, and
 * [openControllers] / [openLicenses] are the two rows that navigate rather than set anything.
 * Every row declares its [GpTab]; the screen shows one tab at a time. */
internal fun buildSettingsRows(
    s: Settings,
    hasBodyVibrator: Boolean,
    hasGyroscope: Boolean,
    av1Capable: Boolean,
    appVersion: String = "",
    openControllers: () -> Unit = {},
    openLicenses: () -> Unit = {},
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
            options = options.map { it.second },
            selectedIndex = idx,
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
        // The rumble mirror's sibling, data flowing the other way — needs a gyroscope to
        // mirror FROM, which a TV box lacks.
        if (hasGyroscope) {
            toggle(
                "phoneGyro", GpTab.CONTROLLER, null, "Gyro from this phone",
                "When the controller has no gyro of its own, send this phone's motion " +
                    "sensors as controller 1's — for clip-on pads without one.",
                s.gyroOnPhone,
            ) { update(s.copy(gyroOnPhone = it)) }
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
        // The diagnostics view — same screen the touch settings reach, same words for it. It was
        // reachable from touch ONLY, which on a TV box means not at all: there is no touch interface
        // to fall back to there, and "my controller does nothing" is the support case it answers.
        //
        // Deliberately NOT gated on the master forwarding switch its neighbours all follow: this is
        // the row you reach for when forwarding looks broken, and a diagnostic that dims itself when
        // the thing it diagnoses is off is worse than none.
        //
        // No value: this row navigates, it doesn't hold a setting (a count read here would be a
        // snapshot, and a stale "none detected" is worse than no number at all — the screen it opens
        // watches hot-plug live).
        GpRow(
            id = "controllers",
            tab = GpTab.CONTROLLER,
            header = "Diagnostics",
            label = "Connected controllers",
            value = "",
            detail = "What the app detects, with a live input test.",
            adjust = { false },
            activate = openControllers,
            adjustable = false,
        ),

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
                "Select + X on a pad, or a 3-finger tap, cycles the tiers live.",
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
    ) + listOfNotNull(
        // WHEN the switch above takes over. Built only while it is ON: turn the switch off from
        // this very screen and the row under the cursor would otherwise be one deciding nothing,
        // on a screen that is itself about to disappear.
        if (s.gamepadUiEnabled) {
            choice(
                "gamepadUIMode", GpTab.INTERFACE, null, "Show it",
                "With a controller: the touch interface comes back when the last one " +
                    "disconnects. Always keeps this layout either way — for a device that lives " +
                    "docked to a TV. A TV itself is always in this mode regardless.",
                GAMEPAD_UI_MODE_OPTIONS, s.gamepadUiMode,
            ) { update(s.copy(gamepadUiMode = it)) }
        } else {
            null
        },
    ) + listOf(
        // About closes the Interface section, the way the touch settings' last category does. The
        // notices are a licence obligation and were reachable from touch only — on a TV box that is
        // nowhere. The version rides in the VALUE slot rather than as a second, inert row: it is the
        // identity half of an About page, and the screen this opens states it again at the top.
        GpRow(
            id = "licenses",
            tab = GpTab.INTERFACE,
            header = "About",
            label = "Open-source licenses",
            value = appVersion,
            detail = "Third-party notices and credits.",
            adjust = { false },
            activate = openLicenses,
            adjustable = false,
        ),
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
internal fun buildProfileRows(
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
            actionHint = "Pin to hosts",
        )
    }
}
