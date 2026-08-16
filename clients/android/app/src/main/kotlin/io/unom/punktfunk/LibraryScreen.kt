package io.unom.punktfunk

import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.PageSize
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.ContentScale
import android.content.res.Configuration
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.zIndex
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import kotlinx.coroutines.launch
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import coil.ImageLoader
import coil.compose.AsyncImage
import coil.request.ImageRequest
import io.unom.punktfunk.components.launcherIcon
import io.unom.punktfunk.kit.link.DeepLinks
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.library.mtlsHttpClient
import io.unom.punktfunk.kit.security.ClientIdentity
import io.unom.punktfunk.kit.security.IdentityStore
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.kit.security.obtainIdentity
import io.unom.punktfunk.models.ActiveSession
import kotlin.math.PI
import kotlin.math.absoluteValue
import kotlin.math.cos
import kotlin.math.sign
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

// The host game-library browser — the Android mirror of the Apple client's LibraryView: ONE screen
// with two presentations of the same shelf, chosen the same way every other screen here chooses.
// The console (gamepad) one is a poster coverflow (centered cover flat + prominent, neighbours
// receding on a 3D Y-tilt), reached with Y from a saved host; the touch one is the poster GRID the
// Apple, GTK and Windows shells draw, reached from a host card's "Browse library…". Both fetch from
// the host's management API over mTLS, and everything data-shaped — the fetch, the states, the
// launch — is shared: only the arrangement differs, because only the input device does.

private sealed class LibState {
    object Loading : LibState()
    // Carries the client identity so a launch can dial the host over the same pinned mTLS trust.
    data class Ready(val games: List<GameEntry>, val loader: ImageLoader, val identity: ClientIdentity) : LibState()
    data class Message(val text: String) : LibState() // unauthorized / empty / error
}

@Composable
fun LibraryScreen(
    host: KnownHost,
    settings: Settings,
    onLaunched: (ActiveSession) -> Unit,
    onBack: () -> Unit,
    navActive: Boolean = true,
    /**
     * The profile this shelf launches with, when it was opened from a PINNED host+profile card
     * (design §5.2a) rather than the host's own tile: a one-off, exactly like the card's plain
     * connect. Null = the host's tile, and the host's binding decides — the same rule
     * [ProfileStore.resolveFor] applies to every other connect.
     */
    pinnedProfileId: String? = null,
    /**
     * Which presentation to draw: the console coverflow (default — this screen's original and only
     * form) or the touch poster grid. The CALLER decides rather than this screen reading the
     * gamepad setting itself, because the two shells reach it by different routes and each already
     * knows which one it is; a screen that guessed could disagree with the shell that pushed it.
     */
    console: Boolean = true,
) {
    BackHandler(onBack = onBack)
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    // Bumped by the touch header's Reload — re-keys the fetch effect below without the screen
    // having to be popped and re-pushed to try a flaky host again.
    var reloadKey by remember { mutableIntStateOf(0) }
    var state by remember { mutableStateOf<LibState>(LibState.Loading) }
    // A launch (connect) in flight: shows an overlay + gates the pad so a second press can't dial twice.
    var launching by remember { mutableStateOf(false) }
    // The profile every launch off this shelf runs with, resolved ONCE per shelf by the same rule
    // the host-list connect uses: this card's pin as the one-off, else the host's binding, else the
    // globals. Resolved here rather than per launch so a profile edited mid-browse cannot make two
    // titles on one shelf stream differently.
    val profile = remember(host.id, pinnedProfileId) {
        ProfileStore(context).resolveFor(host, pinnedProfileId)
    }
    val streamSettings = remember(settings, profile) { settings.effectiveFor(profile) }

    // Keyed on the mgmt port too: a discovery tick can learn it after this screen is composed, and
    // the fetch must redo itself against the real port rather than stay on a stale 47990 failure.
    LaunchedEffect(host.address, host.port, host.fpHex, host.effectiveMgmtPort, reloadKey) {
        state = LibState.Loading
        state = withContext(Dispatchers.IO) {
            val id = runCatching { obtainIdentity(IdentityStore(context)) }.getOrNull()
                ?: return@withContext LibState.Message("Identity unavailable — re-pair may be required.")
            when (val res = LibraryClient.fetch(
                address = host.address,
                mgmtPort = host.effectiveMgmtPort,
                certPem = id.certPem,
                keyPem = id.privateKeyPem,
                fpHex = host.fpHex,
            )) {
                is LibraryResult.Ok -> if (res.games.isEmpty()) {
                    LibState.Message("No games found on this host.")
                } else {
                    val client = mtlsHttpClient(id.certPem, id.privateKeyPem, host.address, host.fpHex)
                    LibState.Ready(res.games, ImageLoader.Builder(context).okHttpClient(client).build(), id)
                }
                is LibraryResult.Unauthorized -> LibState.Message(res.message)
                is LibraryResult.Error -> LibState.Message(res.message)
            }
        }
    }

    // A pinned card's shelf says so, in the card's own `host · profile` shape: what a launch here
    // will use is a property of the shelf, not something to remember from the tile two screens back.
    val title = if (pinnedProfileId != null && profile != null) {
        "${host.name} · ${profile.name} — Library"
    } else {
        "${host.name} — Library"
    }

    // Dial the host over the same pinned mTLS trust, booting straight into this title (the host
    // resolves `launch` = its library id). Shared by both presentations: a tap on a grid tile and A
    // on a centred cover are the same act, and a launch that behaved differently between them would
    // be a bug nobody could see until they switched input device.
    fun launch(identity: ClientIdentity, game: GameEntry) {
        if (launching) return
        launching = true
        scope.launch {
            val handle = connectToHost(
                context, streamSettings, identity,
                host.address, host.port, host.fpHex, launch = game.id,
            )
            launching = false
            if (handle != 0L) {
                onLaunched(
                    ActiveSession(
                        handle,
                        streamSettings,
                        host.clipboardSync,
                        profileName = profile?.name,
                        hostId = host.id,
                        // Where to come back to when this game exits — this shelf, pin and all,
                        // not the host's default one.
                        launchedFromLibrary = true,
                        libraryProfileId = pinnedProfileId,
                    ),
                )
            } else {
                Toast.makeText(
                    context,
                    "Launch failed — check the host and try again.",
                    Toast.LENGTH_LONG,
                ).show()
            }
        }
    }

    // "Copy link" for one TITLE — the self-emitted form a host card already hands out (design/
    // client-deep-links.md §4/§5), plus this game's `launch=` id, so pasting the URL into Playnite
    // or a Stream Deck macro boots straight into it. A shelf opened from a PINNED card copies that
    // card's profile with it, because that combination is the thing being copied.
    fun copyLink(game: GameEntry) {
        val url = DeepLinks.forHost(host, launch = game.id, profile = pinnedProfileId).toUrl()
        // A toast either way here: this screen renders neither the touch home's notice banner nor
        // the console's status line, and both of its presentations are full-bleed over artwork.
        linkCopyMessage(putLinkOnClipboard(context, url))?.let {
            Toast.makeText(context, it, Toast.LENGTH_SHORT).show()
        }
    }

    // Lambdas, NOT `::launch` / `::copyLink`: two callable references to the same local function
    // compare EQUAL however different the frame they captured, so a skipped recomposition would
    // leave the child calling a closure over stale settings. SettingsScreen documents the same
    // trap at `scopeProfile()`, having been bitten by it.
    if (console) {
        ConsoleLibrary(
            title = title,
            state = state,
            launching = launching,
            navActive = navActive,
            onBack = onBack,
            onLaunch = { identity, game -> launch(identity, game) },
            onCopyLink = { game -> copyLink(game) },
        )
    } else {
        TouchLibrary(
            title = title,
            state = state,
            launching = launching,
            onBack = onBack,
            onReload = { reloadKey++ },
            onLaunch = { identity, game -> launch(identity, game) },
            onCopyLink = { game -> copyLink(game) },
        )
    }
}

/** The console (gamepad) shelf: aurora, console header, coverflow, floating legend. */
@Composable
private fun ConsoleLibrary(
    title: String,
    state: LibState,
    launching: Boolean,
    navActive: Boolean,
    onBack: () -> Unit,
    onLaunch: (ClientIdentity, GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
) {
    val ink = LocalGamepadInk.current
    val hazeState = remember { HazeState() }
    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE
    // The cover the legend's X acts on — the coverflow reports it as the cursor settles, so the
    // hint and the press agree about which title they mean.
    var focused by remember { mutableStateOf<GameEntry?>(null) }

    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(hazeState)) {
            GamepadAuroraBackground(Modifier.fillMaxSize())
            Column(Modifier.fillMaxSize().consoleSafeArea()) {
                ConsoleHeader(title)
                Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                    when (state) {
                        is LibState.Loading -> LoadingState()
                        is LibState.Message -> MessageState(state.text)
                        is LibState.Ready -> Coverflow(
                            games = state.games,
                            loader = state.loader,
                            navActive = navActive && !launching,
                            onFocus = { focused = it },
                            onCopyLink = onCopyLink,
                            onLaunch = { game -> onLaunch(state.identity, game) },
                        )
                    }
                }
            }
        }
        // Launching overlay — the connect + host-side game boot takes a moment; block the pad while it runs.
        if (launching) {
            Box(
                Modifier.fillMaxSize().background(ink.modalScrim),
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(14.dp),
                ) {
                    CircularProgressIndicator(color = ink.fg)
                    Text("Launching…", color = ink.fg, style = MaterialTheme.typography.bodyLarge)
                }
            }
        }
        // Floating legend at the shared spot — same landscape-aware inset as every other console
        // screen (ignore the safe area in landscape, where the bottom edge isn't a tap target).
        Box(
            Modifier.align(Alignment.BottomStart)
                .consoleLegendInsets(landscape)
                .padding(ConsoleLegendInset),
        ) {
            GamepadHintBar(
                buildList {
                    if (state is LibState.Ready) {
                        add(PadGlyph.hint('A', "Launch"))
                        // A controller has no right-click, so the grid's context menu becomes a
                        // face button and a legend entry — the one per-game action there is.
                        add(
                            PadGlyph.hint('X', "Copy link") {
                                focused?.let(onCopyLink)
                            },
                        )
                    }
                    add(PadGlyph.hint('B', "Close", onClick = onBack))
                },
                hazeState = hazeState,
            )
        }
    }
}

/**
 * The touch shelf: a Material poster grid under a back/reload header — the same page the Apple,
 * GTK and Windows shells draw, and the reason a finger-driven user can reach the library at all.
 *
 * Deliberately NOT the coverflow with the pad legend hidden: a coverflow is a one-at-a-time strip
 * built for a D-pad, and on a phone it turns a 400-title library into 400 swipes. The grid is what
 * every other touch surface in this app (and every other client's) already shows.
 */
@Composable
private fun TouchLibrary(
    title: String,
    state: LibState,
    launching: Boolean,
    onBack: () -> Unit,
    onReload: () -> Unit,
    onLaunch: (ClientIdentity, GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
) {
    Box(Modifier.fillMaxSize()) {
        Column(Modifier.fillMaxSize().consoleSafeArea()) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.fillMaxWidth().padding(start = 4.dp, end = 4.dp, top = 8.dp),
            ) {
                IconButton(onClick = onBack) {
                    Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "Back")
                }
                Text(
                    title,
                    style = MaterialTheme.typography.titleLarge,
                    maxLines = 2,
                    overflow = TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                // A shelf that failed to load is otherwise a dead end you have to back out of and
                // re-enter; every other client's library page has had this from the start.
                IconButton(onClick = onReload, enabled = state !is LibState.Loading) {
                    Icon(Icons.Filled.Refresh, contentDescription = "Reload")
                }
            }
            when (state) {
                is LibState.Loading -> Box(
                    Modifier.weight(1f).fillMaxWidth(),
                    contentAlignment = Alignment.Center,
                ) {
                    Column(
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(14.dp),
                    ) {
                        CircularProgressIndicator()
                        Text("Loading library…", style = MaterialTheme.typography.bodyLarge)
                    }
                }
                is LibState.Message -> Box(
                    Modifier.weight(1f).fillMaxWidth(),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        state.text,
                        style = MaterialTheme.typography.bodyLarge,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        textAlign = TextAlign.Center,
                        modifier = Modifier.padding(horizontal = 24.dp),
                    )
                }
                is LibState.Ready -> TouchGrid(
                    games = state.games,
                    loader = state.loader,
                    onLaunch = { game -> onLaunch(state.identity, game) },
                    onCopyLink = onCopyLink,
                    modifier = Modifier.weight(1f),
                )
            }
        }
        if (launching) {
            Box(
                Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.55f)),
                contentAlignment = Alignment.Center,
            ) {
                Column(
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(14.dp),
                ) {
                    CircularProgressIndicator(color = Color.White)
                    Text("Launching…", color = Color.White, style = MaterialTheme.typography.bodyLarge)
                }
            }
        }
    }
}

/**
 * The poster grid. Design D4: launcher entries get their own shelf above the titles, never
 * interleaved, and the headings only appear when both groups exist — so a library without
 * launchers looks exactly like a plain grid.
 *
 * Internal (not private) for the same reason as [Coverflow]: the screenshot harness composes the
 * real grid with a mock shelf, because the screen around it takes its state off the network.
 */
@Composable
internal fun TouchGrid(
    games: List<GameEntry>,
    loader: ImageLoader,
    onLaunch: (GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
    modifier: Modifier = Modifier,
) {
    val launchers = games.filter { it.isLauncher }
    val titles = games.filter { !it.isLauncher }
    val both = launchers.isNotEmpty() && titles.isNotEmpty()
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 130.dp),
        modifier = modifier.fillMaxWidth(),
        contentPadding = PaddingValues(16.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        if (launchers.isNotEmpty()) {
            if (both) item(span = { GridItemSpan(maxLineSpan) }) { TouchGroupHeading("Launchers") }
            items(launchers, key = { "launcher-${it.id}" }) {
                TouchPoster(it, loader, onLaunch, onCopyLink)
            }
        }
        if (titles.isNotEmpty()) {
            if (both) item(span = { GridItemSpan(maxLineSpan) }) { TouchGroupHeading("Games") }
            items(titles, key = { "game-${it.id}" }) {
                TouchPoster(it, loader, onLaunch, onCopyLink)
            }
        }
    }
}

@Composable
private fun TouchGroupHeading(text: String) {
    Text(
        text.uppercase(),
        style = MaterialTheme.typography.labelSmall,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        letterSpacing = 1.4.sp,
    )
}

/**
 * One touch tile: 2:3 poster, store badge, title. Tap launches; a LONG PRESS opens this title's own
 * actions — the finger's context menu, and the same gesture the host cards' overflow answers to.
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun TouchPoster(
    game: GameEntry,
    loader: ImageLoader,
    onLaunch: (GameEntry) -> Unit,
    onCopyLink: (GameEntry) -> Unit,
) {
    var menu by remember { mutableStateOf(false) }
    val shape = MaterialTheme.shapes.medium
    Box {
        Column(
            Modifier.combinedClickable(
                onClickLabel = "Launch ${game.title}",
                onLongClickLabel = "More options for ${game.title}",
                onClick = { onLaunch(game) },
                onLongClick = { menu = true },
            ),
        ) {
            Box(
                Modifier
                    .fillMaxWidth()
                    .aspectRatio(2f / 3f)
                    .clip(shape)
                    .background(MaterialTheme.colorScheme.surfaceVariant),
                contentAlignment = Alignment.Center,
            ) {
                TouchPosterArt(game, loader)
                Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopStart) {
                    Text(
                        game.storeLabel,
                        style = MaterialTheme.typography.labelSmall,
                        // A launcher's badge is brand-filled (design D4); a game's sits on a dark
                        // wash over its own art, where the theme's own ink would be a coin toss.
                        color = if (game.isLauncher) {
                            MaterialTheme.colorScheme.onPrimary
                        } else {
                            Color.White
                        },
                        modifier = Modifier
                            .semantics {
                                contentDescription = if (game.isLauncher) {
                                    "Opens ${game.storeLabel}"
                                } else {
                                    "From ${game.storeLabel}"
                                }
                            }
                            .clip(ConsoleShape.Pill)
                            .background(
                                if (game.isLauncher) {
                                    MaterialTheme.colorScheme.primary
                                } else {
                                    Color.Black.copy(alpha = 0.5f)
                                },
                            )
                            .padding(horizontal = 8.dp, vertical = 3.dp),
                    )
                }
            }
            Text(
                game.title,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 2,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.padding(top = 6.dp),
            )
        }
        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
            DropdownMenuItem(
                text = { Text("Copy link") },
                onClick = {
                    menu = false
                    onCopyLink(game)
                },
            )
        }
    }
}

/** The tile's artwork: the candidates in order (portrait → header → hero), then a placeholder. */
@Composable
private fun TouchPosterArt(game: GameEntry, loader: ImageLoader) {
    val candidates = game.art.posterCandidates
    var idx by remember(game.id) { mutableIntStateOf(0) }
    if (idx < candidates.size) {
        AsyncImage(
            model = ImageRequest.Builder(LocalContext.current).data(candidates[idx]).build(),
            imageLoader = loader,
            contentDescription = game.title,
            contentScale = ContentScale.Crop,
            modifier = Modifier.fillMaxSize(),
            onError = { idx++ }, // this candidate failed — try the next, or fall to the placeholder
        )
        return
    }
    // A launcher ships no poster by design, so its brand mark IS the poster; falling back to the
    // launcher's NAME says "opens Steam", where a title would read as "a cover that failed to load".
    val mark = launcherIcon(game.iconToken)
    if (mark != null) {
        Icon(
            imageVector = mark,
            contentDescription = game.title,
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.fillMaxSize(0.45f),
        )
    } else {
        Text(
            if (game.isLauncher) game.storeLabel else game.title,
            style = MaterialTheme.typography.titleSmall,
            fontWeight = FontWeight.SemiBold,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            textAlign = TextAlign.Center,
            modifier = Modifier.padding(12.dp),
        )
    }
}

@Composable
private fun LoadingState() {
    val ink = LocalGamepadInk.current
    Column(horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(14.dp)) {
        CircularProgressIndicator(color = ink.fg)
        Text("Loading library…", color = ink.fg(0.7f), style = MaterialTheme.typography.bodyLarge)
    }
}

@Composable
private fun MessageState(text: String) {
    val ink = LocalGamepadInk.current
    Text(
        text,
        color = ink.fg(0.75f),
        style = MaterialTheme.typography.bodyLarge,
        textAlign = TextAlign.Center,
        modifier = Modifier.padding(horizontal = 24.dp),
    )
}

// Internal (not private): the screenshot harness composes the real coverflow with mock games —
// the library screen itself can't be shot, its state comes off the network.
@Composable
internal fun Coverflow(
    games: List<GameEntry>,
    loader: ImageLoader,
    navActive: Boolean,
    /** Reports the CENTRED title as the cursor settles, so the screen's legend acts on it too. */
    onFocus: (GameEntry?) -> Unit = {},
    /** X — copy the centred title's link. Defaulted so the screenshot harness needs no wiring. */
    onCopyLink: (GameEntry) -> Unit = {},
    onLaunch: (GameEntry) -> Unit,
) {
    val ink = LocalGamepadInk.current
    BoxWithConstraints(Modifier.fillMaxSize()) {
        // Fit a 2:3 poster into the height the detail line leaves; clamp so it never dwarfs the screen.
        val coverHeight = (maxHeight * 0.72f).coerceAtMost(360.dp)
        val coverWidth = coverHeight * 2f / 3f
        val sidePad = ((maxWidth - coverWidth) / 2).coerceAtLeast(0.dp)
        val pagerState = rememberPagerState(pageCount = { games.size })
        val scope = rememberCoroutineScope()
        var navTarget by remember { mutableIntStateOf(0) }
        LaunchedEffect(pagerState.settledPage) { navTarget = pagerState.settledPage }
        val current = games.getOrNull(navTarget)
        // Publish the centred title outward. Keyed on the ENTRY, not the index, so a library
        // refresh that shortens the strip can't leave the legend pointing at a title that moved.
        LaunchedEffect(current) { onFocus(current) }

        // Controller nav: the pad drives the coverflow. Left/right steps a coalesced target the pager
        // chases; A launches the centred title; X copies its link; B closes via the screen's
        // BackHandler.
        GamepadNavEffect(
            active = navActive && games.isNotEmpty(),
            onMove = { dir ->
                val t = (navTarget + dir).coerceIn(0, games.lastIndex)
                if (t != navTarget) { navTarget = t; scope.launch { pagerState.animateScrollToPage(t) } }
            },
            onActivate = { games.getOrNull(navTarget)?.let(onLaunch) },
            // Read at press time rather than closed over: `navTarget` moves under this callback,
            // and the link must be the one the cover under the cursor now points at.
            onTertiary = { games.getOrNull(navTarget)?.let(onCopyLink) },
        )

        // Design D4: the launcher entries lead the strip (the client groups them at parse time).
        // A coverflow is one-dimensional, so instead of a second focus rail the heading names the
        // group the cursor is in and changes as it crosses the boundary. Only drawn when the
        // library actually has both groups — otherwise the screen is exactly what it was.
        val bothGroups = games.any { it.isLauncher } && games.any { !it.isLauncher }
        Column(Modifier.fillMaxSize(), verticalArrangement = Arrangement.Center) {
            if (bothGroups) {
                Text(
                    if (current?.isLauncher == true) "LAUNCHERS" else "GAMES",
                    style = MaterialTheme.typography.labelSmall,
                    // The palette's ink, not white: on a pale field this heading was white on
                    // near-white and simply wasn't there.
                    color = ink.fg(0.45f),
                    letterSpacing = 2.sp,
                    textAlign = TextAlign.Center,
                    modifier = Modifier
                        .fillMaxWidth()
                        // A live region: this heading is the ONLY signal that the cursor has
                        // crossed from the launchers into the games, and a coverflow gives a
                        // reader no other way to notice — it is one strip, not two lists.
                        .semantics { liveRegion = LiveRegionMode.Polite }
                        .padding(bottom = 8.dp),
                )
            }
            HorizontalPager(
                state = pagerState,
                pageSize = PageSize.Fixed(coverWidth),
                contentPadding = PaddingValues(horizontal = sidePad),
                pageSpacing = 0.dp,          // translationX (below) does the spacing so covers sit closer
                beyondViewportPageCount = 3, // render more neighbours so a denser fan is visible
                modifier = Modifier.fillMaxWidth().height(coverHeight + 24.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) { page ->
                val signed = (pagerState.currentPage - page) + pagerState.currentPageOffsetFraction
                val d = signed.absoluteValue
                Poster(
                    game = games[page],
                    loader = loader,
                    modifier = Modifier
                        .zIndex(-d) // centred cover on top, neighbours stacked behind
                        .width(coverWidth)
                        .height(coverHeight)
                        // Touch: tap the centred cover to launch it; tap a neighbour to bring it centre.
                        // The label says which of the two a press does, because from the poster
                        // alone they are indistinguishable — and the CENTRED one is the only one A
                        // acts on, which nothing else in the tree says.
                        .clickable(
                            onClickLabel = if (page == pagerState.currentPage) {
                                "Launch ${games[page].title}"
                            } else {
                                "Bring ${games[page].title} to the centre"
                            },
                        ) {
                            if (page == pagerState.currentPage) onLaunch(games[page])
                            else scope.launch { pagerState.animateScrollToPage(page) }
                        }
                        .semantics {
                            if (page == pagerState.currentPage) selected = true
                        }
                        .graphicsLayer {
                            // Centre at full size; EVERY neighbour settles to one size, so an even pitch
                            // yields even VISUAL gaps. (A progressive shrink made the outer gaps grow —
                            // the "edges spread apart while the centre gets crowded" look.)
                            val scale = 1f - 0.28f * d.coerceAtMost(1f)
                            scaleX = scale
                            scaleY = scale
                            alpha = (1f - 0.26f * d).coerceAtLeast(0.15f) // depth via fade, not size
                            val rotDeg = signed.coerceIn(-2.5f, 2.5f) * 26f // tilt inward
                            rotationY = rotDeg
                            // Even neighbour pitch (0.8·cover) + a little extra outward push (ramped over
                            // the first step so scrolling stays smooth) so the CENTRE card breathes.
                            val base = signed * size.width * 0.2f - signed.coerceIn(-1f, 1f) * size.width * 0.14f
                            // Counter-balance: a rotated card projects narrower (≈cos θ), which opens its
                            // inner gap — pull it back toward centre by the half-width it loses so the
                            // gaps stay even no matter the tilt.
                            val halfW = size.width * scale * 0.5f
                            val counter = sign(signed) * halfW * (1f - cos(rotDeg * (PI.toFloat() / 180f)))
                            translationX = base + counter
                            // Lower cameraDistance = stronger perspective (CSS `perspective`); the flat
                            // 22 washed the tilt out. 9 makes the same angle read as real depth.
                            cameraDistance = 9f * density
                            transformOrigin = TransformOrigin(0.5f, 0.5f)
                        },
                )
            }
            Column(
                Modifier.fillMaxWidth().padding(top = 14.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                Text(
                    current?.title ?: " ",
                    style = MaterialTheme.typography.titleLarge,
                    fontWeight = FontWeight.Bold,
                    color = ink.fg,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
                if (current != null) {
                    Text(
                        if (current.isLauncher) "${current.storeLabel.uppercase()} \u00B7 LAUNCHER"
                        else current.storeLabel.uppercase(),
                        style = MaterialTheme.typography.labelMedium,
                        color = ink.fg(0.5f),
                        letterSpacing = 2.sp,
                    )
                }
            }
        }
    }
}

/** One cover: walks the art candidates (portrait → header → hero) then a text placeholder. */
@Composable
private fun Poster(game: GameEntry, loader: ImageLoader, modifier: Modifier = Modifier) {
    val ink = LocalGamepadInk.current
    val candidates = game.art.posterCandidates
    var idx by remember(game.id) { mutableStateOf(0) }
    val shape = ConsoleShape.Poster
    Box(
        modifier = modifier
            .clip(shape)
            // The ground a cover sits on while its art loads (and the permanent one for a launcher
            // entry, which rarely has art). Palette-derived rather than a fixed indigo, so a poster
            // wall on a pale field isn't a grid of dark holes.
            .background(LocalGamepadPalette.current.groundColor)
            .border(1.dp, ink.fg(0.12f), shape),
        contentAlignment = Alignment.Center,
    ) {
        if (idx < candidates.size) {
            AsyncImage(
                model = ImageRequest.Builder(LocalContext.current).data(candidates[idx]).build(),
                imageLoader = loader,
                contentDescription = game.title,
                contentScale = ContentScale.Crop,
                modifier = Modifier.fillMaxSize(),
                onError = { idx++ }, // this candidate failed — try the next, or fall to the placeholder
            )
        } else {
            // A launcher ships no poster by design, so its brand mark IS the poster — drawn big and
            // centred, tinted like the text it replaces. Falling back to the launcher's name says
            // "opens Steam" for a mark we don't ship; the title would read as "a game whose cover
            // failed to load".
            val mark = launcherIcon(game.iconToken)
            if (mark != null) {
                Icon(
                    imageVector = mark,
                    contentDescription = game.title,
                    tint = ink.fg(0.75f),
                    modifier = Modifier.fillMaxSize(0.45f),
                )
            } else {
                Text(
                    if (game.isLauncher) game.storeLabel else game.title,
                    style = MaterialTheme.typography.titleMedium,
                    fontWeight = FontWeight.SemiBold,
                    color = ink.fg(0.75f),
                    textAlign = TextAlign.Center,
                    modifier = Modifier.padding(12.dp),
                )
            }
        }
        // Store badge, top-start — brand-filled for a launcher entry (design D4).
        Box(Modifier.fillMaxSize().padding(8.dp), contentAlignment = Alignment.TopStart) {
            Text(
                game.storeLabel,
                style = MaterialTheme.typography.labelSmall,
                // A launcher's badge is brand-filled, so it reads on the ACCENT; a game's sits on
                // a plain dark wash over its own art.
                color = if (game.isLauncher) ink.onAccent else Color.White,
                modifier = Modifier
                    // A bare store name read out after the title says nothing about WHY it is
                    // there; the poster's own description already carries the title.
                    .semantics {
                        contentDescription = if (game.isLauncher) {
                            "Opens ${game.storeLabel}"
                        } else {
                            "From ${game.storeLabel}"
                        }
                    }
                    .clip(ConsoleShape.Pill)
                    .background(
                        // The console's palette accent, not `MaterialTheme.colorScheme.primary` —
                        // that is the TOUCH theme's colour (Material You, seeded from the user's
                        // wallpaper), which had nothing to do with the field this poster sits on.
                        if (game.isLauncher) ink.accent
                        else Color.Black.copy(alpha = 0.5f),
                    )
                    .padding(horizontal = 8.dp, vertical = 3.dp),
            )
        }
    }
}
