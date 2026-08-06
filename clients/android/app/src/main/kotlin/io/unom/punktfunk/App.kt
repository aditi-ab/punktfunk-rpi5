package io.unom.punktfunk

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.Crossfade
import androidx.compose.animation.ExperimentalAnimationApi
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
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
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

    // Console (gamepad) mode mirrors the Apple client: the setting AND (a pad is attached OR this is
    // a TV OR the dev force flag). Flips live as controllers connect/disconnect.
    val tv = remember { isTvDevice(context) }
    val controllerConnected by rememberControllerConnected()
    val gamepadUi = gamepadUiActive(settings.gamepadUiEnabled, controllerConnected, tv, forceGamepadUi)

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
                        // don't, since they used to rely on the Scaffold's padding).
                        Box(Modifier.weight(1f).fillMaxHeight().systemBarsPadding()) { tabContent(true) }
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

/** Which console screen the gamepad shell is showing. */
private enum class GamepadScreen { Home, Settings, Library }

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

    CompositionLocalProvider(LocalDensity provides Density(consoleDensity, baseDensity.fontScale)) {
    // Cross-fade between console screens so switches are smooth. Each slot's controller nav is gated
    // on being the CURRENT target (`s == screen`), so during the fade only the incoming screen drives
    // the pad. All screens pin their legend at the same ConsoleLegendInset, so it reads as fixed while
    // the content behind it fades.
    Crossfade(targetState = screen, animationSpec = tween(240), label = "consoleScreen") { s ->
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
                onBack = { screen = GamepadScreen.Home },
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
