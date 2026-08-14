package io.unom.punktfunk.screenshots

import android.content.Context
import android.content.res.Configuration
import android.graphics.Bitmap
import android.graphics.BlendMode
import android.graphics.Canvas
import android.graphics.LinearGradient
import android.graphics.Paint
import android.graphics.Path
import android.graphics.RadialGradient
import android.graphics.Shader
import android.graphics.Typeface
import android.graphics.drawable.BitmapDrawable
import android.graphics.drawable.ColorDrawable
import android.graphics.drawable.Drawable
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.BatteryFull
import androidx.compose.material.icons.filled.SignalCellular4Bar
import androidx.compose.material.icons.filled.Wifi
import androidx.compose.material3.Icon
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.BrandDark
import io.unom.punktfunk.ConnectModal
import io.unom.punktfunk.ConnectPhase
import io.unom.punktfunk.ConnectTakeover
import androidx.compose.runtime.CompositionLocalProvider
import io.unom.punktfunk.GamepadHome
import io.unom.punktfunk.GamepadInk
import io.unom.punktfunk.GamepadPalette
import coil.ImageLoader
import coil.test.FakeImageLoaderEngine
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import io.unom.punktfunk.AddHostSheet
import io.unom.punktfunk.ConsoleControllersScreen
import io.unom.punktfunk.ConsoleHeader
import io.unom.punktfunk.ConsoleLegendInset
import io.unom.punktfunk.ConsoleLicensesScreen
import io.unom.punktfunk.ControllersScreen
import io.unom.punktfunk.Coverflow
import io.unom.punktfunk.GamepadAuroraBackground
import io.unom.punktfunk.GamepadHintBar
import io.unom.punktfunk.PadGlyph
import io.unom.punktfunk.PadInfo
import io.unom.punktfunk.consoleLegendInsets
import io.unom.punktfunk.consoleSafeArea
import io.unom.punktfunk.kit.Gamepad
import io.unom.punktfunk.kit.library.Artwork
import io.unom.punktfunk.kit.library.GameEntry
import androidx.compose.ui.platform.LocalConfiguration
import io.unom.punktfunk.GamepadSettingsScreen
import io.unom.punktfunk.HomeTile
import io.unom.punktfunk.LocalGamepadInk
import io.unom.punktfunk.LocalGamepadPalette
import io.unom.punktfunk.Settings
import io.unom.punktfunk.TouchMode
import io.unom.punktfunk.SettingsCategory
import io.unom.punktfunk.SettingsScreen
import io.unom.punktfunk.StatsOverlay
import io.unom.punktfunk.StatsVerbosity
import io.unom.punktfunk.StreamStartBanner
import io.unom.punktfunk.ProfileEditorFields
import io.unom.punktfunk.ProfileStore
import io.unom.punktfunk.SettingsOverlay
import io.unom.punktfunk.SpeedTestPrompt
import io.unom.punktfunk.SpeedTestPhase
import io.unom.punktfunk.SpeedTestTarget
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.HostMenuItem
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.newProfile
import io.unom.punktfunk.models.HostStatus

// The CI screenshot scenes: the REAL app composables, fed embedded mock state, under the forced
// brand palette (Material You has no wallpaper to seed from on the JVM). The stream-video surface
// and ConnectScreen/App are intentionally absent — they require the live JNI core / a session.

/** Forces the deterministic punktfunk brand scheme (see Theme.kt) instead of dynamic colour. */
@Composable
internal fun ShotTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = BrandDark, content = content)
}

/**
 * Robolectric has no system UI, so every capture was missing the status bar and the content sat
 * where the bar belongs — on the Pixel render the app title collided with the camera punch-hole.
 * This frame draws a plausible bar (time left, radios right, the CENTRE left empty for the hole)
 * and pushes the scene below it, the same geometry real insets produce. The height mirrors a
 * Pixel's tall bar as measured off a real 1344×2992 capture (~145 px ≈ 40 dp).
 */
@Composable
internal fun ShotStatusFrame(content: @Composable () -> Unit) {
    Column(Modifier.fillMaxSize().background(MaterialTheme.colorScheme.background)) {
        Row(
            Modifier.fillMaxWidth().height(40.dp).padding(horizontal = 28.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                "21:47",
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
            )
            Row(
                horizontalArrangement = Arrangement.spacedBy(5.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(
                    Icons.Filled.Wifi, contentDescription = null,
                    tint = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
                    modifier = Modifier.size(15.dp),
                )
                Icon(
                    Icons.Filled.SignalCellular4Bar, contentDescription = null,
                    tint = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
                    modifier = Modifier.size(14.dp),
                )
                Icon(
                    Icons.Filled.BatteryFull, contentDescription = null,
                    tint = MaterialTheme.colorScheme.onBackground.copy(alpha = 0.9f),
                    modifier = Modifier.size(16.dp),
                )
            }
        }
        Box(Modifier.weight(1f).fillMaxWidth()) { content() }
    }
}

private data class MockHost(
    val name: String,
    val address: String,
    val status: HostStatus,
    val profile: String? = null,
    val pin: String? = null,
    val accent: Color? = null,
    val online: Boolean = false,
)

// Ordered so an UNCHIPPED card sits beside a CHIPPED one in the same grid row, and a long trust
// label ("Trust on first use") beside a short one ("Paired"). Both are what used to make cards in a
// row step up and down — the grid sizes a row to its tallest item and doesn't stretch the rest — so
// this arrangement is the regression net for it.
private val SAVED = listOf(
    MockHost("Office", "192.168.1.50:9777", HostStatus.TOFU),
    MockHost(
        "Living Room PC", "192.168.1.42:9777", HostStatus.PAIRED,
        profile = "Game", pin = "Work", accent = Color(0xFFFF8A4C), online = true,
    ),
)
private val DISCOVERED = listOf(
    // Discovered ⇒ advertising right now, so both are online.
    MockHost("studio-deck", "192.168.1.61:9777", HostStatus.PAIRING, online = true),
    MockHost("HTPC", "192.168.1.70:9777", HostStatus.TOFU, online = true),
)

/** The connect screen's host grid, reconstructed from the real HostCard/SectionLabel components. */
@Composable
internal fun HostsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 160.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(16.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Spacer(Modifier.height(8.dp))
                    Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
                    Text(
                        "stream a remote desktop",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(24.dp))
                }
            }
            item(span = { GridItemSpan(maxLineSpan) }) { SectionLabel("Saved hosts") }
            // A pinned card is its OWN grid cell right after its host — the same flat list the
            // connect screen builds, not a second card crammed into the host's cell.
            SAVED.forEach { h ->
                item {
                    HostCard(
                        h.name, h.address, h.status, online = h.online, enabled = true,
                        onConnect = {}, onForget = {}, onEdit = {},
                        // The bound profile is a quiet chip: the card says what a tap will do.
                        profileLabel = h.profile,
                        accent = h.accent,
                        menuItems = listOf(
                            HostMenuItem("Connect with: Default settings", startsSection = true) {},
                            HostMenuItem("Connect with: Game") {},
                        ),
                        // One card in this section has a chip, so every card reserves its space —
                        // the shot is here to catch a row that steps.
                        reserveProfileSlot = true,
                    )
                }
                if (h.pin != null) {
                    item {
                        HostCard(
                            h.name, h.address, h.status, online = h.online, enabled = true,
                            onConnect = {}, onForget = null,
                            profileLabel = h.pin, profileProminent = true, accent = h.accent,
                            menuItems = listOf(HostMenuItem("Unpin card", startsSection = true) {}),
                            reserveProfileSlot = true,
                        )
                    }
                }
            }
            item(span = { GridItemSpan(maxLineSpan) }) {
                Spacer(Modifier.height(12.dp))
                SectionLabel("Discovered on the network")
            }
            items(DISCOVERED) { h ->
                HostCard(
                    h.name, h.address, h.status, online = h.online,
                    enabled = true, onConnect = {}, onForget = null,
                )
            }
        }
    }
}

/** A representative non-default settings state, shared by the settings scenes. */
private val SHOT_SETTINGS = Settings(
    width = 1920,
    height = 1080,
    hz = 120,
    bitrateKbps = 50_000,
    compositor = 1,
    gamepad = 2,
    micEnabled = true,
    statsVerbosity = StatsVerbosity.DETAILED,
    touchMode = TouchMode.TRACKPAD,
)

/**
 * The real SettingsScreen at its root — the shared category map (General / Display / Input /
 * Audio / Controllers / About) every client now presents.
 */
@Composable
internal fun SettingsScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(initial = SHOT_SETTINGS, onChange = {}, onBack = {})
    }
}

/**
 * One category page, seeded through `initialCategory` — the sub-section headers, the
 * caption-under-control fields and the "applies from the next session" footer only exist inside a
 * category, so the root shot alone can't regress-catch them. Display is the richest page.
 */
@Composable
internal fun SettingsCategoryScene(category: SettingsCategory) {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(
            initial = SHOT_SETTINGS,
            onChange = {},
            onBack = {},
            initialCategory = category,
        )
    }
}

/**
 * The same settings surface in a PROFILE's scope: the scope chips with "Game" selected, only
 * profileable rows, every row showing the effective value, and the overridden ones carrying their
 * marker and reset. One settings UI, two layers — this shot is what proves it stayed one.
 */
@Composable
internal fun SettingsProfileScene() {
    val store = ProfileStore(LocalContext.current)
    val profile = remember {
        val p = newProfile("Game").copy(
            accent = "#FF8A4C",
            // A representative mix: a resolution and refresh the profile pins, and a codec — the
            // rest of the page keeps following the defaults, visibly unmarked.
            overrides = SettingsOverlay(width = 3840, height = 2160, hz = 120, codec = "h264"),
        )
        store.save(p)
        store.save(newProfile("Work"))
        p
    }
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        SettingsScreen(
            initial = SHOT_SETTINGS,
            onChange = {},
            onBack = {},
            initialCategory = SettingsCategory.Display,
            initialProfileId = profile.id,
        )
    }
}

/**
 * The speed test's result, in its most interesting shape: a host bound to a profile that INHERITS
 * bitrate, so both layers are defensible and both buttons are offered. The note under the numbers
 * is what stops "Apply" from being a write in an unknown direction.
 */
@Composable
internal fun SpeedTestScene() {
    SpeedTestPrompt(
        gamepadUi = false,
        hostName = "Living Room PC",
        target = SpeedTestTarget.Ask(newProfile("Game")),
        phase = SpeedTestPhase.Done(throughputKbps = 412_000, lossPct = 0.3, recommendedKbps = 288_400),
        onApply = {},
        onDismiss = {},
    )
}

/**
 * Creating a profile. Small, but it is the first thing a user meets when they reach for this
 * feature — and dialogs only get a shot each because a layout slip inside one is invisible from
 * every other scene (this one shipped with the field and its caption touching).
 */
@Composable
internal fun NewProfileScene() {
    Surface(Modifier.fillMaxSize(), color = MaterialTheme.colorScheme.background) {
        Column(Modifier.padding(24.dp), verticalArrangement = Arrangement.spacedBy(16.dp)) {
            Text("New profile", style = MaterialTheme.typography.headlineSmall)
            // The dialog's own body, not a rebuild of it — the layout under test is the real one.
            ProfileEditorFields(
                name = "Travel",
                accent = "#60A5FA",
                duplicate = false,
                creating = true,
                onNameChange = {},
                onAccentChange = {},
            )
            Text("Duplicate name", style = MaterialTheme.typography.headlineSmall)
            ProfileEditorFields(
                name = "Game",
                accent = "#FF8A4C",
                duplicate = true,
                creating = false,
                onNameChange = {},
                onAccentChange = {},
            )
        }
    }
}

/** The real TOFU AlertDialog (mirrors ConnectScreen's PendingTrust.Kind.TRUST_NEW), shown over the host grid. */
@Composable
internal fun TrustDialog() {
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Trust this host?") },
        text = {
            Column {
                Text("First connection to 192.168.1.61:9777.")
                Text("Fingerprint 9f8e7d6c5b4a3928…")
                Text(
                    "This host allows trust-on-first-use, but that can't tell an impostor " +
                        "from the real host. Pairing with a PIN is stronger — it proves both sides.",
                )
            }
        },
        confirmButton = { TextButton({}) { Text("Trust (TOFU)") } },
        dismissButton = { TextButton({}) { Text("Pair with PIN…") } },
    )
}

/** The PIN-pairing AlertDialog (mirrors ConnectScreen's PendingTrust.Kind.PAIR). The live screen
 *  uses OutlinedTextFields, but a TextField inside a Dialog window never reaches idle under
 *  Robolectric (its focus/cursor machinery animates forever) — so the PIN is shown as a static
 *  display here, which also reads better in a marketing shot. */
@Composable
internal fun PairDialog() {
    AlertDialog(
        onDismissRequest = {},
        title = { Text("Pair with PIN") },
        text = {
            Column {
                Text("Enter the 4-digit PIN shown on the host.")
                Spacer(Modifier.height(16.dp))
                Surface(
                    color = MaterialTheme.colorScheme.surfaceVariant,
                    shape = MaterialTheme.shapes.medium,
                    modifier = Modifier.fillMaxWidth(),
                ) {
                    Text(
                        "4  8  2  7",
                        style = MaterialTheme.typography.headlineMedium,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.fillMaxWidth().padding(vertical = 16.dp),
                    )
                }
                Spacer(Modifier.height(12.dp))
                Text(
                    "This device: Pixel 9 Pro",
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        confirmButton = { TextButton({}) { Text("Pair") } },
        dismissButton = { TextButton({}) { Text("Cancel") } },
    )
}

/**
 * The live stats HUD (the real StatsOverlay) over a synthetic "streamed frame" gradient, at the
 * given [verbosity] tier — one scene per tier documents how far each tones the overlay down.
 */
@Composable
internal fun StreamScene(verbosity: StatsVerbosity = StatsVerbosity.DETAILED) {
    Box(
        Modifier
            .fillMaxSize()
            .background(
                Brush.linearGradient(listOf(Color(0xFF2A1E5C), Color(0xFF0E1B3D), Color(0xFF06122B))),
            ),
    ) {
        // The full 35-double unified layout — NativeBridge.nativeVideoStats' KDoc is the
        // authoritative index list: [fps, mbps, e2eP50, e2eP95, latValid, skew, w, h, hz,
        // lostTotal, bitDepth, colorPrimaries, colorTransfer, chromaFormatIdc, hostNetP50,
        // decodeP50, hostP50, netP50, lost, skipped, fec, frames, dispValid, displayP50,
        // e2eDispP50, e2eDispP95, paceP50, latchP50, presents, presenterActive, feedP50, codecP50,
        // skippedOverflow, audioBufferMs, audioAvOffsetMs].
        // 10/9/16/1 = a 10-bit BT.2020 PQ (HDR) 4:2:0 feed so the DETAILED HUD renders its
        // video-feed line; the display stage is valid (dispValid 1) so the headline is the
        // directly-measured capture→displayed pair, less the excluded OS present floor (the 0.3
        // latch p50) — 1.5/2.3 shown from 1.8/2.6 raw — and the Phase-2 stage terms
        // (host 0.6 + network 0.3 + decode 0.4 + display 0.2) tile the shaved headline, with the
        // `os present +0.3 excluded` line naming what came off; the decoder label shows the ranked
        // low-latency decoder. Light per-window loss
        // (lost 2 · skipped 1 · FEC 5 of 238) so the reliability line (NORMAL/DETAILED) and the
        // compact loss flag both render.
        StatsOverlay(
            doubleArrayOf(
                238.0, 921.4, 1.3, 2.1, 1.0, 1.0, 5120.0, 1440.0, 240.0, 2.0,
                10.0, 9.0, 16.0, 1.0, 0.9, 0.4, 0.6, 0.3,
                2.0, 1.0, 5.0, 238.0,
                1.0, 0.5, 1.8, 2.6,
                // Timeline-presenter split: pace + latch tile the display term; presents ≈ fps.
                0.2, 0.3, 236.0, 1.0,
                // The decode term's own split (feed + codec = 0.4), and no overflow — the one
                // `skipped` above is benign newest-wins pacing, not a decoder falling behind.
                0.1, 0.3, 0.0,
                // The audio plane: a 28 ms ring placed 4 ms behind the picture — a converged sync
                // loop, i.e. inside the deadband it deliberately leaves alone.
                28.0, 4.0,
            ),
            verbosity = verbosity,
            decoderLabel = "c2.qti.hevc.decoder · low-latency",
            codecLabel = "HEVC",
            modifier = Modifier.align(Alignment.TopStart).padding(12.dp),
        )
    }
}

/**
 * The default-UI connect flow (the real [ConnectModal]) in each phase — instant "Connecting…"
 * feedback, the "Waking…" wait, and the wake-timed-out prompt. These render as a Material dialog over
 * the host grid, so the test composes [HostsScene] behind them and captures the whole screen.
 */
@Composable
internal fun ConnectingScene() =
    ConnectModal(ConnectPhase.Connecting("Living Room PC"), onCancel = {}, onRetry = {})

@Composable
internal fun WakingScene() =
    ConnectModal(
        ConnectPhase.Waking("Living Room PC", seconds = 12, connectsAfter = true),
        onCancel = {}, onRetry = {},
    )

@Composable
internal fun WakeTimedOutScene() =
    ConnectModal(ConnectPhase.WakeTimedOut("Living Room PC"), onCancel = {}, onRetry = {})

/**
 * The console / gamepad connect flow (the real full-screen [ConnectTakeover]) — the aurora backdrop
 * with a bottom hint bar, the same signature look the console home uses.
 */
@Composable
internal fun ConnectConsoleScene() =
    ConnectTakeover(ConnectPhase.Connecting("Living Room PC"), onCancel = {}, onRetry = {})

/**
 * The real console settings screen — the section tab strip, the glass rows, the focused row's
 * unfolded detail, and the living (calmed) backdrop behind them. The touch [SettingsScene] can't
 * stand in for it: this is a different screen with different navigation, and the strip is the part
 * a layout regression would eat first.
 */
/**
 * The start-of-stream banner over the same synthetic "streamed frame" — the real
 * [StreamStartBanner] at full opacity, since the caller owns the 6 s timer and a shot must not race
 * it. Two variants because the WORDS are the point: the banner names pad chords or touch gestures
 * depending on what the session actually has, and a screenshot is the only place the two can be
 * compared side by side.
 */
@Composable
internal fun StreamBannerScene(pad: Boolean) {
    Box(
        Modifier
            .fillMaxSize()
            .background(
                Brush.linearGradient(
                    listOf(Color(0xFF2A1E5C), Color(0xFF0E1B3D), Color(0xFF06122B)),
                ),
            ),
    ) {
        StreamStartBanner(
            text = if (pad) {
                "Hold Select + Start + L1 + R1 to leave · Select + Y mic · Select + X stats"
            } else {
                "Back leaves the stream · three-finger tap for stats"
            },
            alpha = 1f,
            modifier = Modifier.align(Alignment.BottomCenter).padding(bottom = 24.dp),
        )
    }
}

/**
 * The console HOME — the host carousel over the living backdrop, which is the screen the aurora is
 * most of. Worth its own shot for exactly that reason: on API 33+ the field is the real bicubic
 * MESH (`GamepadAurora`'s AGSL port of the desktop console's shader) and below it the four-blob
 * fallback, and the two are only comparable side by side. The scene composes [GamepadHome]
 * directly with mock tiles — it needs no JNI core and no session, unlike the ConnectScreen that
 * normally feeds it.
 */
@Composable
internal fun ConsoleHomeScene(paletteId: String = "violet") {
    val palette = GamepadPalette.named(paletteId)
    val tiles = listOf(
        HomeTile(
            id = "living", title = "Living Room PC", subtitle = "192.168.1.42 · Paired",
            filled = true, online = true, paired = true, activate = {},
        ),
        HomeTile(
            id = "studio", title = "studio-deck", subtitle = "192.168.1.61 · Discovered",
            online = true, activate = {},
        ),
        HomeTile(id = "add", title = "Add Host", subtitle = "By address", isAdd = true, activate = {}),
    )
    CompositionLocalProvider(
        LocalGamepadPalette provides palette,
        LocalGamepadInk provides GamepadInk.of(palette),
    ) {
        GamepadHome(
            tiles = tiles,
            libraryEnabled = true,
            controllerName = "Xbox Wireless Controller",
            navActive = false,
            onActivate = {},
            onOpenLibrary = {},
            onOpenSettings = {},
        )
    }
}

/**
 * The two screens the console could not reach at all until WP8.3 — the open-source notices and the
 * connected-controllers view — in their console presentation.
 *
 * Worth a shot each, and worth a PALE one: both are ordinary Material screens underneath, and the
 * console shows them through a `ColorScheme` derived from the palette's ink. That derivation is the
 * whole risk. Their touch presentation is inked by the app theme, which is always dark, so nothing
 * before this could catch light-grey body text stranded on a pastel field.
 *
 * Robolectric enumerates no input devices, so the controllers scenes inject [shotPads] — the
 * deterministic connected-pads state the store listing needs.
 */
@Composable
internal fun ConsoleLicensesScene(paletteId: String = "violet") =
    ConsolePalette(paletteId) { ConsoleLicensesScreen(onBack = {}, navActive = false) }

@Composable
internal fun ConsoleControllersScene(paletteId: String = "violet") =
    ConsolePalette(paletteId) {
        // Robolectric enumerates no input devices, so the shot injects the two pads the store
        // listing talks about — the empty "no controller detected" state proves the palette but
        // sells nothing.
        ConsoleControllersScreen(
            gamepadSetting = 0, onBack = {}, navActive = false, padsOverride = shotPads(),
        )
    }

/**
 * The touch presentation of the same screen, with the same injected pads. Wrapped in a background
 * [Surface]: the activity provides the dark ground in the app, and without one here the content
 * color falls back to black-on-white while the cards stay dark.
 */
@Composable
internal fun ControllersScene() =
    Surface(color = MaterialTheme.colorScheme.background) {
        ControllersScreen(gamepadSetting = 0, onBack = {}, padsOverride = shotPads())
    }

/**
 * The "Add a host" bottom sheet over the host grid — the store's onboarding frame. State is
 * hoisted in production (ConnectScreen), so the scene passes a filled-in form directly; the
 * mode label mirrors what a paired 120 Hz phone shows on the connect button.
 */
@Composable
internal fun AddHostScene() {
    HostsScene()
    AddHostSheet(
        hostName = "Living Room PC", onHostNameChange = {},
        host = "192.168.1.42", onHostChange = {},
        port = "9777", onPortChange = {},
        connecting = false, modeLabel = "2992×1344@120",
        onDismiss = {}, onConnect = { _, _, _ -> },
    )
}

/** The two pads the store listing names: DualSense (adaptive triggers, LEDs, rumble) and Xbox. */
internal fun shotPads() = listOf(
    PadInfo(
        name = "DualSense Wireless Controller",
        detail = "054C:0CE6 · gamepad · joystick",
        forwarded = true, controllerNumber = 1,
        resolvedPref = Gamepad.PREF_DUALSENSE, canRumble = true,
    ),
    PadInfo(
        name = "Xbox Wireless Controller",
        detail = "045E:0B13 · gamepad · joystick",
        forwarded = true, controllerNumber = 2,
        resolvedPref = Gamepad.PREF_XBOXONE, canRumble = true,
    ),
)

/**
 * Publish the palette locals `App` would normally provide. A scene that calls a console screen
 * directly gets the DEFAULT dark ink without this, and a pale-palette shot would then silently
 * prove nothing at all.
 */
/**
 * The game-library coverflow (the real [Coverflow] over the real console chrome) with a mock shelf.
 * The library screen itself can't be shot — its state comes off the network — so the scene rebuilds
 * the same shell [io.unom.punktfunk.LibraryScreen] draws around it: aurora, header, floating hint
 * bar. Cover art is answered synchronously by coil-test's [FakeImageLoaderEngine] with generated
 * posters, so the frozen animation clock never races an async load.
 */
@Composable
internal fun LibraryScene(paletteId: String = "violet") = ConsolePalette(paletteId) {
    val context = LocalContext.current
    val loader = remember { shotLibraryLoader(context) }
    val games = remember { shotGames() }
    val hazeState = remember { HazeState() }
    val landscape =
        LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE
    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(hazeState)) {
            GamepadAuroraBackground(Modifier.fillMaxSize())
            Column(Modifier.fillMaxSize().consoleSafeArea()) {
                ConsoleHeader("Living Room PC — Library")
                Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                    Coverflow(games, loader, navActive = false, onLaunch = {})
                }
            }
        }
        Box(
            Modifier.align(Alignment.BottomStart)
                .consoleLegendInsets(landscape)
                .padding(ConsoleLegendInset),
        ) {
            GamepadHintBar(
                listOf(PadGlyph.hint('A', "Launch"), PadGlyph.hint('B', "Close")),
                hazeState = hazeState,
            )
        }
    }
}

/** A believable shelf: four titles with art plus the Steam launcher entry (brand-mark tile). */
private fun shotGames() = listOf(
    GameEntry("custom:aurora", "custom", "Aurora Drift", Artwork("shot://art/aurora", null, null)),
    GameEntry("steam:starfall", "steam", "Starfall Vale", Artwork("shot://art/starfall", null, null)),
    GameEntry("heroic:neon", "heroic", "Neon Circuit", Artwork("shot://art/neon", null, null)),
    GameEntry("gog:ember", "gog", "Ember Peaks", Artwork("shot://art/ember", null, null)),
    GameEntry("steam:launcher", "steam", "Steam", Artwork(null, null, null), role = "launcher", icon = "steam"),
)

private fun shotLibraryLoader(context: Context): ImageLoader {
    val engine = FakeImageLoaderEngine.Builder()
        .intercept("shot://art/aurora", poster(context, "AURORA DRIFT", ::drawAurora))
        .intercept("shot://art/starfall", poster(context, "STARFALL VALE", ::drawStarfall))
        .intercept("shot://art/neon", poster(context, "NEON CIRCUIT", ::drawNeon))
        .intercept("shot://art/ember", poster(context, "EMBER PEAKS", ::drawEmber))
        .default(ColorDrawable(0xFF221E44.toInt()))
        .build()
    return ImageLoader.Builder(context).components { add(engine) }.build()
}

// The four shelf posters, drawn procedurally at capture time — the same designs the Apple
// harness draws with CoreGraphics (`ShotPosterArt.swift`), so both listings show the same shelf.
// All geometry below is in a 600×900, y-UP space (matching the CG source); `posterY()` flips it.

private const val POSTER_W = 600
private const val POSTER_H = 900

private fun posterY(v: Float) = POSTER_H - v

/** Deterministic LCG (same constants and seeds as the Swift twin) so every capture is identical. */
private class ShotRand(var state: ULong) {
    fun next(): Float {
        state = state * 6364136223846793005UL + 1442695040888963407UL
        return (state shr 33).toFloat() / (1L shl 31).toFloat()
    }
    fun range(lo: Float, hi: Float) = lo + next() * (hi - lo)
}

private fun poster(context: Context, title: String, draw: (Canvas) -> Unit): Drawable {
    val bmp = Bitmap.createBitmap(POSTER_W, POSTER_H, Bitmap.Config.ARGB_8888)
    val canvas = Canvas(bmp)
    draw(canvas)
    posterTitle(canvas, title)
    return BitmapDrawable(context.resources, bmp)
}

/** Vertical gradient over the full canvas; stops bottom-to-top as (location, color). */
private fun sky(canvas: Canvas, stops: List<Pair<Float, Int>>) {
    canvas.drawRect(
        0f, 0f, POSTER_W.toFloat(), POSTER_H.toFloat(),
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            shader = LinearGradient(
                0f, POSTER_H.toFloat(), 0f, 0f,
                stops.map { it.second }.toIntArray(),
                stops.map { it.first }.toFloatArray(),
                Shader.TileMode.CLAMP,
            )
        },
    )
}

private fun glowDot(canvas: Canvas, x: Float, y: Float, radius: Float, color: Int) {
    canvas.drawCircle(
        x, posterY(y), radius,
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            shader = RadialGradient(
                x, posterY(y), radius, color, color and 0x00FFFFFF, Shader.TileMode.CLAMP,
            )
        },
    )
}

private fun shotAlpha(color: Int, a: Float) = (color and 0x00FFFFFF) or ((a * 255).toInt() shl 24)

/** Three strokes, wide-and-faint to thin-and-bright, in screen blend — the cheap neon glow. */
private fun glowStroke(canvas: Canvas, path: Path, width: Float, color: Int) {
    for ((mult, a) in listOf(2.6f to 0.12f, 1.3f to 0.28f, 0.55f to 0.85f)) {
        canvas.drawPath(
            path,
            Paint(Paint.ANTI_ALIAS_FLAG).apply {
                style = Paint.Style.STROKE
                strokeCap = Paint.Cap.ROUND
                strokeJoin = Paint.Join.ROUND
                strokeWidth = width * mult
                this.color = shotAlpha(color, a)
                blendMode = BlendMode.SCREEN
            },
        )
    }
}

private fun posterTitle(canvas: Canvas, title: String) {
    sky(canvas, listOf(0f to shotAlpha(0x000000, 0.55f), 0.22f to shotAlpha(0x000000, 0f)))
    canvas.drawText(
        title, POSTER_W / 2f, posterY(72f),
        Paint(Paint.ANTI_ALIAS_FLAG).apply {
            color = shotAlpha(0xFFFFFF, 0.94f)
            textSize = 46f
            letterSpacing = 5f / 46f
            typeface = Typeface.create("sans-serif-condensed", Typeface.BOLD)
            textAlign = Paint.Align.CENTER
            setShadowLayer(8f, 0f, 2f, shotAlpha(0x000000, 0.6f))
        },
    )
}

private fun drawAurora(canvas: Canvas) {
    sky(canvas, listOf(0f to 0xFF221E5C.toInt(), 0.45f to 0xFF141040.toInt(), 1f to 0xFF0B0830.toInt()))
    val rng = ShotRand(11UL)
    repeat(48) {
        val x = rng.range(0f, 600f)
        val y = rng.range(300f, 890f)
        val r = rng.range(1.4f, 3.2f)
        glowDot(canvas, x, y, r, shotAlpha(0xFFFFFF, rng.range(0.25f, 0.8f)))
    }
    data class Ribbon(
        val base: Float, val amp: Float, val freq: Float,
        val phase: Float, val w: Float, val c: Int,
    )
    for (r in listOf(
        Ribbon(700f, 55f, 1.15f, 0.4f, 30f, 0xFF6656F2.toInt()),
        Ribbon(615f, 70f, 1.4f, 2.2f, 24f, 0xFF8F7BFF.toInt()),
        Ribbon(530f, 45f, 0.95f, 4.1f, 18f, 0xFF35D0C5.toInt()),
    )) {
        val path = Path()
        for (i in 0..60) {
            val t = i / 60f
            val x = t * 600f
            val y = r.base + r.amp * kotlin.math.sin(t * Math.PI.toFloat() * r.freq + r.phase) + 40f * t
            if (i == 0) path.moveTo(x, posterY(y)) else path.lineTo(x, posterY(y))
        }
        glowStroke(canvas, path, r.w, r.c)
    }
    // A low ridge grounds the scene — without it the poster's bottom half is bare sky.
    for ((fill, baseline, rough) in listOf(
        Triple(0xFF191345.toInt(), 212f, 30f),
        Triple(0xFF0E0A2E.toInt(), 148f, 38f),
    )) {
        val path = Path()
        path.moveTo(0f, posterY(0f))
        path.lineTo(0f, posterY(baseline + rng.range(-rough, rough)))
        for (i in 1..9) {
            val x = i / 9f * 600f
            path.lineTo(x, posterY(baseline + rng.range(-rough, rough)))
        }
        path.lineTo(600f, posterY(0f))
        path.close()
        canvas.drawPath(path, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = fill })
    }
}

private fun drawStarfall(canvas: Canvas) {
    sky(
        canvas,
        listOf(
            0f to 0xFF2A0C24.toInt(), 0.35f to 0xFF7A2B58.toInt(),
            0.8f to 0xFFE86FA8.toInt(), 1f to 0xFFF7A8C8.toInt(),
        ),
    )
    val rng = ShotRand(23UL)
    repeat(6) {
        val hx = rng.range(60f, 560f)
        val hy = rng.range(420f, 840f)
        val len = rng.range(90f, 170f)
        val dx = kotlin.math.cos(2.15f)
        val dy = kotlin.math.sin(2.15f)
        val path = Path()
        path.moveTo(hx, posterY(hy))
        path.lineTo(hx + dx * len, posterY(hy + dy * len))
        glowStroke(canvas, path, 4f, 0xFFFFE3EF.toInt())
        glowDot(canvas, hx, hy, 11f, shotAlpha(0xFFFFFF, 0.9f))
    }
    for ((fill, baseline, rough) in listOf(
        Triple(0xFF3A1430.toInt(), 300f, 26f),
        Triple(0xFF1D0818.toInt(), 216f, 34f),
    )) {
        val path = Path()
        path.moveTo(0f, posterY(0f))
        path.lineTo(0f, posterY(baseline))
        for (i in 1..8) {
            val x = i / 8f * 600f
            path.lineTo(x, posterY(baseline + rng.range(-rough, rough)))
        }
        path.lineTo(600f, posterY(0f))
        path.close()
        canvas.drawPath(path, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = fill })
    }
}

private fun drawNeon(canvas: Canvas) {
    sky(canvas, listOf(0f to 0xFF0A2A33.toInt(), 1f to 0xFF04161C.toInt()))
    val rng = ShotRand(7UL)
    val ring = Path().apply {
        addOval(300f - 105f, posterY(560f) - 105f, 300f + 105f, posterY(560f) + 105f, Path.Direction.CW)
    }
    glowStroke(canvas, ring, 10f, 0xFF35D0C5.toInt())
    val gateX = listOf(-105f, 105f, 0f, 0f)
    val gateY = listOf(0f, 0f, -105f, 105f)
    for (i in 0 until 9) {
        var px: Float
        var py: Float
        if (i < 4) {
            px = 300f + gateX[i]
            py = 560f + gateY[i]
        } else {
            px = 40f * kotlin.math.round(rng.range(1f, 14f))
            py = 40f * kotlin.math.round(rng.range(1f, 21f))
        }
        val path = Path()
        path.moveTo(px, posterY(py))
        var horizontal = rng.next() > 0.5f
        repeat(rng.range(3f, 6f).toInt()) {
            val step = 40f * kotlin.math.round(rng.range(1f, 4f)) * (if (rng.next() > 0.5f) 1f else -1f)
            if (horizontal) px = (px + step).coerceIn(20f, 580f) else py = (py + step).coerceIn(20f, 880f)
            path.lineTo(px, posterY(py))
            horizontal = !horizontal
        }
        val color = if (rng.next() > 0.6f) 0xFF7FE8DE.toInt() else 0xFF35D0C5.toInt()
        glowStroke(canvas, path, 5f, color)
        glowDot(canvas, px, py, 12f, shotAlpha(color, 0.9f))
    }
}

private fun drawEmber(canvas: Canvas) {
    sky(
        canvas,
        listOf(
            0f to 0xFF200A04.toInt(), 0.3f to 0xFF7A2E12.toInt(),
            0.42f to 0xFFEF8F4B.toInt(), 1f to 0xFF2A0E06.toInt(),
        ),
    )
    glowDot(canvas, 300f, 385f, 160f, shotAlpha(0xFFC37A, 0.85f))
    val rng = ShotRand(41UL)
    for ((fill, baseline, rough) in listOf(
        Triple(0xFF5A2410.toInt(), 340f, 42f),
        Triple(0xFF401708.toInt(), 255f, 56f),
        Triple(0xFF200A04.toInt(), 165f, 48f),
    )) {
        val path = Path()
        path.moveTo(0f, posterY(0f))
        path.lineTo(0f, posterY(baseline + rng.range(-rough, rough)))
        for (i in 1..10) {
            val x = i / 10f * 600f
            path.lineTo(x, posterY(baseline + rng.range(-rough, rough)))
        }
        path.lineTo(600f, posterY(0f))
        path.close()
        canvas.drawPath(path, Paint(Paint.ANTI_ALIAS_FLAG).apply { color = fill })
    }
    repeat(20) {
        val x = rng.range(30f, 570f)
        val y = rng.range(180f, 620f)
        val r = rng.range(2.5f, 6f)
        glowDot(canvas, x, y, r, shotAlpha(0xFFB067, rng.range(0.35f, 0.9f)))
    }
}

@Composable
private fun ConsolePalette(paletteId: String, content: @Composable () -> Unit) {
    val palette = GamepadPalette.named(paletteId)
    CompositionLocalProvider(
        LocalGamepadPalette provides palette,
        LocalGamepadInk provides GamepadInk.of(palette),
        content = content,
    )
}

@Composable
internal fun ConsoleSettingsScene(paletteId: String = "violet") {
    // The scene calls the screen directly, so it has to publish the palette locals `App` would
    // normally provide — without them a light palette would render with the default DARK ink and
    // the shot would silently prove nothing.
    val palette = GamepadPalette.named(paletteId)
    CompositionLocalProvider(
        LocalGamepadPalette provides palette,
        LocalGamepadInk provides GamepadInk.of(palette),
    ) {
        GamepadSettingsScreen(
            initial = SHOT_SETTINGS.copy(uiPalette = paletteId), onChange = {}, onBack = {},
        )
    }
}
