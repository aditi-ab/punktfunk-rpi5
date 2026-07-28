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
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
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
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedCard
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.VerticalDivider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
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

/**
 * Stream settings, organised as an iOS-Settings / Android-system-settings style list of category
 * subpages. On a phone the category list pushes to a full-screen detail; on a tablet / large screen
 * it becomes a two-pane list-detail (the list stays on the left, the detail on the right). Edits
 * persist immediately via [onChange]; [onBack] returns to the connect screen.
 *
 * **Structure mirrors the desktop/Apple settings revamp** ([SettingsCategory], and the Windows
 * client's `app/settings.rs`), so every client reads the same way: General = session/app behaviour,
 * Display = everything about the picture, Input = touch/keyboard/mouse, Audio, Controllers, About.
 * Each field carries its explanation DIRECTLY under it (the `described()` idiom — see
 * [SettingDropdown]'s `caption` and [ToggleRow]'s `subtitle`) rather than as loose paragraphs
 * floating between controls; the only form-level notes are the "applies from the next session"
 * footers, one per affected category.
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
) {
    var s by remember { mutableStateOf(initial) }
    val context = LocalContext.current
    var showLicenses by remember { mutableStateOf(false) }
    var showControllers by remember { mutableStateOf(false) }
    fun update(next: Settings) {
        s = next
        onChange(next)
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
    val selected = selectedName?.let { n -> SettingsCategory.entries.firstOrNull { it.name == n } }

    BoxWithConstraints(Modifier.fillMaxSize()) {
        val twoPane = maxWidth >= 640.dp
        // A two-column layout must never show an empty detail — land on the first category.
        LaunchedEffect(twoPane) {
            if (twoPane && selected == null) selectedName = SettingsCategory.General.name
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
                    selected = selected,
                    twoPane = true,
                    onSelect = { selectedName = it.name },
                    modifier = Modifier.width(300.dp).fillMaxHeight(),
                )
                VerticalDivider()
                Box(Modifier.weight(1f).fillMaxHeight()) {
                    // Cross-fade the detail pane as the selected category changes.
                    AnimatedContent(
                        targetState = selected ?: SettingsCategory.General,
                        transitionSpec = { fadeIn(tween(200)) togetherWith fadeOut(tween(200)) },
                        label = "SettingsPane",
                    ) { cat -> detail(cat, null) }
                }
            }
        } else {
            // Compact: the category list pushes to a full-screen detail and back, like the iOS /
            // Android system settings — a horizontal slide that tracks the drill-in direction.
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

/**
 * The top-level settings groups — each opens its own subpage (list on phone, split on tablet).
 * The map and its order are the cross-client one (Apple's `SettingsCategory`, the Windows
 * NavigationView, the GTK pages): General, Display, Input, Audio, Controllers, About.
 */
enum class SettingsCategory(val title: String, val icon: ImageVector) {
    General("General", Icons.Filled.Tune),
    Display("Display", Icons.Filled.Tv),
    Input("Input", Icons.Filled.TouchApp),
    Audio("Audio", Icons.AutoMirrored.Filled.VolumeUp),
    Controllers("Controllers", Icons.Filled.SportsEsports),
    About("About", Icons.Filled.Info),
}

/** The category list — the settings root. Highlights the [selected] row when it drives a detail pane. */
@Composable
private fun CategoryList(
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
        SettingsCategory.entries.forEach { cat ->
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

// ---- The categories ----------------------------------------------------------------------------

@Composable
private fun GeneralSettings(s: Settings, update: (Settings) -> Unit) {
    SettingsGroup("Session") {
        ToggleRow(
            title = "Auto-wake on connect",
            subtitle = "Connecting to a saved host that isn't seen on the network sends " +
                "Wake-on-LAN and waits for it to boot. Turn off if hosts behind a VPN look " +
                "offline when they aren't — connects then go straight through.",
            checked = s.autoWakeEnabled,
            onCheckedChange = { on -> update(s.copy(autoWakeEnabled = on)) },
        )
    }
    SettingsGroup("Statistics") {
        SettingDropdown(
            label = "Stats overlay",
            options = STATS_VERBOSITY_OPTIONS,
            selected = s.statsVerbosity,
            caption = "Live session stats in a corner overlay — Compact is a single " +
                "fps · latency · bitrate line, Normal adds the resolution and reliability lines, " +
                "Detailed adds the decoder, colour and latency-breakdown lines. A 3-finger tap " +
                "cycles the tiers live.",
        ) { v -> update(s.copy(statsVerbosity = v)) }
    }
    SettingsGroup("Library") {
        ToggleRow(
            title = "Game library",
            subtitle = "Browse a paired host's Steam and custom games and launch one directly " +
                "(press Y on a saved host). No extra host setup.",
            checked = s.libraryEnabled,
            onCheckedChange = { on -> update(s.copy(libraryEnabled = on)) },
        )
    }
    SettingsGroup("Interface") {
        ToggleRow(
            title = "Controller-optimized UI",
            subtitle = "Switch to the console home (host carousel) whenever a controller is " +
                "connected. Turn off to keep the touch interface. A TV is always in this mode.",
            checked = s.gamepadUiEnabled,
            onCheckedChange = { on -> update(s.copy(gamepadUiEnabled = on)) },
        )
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
            caption = "The host drives a real virtual output at exactly this size — true pixels, " +
                "no scaling. “Native display” follows this device's panel.",
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
            caption = "“Native” resolves to this display's refresh rate at connect.",
        ) { hz -> update(s.copy(hz = hz)) }
    }

    SettingsGroup("Quality") {
        SettingDropdown(
            label = "Render scale",
            options = RENDER_SCALE_OPTIONS,
            // Snap the stored value (a Float round-tripped to Double) to the nearest preset so the
            // exact Double keys match.
            selected = RenderScale.PRESETS.minByOrNull { kotlin.math.abs(it - s.renderScale) } ?: 1.0,
            caption = "Above native supersamples for sharpness, at more bandwidth AND decode; " +
                "below renders lighter on the host and the link. This device resamples the " +
                "result to the screen.",
        ) { scale -> update(s.copy(renderScale = scale)) }

        SettingDropdown(
            label = "Bitrate",
            options = BITRATE_OPTIONS,
            selected = s.bitrateKbps,
            caption = "Automatic lets the host decide (its default, clamped to what it supports).",
        ) { kbps -> update(s.copy(bitrateKbps = kbps)) }

        // AV1 is only offered when the device has a real AV1 decoder (it's never advertised to the
        // host otherwise, so preferring it would be a dead setting). A stored "av1" from a capable
        // device stays visible so the selection is always representable.
        val av1Capable = remember { VideoDecoders.pickDecoder("video/av01") != null }
        val codecOptions = CODEC_OPTIONS.filter { (v, _) -> v != "av1" || av1Capable || s.codec == "av1" }
        SettingDropdown(
            label = "Video codec",
            options = codecOptions,
            selected = s.codec,
            caption = "A preference — the host falls back if it can't encode this one.",
        ) { c -> update(s.copy(codec = c)) }

        // HDR is only meaningful on a panel that can present HDR10; on an SDR display the toggle is
        // disabled (and HDR is never advertised) so the host doesn't send PQ the panel mis-tone-maps.
        val hdrCapable = remember { displaySupportsHdr(context) }
        ToggleRow(
            title = "HDR",
            subtitle = if (hdrCapable) {
                "Stream 10-bit HDR (BT.2020 PQ) when the host has HDR content. HEVC only; " +
                    "otherwise the stream stays SDR."
            } else {
                "This display can't present HDR10 — streams stay SDR"
            },
            checked = s.hdrEnabled && hdrCapable,
            enabled = hdrCapable,
            onCheckedChange = { on -> update(s.copy(hdrEnabled = on)) },
        )
    }

    // The decode pipeline is a fact about THIS device's SoC, not about the stream — the desktop
    // clients group their decoder/GPU pickers here for the same reason. Android has no decoder or
    // adapter choice (MediaCodec resolves both), so the master toggle is this group's only row.
    SettingsGroup("Decoding") {
        ToggleRow(
            title = "Low-latency mode",
            subtitle = "The fast pipeline (async decode, per-device decoder selection, HDMI game " +
                "mode). On by default — turn off to fall back to the plain decode path if the " +
                "stream stutters or glitches on this device.",
            checked = s.lowLatencyMode,
            onCheckedChange = { on -> update(s.copy(lowLatencyMode = on)) },
        )
    }

    SettingsGroup("Host output", footer = "Display changes apply from the next session.") {
        SettingDropdown(
            label = "Compositor",
            options = COMPOSITOR_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.compositor,
            caption = "The backend the host uses for its virtual output (Linux hosts only). A " +
                "specific choice falls back to auto-detection when that backend isn't available.",
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
            caption = "Trackpad: relative cursor like a laptop touchpad — tap to click, " +
                "two-finger tap right-clicks, two fingers scroll, tap-then-drag holds the " +
                "button. Direct pointer: the cursor jumps to your finger. Touch passthrough: " +
                "real multi-touch reaches the host, for apps that understand touch.",
        ) { mode -> update(s.copy(touchMode = mode)) }
    }
    SettingsGroup("Keyboard & mouse") {
        ToggleRow(
            title = "Capture pointer for games",
            subtitle = "Lock a connected mouse to the stream and send raw relative motion " +
                "(mouse-look). Ctrl+Alt+Shift+Q toggles it live; click the stream to re-capture. " +
                "Off: the mouse points at the desktop directly",
            checked = s.pointerCapture,
            onCheckedChange = { on -> update(s.copy(pointerCapture = on)) },
        )
        ToggleRow(
            title = "Invert scroll direction",
            subtitle = "Reverses the wheel and two-finger touch scroll direction sent to the host",
            checked = s.invertScroll,
            onCheckedChange = { on -> update(s.copy(invertScroll = on)) },
        )
        ToggleRow(
            title = "Shared clipboard",
            subtitle = "Text copied here pastes on the host and vice versa (hosts with " +
                "clipboard sharing enabled)",
            checked = s.clipboardSync,
            onCheckedChange = { on -> update(s.copy(clipboardSync = on)) },
        )
    }
}

@Composable
private fun AudioSettings(s: Settings, update: (Settings) -> Unit, onMicChange: (Boolean) -> Unit) {
    SettingsGroup(footer = "Applies from the next session.") {
        SettingDropdown(
            label = "Audio channels",
            options = AUDIO_CHANNEL_OPTIONS,
            selected = s.audioChannels,
            caption = "The speaker layout requested from the host. It downmixes if its own " +
                "output has fewer channels.",
        ) { ch -> update(s.copy(audioChannels = ch)) }
        ToggleRow(
            title = "Microphone",
            subtitle = "This device's microphone feeds the host's virtual microphone",
            checked = s.micEnabled,
            onCheckedChange = onMicChange,
        )
    }
}

@Composable
private fun ControllerSettings(s: Settings, update: (Settings) -> Unit, onOpenControllers: () -> Unit) {
    SettingsGroup(footer = "Applies from the next session.") {
        SettingDropdown(
            label = "Controller type",
            options = GAMEPAD_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.gamepad,
            caption = "The virtual pad created on the host. Automatic matches your controller — " +
                "a DualSense keeps adaptive triggers, lightbar, touchpad and motion. Every " +
                "connected controller is forwarded, each as its own player.",
        ) { g -> update(s.copy(gamepad = g)) }
        ClickableRow(
            title = "Connected controllers",
            subtitle = "What the app detects, with a live input test",
            onClick = onOpenControllers,
        )
        // Only where the device has a body vibrator to mirror onto (a TV box doesn't).
        val context = LocalContext.current
        val hasBodyVibrator = remember { deviceBodyVibrator(context) != null }
        if (hasBodyVibrator) {
            ToggleRow(
                title = "Rumble on this phone",
                subtitle = "Also play controller 1's rumble on this phone's own vibration " +
                    "motor — for clip-on pads without rumble motors",
                checked = s.rumbleOnPhone,
                onCheckedChange = { on -> update(s.copy(rumbleOnPhone = on)) },
            )
            ToggleRow(
                title = "Steam Controller 2 passthrough",
                subtitle = "Capture a Steam Controller 2 (wired, Puck dongle, or paired " +
                    "Bluetooth): it navigates these menus and streams as-is — Steam on the " +
                    "host drives it like the physical pad (trackpads, gyro, haptics)",
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

/** A title + subtitle on the left, a Switch on the right. [enabled] greys out the whole row. */
@Composable
private fun ToggleRow(
    title: String,
    subtitle: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    enabled: Boolean = true,
) {
    // Dim the labels when disabled so the row reads as inactive (the Switch dims itself).
    val labelAlpha = if (enabled) 1f else 0.38f
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
 * loose paragraph between rows.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun <T> SettingDropdown(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    caption: String? = null,
    onSelect: (T) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = options.firstOrNull { it.first == selected }?.second
        ?: options.firstOrNull()?.second.orEmpty()
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
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
