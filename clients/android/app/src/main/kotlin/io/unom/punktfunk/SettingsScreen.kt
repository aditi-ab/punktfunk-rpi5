package io.unom.punktfunk

import android.Manifest
import android.content.pm.PackageManager
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.automirrored.filled.KeyboardArrowRight
import androidx.compose.material.icons.automirrored.filled.VolumeUp
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.TouchApp
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material.icons.filled.Tv
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.VerticalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.deviceBodyVibrator
import io.unom.punktfunk.kit.security.KnownHostStore

/**
 * Stream settings, organised as an iOS-Settings / Android-system-settings style list of category
 * subpages. On a phone the category list pushes to a full-screen detail; on a tablet / large screen
 * it becomes a two-pane list-detail (the list stays on the left, the detail on the right). Edits
 * persist immediately; [onBack] returns to the connect screen.
 *
 * **Structure mirrors the desktop/Apple settings revamp** ([SettingsCategory], and the Windows
 * client's `app/settings.rs`), so every client reads the same way: General = session/app behaviour,
 * Display = everything about the picture, Input = touch/keyboard/mouse, Audio, Controllers, About.
 * Each field carries its explanation DIRECTLY under it (the `described()` idiom — see
 * [SettingDropdown]'s `caption` and [ToggleRow]'s `subtitle`) rather than as loose paragraphs
 * floating between controls; the only form-level notes are the "applies from the next session"
 * footers, one per affected category.
 *
 * **One settings UI, not two.** The scope switcher on top edits either the global defaults
 * ([onChange], the base layer every profile inherits from) or one profile's overrides — the same
 * rows either way, so a profile can never drift from the surface it overrides. In profile scope
 * only profileable settings render, every row shows the EFFECTIVE value, and a row the profile
 * overrides carries a marker and a reset. See [SettingsOverlay] for the model.
 */
@Composable
fun SettingsScreen(
    initial: Settings,
    onChange: (Settings) -> Unit,
    onBack: () -> Unit,
    /**
     * Seeds the pushed detail page. The live app always starts on the category list (null); the
     * screenshot harness passes a category to capture one, the way the GTK client's
     * `PUNKTFUNK_SHOT_SETTINGS_SCOPE` seeds its scope.
     */
    initialCategory: SettingsCategory? = null,
    /** Seeds the scope the same way, for a screenshot of a profile being edited. */
    initialProfileId: String? = null,
) {
    var globals by remember { mutableStateOf(initial) }
    val context = LocalContext.current
    val profileStore = remember { ProfileStore(context) }
    val hostStore = remember { KnownHostStore(context) }
    var profiles by remember { mutableStateOf(profileStore.all()) }
    // Which layer is being edited: null = the global defaults, else a profile id. Survives rotation
    // but not a trip out of Settings — coming back should land on the defaults, which is what the
    // host cards actually connect with unless they say otherwise.
    var scopeId by rememberSaveable { mutableStateOf(initialProfileId) }
    // A profile deleted from under the scope (or a store that never had it) falls back to defaults.
    val active = scopeId?.let { id -> profiles.firstOrNull { it.id == id } }
    if (scopeId != null && active == null) scopeId = null

    var showLicenses by remember { mutableStateOf(false) }
    var showControllers by remember { mutableStateOf(false) }
    var naming by remember { mutableStateOf<NameIntent?>(null) }
    var deleting by remember { mutableStateOf<StreamProfile?>(null) }

    // Every row renders the EFFECTIVE value: the globals with this profile's overrides on top, so a
    // row the profile doesn't override reads as the live global — and keeps following it.
    val s = globals.effectiveFor(active)

    fun update(next: Settings) {
        val profile = active
        if (profile == null) {
            globals = next
            onChange(next)
        } else {
            // `absorb` against what the control was SHOWING, so picking today's global value is
            // still recorded as an override (the pin) — see [SettingsOverlay.absorb].
            profileStore.save(profile.copy(overrides = profile.overrides.absorb(s, next)))
            profiles = profileStore.all()
        }
    }

    fun resetField(field: String) {
        val profile = active ?: return
        profileStore.save(profile.copy(overrides = profile.overrides.clear(field)))
        profiles = profileStore.all()
    }

    // Mic uplink — turning it on requests RECORD_AUDIO; if denied, the toggle stays off.
    val micLauncher = rememberLauncherForActivityResult(
        ActivityResultContracts.RequestPermission(),
    ) { granted -> update(s.copy(micEnabled = granted)) }
    val onMicChange: (Boolean) -> Unit = { on ->
        when {
            !on -> update(s.copy(micEnabled = false))
            ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO) ==
                PackageManager.PERMISSION_GRANTED -> update(s.copy(micEnabled = true))
            else -> micLauncher.launch(Manifest.permission.RECORD_AUDIO)
        }
    }

    // Deep sub-screens replace the whole settings surface (they carry their own back).
    if (showLicenses) {
        LicensesScreen(onBack = { showLicenses = false })
        return
    }
    if (showControllers) {
        ControllersScreen(gamepadSetting = s.gamepad, onBack = { showControllers = false })
        return
    }

    // Selected category persists across rotation (stored by name — null = the bare list on a phone).
    var selectedName by rememberSaveable { mutableStateOf(initialCategory?.name) }
    val categories = SettingsCategory.entries.filter { active == null || it.profileable }
    val selected = selectedName
        ?.let { n -> categories.firstOrNull { it.name == n } }

    Column(Modifier.fillMaxSize()) {
        ProfileScopeChips(
            profiles = profiles,
            selectedId = active?.id,
            onSelect = { id ->
                scopeId = id
                // About has no profileable rows at all; don't strand the pane on it.
                if (id != null && selected?.profileable == false) selectedName = null
            },
            onNew = { naming = NameIntent.New },
            onRename = { active?.let { naming = NameIntent.Rename(it) } },
            onDuplicate = {
                active?.let { p ->
                    val copy = newProfile(uniqueName(profileStore, p.name)).copy(
                        accent = p.accent,
                        overrides = p.overrides,
                    )
                    profileStore.save(copy)
                    profiles = profileStore.all()
                    scopeId = copy.id
                }
            },
            onDelete = { deleting = active },
            modifier = Modifier.padding(top = 12.dp),
        )
        if (active != null) {
            Text(
                "Overrides the default settings for hosts that use “${active.name}”.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 20.dp, end = 20.dp, top = 10.dp),
            )
        }
        Spacer(Modifier.height(12.dp))
        HorizontalDivider()

        CompositionLocalProvider(
            LocalSettingsScope provides SettingsScopeState(
                profileScope = active != null,
                overridden = active?.overrides?.overridden() ?: emptySet(),
                onReset = ::resetField,
            ),
        ) {
            BoxWithConstraints(Modifier.fillMaxSize()) {
                val twoPane = maxWidth >= 640.dp
                // A two-column layout must never show an empty detail — land on the first category.
                LaunchedEffect(twoPane, categories) {
                    if (twoPane && selected == null) selectedName = categories.first().name
                }

                val detail: @Composable (SettingsCategory, (() -> Unit)?) -> Unit = { cat, back ->
                    CategoryDetail(
                        category = cat,
                        settings = s,
                        onChange = ::update,
                        context = context,
                        onMicChange = onMicChange,
                        onOpenControllers = { showControllers = true },
                        onOpenLicenses = { showLicenses = true },
                        onBack = back,
                    )
                }

                if (twoPane) {
                    BackHandler(onBack = onBack)
                    Row(Modifier.fillMaxSize()) {
                        CategoryList(
                            categories = categories,
                            selected = selected,
                            twoPane = true,
                            onSelect = { selectedName = it.name },
                            modifier = Modifier.width(300.dp).fillMaxHeight(),
                        )
                        VerticalDivider()
                        Box(Modifier.weight(1f).fillMaxHeight()) {
                            // Cross-fade the detail pane as the selected category changes.
                            AnimatedContent(
                                targetState = selected ?: categories.first(),
                                transitionSpec = { fadeIn(tween(200)) togetherWith fadeOut(tween(200)) },
                                label = "SettingsPane",
                            ) { cat -> detail(cat, null) }
                        }
                    }
                } else {
                    // Compact: the category list pushes to a full-screen detail and back, like the
                    // iOS / Android system settings — a horizontal slide tracking the drill-in.
                    BackHandler { if (selected != null) selectedName = null else onBack() }
                    AnimatedContent(
                        targetState = selected,
                        transitionSpec = {
                            if (targetState != null) {
                                slideInHorizontally { it } + fadeIn() togetherWith
                                    slideOutHorizontally { -it } + fadeOut()
                            } else {
                                slideInHorizontally { -it } + fadeIn() togetherWith
                                    slideOutHorizontally { it } + fadeOut()
                            }
                        },
                        label = "SettingsPush",
                    ) { sel ->
                        if (sel == null) {
                            CategoryList(
                                categories = categories,
                                selected = null,
                                twoPane = false,
                                onSelect = { selectedName = it.name },
                                modifier = Modifier.fillMaxSize(),
                            )
                        } else {
                            detail(sel) { selectedName = null }
                        }
                    }
                }
            }
        }
    }

    naming?.let { intent ->
        val editing = (intent as? NameIntent.Rename)?.profile
        ProfileNameDialog(
            title = if (editing == null) "New profile" else "Rename profile",
            initialName = editing?.name.orEmpty(),
            confirmLabel = if (editing == null) "Create" else "Rename",
            taken = { profileStore.nameTaken(it, except = editing?.id) },
            onConfirm = { name ->
                val saved = editing?.copy(name = name) ?: newProfile(name)
                profileStore.save(saved)
                profiles = profileStore.all()
                scopeId = saved.id
                naming = null
            },
            onDismiss = { naming = null },
        )
    }

    deleting?.let { profile ->
        val hosts = hostStore.all()
        DeleteProfileDialog(
            profile = profile,
            boundHosts = hosts.count { it.profileId == profile.id },
            pinnedCards = hosts.count { profile.id in it.pinnedProfileIds },
            onConfirm = {
                profileStore.delete(profile.id)
                profiles = profileStore.all()
                scopeId = null
                deleting = null
                // Bindings and pins are left dangling on purpose: they resolve to "no profile" and
                // to "no card", which is exactly right, and rewriting every host record here would
                // be a second write pass over data the user didn't ask us to touch.
            },
            onDismiss = { deleting = null },
        )
    }
}

/** What the name dialog is for. */
private sealed interface NameIntent {
    data object New : NameIntent
    data class Rename(val profile: StreamProfile) : NameIntent
}

/** "Work" → "Work copy" → "Work copy 2" — the first name Duplicate can actually save. */
private fun uniqueName(store: ProfileStore, base: String): String {
    val first = "$base copy"
    if (!store.nameTaken(first)) return first
    var n = 2
    while (store.nameTaken("$first $n")) n++
    return "$first $n"
}

// ---- Scope plumbing ----------------------------------------------------------------------------

/**
 * What the rows need to know about the layer being edited. Carried as a composition local rather
 * than threaded through every category and row: the rows are the same rows in both scopes, and the
 * scope only decides whether tier-G rows render at all and whether a row wears an override marker.
 */
private class SettingsScopeState(
    val profileScope: Boolean,
    val overridden: Set<String>,
    val onReset: (String) -> Unit,
)

private val LocalSettingsScope = compositionLocalOf {
    SettingsScopeState(profileScope = false, overridden = emptySet(), onReset = {})
}

/**
 * Wraps rows that are facts about THIS DEVICE or this app rather than about a stream — the console
 * UI toggle, the library browser, auto-wake, the controller diagnostics, rumble mirroring, SC2
 * capture (design §3, tiers G and H). They never belong to a profile, so in profile scope they
 * simply don't render.
 */
@Composable
private fun DeviceScopeOnly(content: @Composable () -> Unit) {
    if (!LocalSettingsScope.current.profileScope) content()
}

/**
 * The accent marker and reset a row wears when the selected profile overrides it. Nothing renders
 * in the defaults scope, or on a row the profile inherits — an inherited row shows the live global
 * value in the ordinary quiet style, which is the whole point of inherit-by-default.
 */
@Composable
private fun OverrideBadge(field: String?) {
    val scope = LocalSettingsScope.current
    if (!scope.profileScope || field == null || field !in scope.overridden) return
    // One compact line. A `TextButton` here carried its own 48dp touch target and dwarfed both the
    // control it annotates and the caption under it; the reset keeps a generous padded hit area
    // instead, which is the right trade for a secondary action inside a dense settings list.
    Row(
        verticalAlignment = Alignment.CenterVertically,
        // No vertical padding of its own: this row is the FIRST thing in a card whose column
        // already pads 16dp, and anything added here reads as double the gap every other row has.
        // The bottom padding is the badge's tie to the control it annotates — deliberately tighter
        // than the gap down to the caption, so the marker groups upward with its own field.
        modifier = Modifier.fillMaxWidth().padding(bottom = 6.dp),
    ) {
        AccentDot(MaterialTheme.colorScheme.primary, size = 6)
        Text(
            "Overridden",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier.padding(start = 6.dp).weight(1f),
        )
        Text(
            "Reset",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.primary,
            modifier = Modifier
                .clip(RoundedCornerShape(6.dp))
                .clickable { scope.onReset(field) }
                .padding(horizontal = 10.dp, vertical = 2.dp),
        )
    }
}

// ---- Categories --------------------------------------------------------------------------------

/**
 * The top-level settings groups — each opens its own subpage (list on phone, split on tablet).
 * The map and its order are the cross-client one (Apple's `SettingsCategory`, the Windows
 * NavigationView, the GTK pages): General, Display, Input, Audio, Controllers, About.
 *
 * [profileable] is false for a category with no profileable rows at all — it isn't offered in
 * profile scope, rather than opening onto an empty page.
 */
enum class SettingsCategory(
    val title: String,
    val icon: ImageVector,
    internal val profileable: Boolean = true,
) {
    General("General", Icons.Filled.Tune),
    Display("Display", Icons.Filled.Tv),
    Input("Input", Icons.Filled.TouchApp),
    Audio("Audio", Icons.AutoMirrored.Filled.VolumeUp),
    Controllers("Controllers", Icons.Filled.SportsEsports),
    About("About", Icons.Filled.Info, profileable = false),
}

/** The category list — the settings root. Highlights the [selected] row when it drives a detail pane. */
@Composable
private fun CategoryList(
    categories: List<SettingsCategory>,
    selected: SettingsCategory?,
    twoPane: Boolean,
    onSelect: (SettingsCategory) -> Unit,
    modifier: Modifier = Modifier,
) {
    Column(
        modifier
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 12.dp, vertical = 20.dp),
        verticalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Text(
            "Settings",
            style = MaterialTheme.typography.headlineMedium,
            modifier = Modifier.padding(start = 8.dp, bottom = 12.dp),
        )
        categories.forEach { cat ->
            val highlighted = twoPane && selected == cat
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .clip(RoundedCornerShape(14.dp))
                    .background(if (highlighted) MaterialTheme.colorScheme.secondaryContainer else Color.Transparent)
                    .clickable { onSelect(cat) }
                    .padding(horizontal = 14.dp, vertical = 15.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    cat.icon,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.padding(end = 16.dp),
                )
                Text(cat.title, style = MaterialTheme.typography.bodyLarge, modifier = Modifier.weight(1f))
                if (!twoPane) {
                    Icon(
                        Icons.AutoMirrored.Filled.KeyboardArrowRight,
                        contentDescription = null,
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
            }
        }
    }
}

/** One category's controls. [onBack] non-null (phone push) shows a back arrow; null (tablet pane) hides it. */
@Composable
private fun CategoryDetail(
    category: SettingsCategory,
    settings: Settings,
    onChange: (Settings) -> Unit,
    context: android.content.Context,
    onMicChange: (Boolean) -> Unit,
    onOpenControllers: () -> Unit,
    onOpenLicenses: () -> Unit,
    onBack: (() -> Unit)?,
) {
    Column(
        Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(horizontal = 20.dp, vertical = 16.dp),
        verticalArrangement = Arrangement.spacedBy(20.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            if (onBack != null) {
                IconButton(onClick = onBack, modifier = Modifier.padding(end = 4.dp)) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                }
            }
            Text(category.title, style = MaterialTheme.typography.headlineMedium)
        }
        when (category) {
            SettingsCategory.General -> GeneralSettings(settings, onChange)
            SettingsCategory.Display -> DisplaySettings(settings, onChange, context)
            SettingsCategory.Input -> InputSettings(settings, onChange)
            SettingsCategory.Audio -> AudioSettings(settings, onChange, onMicChange)
            SettingsCategory.Controllers -> ControllerSettings(settings, onChange, onOpenControllers)
            SettingsCategory.About -> AboutSettings(context, onOpenLicenses)
        }
    }
}

@Composable
private fun GeneralSettings(s: Settings, update: (Settings) -> Unit) {
    DeviceScopeOnly {
        SettingsGroup("Session") {
            ToggleRow(
                title = "Auto-wake on connect",
                subtitle = "Wake a sleeping saved host before connecting. Turn off if hosts " +
                    "behind a VPN look offline when they aren't.",
                checked = s.autoWakeEnabled,
                onCheckedChange = { on -> update(s.copy(autoWakeEnabled = on)) },
            )
        }
    }
    SettingsGroup("Statistics") {
        SettingDropdown(
            label = "Stats overlay",
            options = STATS_VERBOSITY_OPTIONS,
            selected = s.statsVerbosity,
            field = "stats_verbosity",
            caption = "Compact is one line; Detailed adds the decoder and latency breakdown. " +
                "A 3-finger tap cycles the tiers in-stream.",
        ) { v -> update(s.copy(statsVerbosity = v)) }
    }
    DeviceScopeOnly {
        SettingsGroup("Library") {
            ToggleRow(
                title = "Game library",
                subtitle = "Browse a paired host's games and launch one directly.",
                checked = s.libraryEnabled,
                onCheckedChange = { on -> update(s.copy(libraryEnabled = on)) },
            )
        }
        SettingsGroup("Interface") {
            ToggleRow(
                title = "Controller-optimized UI",
                subtitle = "Switch to the console home when a controller is connected. A TV " +
                    "always uses it.",
                checked = s.gamepadUiEnabled,
                onCheckedChange = { on -> update(s.copy(gamepadUiEnabled = on)) },
            )
        }
    }
}

@Composable
private fun DisplaySettings(s: Settings, update: (Settings) -> Unit, context: android.content.Context) {
    val (nw, nh, nhz) = nativeDisplayMode(context)
    // "Custom…" picked while the stored size is still a preset — keeps the size fields visible
    // until an edit actually makes it custom (or a preset is re-picked). Custom itself is detected
    // from the stored size, never flagged (see [isCustomResolution]), so nothing new persists.
    var customPicked by remember { mutableStateOf(false) }
    val showCustom = customPicked || s.isCustomResolution()
    SettingsGroup("Resolution") {
        SettingDropdown(
            label = "Resolution",
            options = RESOLUTION_OPTIONS.map { (w, h, lbl) -> (w to h) to (if (w == 0) "$lbl ($nw × $nh)" else lbl) } +
                // The (-1, -1) sentinel can't collide with a real size; once a custom size is
                // stored its label carries the live value, like the native row carries ($nw × $nh).
                ((-1 to -1) to if (s.isCustomResolution()) "Custom (${s.width} × ${s.height})" else "Custom…"),
            selected = if (showCustom) -1 to -1 else s.width to s.height,
            field = SettingsOverlay.FIELD_RESOLUTION,
            caption = "The host makes a display exactly this size — no scaling. Native follows " +
                "this device's panel.",
        ) { (w, h) ->
            if (w < 0) {
                // Seed from the current *effective* size so the fields start from something
                // sensible (the resolved native mode, not the 0 × 0 placeholder).
                customPicked = true
                update(s.copy(width = if (s.width > 0) s.width else nw, height = if (s.height > 0) s.height else nh))
            } else {
                customPicked = false
                update(s.copy(width = w, height = h))
            }
        }
        if (showCustom) {
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                ResolutionField(label = "Width", value = s.width, modifier = Modifier.weight(1f)) { w ->
                    update(s.copy(width = w))
                }
                ResolutionField(label = "Height", value = s.height, modifier = Modifier.weight(1f)) { h ->
                    update(s.copy(height = h))
                }
            }
        }

        SettingDropdown(
            label = "Refresh rate",
            options = REFRESH_OPTIONS.map { (hz, lbl) -> hz to (if (hz == 0) "$lbl ($nhz Hz)" else lbl) },
            selected = s.hz,
            field = "refresh_hz",
            caption = "Native follows this device's refresh rate.",
        ) { hz -> update(s.copy(hz = hz)) }
    }

    SettingsGroup("Quality") {
        SettingDropdown(
            label = "Render scale",
            options = RENDER_SCALE_OPTIONS,
            // Snap the stored value (a Float round-tripped to Double) to the nearest preset so the
            // exact Double keys match.
            selected = RenderScale.PRESETS.minByOrNull { kotlin.math.abs(it - s.renderScale) } ?: 1.0,
            field = "render_scale",
            caption = "Above native is sharper but costs bandwidth and decode; below is " +
                "lighter on the host.",
        ) { scale -> update(s.copy(renderScale = scale)) }

        SettingDropdown(
            label = "Bitrate",
            options = BITRATE_OPTIONS,
            selected = s.bitrateKbps,
            field = "bitrate_kbps",
            caption = "Automatic lets the host decide.",
        ) { kbps -> update(s.copy(bitrateKbps = kbps)) }

        // Only codecs this device can actually decode are offered — a preference the client never
        // advertises would be a dead setting (see [codecOptionsFor]).
        val av1Capable = remember { VideoDecoders.pickDecoder("video/av01") != null }
        SettingDropdown(
            label = "Video codec",
            options = codecOptionsFor(s.codec, av1Capable),
            selected = s.codec,
            field = "codec",
            caption = "A preference — the host falls back if it can't encode this one.",
        ) { c -> update(s.copy(codec = c)) }

        // HDR is only meaningful on a panel that can present HDR10; on an SDR display the toggle is
        // disabled (and HDR is never advertised) so the host doesn't send PQ the panel mis-tone-maps.
        val hdrCapable = remember { displaySupportsHdr(context) }
        ToggleRow(
            title = "HDR",
            subtitle = if (hdrCapable) {
                "10-bit HDR10, when the host has HDR content to send."
            } else {
                "This display can't present HDR10 — streams stay SDR"
            },
            checked = s.hdrEnabled && hdrCapable,
            enabled = hdrCapable,
            field = "hdr_enabled",
            onCheckedChange = { on -> update(s.copy(hdrEnabled = on)) },
        )
    }

    // The desktop clients group their decoder and GPU pickers here, and keep them out of profiles —
    // they are facts about that machine's hardware. Android has neither choice (MediaCodec resolves
    // both), and the one knob it does have IS worth varying per host: a marginal link is exactly
    // where you want the plain decode path back (design §3 lists it as an Android-only profileable).
    SettingsGroup("Decoding") {
        ToggleRow(
            title = "Low-latency mode",
            subtitle = "The fast decode pipeline. Turn it off if the stream stutters or " +
                "glitches on this device.",
            checked = s.lowLatencyMode,
            field = "low_latency_mode",
            onCheckedChange = { on -> update(s.copy(lowLatencyMode = on)) },
        )
    }

    SettingsGroup("Host output", footer = "Display changes apply from the next session.") {
        SettingDropdown(
            label = "Compositor",
            options = COMPOSITOR_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.compositor,
            field = "compositor",
            caption = "Linux hosts only; falls back to auto-detection when unavailable.",
        ) { c -> update(s.copy(compositor = c)) }
    }
}

@Composable
private fun InputSettings(s: Settings, update: (Settings) -> Unit) {
    SettingsGroup("Touch & pointer") {
        SettingDropdown(
            label = "Touch input",
            options = TOUCH_MODE_OPTIONS,
            selected = s.touchMode,
            field = "touch_mode",
            caption = "Trackpad moves the cursor by relative swipes; Direct pointer jumps it " +
                "to your finger; Passthrough sends real multi-touch.",
        ) { mode -> update(s.copy(touchMode = mode)) }
    }
    SettingsGroup("Keyboard & mouse") {
        SettingDropdown(
            label = "Mouse input",
            options = MOUSE_MODE_OPTIONS,
            selected = s.mouseMode,
            field = "mouse_mode",
            caption = "Capture locks the pointer to the stream for mouse-look; Desktop leaves " +
                "it free. Ctrl+Alt+Shift+Q flips it live.",
        ) { mode -> update(s.copy(mouseMode = mode)) }
        ToggleRow(
            title = "Invert scroll direction",
            subtitle = "Reverses wheel and two-finger scrolling",
            checked = s.invertScroll,
            field = "invert_scroll",
            onCheckedChange = { on -> update(s.copy(invertScroll = on)) },
        )
        // "Shared clipboard" is NOT here: it is a trust decision about one host, so it lives on the
        // host record and is edited from that host's Edit sheet.
    }
}

@Composable
private fun AudioSettings(s: Settings, update: (Settings) -> Unit, onMicChange: (Boolean) -> Unit) {
    SettingsGroup(footer = "Applies from the next session.") {
        SettingDropdown(
            label = "Audio channels",
            options = AUDIO_CHANNEL_OPTIONS,
            selected = s.audioChannels,
            field = "audio_channels",
            caption = "Requested from the host; it downmixes if it has fewer.",
        ) { ch -> update(s.copy(audioChannels = ch)) }
        ToggleRow(
            title = "Microphone",
            subtitle = "Feeds this device's microphone to the host",
            checked = s.micEnabled,
            field = "mic_enabled",
            onCheckedChange = onMicChange,
        )
    }
}

@Composable
private fun ControllerSettings(s: Settings, update: (Settings) -> Unit, onOpenControllers: () -> Unit) {
    SettingsGroup(footer = "Applies from the next session.") {
        SettingDropdown(
            label = "Controller type",
            options = GAMEPAD_OPTIONS,
            selected = s.gamepad,
            field = "gamepad",
            caption = "The virtual pad the host creates. Automatic matches your controller; " +
                "every connected one is forwarded as its own player.",
        ) { g -> update(s.copy(gamepad = g)) }
        DeviceScopeOnly {
            ClickableRow(
                title = "Connected controllers",
                subtitle = "What the app detects, with a live input test",
                onClick = onOpenControllers,
            )
            // Rumble mirroring needs a body vibrator to mirror ONTO — a TV box has none, so the row
            // would be a silent no-op there.
            val context = LocalContext.current
            val hasBodyVibrator = remember { deviceBodyVibrator(context) != null }
            if (hasBodyVibrator) {
                ToggleRow(
                    title = "Rumble on this phone",
                    subtitle = "Also play controller 1's rumble on this phone's motor",
                    checked = s.rumbleOnPhone,
                    onCheckedChange = { on -> update(s.copy(rumbleOnPhone = on)) },
                )
            }
            // NOT gated on the vibrator: SC2 passthrough is a USB/BLE capture that has nothing to do
            // with rumbling this device's body, and the gate hid the toggle on exactly the machines
            // that most want it — TV boxes, where a Steam Controller 2 is the whole input story.
            ToggleRow(
                title = "Steam Controller 2 passthrough",
                subtitle = "Stream a Steam Controller 2 as-is — Steam on the host drives its " +
                    "trackpads, gyro and haptics directly",
                checked = s.sc2Capture,
                onCheckedChange = { on -> update(s.copy(sc2Capture = on)) },
            )
        }
    }
}

@Composable
private fun AboutSettings(context: android.content.Context, onOpenLicenses: () -> Unit) {
    // The app's own version, read from the installed package (the WinUI/Apple About convention:
    // identity first, then the legal rows). Empty on a harness with no real package info.
    val version = remember {
        runCatching {
            @Suppress("DEPRECATION")
            context.packageManager.getPackageInfo(context.packageName, 0).versionName
        }.getOrNull().orEmpty()
    }
    SettingsGroup {
        Column {
            Text("Punktfunk", style = MaterialTheme.typography.titleLarge)
            if (version.isNotEmpty()) {
                Text(
                    "Version $version",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        ClickableRow(
            title = "Open-source licenses",
            subtitle = "Third-party notices and credits",
            onClick = onOpenLicenses,
        )
    }
}

// ---- Row / group primitives --------------------------------------------------------------------

/**
 * A group of settings rendered inside an outlined card, with an optional sub-section [header]
 * above it and an optional form-level [footer] beneath it. The header is what turns a long
 * category into the scannable sub-sections the desktop clients have ("Resolution", "Quality",
 * "Host output"); the footer carries the one "applies from the next session" note per category —
 * per-field guidance lives on the fields themselves.
 */
@Composable
private fun SettingsGroup(
    header: String? = null,
    footer: String? = null,
    content: @Composable ColumnScope.() -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        if (header != null) {
            Text(
                header.uppercase(),
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.primary,
                letterSpacing = 1.2.sp,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
        OutlinedCard(modifier = Modifier.fillMaxWidth()) {
            Column(
                modifier = Modifier.padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(16.dp),
                content = content,
            )
        }
        if (footer != null) {
            Text(
                footer,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
    }
}

/**
 * A title + subtitle on the left, a Switch on the right. [enabled] greys out the whole row;
 * [field] is the overlay field this row writes, which is what its override marker keys on.
 */
@Composable
private fun ToggleRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    enabled: Boolean = true,
    field: String? = null,
) {
    // Dim the labels when disabled so the row reads as inactive (the Switch dims itself).
    val labelAlpha = if (enabled) 1f else 0.38f
    Column {
        OverrideBadge(field)
        Row(modifier = Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(
                    title,
                    style = MaterialTheme.typography.bodyLarge,
                    color = MaterialTheme.colorScheme.onSurface.copy(alpha = labelAlpha),
                )
                Text(
                    subtitle,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant.copy(alpha = labelAlpha),
                )
            }
            Switch(checked = checked, onCheckedChange = onCheckedChange, enabled = enabled)
        }
    }
}

/** A title + subtitle on the left; the whole row is clickable (opens a sub-screen). */
@Composable
private fun ClickableRow(title: String, subtitle: String, onClick: () -> Unit) {
    Row(
        modifier = Modifier.fillMaxWidth().clickable(onClick = onClick),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, style = MaterialTheme.typography.bodyLarge)
            Text(
                subtitle,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Icon(
            Icons.AutoMirrored.Filled.KeyboardArrowRight,
            contentDescription = null,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.size(20.dp),
        )
    }
}

/**
 * A labelled read-only dropdown over [options] (value → label); calls [onSelect] on a pick.
 * [caption] is the field's own explanation, rendered directly under the control — the `described()`
 * idiom the other clients use, so a dropdown's guidance belongs to it instead of floating as a
 * loose paragraph between rows. [field] is the overlay field this row writes, which is what its
 * override marker keys on.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun <T> SettingDropdown(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    field: String? = null,
    caption: String? = null,
    onSelect: (T) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = options.firstOrNull { it.first == selected }?.second
        ?: options.firstOrNull()?.second.orEmpty()
    Column {
        OverrideBadge(field)
        ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { expanded = it }) {
            OutlinedTextField(
                value = selectedLabel,
                onValueChange = {},
                readOnly = true,
                label = { Text(label) },
                trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
                modifier = Modifier
                    .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable)
                    .fillMaxWidth(),
            )
            ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
                options.forEach { (value, lbl) ->
                    DropdownMenuItem(
                        text = { Text(lbl) },
                        onClick = {
                            onSelect(value)
                            expanded = false
                        },
                    )
                }
            }
        }
        if (caption != null) {
            Text(
                caption,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.padding(top = 10.dp),
            )
        }
    }
}

/** One side of a custom resolution. Digits only; every usable keystroke commits — coerced even
 * (encoders reject odd dimensions) and capped at 8192, the HEVC/AV1 per-side ceiling (the host
 * clamps H.264's tighter 4096 itself) — while the field keeps the raw text so intermediate states
 * ("15" on the way to "1512") aren't rewritten mid-typing; it snaps to the committed value when
 * focus leaves. */
@Composable
private fun ResolutionField(
    label: String,
    value: Int,
    modifier: Modifier = Modifier,
    onCommit: (Int) -> Unit,
) {
    var text by remember { mutableStateOf(if (value > 0) value.toString() else "") }
    OutlinedTextField(
        value = text,
        onValueChange = { raw ->
            text = raw.filter { it.isDigit() }.take(4)
            val v = (text.toIntOrNull() ?: 0).let { it - it % 2 }.coerceAtMost(8192)
            if (v > 0) onCommit(v)
        },
        label = { Text(label) },
        singleLine = true,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
        modifier = modifier.onFocusChanged { if (!it.isFocused) text = if (value > 0) value.toString() else "" },
    )
}
