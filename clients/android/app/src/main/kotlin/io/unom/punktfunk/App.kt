package io.unom.punktfunk

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.gestures.animateScrollBy
import androidx.compose.foundation.gestures.scrollBy
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.compositionLocalOf
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Density
import androidx.compose.ui.unit.dp
import android.widget.Toast
import io.unom.punktfunk.kit.link.DeepLinkResult
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.link.HostResolution
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.kit.security.KnownHostStore
import io.unom.punktfunk.models.ActiveSession
import io.unom.punktfunk.models.Tab
import kotlin.math.roundToInt
import kotlinx.coroutines.launch

@Composable
fun App(forceGamepadUi: Boolean = false) {
    val context = LocalContext.current
    val activity = context as? MainActivity
    val settingsStore = remember { SettingsStore(context) }
    var settings by remember { mutableStateOf(settingsStore.load()) }
    // The active session (null = not streaming). It carries the settings the connect resolved,
    // so the stream screen never re-reads the store behind its own connect's back.
    var session by remember { mutableStateOf<ActiveSession?>(null) }
    var tab by remember { mutableStateOf(Tab.Connect) }
    // Set when a session ends because its game exited and it began as a library launch: the host
    // whose library the console shell should come back to. Held HERE because the shell's own
    // navigation state does not outlive the stream. Cleared once the shell has consumed it, so a
    // later manual Back out of the library is not undone by a stale value.
    var reopenLibraryHostId by remember { mutableStateOf<String?>(null) }

    // Console (gamepad) mode mirrors the Apple client: the setting AND (its mode says Always OR a
    // pad is attached OR this is a TV OR the dev force flag). Flips live as controllers
    // connect/disconnect — unless the mode is Always, where it simply stays.
    val tv = remember { isTvDevice(context) }
    val controllerConnected by rememberControllerConnected()
    val gamepadUi = gamepadUiActive(
        settings.gamepadUiEnabled, settings.gamepadUiMode, controllerConnected, tv, forceGamepadUi,
    )

    // Publish the live session process-wide, so a `punktfunk://` link that arrives as a SECOND
    // activity instance (the normal case under `launchMode = standard`) can refuse it before that
    // instance is ever resumed — see MainActivity.onCreate. Cleared on dispose, so an activity
    // destroyed mid-stream doesn't leave a ghost that blocks every future link.
    DisposableEffect(session) {
        MainActivity.liveStream = session?.let { MainActivity.LiveStream(it.hostId) }
        onDispose { MainActivity.liveStream = null }
    }

    // The same rule for the rare in-instance case (a caller that set FLAG_ACTIVITY_SINGLE_TOP, so
    // the link reached `onNewIntent` on the streaming activity itself). Pointing at the host
    // already being streamed is the one exception, and its right answer is to do nothing — the
    // intent has already brought the app forward, which is exactly what "focus it" means here.
    val pendingLink = activity?.pendingDeepLink
    LaunchedEffect(pendingLink, session) {
        val url = pendingLink ?: return@LaunchedEffect
        val live = session ?: return@LaunchedEffect // not streaming: ConnectScreen routes it
        activity.pendingDeepLink = null
        val parsed = DeepLinks.parse(url) as? DeepLinkResult.Parsed ?: return@LaunchedEffect
        val target = DeepLinks.resolveHost(parsed.link, KnownHostStore(context).all())
        val sameHost = target is HostResolution.Known && target.host.id == live.hostId
        if (!sameHost) {
            Toast.makeText(
                context,
                "Already streaming — end this session first.",
                Toast.LENGTH_LONG,
            ).show()
        }
    }

    // The console backdrop's colour family, published once from the live settings rather than
    // threaded through every screen that draws a backdrop. Because it is read from the SAME
    // `settings` state the gamepad settings screen writes, stepping the Background row recolours
    // the field behind that very row.
    val palette = GamepadPalette.named(settings.uiPalette)
    CompositionLocalProvider(
        LocalGamepadPalette provides palette,
        LocalGamepadInk provides GamepadInk.of(palette),
    ) {
    AnimatedContent(
        targetState = session,
        transitionSpec = {
            fadeIn() togetherWith fadeOut()
        },
        label = "StreamTransition"
    ) { active ->
        if (active != null) {
            // Immersive: the stream takes the whole screen, no bottom bar.
            StreamScreen(active) { reason ->
                // A game launched from a library exiting is a normal finish, and the player is
                // almost certainly after the next title — so send them back to that library rather
                // than all the way out to host selection. The console shell's own screen state does
                // not survive the stream (StreamScreen replaces it in the composition, discarding
                // its `remember`s), so the intent is hoisted here and handed back on the way in.
                reopenLibraryHostId =
                    if (reason == SessionEndReason.GAME_EXITED && active.launchedFromLibrary) {
                        active.hostId
                    } else {
                        null
                    }
                session = null
            }
        } else if (gamepadUi) {
            GamepadShell(
                settings = settings,
                onSettingsChange = { settings = it; settingsStore.save(it) },
                onConnected = { session = it },
                deepLink = pendingLink,
                onDeepLinkHandled = { activity?.pendingDeepLink = null },
                reopenLibraryHostId = reopenLibraryHostId,
                onReopenLibraryHandled = { reopenLibraryHostId = null },
            )
        } else {
            // Adaptive nav: a bottom bar on phones; on tablets / large windows a side NavigationRail
            // with its items centred vertically (the common Android tablet idiom, mirroring iPad's
            // side navigation). A short landscape phone keeps the bottom bar (rail needs height too).
            // Tabs slide along the axis the nav sits on: horizontally with the bottom bar (phone),
            // vertically with the side rail (tablet), so the motion tracks the direction you moved.
            val tabContent: @Composable (vertical: Boolean) -> Unit = { vertical ->
                AnimatedContent(
                    targetState = tab,
                    transitionSpec = {
                        val forward = targetState.ordinal > initialState.ordinal
                        when {
                            vertical && forward ->
                                slideInVertically { it } + fadeIn() togetherWith
                                        slideOutVertically { -it } + fadeOut()
                            vertical ->
                                slideInVertically { -it } + fadeIn() togetherWith
                                        slideOutVertically { it } + fadeOut()
                            forward ->
                                slideInHorizontally { it } + fadeIn() togetherWith
                                        slideOutHorizontally { -it } + fadeOut()
                            else ->
                                slideInHorizontally { -it } + fadeIn() togetherWith
                                        slideOutHorizontally { it } + fadeOut()
                        }
                    },
                    label = "TabTransition"
                ) { targetTab ->
                    when (targetTab) {
                        Tab.Connect -> ConnectScreen(
                            settings = settings,
                            onConnected = { session = it },
                            onSettingsChange = { settings = it; settingsStore.save(it) },
                            deepLink = pendingLink,
                            onDeepLinkHandled = { activity?.pendingDeepLink = null },
                        )
                        Tab.Settings -> SettingsScreen(
                            initial = settings,
                            onChange = { settings = it; settingsStore.save(it) },
                            onBack = { tab = Tab.Connect },
                        )
                    }
                }
            }

            BoxWithConstraints(Modifier.fillMaxSize()) {
                if (maxWidth >= 600.dp && maxHeight >= 480.dp) {
                    Row(Modifier.fillMaxSize()) {
                        NavigationRail(Modifier.fillMaxHeight()) {
                            Spacer(Modifier.weight(1f)) // centre the rail items vertically
                            Tab.entries.forEach { t ->
                                NavigationRailItem(
                                    selected = tab == t,
                                    onClick = { tab = t },
                                    icon = { Icon(t.icon, contentDescription = t.label) },
                                    label = { Text(t.label) },
                                )
                            }
                            Spacer(Modifier.weight(1f))
                        }
                        // The rail handles its own insets; the content pane insets itself (the screens
                        // don't, since they used to rely on the Scaffold's padding). Cutout included:
                        // a tablet in landscape puts its punch on exactly this pane's leading edge.
                        Box(Modifier.weight(1f).fillMaxHeight().consoleSafeArea()) { tabContent(true) }
                    }
                } else {
                    Scaffold(
                        bottomBar = {
                            NavigationBar {
                                Tab.entries.forEach { t ->
                                    NavigationBarItem(
                                        selected = tab == t,
                                        onClick = { tab = t },
                                        icon = { Icon(t.icon, contentDescription = t.label) },
                                        label = { Text(t.label) },
                                    )
                                }
                            }
                        },
                    ) { innerPadding ->
                        Box(Modifier.fillMaxSize().padding(innerPadding)) { tabContent(false) }
                    }
                }
            }
        }
    }
    }
}

/**
 * The console backdrop's colour family for everything under [App] — provided from the live
 * settings so a change on the gamepad settings screen recolours every backdrop at once. Defaults
 * to the brand violet, which is also what a preview or a test composition gets.
 */
val LocalGamepadPalette = compositionLocalOf { GamepadPalette.named("violet") }

/**
 * Which console screen the gamepad shell is showing, and how deep it sits — Home is the root, and
 * everything reachable from it is one level in. The DEPTH is what decides whether a change is a
 * push or a pop, and therefore which way the screens travel.
 */
private enum class GamepadScreen(val depth: Int) {
    Home(0),
    Settings(1),
    Library(1),
    // Reached FROM Settings, not from Home, so they sit a level deeper again — which is precisely
    // what makes Settings → Controllers travel like a push and the way back like a pop. Give one of
    // these depth 1 and the transition would read as a sideways swap between two peers.
    Controllers(2),
    Licenses(2),
}

/**
 * The console (gamepad) shell — the Android mirror of the Apple client's ContentView gamepad branch:
 * a full-screen host carousel with X → Settings and Y → a saved host's library, all sharing
 * [ConnectScreen]'s connect logic. No bottom bar; navigation is button-driven.
 */
@Composable
fun GamepadShell(
    settings: Settings,
    onSettingsChange: (Settings) -> Unit,
    onConnected: (ActiveSession) -> Unit,
    deepLink: String? = null,
    onDeepLinkHandled: () -> Unit = {},
    /**
     * Open this saved host's library instead of Home on the way in — set when a game launched from
     * it has just exited. Null (the default) starts on Home exactly as before.
     */
    reopenLibraryHostId: String? = null,
    onReopenLibraryHandled: () -> Unit = {},
) {
    val context = LocalContext.current
    var screen by remember { mutableStateOf(GamepadScreen.Home) }
    var libraryHost by remember { mutableStateOf<io.unom.punktfunk.kit.security.KnownHost?>(null) }
    // Where the settings screen was when a sub-screen took over. The shell's AnimatedContent
    // discards a screen's `remember`s the moment it stops being the target, so a trip out to the
    // Controllers view and back would otherwise land on the Stream tab's first row — the couch
    // equivalent of a browser losing your scroll position on Back. Held here because this is the
    // only thing that outlives the screen.
    var settingsPlace by remember { mutableStateOf<GpSettingsPlace?>(null) }

    // Consume the "come back to this library" intent once, on entry. Keyed on the id so a second
    // game exit re-fires it; the parent clears it immediately, so a manual Back stays backed out.
    // A host that has since been forgotten simply leaves us on Home rather than failing.
    LaunchedEffect(reopenLibraryHostId) {
        val id = reopenLibraryHostId ?: return@LaunchedEffect
        // Navigate BEFORE acknowledging: acknowledging clears the parent's state, which re-keys
        // this effect and cancels the coroutine running it. Nothing suspends in between today, so
        // either order happens to work — but this one cannot be broken by a later edit that adds a
        // suspending call. A host that has since been forgotten just leaves us on Home.
        KnownHostStore(context).all()
            .firstOrNull { it.id == id }
            ?.let { libraryHost = it; screen = GamepadScreen.Library }
        onReopenLibraryHandled()
    }

    // On a TV, shrink the 10-foot UI so its elements aren't oversized. Density-aware: expand the
    // effective dp footprint to at least CONSOLE_TV_MIN_WIDTH_DP (→ smaller elements) ONLY when the
    // panel reports fewer dp than that; a low-density TV that's already spacious, and every phone /
    // tablet, keep their real density unchanged. This is the "based on pixel density" scale the layout
    // wanted — one uniform factor across text, cards, spacing, and insets.
    val isTv = remember { isTvDevice(context) }
    val baseDensity = LocalDensity.current
    val screenWidthPx = LocalConfiguration.current.screenWidthDp * baseDensity.density
    val fitDensity = screenWidthPx / CONSOLE_TV_MIN_WIDTH_DP
    val consoleDensity = if (isTv && fitDensity < baseDensity.density) fitDensity else baseDensity.density

    // The console's screen transition, and the desktop console's contract rather than a plain
    // cross-fade (see ConsoleMotion for the numbers and where they come from): a PUSH slides the
    // incoming screen up out of a fade while the outgoing one recedes; a POP runs it backwards, the
    // leaving screen sliding down and the revealed one growing back. Direction comes from the
    // screens' nav DEPTH, so Settings → Home pops even though nothing tracks a stack.
    //
    // Each slot's controller nav is gated on being the CURRENT target (`s == screen`), so mid-
    // transition only the incoming screen drives the pad. All screens pin their legend at the same
    // ConsoleLegendInset, so it reads as fixed while the content behind it moves.
    val animated = animationsEnabled()
    CompositionLocalProvider(LocalDensity provides Density(consoleDensity, baseDensity.fontScale)) {
    // Measured INSIDE the console's own density, not the device's: on a TV the console UI runs at a
    // reduced density to shrink the 10-foot layout, and a slide sized in device pixels would travel
    // further than every other dp in the same animation.
    val slidePx = with(LocalDensity.current) { ConsoleMotion.PUSH_SLIDE.toPx() }.roundToInt()
    AnimatedContent(
        targetState = screen,
        transitionSpec = {
            if (!animated) {
                // Reduce-motion: no travel, no scale — just a fast cross-fade, the same courtesy
                // the frozen backdrop pays.
                fadeIn(tween(ConsoleMotion.REDUCED_MS)) togetherWith
                    fadeOut(tween(ConsoleMotion.REDUCED_MS))
            } else if (targetState.depth > initialState.depth) {
                (
                    fadeIn(ConsoleMotion.ease()) +
                        slideInVertically(ConsoleMotion.ease()) { slidePx } +
                        scaleIn(ConsoleMotion.ease(), initialScale = ConsoleMotion.ENTER_SCALE)
                    ) togetherWith (
                    fadeOut(ConsoleMotion.ease()) +
                        scaleOut(ConsoleMotion.ease(), targetScale = ConsoleMotion.EXIT_SCALE)
                    )
            } else {
                (
                    fadeIn(ConsoleMotion.ease(), initialAlpha = ConsoleMotion.REVEAL_ALPHA) +
                        scaleIn(ConsoleMotion.ease(), initialScale = ConsoleMotion.EXIT_SCALE)
                    ) togetherWith (
                    fadeOut(ConsoleMotion.ease()) +
                        slideOutVertically(ConsoleMotion.ease()) { slidePx }
                    )
            }
        },
        label = "consoleScreen",
    ) { s ->
        when (s) {
            GamepadScreen.Home -> ConnectScreen(
                settings = settings,
                onConnected = onConnected,
                onSettingsChange = onSettingsChange,
                deepLink = deepLink,
                onDeepLinkHandled = onDeepLinkHandled,
                gamepadUi = true,
                onOpenSettings = { screen = GamepadScreen.Settings },
                onOpenLibrary = { host -> libraryHost = host; screen = GamepadScreen.Library },
                navGate = s == screen,
            )
            GamepadScreen.Settings -> GamepadSettingsScreen(
                initial = settings,
                onChange = onSettingsChange,
                // Leaving for HOME forgets the place: coming back in from the carousel should start
                // at the top of the first section, exactly as it always has. Only a sub-screen's
                // Back is a return.
                onBack = { screen = GamepadScreen.Home; settingsPlace = null },
                navActive = s == screen,
                resume = settingsPlace,
                onPlace = { settingsPlace = it },
                onOpenControllers = { screen = GamepadScreen.Controllers },
                onOpenLicenses = { screen = GamepadScreen.Licenses },
            )
            GamepadScreen.Controllers -> ConsoleControllersScreen(
                gamepadSetting = settings.gamepad,
                onBack = { screen = GamepadScreen.Settings },
                navActive = s == screen,
            )
            GamepadScreen.Licenses -> ConsoleLicensesScreen(
                onBack = { screen = GamepadScreen.Settings },
                navActive = s == screen,
            )
            GamepadScreen.Library -> libraryHost?.let { host ->
                LibraryScreen(
                    host = host,
                    settings = settings,
                    onLaunched = onConnected,
                    onBack = { screen = GamepadScreen.Home; libraryHost = null },
                    navActive = s == screen,
                )
            } ?: run { screen = GamepadScreen.Home }
        }
    }
    }
}

/** Minimum effective dp width the console UI targets on a TV (bigger → the 10-foot UI shrinks). */
private const val CONSOLE_TV_MIN_WIDTH_DP = 1180f

// --- Showing a TOUCH-written screen on the console's field -------------------------------------
//
// Two screens (Controllers, Licenses) exist once and are shown in both interfaces. They live beside
// the shell rather than in `GamepadChrome.kt` because they are about the SHELL's job — putting a
// screen that was written for one interface onto the other's field — rather than about the console's
// own material.

/**
 * Re-inks a screen written against the TOUCH theme so it can be shown on the console's field.
 *
 * `ControllersScreen` alone pulls `MaterialTheme.colorScheme` at 27 explicit sites, plus implicitly
 * through every `OutlinedCard`, `Switch`, `OutlinedButton` and `LinearProgressIndicator` it draws.
 * Dropped into the shell those keep the touch palette — light-grey body text with no background of
 * its own, which over the six PALE console palettes (`GamepadPalette`, `light = true`) is grey on
 * pastel: technically painted, in practice unreadable. That is the same class of bug as the console
 * dialogs that spent a release rendering dark ink on a dark card.
 *
 * The fix is deliberately ONE derived colour scheme rather than 27 call-site branches:
 *  * a call-site branch cannot reach the IMPLICIT pulls at all — a `Switch`'s track and an
 *    `OutlinedCard`'s border are resolved inside Material, not here;
 *  * two colours per site is exactly the shape that drifts, and it would leave the touch screen
 *    carrying console vocabulary it has no use for.
 *
 * The alternative — give the console presentation an opaque backdrop and let the touch theme read on
 * its own ground — was rejected because it splits the screen's material in two: an opaque touch-grey
 * slab under a palette-inked header and legend, with a visible seam between them, on a field whose
 * whole point is that one look runs through it.
 *
 * The base scheme follows the field's lightness, so anything not overridden here (a container role
 * some Material component reaches for) still lands on the right side of the contrast line.
 */
@Composable
internal fun ConsoleInkedTheme(content: @Composable () -> Unit) {
    val ink = LocalGamepadInk.current
    val scheme = remember(ink) {
        val base = if (ink.isLight) lightColorScheme() else darkColorScheme()
        base.copy(
            primary = ink.accent,
            onPrimary = ink.onAccent,
            // A card becomes a PANE over the aurora rather than a slab on top of it: the console's
            // own glass fill, so an OutlinedCard here is cut from the material the settings rows are.
            surface = ink.glass,
            onSurface = ink.fg,
            surfaceVariant = ink.fg(0.12f),
            onSurfaceVariant = ink.fg(0.68f),
            outline = ink.fg(0.30f),
            outlineVariant = ink.fg(0.16f),
            // Nothing here paints a background — the aurora is the ground — but a component that
            // resolves `background` (or the content colour for it) must still land on the palette.
            background = Color.Transparent,
            onBackground = ink.fg,
        )
    }
    // The typography and shapes are the app's, not Material's defaults: this swaps the INK, not the
    // brand typeface. And `LocalContentColor` has to be provided by hand — outside a Surface or a
    // Scaffold it defaults to BLACK, which is how an unstyled `Text` would vanish into a dark field.
    MaterialTheme(
        colorScheme = scheme,
        typography = MaterialTheme.typography,
        shapes = MaterialTheme.shapes,
    ) {
        CompositionLocalProvider(LocalContentColor provides ink.fg, content = content)
    }
}

/**
 * The console's scroll route for a screen that is a WALL of content rather than a list of focusable
 * rows.
 *
 * Compose only scrolls a container to keep a FOCUSED child visible, so a screen whose body holds no
 * focusable nodes (the licenses notices are one enormous `Text`) simply cannot be scrolled by a
 * controller: the D-pad has nothing to move to. These screens therefore drive the scroll state
 * directly — up/down steps, the shoulders page.
 *
 * Returned as a plain function so a screen's nav callbacks read `scroll(-1, page = false)` rather
 * than each screen minting its own coroutine + viewport arithmetic (which is how the two would end
 * up scrolling at different speeds).
 */
@Composable
internal fun rememberConsoleScroller(scroll: ScrollState): (dir: Int, page: Boolean) -> Unit {
    val scope = rememberCoroutineScope()
    val animated = animationsEnabled()
    return remember(scroll, animated) {
        { dir, page ->
            val delta = consoleScrollDelta(scroll.viewportSize.toFloat(), page, dir)
            if (delta != 0f) {
                scope.launch {
                    // Auto-repeat fires every 150 ms while a direction is held, so each animation is
                    // short enough to have landed (or nearly) before the next one cancels it —
                    // otherwise a held D-pad crawls, each step restarting from where the last was
                    // interrupted.
                    if (animated) {
                        scroll.animateScrollBy(
                            delta,
                            ConsoleMotion.ease(
                                if (page) ConsoleMotion.TRANSITION_MS else ConsoleMotion.FOCUS_MS,
                            ),
                        )
                    } else {
                        scroll.scrollBy(delta)
                    }
                }
            }
        }
    }
}

/**
 * How far one console scroll press travels: [dir] is -1 (up/left) or +1 (down/right), [page] picks
 * the shoulders' full page over a D-pad step. Zero while the viewport is unmeasured — a first press
 * that arrived before layout must do nothing rather than fling the content by zero-times-nothing.
 */
internal fun consoleScrollDelta(viewportPx: Float, page: Boolean, dir: Int): Float =
    if (viewportPx <= 0f) 0f else viewportPx * (if (page) CONSOLE_PAGE else CONSOLE_STEP) * dir

/**
 * A page keeps a band of what you were reading on screen rather than jumping a clean screenful — the
 * overlap every reader has used since the printed page, and the difference between "I moved down"
 * and "where was I".
 */
private const val CONSOLE_PAGE = 0.88f

/** A D-pad step is about a quarter screen, so holding the direction walks the wall rather than flicking it. */
private const val CONSOLE_STEP = 0.28f
