package io.unom.punktfunk

import android.content.res.Configuration
import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.Spring
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.PageSize
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.BlurredEdgeTreatment
import androidx.compose.ui.draw.blur
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.util.lerp
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import io.unom.punktfunk.kit.security.KnownHost
import kotlin.math.absoluteValue
import kotlin.math.cos
import kotlinx.coroutines.launch

// The gamepad-driven home — the Android mirror of the Apple client's GamepadHomeView: a distinct,
// "10-foot" console-style host launcher shown INSTEAD of the touch grid while the console UI is
// active. A center-snapping carousel of hosts (saved first, then discovered, then a trailing Add
// Host tile), driven from the couch: A connects, X opens Settings, Y opens a saved host's library.

/**
 * How far a fully off-centre card turns away from the viewer, in radians (~48°). Never rendered as
 * a rotation — see the projection note at the call site.
 */
private const val CARD_TURN_RAD = 0.838f

/** One navigable launcher tile — a saved host, a discovered-but-unsaved host, or the Add Host action. */
class HomeTile(
    val id: String,
    val title: String,
    val subtitle: String,
    val filled: Boolean = false,     // saved (solid monogram) vs discovered / action (tinted outline)
    val online: Boolean = false,     // advertising on the LAN right now
    val paired: Boolean = false,     // pinned identity (shows a lock)
    val connecting: Boolean = false,
    val isAdd: Boolean = false,      // the trailing Add Host tile (plus icon, not a monogram)
    val knownHost: KnownHost? = null, // set for saved hosts → enables the library (Y)
    /**
     * Set when this tile is a PINNED host+profile combination rather than the host's own tile.
     * A pin is a shortcut, not a second host: the host-level actions (wake, edit, forget, library)
     * belong to the host's own tile, and this one offers only Unpin.
     */
    val pinnedProfileId: String? = null,
    /**
     * The profile a press will actually connect with — the host's binding, or the pin's own
     * profile. Rendered as a chip on the card rather than appended to the subtitle: on a PIN card
     * the profile is the entire reason the card exists, and a card that only whispers it in grey
     * body text can't say that. Matches the Apple client's tile.
     */
    val profileName: String? = null,
    /** The profile's `#RRGGBB` chip colour, if it set one. */
    val profileAccent: Color? = null,
    val activate: () -> Unit,
) {
    // Any SAVED host offers the library (matches Apple) — the fetch itself returns a clear "pair
    // first" message if the host hasn't authorized this device for its management API.
    val hasLibrary: Boolean get() = knownHost != null && pinnedProfileId == null
}

/**
 * The console home. [tiles] is rebuilt by the caller from the live host stores; [onActivate] runs a
 * tile's action, [onOpenLibrary]/[onOpenSettings] are the Y/X actions. Fully driven by D-pad / stick
 * / face buttons (MainActivity already maps a pad's A→center, B→back, sticks→D-pad) and by touch.
 */
@Composable
fun GamepadHome(
    tiles: List<HomeTile>,
    libraryEnabled: Boolean,
    controllerName: String?,
    // False while a sheet/dialog is on top → the carousel stops consuming the pad so the overlay
    // can be driven instead.
    navActive: Boolean,
    onActivate: (HomeTile) -> Unit,
    onOpenLibrary: (HomeTile) -> Unit,
    onOpenSettings: () -> Unit,
    // Up on a saved host opens its options (Wake / Edit / Forget). Only saved tiles carry a knownHost.
    onOptions: (HomeTile) -> Unit = {},
) {
    // Equal inset for the pinned title + hint bar, measured from the safe-area edges (so the legend
    // sits the same distance from the left and the bottom).
    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE

    val pagerState = rememberPagerState(pageCount = { tiles.size })
    val scope = rememberCoroutineScope()
    // navTarget is the navigation authority — a controller move steps THIS, and the pager is pointed
    // at it, so a fast repeat coalesces to the latest target instead of reading a lagging currentPage
    // mid-animation (which is what let a flick overshoot by two).
    var navTarget by remember { mutableStateOf(0) }
    LaunchedEffect(pagerState.settledPage) { navTarget = pagerState.settledPage }
    val current = tiles.getOrNull(navTarget)

    // Bumped on every confirm — the centred card dips under the press and springs back, so A reads
    // as a button being pushed rather than as a screen simply changing.
    var pressToken by remember { mutableIntStateOf(0) }
    val press = remember { Animatable(1f) }
    LaunchedEffect(pressToken) {
        if (pressToken == 0) return@LaunchedEffect
        press.animateTo(0.97f, ConsoleMotion.ease(70))
        press.animateTo(1f, spring(dampingRatio = 0.45f, stiffness = Spring.StiffnessMedium))
    }

    GamepadNavEffect(
        active = navActive && tiles.isNotEmpty(),
        onMove = { dir ->
            val target = (navTarget + dir).coerceIn(0, tiles.lastIndex)
            if (target != navTarget) {
                navTarget = target
                scope.launch { pagerState.animateScrollToPage(target) }
            }
        },
        // A / D-pad-center → Connect
        onActivate = { pressToken++; tiles.getOrNull(navTarget)?.let(onActivate) },
        onSecondary = { // Y (gamepad) → Library
            tiles.getOrNull(navTarget)?.takeIf { libraryEnabled && it.hasLibrary }?.let(onOpenLibrary)
        },
        onTertiary = onOpenSettings, // X (gamepad) → Settings
        // A TV remote has no A/B/X/Y: Up → Settings, Down → a saved host's Options (Wake / Library /
        // Edit / Forget). A gamepad instead opens Options on its Select/View button.
        onUp = onOpenSettings,
        onDown = { tiles.getOrNull(navTarget)?.takeIf { it.knownHost != null }?.let(onOptions) },
        onOptions = { tiles.getOrNull(navTarget)?.takeIf { it.knownHost != null }?.let(onOptions) },
    )

    // The legend follows the LAST-USED input: a real gamepad shows its A/X/Y face buttons + the
    // Select/View button for Options; a TV D-pad remote (no face buttons) shows a select ring + Up
    // (Settings) / Down (Options) arrows, with Library folded into Options. Input is universal either
    // way. Each hint is also TAPPABLE (touch hatch).
    val padIsGamepad = (LocalContext.current as? MainActivity)?.lastPadIsGamepad ?: false
    val connectLabel = if (current?.isAdd == true) "Add Host" else "Connect"
    val connectAction: () -> Unit = { pressToken++; tiles.getOrNull(navTarget)?.let(onActivate) }
    val optionsAction: () -> Unit = { current?.let(onOptions) }
    val arrowTint = PadGlyph.Arrow
    val hints = buildList {
        if (padIsGamepad) {
            add(PadGlyph.hint('A', connectLabel, onClick = connectAction))
            if (libraryEnabled && current?.hasLibrary == true) add(PadGlyph.hint('Y', "Library") {
                tiles.getOrNull(navTarget)?.takeIf { it.hasLibrary }?.let(onOpenLibrary)
            })
            add(PadGlyph.hint('X', "Settings", onClick = onOpenSettings))
            // The pad's Select/View button (drawn as its capsule glyph) opens host options.
            if (current?.knownHost != null) add(GamepadHint(' ', arrowTint, "Options", onClick = optionsAction, viewButton = true))
        } else {
            add(GamepadHint(' ', PadGlyph.A, connectLabel, onClick = connectAction, select = true))
            add(GamepadHint('↑', arrowTint, "Settings", onClick = { onOpenSettings() }))
            if (current?.knownHost != null) add(GamepadHint('↓', arrowTint, "Options", onClick = optionsAction))
        }
    }

    val hazeState = remember { HazeState() }

    Box(Modifier.fillMaxSize()) {
        // The whole backdrop (aurora + carousel) is the haze source, so the floating legend can blur
        // whatever scrolls under it.
        BoxWithConstraints(Modifier.fillMaxSize().hazeSource(hazeState)) {
            GamepadAuroraBackground(Modifier.fillMaxSize())

            // Carousel centred on the FULL screen — the title + legend FLOAT over it (below), so they
            // no longer push the cards below the true centre.
            val cardWidth = (maxWidth * 0.82f).coerceAtMost(360.dp)
            val cardHeight = (maxHeight * 0.56f).coerceAtMost(216.dp)
            val sidePad = ((maxWidth - cardWidth) / 2).coerceAtLeast(0.dp)
            Box(Modifier.fillMaxSize().consoleSafeArea()) {
                HorizontalPager(
                    state = pagerState,
                    pageSize = PageSize.Fixed(cardWidth),
                    contentPadding = PaddingValues(horizontal = sidePad),
                    pageSpacing = 22.dp,
                    modifier = Modifier.fillMaxSize(),
                    verticalAlignment = Alignment.CenterVertically,
                ) { page ->
                    val tile = tiles[page]
                    // Real distance-from-centered (page + fractional drag), so the pop tracks the
                    // live scroll: centered tile at full scale/brightness, neighbours recede + blur.
                    // Signed, because which SIDE a card fans to decides which edge it turns on.
                    val signed = (page - pagerState.currentPage) - pagerState.currentPageOffsetFraction
                    val offset = signed.absoluteValue.coerceIn(0f, 1f)
                    GamepadHostTile(
                        tile = tile,
                        centred = offset < 0.5f,
                        modifier = Modifier
                            .graphicsLayer {
                                // The press dip applies to the CENTRED card only — it is the one
                                // the button acted on, and a whole carousel flinching would read
                                // as the screen moving rather than a card being pressed.
                                val s = lerp(1f, 0.86f, offset) * lerp(press.value, 1f, offset)
                                scaleX = s
                                scaleY = s
                                alpha = lerp(1f, 0.5f, offset)
                            }
                            .graphicsLayer {
                                // The neighbours TURN away, projected rather than rendered in 3D.
                                // `cos(angle)` as a horizontal squeeze IS the orthographic
                                // projection of a Y-axis rotation, and hinging it on the edge the
                                // card fans from is what carries the direction the rotation's sign
                                // would have. The Apple client arrived here the hard way (see
                                // GamepadCarousel.swift): a real `rotation3DEffect` renders the
                                // card through an offscreen pass and flashed as the strip settled.
                                // Affine transforms don't.
                                scaleX = cos(CARD_TURN_RAD * offset)
                                transformOrigin =
                                    TransformOrigin(if (signed > 0f) 0f else 1f, 0.5f)
                            }
                            // Unbounded so the depth blur isn't hard-clipped at the card's rectangle
                            // (the cut-off edge). No-op below API 31; a soft blur above.
                            .blur(radius = (offset * 12f).dp, edgeTreatment = BlurredEdgeTreatment.Unbounded)
                            .height(cardHeight)
                            .clickable(
                                interactionSource = remember { MutableInteractionSource() },
                                indication = null,
                            ) {
                                if (page == navTarget) {
                                    pressToken++
                                    onActivate(tile)
                                } else {
                                    navTarget = page
                                    scope.launch { pagerState.animateScrollToPage(page) }
                                }
                            },
                    )
                }
            }
        }

        // Title floats over the top (out of the carousel's layout, so the cards stay centred). Uses
        // the shared ConsoleHeader so it lines up with every other screen's heading.
        Row(
            Modifier.align(Alignment.TopStart).fillMaxWidth().consoleSafeArea()
                .padding(end = ConsoleEdgeInset),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            // The TITLE has priority (unweighted, so it is measured at its full width first) and the
            // chip takes what is left, ellipsizing its device name. The other way round — which is
            // what a weighted header gave — a talkative controller name ("Xbox Wireless Controller")
            // ate a 360 dp portrait phone's title down to "Selec…".
            ConsoleHeader("Select a Host")
            if (controllerName != null) {
                ControllerStatusChip(controllerName, Modifier.weight(1f, fill = false))
            }
        }

        // Legend floats bottom-start with a real backdrop blur of the content behind it. In LANDSCAPE
        // it ignores the system bars (the nav-bar inset made the bottom gap look oversized) but never
        // the cutout — reverse-landscape parks the punch on this very corner.
        Box(
            Modifier
                .align(Alignment.BottomStart)
                .consoleLegendInsets(landscape)
                .padding(ConsoleLegendInset),
        ) {
            GamepadHintBar(hints, hazeState = hazeState)
        }
    }
}

/**
 * One glass landscape console tile — bigger and bolder than the touch grid's HostCard, and cut from
 * the same [Modifier.consoleGlass] every console surface is, so a card and a settings row catch the
 * light the same way. [centred] is the carousel's own focus: the tile the pad is pointing at, which
 * earns the lift and the accent bloom.
 */
@Composable
private fun GamepadHostTile(tile: HomeTile, centred: Boolean, modifier: Modifier = Modifier) {
    val ink = LocalGamepadInk.current
    val visuals = animateConsoleFocus(active = centred)
    // A SAVED host wears the palette's accent; a discovered one (or the Add tile) stays neutral
    // glass, so "already yours" reads before you get to the label.
    val fill = if (tile.filled) ink.accent(0.20f) else ink.glass
    Column(
        modifier = modifier
            .fillMaxWidth()
            // The carousel already drives its own scale; the glass must not fight it with a second.
            .consoleGlass(
                ConsoleShape.Tile,
                ConsoleFocusVisuals(1f, fill, ink.fg(0.16f), visuals.focus),
                // A DASHED edge on anything not yet saved — a host found on the network, and the
                // Add tile. It is the touch grid's own convention and the Apple client's, and it
                // says "not yours yet" before the subtitle has to.
                dashed = !tile.filled,
            )
            .padding(22.dp),
    ) {
        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.Top) {
            MonogramBadge(tile)
            Spacer(Modifier.weight(1f))
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (tile.paired) {
                    Icon(
                        Icons.Filled.Lock,
                        contentDescription = "Paired",
                        tint = ink.fg(0.7f),
                        modifier = Modifier.padding(end = 6.dp).size(15.dp),
                    )
                }
                if (tile.online) {
                    Box(
                        Modifier.size(10.dp).clip(androidx.compose.foundation.shape.CircleShape)
                            .background(Color(0xFF3CD070)),
                    )
                }
            }
        }
        Spacer(Modifier.weight(1f))
        Text(
            tile.title,
            style = MaterialTheme.typography.titleLarge,
            fontWeight = FontWeight.Bold,
            color = ink.fg,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        if (tile.profileName != null) {
            ConsoleProfileChip(
                name = tile.profileName,
                accent = tile.profileAccent,
                // On a PIN card the profile is why the card exists; on a bound host's own card it
                // is a note about what a press will do. Same chip, two weights.
                prominent = tile.pinnedProfileId != null,
                modifier = Modifier.padding(top = 5.dp),
            )
        }
        Text(
            tile.subtitle,
            style = MaterialTheme.typography.bodyMedium,
            color = ink.fg(0.55f),
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(top = 2.dp),
        )
    }
}

/**
 * The profile a card connects with, worn as a tinted capsule. The console counterpart of the touch
 * grid's own chip (`HostComponents.kt`) — same shape and the same quiet/prominent split, but inked
 * from the console palette rather than `MaterialTheme`, since it sits on the aurora.
 *
 * A profile that set no accent falls back to the palette's, not to the touch theme's primary: on a
 * moss or copper field the brand violet would be the one foreign colour on the card.
 */
@Composable
private fun ConsoleProfileChip(
    name: String,
    accent: Color?,
    prominent: Boolean,
    modifier: Modifier = Modifier,
) {
    val ink = LocalGamepadInk.current
    val tint = accent ?: ink.accent
    Row(
        modifier = modifier
            .clip(ConsoleShape.Pill)
            .background(tint.copy(alpha = if (prominent) 0.24f else 0.12f))
            .padding(horizontal = 9.dp, vertical = 3.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(7.dp).clip(CircleShape).background(tint))
        Spacer(Modifier.width(6.dp))
        Text(
            name,
            style = if (prominent) {
                MaterialTheme.typography.labelLarge
            } else {
                MaterialTheme.typography.labelMedium
            },
            fontWeight = if (prominent) FontWeight.Bold else FontWeight.SemiBold,
            color = tint,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

@Composable
private fun MonogramBadge(tile: HomeTile) {
    val ink = LocalGamepadInk.current
    val shape = RoundedCornerShape(15.dp)
    // Lit from the top like every other console surface — and the unsaved badge takes the palette's
    // own accent at low opacity rather than the brand violet, which on a copper or moss field was
    // the one square of the wrong hue on the screen.
    val fill = if (tile.filled) {
        Brush.verticalGradient(listOf(ink.accent.copy(alpha = 0.92f), ink.accent))
    } else {
        Brush.verticalGradient(listOf(ink.accent(0.20f), ink.accent(0.14f)))
    }
    Box(
        modifier = Modifier.size(52.dp).clip(shape).background(fill),
        contentAlignment = Alignment.Center,
    ) {
        when {
            tile.connecting -> CircularProgressIndicator(
                modifier = Modifier.size(24.dp),
                strokeWidth = 2.dp,
                color = ink.fg,
            )
            tile.isAdd -> Icon(
                Icons.Filled.Add,
                contentDescription = null,
                tint = if (tile.filled) ink.fg else ink.accent,
            )
            else -> Text(
                tile.title.trim().firstOrNull()?.uppercaseChar()?.toString() ?: "•",
                style = MaterialTheme.typography.titleLarge,
                fontWeight = FontWeight.Bold,
                color = if (tile.filled) ink.fg else ink.accent,
            )
        }
    }
}
