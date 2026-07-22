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
import androidx.compose.material.icons.filled.Info
import androidx.compose.material.icons.filled.SportsEsports
import androidx.compose.material.icons.filled.Tune
import androidx.compose.material.icons.filled.Tv
import androidx.compose.material.icons.filled.VolumeUp
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
import androidx.core.content.ContextCompat
import io.unom.punktfunk.kit.VideoDecoders
import io.unom.punktfunk.kit.deviceBodyVibrator

/**
 * Stream settings, organised as an iOS-Settings / Android-system-settings style list of category
 * subpages. On a phone the category list pushes to a full-screen detail; on a tablet / large screen
 * it becomes a two-pane list-detail (the list stays on the left, the detail on the right). Edits
 * persist immediately via [onChange]; [onBack] returns to the connect screen.
 */
@Composable
fun SettingsScreen(
    initial: Settings,
    onChange: (Settings) -> Unit,
    onBack: () -> Unit,
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
    var selectedName by rememberSaveable { mutableStateOf<String?>(null) }
    val selected = selectedName?.let { n -> SettingsCategory.entries.firstOrNull { it.name == n } }

    BoxWithConstraints(Modifier.fillMaxSize()) {
        val twoPane = maxWidth >= 640.dp
        // A two-column layout must never show an empty detail — land on the first category.
        LaunchedEffect(twoPane) {
            if (twoPane && selected == null) selectedName = SettingsCategory.Display.name
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
                        targetState = selected ?: SettingsCategory.Display,
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

/** The top-level settings groups — each opens its own subpage (list on phone, split on tablet). */
enum class SettingsCategory(val title: String, val icon: ImageVector) {
    Display("Display", Icons.Filled.Tv),
    Audio("Audio", Icons.Filled.VolumeUp),
    Controls("Controls", Icons.Filled.SportsEsports),
    Interface("Interface", Icons.Filled.Tune),
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
            SettingsCategory.Display -> DisplaySettings(settings, onChange, context)
            SettingsCategory.Audio -> AudioSettings(settings, onChange, onMicChange)
            SettingsCategory.Controls -> ControlsSettings(settings, onChange, onOpenControllers)
            SettingsCategory.Interface -> InterfaceSettings(settings, onChange)
            SettingsCategory.About -> AboutSettings(onOpenLicenses)
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
    SettingsCard {
        SettingDropdown(
            label = "Resolution",
            options = RESOLUTION_OPTIONS.map { (w, h, lbl) -> (w to h) to (if (w == 0) "$lbl ($nw × $nh)" else lbl) } +
                // The (-1, -1) sentinel can't collide with a real size; once a custom size is
                // stored its label carries the live value, like the native row carries ($nw × $nh).
                ((-1 to -1) to if (s.isCustomResolution()) "Custom (${s.width} × ${s.height})" else "Custom…"),
            selected = if (showCustom) -1 to -1 else s.width to s.height,
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
        ) { hz -> update(s.copy(hz = hz)) }

        SettingDropdown(label = "Bitrate", options = BITRATE_OPTIONS, selected = s.bitrateKbps) { kbps ->
            update(s.copy(bitrateKbps = kbps))
        }

        SettingDropdown(
            label = "Render scale",
            options = RENDER_SCALE_OPTIONS,
            // Snap the stored value (a Float round-tripped to Double) to the nearest preset so the
            // exact Double keys match. > 1 supersamples for sharpness (more bandwidth AND decode);
            // < 1 renders under native for a lighter host — this device resamples to the display.
            selected = RenderScale.PRESETS.minByOrNull { kotlin.math.abs(it - s.renderScale) } ?: 1.0,
        ) { scale -> update(s.copy(renderScale = scale)) }

        // AV1 is only offered when the device has a real AV1 decoder (it's never advertised to the
        // host otherwise, so preferring it would be a dead setting). A stored "av1" from a capable
        // device stays visible so the selection is always representable.
        val av1Capable = remember { VideoDecoders.pickDecoder("video/av01") != null }
        val codecOptions = CODEC_OPTIONS.filter { (v, _) -> v != "av1" || av1Capable || s.codec == "av1" }
        SettingDropdown(label = "Video codec", options = codecOptions, selected = s.codec) { c ->
            update(s.copy(codec = c))
        }

        // HDR is only meaningful on a panel that can present HDR10; on an SDR display the toggle is
        // disabled (and HDR is never advertised) so the host doesn't send PQ the panel mis-tone-maps.
        val hdrCapable = remember { displaySupportsHdr(context) }
        ToggleRow(
            title = "HDR",
            subtitle = if (hdrCapable) {
                "Stream 10-bit HDR (BT.2020 PQ) when the host supports it"
            } else {
                "This display can't present HDR10 — streams stay SDR"
            },
            checked = s.hdrEnabled && hdrCapable,
            enabled = hdrCapable,
            onCheckedChange = { on -> update(s.copy(hdrEnabled = on)) },
        )

        SettingDropdown(
            label = "Compositor",
            options = COMPOSITOR_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.compositor,
        ) { c -> update(s.copy(compositor = c)) }

        ToggleRow(
            title = "Low-latency mode",
            subtitle = "The fast pipeline (async decode, per-device decoder selection, HDMI game " +
                "mode). On by default — turn off to fall back to the plain decode path if the stream " +
                "stutters or glitches on this device.",
            checked = s.lowLatencyMode,
            onCheckedChange = { on -> update(s.copy(lowLatencyMode = on)) },
        )
    }
}

@Composable
private fun AudioSettings(s: Settings, update: (Settings) -> Unit, onMicChange: (Boolean) -> Unit) {
    SettingsCard {
        SettingDropdown(label = "Audio channels", options = AUDIO_CHANNEL_OPTIONS, selected = s.audioChannels) { ch ->
            update(s.copy(audioChannels = ch))
        }
        ToggleRow(
            title = "Microphone",
            subtitle = "Send your mic to the host's virtual microphone",
            checked = s.micEnabled,
            onCheckedChange = onMicChange,
        )
    }
}

@Composable
private fun ControlsSettings(s: Settings, update: (Settings) -> Unit, onOpenControllers: () -> Unit) {
    SettingsCard {
        SettingDropdown(label = "Touch input", options = TOUCH_MODE_OPTIONS, selected = s.touchMode) { mode ->
            update(s.copy(touchMode = mode))
        }
        Text(
            "Trackpad: relative cursor like a laptop touchpad — tap to click, two-finger tap " +
                "right-clicks, two fingers scroll, tap-then-drag holds the button. Direct pointer: " +
                "the cursor jumps to your finger. Touch passthrough: real multi-touch reaches the " +
                "host, for apps that understand touch.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
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
            subtitle = "Flip the mouse wheel and two-finger touch scrolling",
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
    SettingsCard {
        SettingDropdown(
            label = "Controller type",
            options = GAMEPAD_OPTIONS.mapIndexed { i, lbl -> i to lbl },
            selected = s.gamepad,
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
private fun InterfaceSettings(s: Settings, update: (Settings) -> Unit) {
    SettingsCard {
        ToggleRow(
            title = "Controller-optimized UI",
            subtitle = "Switch to the console home (host carousel) when a controller is connected",
            checked = s.gamepadUiEnabled,
            onCheckedChange = { on -> update(s.copy(gamepadUiEnabled = on)) },
        )
        ToggleRow(
            title = "Game library",
            subtitle = "Browse a paired host's game library (press Y on a saved host)",
            checked = s.libraryEnabled,
            onCheckedChange = { on -> update(s.copy(libraryEnabled = on)) },
        )
        ToggleRow(
            title = "Auto-wake on connect",
            subtitle = "Send Wake-on-LAN and wait for a saved host to reappear on mDNS before " +
                "connecting. Turn off if a host that's already on isn't seen on mDNS, so connects " +
                "go straight through instead of waiting out the wake timeout.",
            checked = s.autoWakeEnabled,
            onCheckedChange = { on -> update(s.copy(autoWakeEnabled = on)) },
        )
        SettingDropdown(
            label = "Stats overlay",
            options = STATS_VERBOSITY_OPTIONS,
            selected = s.statsVerbosity,
        ) { v -> update(s.copy(statsVerbosity = v)) }
        Text(
            "How much the in-stream overlay shows: Compact is a single fps · latency · bitrate " +
                "line; Normal adds the resolution and reliability lines; Detailed adds the decoder, " +
                "colour and latency-breakdown lines. A 3-finger tap cycles the tiers live.",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun AboutSettings(onOpenLicenses: () -> Unit) {
    SettingsCard {
        ClickableRow(
            title = "Open-source licenses",
            subtitle = "Third-party notices and credits",
            onClick = onOpenLicenses,
        )
    }
}

/** A group of settings rendered inside an outlined card. */
@Composable
private fun SettingsCard(content: @Composable ColumnScope.() -> Unit) {
    OutlinedCard(modifier = Modifier.fillMaxWidth()) {
        Column(
            modifier = Modifier.padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(16.dp),
            content = content,
        )
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

/** A labelled read-only dropdown over [options] (value → label); calls [onSelect] on a pick. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun <T> SettingDropdown(
    label: String,
    options: List<Pair<T, String>>,
    selected: T,
    onSelect: (T) -> Unit,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = options.firstOrNull { it.first == selected }?.second
        ?: options.firstOrNull()?.second.orEmpty()
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
