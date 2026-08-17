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
import androidx.compose.foundation.lazy.grid.rememberLazyGridState
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.PageSize
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Refresh
import androidx.compose.material.icons.filled.PlayArrow
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
import androidx.compose.runtime.saveable.rememberSaveable
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
import io.unom.punktfunk.kit.NativeBridge
import io.unom.punktfunk.kit.library.GameEntry
import io.unom.punktfunk.kit.library.LibraryClient
import io.unom.punktfunk.kit.library.LibraryResult
import io.unom.punktfunk.kit.library.LibraryCache
import io.unom.punktfunk.kit.library.RunningGame
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
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext

// The host game-library browser — the Android mirror of the Apple client's LibraryView: ONE screen
// with two presentations of the same shelf, chosen the same way every other screen here chooses.
// The console (gamepad) one is a poster coverflow (centered cover flat + prominent, neighbours
// receding on a 3D Y-tilt), reached with Y from a saved host; the touch one is the poster GRID the
// Apple, GTK and Windows shells draw, reached from a host card's "Browse library…". Both fetch from
// the host's management API over mTLS, and everything data-shaped — the fetch, the states, the
// launch — is shared: only the arrangement differs, because only the input device does.

/**
 * Whether the shelf on screen is an observation or a memory — and, if a memory, whether anything is
 * still being done about it.
 *
 * Three states rather than a flag because the two cached ones want different words. "Waking the
 * host…" says a shelf is about to become current; "last known library" says it isn't going to.
 * Telling a player the first thing while nothing is happening is the kind of lie a progress
 * indicator tells, and it is worth one extra state to never tell it.
 */
private enum class Stale(val note: String?) {
    No(null),
    Waking("Last known library — waking the host…"),
    Offline("Last known library — the host didn't answer"),
}

private sealed class LibState {
    object Loading : LibState()

    /**
     * The shelf. Carries the client identity so a launch can dial the host over the same pinned
     * mTLS trust.
     *
     * [running] is keyed by library id and is HOST state, never catalog state — it arrives from
     * `/status` separately and later, and is deliberately absent from what [LibraryCache] writes,
     * so a shelf served from disk can never claim a game is up because it was up last time
     * anybody looked.
     */
    data class Ready(
        val games: List<GameEntry>,
        val loader: ImageLoader,
        val identity: ClientIdentity,
        val running: Map<String, RunningGame> = emptyMap(),
        val stale: Stale = Stale.No,
    ) : LibState() {
        /**
         * Display order: anything already running first, so getting back into it is the first
         * thing on screen rather than something to scroll for.
         *
         * Applied on top of the launchers-first grouping the fetch already did, never instead of
         * it — a launcher that is up still belongs with the launchers, which is what keeps design
         * D4's two sections from interleaving. `sortedBy` is stable, so the host's own title order
         * survives inside each of the four resulting bands.
         */
        val ordered: List<GameEntry>
            get() = if (running.isEmpty()) {
                games
            } else {
                games.sortedBy { (if (it.isLauncher) 0 else 2) + (if (running[it.id] != null) 0 else 1) }
            }
    }

    data class Message(val text: String) : LibState() // unauthorized / empty / error
}

/**
 * How long to keep asking a host we have just sent a magic packet to. A cold box takes 20–60 s to
 * POST and start serving, so one attempt would almost always land on a machine that is still
 * booting — the same 90-second budget [WakeController] allows.
 */
private const val WAKE_ATTEMPTS = 12
private const val WAKE_RETRY_MS = 5_000L

/**
 * Re-send the magic packet every other attempt (≈ every 10 s). A single packet can be missed, and
 * some NICs only wake on a fresh one after dropping into a deeper sleep state — [WakeController]'s
 * rule, expressed in this loop's units.
 */
private const val WAKE_RESEND_EVERY = 2

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
    //
    // Four things happen here and the ORDER is the point:
    //   1. the CACHED catalog goes up immediately, marked stale — a library is the screen a player
    //      uses to decide what to play, and an empty one while a sleeping box boots is the opposite
    //      of useful;
    //   2. a magic packet goes out, so the box warms while they are still choosing — waking used to
    //      be bound to CONNECTING, which is far too late to help;
    //   3. the live fetch runs, retrying across the boot window, and replaces the cached shelf;
    //   4. `/status` says which titles are already up, AFTER the catalog so a slow answer can never
    //      hold the titles back.
    //
    // A cached catalog also outranks a failure: if the host never answers, the titles on screen are
    // still the right ones to choose from, and replacing them with an error because a box is asleep
    // is precisely what the cache exists to prevent.
    LaunchedEffect(host.address, host.port, host.fpHex, host.effectiveMgmtPort, reloadKey) {
        state = LibState.Loading
        val cache = LibraryCache.standard(context.cacheDir)
        // The identity and the art loader are needed whether or not the host ever answers — cached
        // posters have to render behind a cached catalog with the box still down.
        val prepared = withContext(Dispatchers.IO) {
            val id = runCatching { obtainIdentity(IdentityStore(context)) }.getOrNull()
            val loader = id?.let {
                runCatching {
                    ImageLoader.Builder(context)
                        .okHttpClient(mtlsHttpClient(it.certPem, it.privateKeyPem, host.address, host.fpHex))
                        .build()
                }.getOrNull()
            }
            if (id != null && loader != null) id to loader else null
        }
        if (prepared == null) {
            state = LibState.Message("Identity unavailable — re-pair may be required.")
            return@LaunchedEffect
        }
        val (identity, loader) = prepared

        // Keyed on the host RECORD id, not its address: a box that came back on a new DHCP lease
        // is the same host with the same library, and keying on where it lives would lose the
        // cache exactly when a cold-booted machine needs it most.
        val cached = withContext(Dispatchers.IO) { cache.load(host.id)?.games }?.takeIf { it.isNotEmpty() }
        if (cached != null) {
            state = LibState.Ready(cached, loader, identity, stale = Stale.Waking)
        }

        // Fire-and-forget, and deliberately unconditional rather than only when the host looks
        // offline: a magic packet is one datagram an already-awake machine ignores, so finding out
        // whether it is needed costs more than sending it.
        val macs = host.mac.joinToString(",")
        val waking = host.mac.isNotEmpty()
        if (waking) {
            withContext(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(macs, host.address) }
        }

        val attempts = if (waking) WAKE_ATTEMPTS else 1
        var last: LibraryResult? = null
        for (attempt in 0 until attempts) {
            val res = withContext(Dispatchers.IO) {
                LibraryClient.fetch(
                    address = host.address,
                    mgmtPort = host.effectiveMgmtPort,
                    certPem = identity.certPem,
                    keyPem = identity.privateKeyPem,
                    fpHex = host.fpHex,
                )
            }
            last = res
            // Anything other than "can't reach it" is settled — see `LibraryResult.isTransient`.
            if (res is LibraryResult.Ok || !res.isTransient || attempt + 1 >= attempts) break
            if (attempt % WAKE_RESEND_EVERY == WAKE_RESEND_EVERY - 1) {
                withContext(Dispatchers.IO) { NativeBridge.nativeWakeOnLan(macs, host.address) }
            }
            delay(WAKE_RETRY_MS)
        }

        when (val res = last) {
            is LibraryResult.Ok -> if (res.games.isEmpty()) {
                state = LibState.Message("No games found on this host.")
            } else {
                state = LibState.Ready(res.games, loader, identity)
                // Remembered AFTER it is on screen: the disk write is not on the path to a shelf.
                withContext(Dispatchers.IO) { cache.store(host.id, res.games) }
                val running = withContext(Dispatchers.IO) {
                    LibraryClient.fetchRunning(
                        address = host.address,
                        mgmtPort = host.effectiveMgmtPort,
                        certPem = identity.certPem,
                        keyPem = identity.privateKeyPem,
                        fpHex = host.fpHex,
                    )
                }
                    .filter { it.isUp }
                    // Two sessions can have the same title up (the host admits concurrent
                    // sessions); for a Resume badge either one is the same answer.
                    .mapNotNull { g -> g.appId?.let { it to g } }
                    .toMap()
                (state as? LibState.Ready)?.let { state = it.copy(running = running) }
            }
            // The shelf stays if we have one; only the words change. The player can still pick a
            // title — the launch dials and wakes the host on its own.
            else -> state = if (cached != null) {
                LibState.Ready(cached, loader, identity, stale = Stale.Offline)
            } else {
                LibState.Message(
                    when (res) {
                        is LibraryResult.Unauthorized -> res.message
                        is LibraryResult.Error -> res.message
                        else -> "Couldn't load the library."
                    },
                )
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
        // The player's place in this shelf, remembered as the TITLE rather than an index or a
        // scroll offset — the only moment they are definitely leaving the grid for one. Recorded
        // before the dial rather than after it succeeds: a failed launch still means "this is the
        // one I was going for", and coming back to it is right either way.
        LibraryPosition.remember(context, host.id, game.id)
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
    // Where this shelf should open. Read ONCE per screen (not per recomposition) so a launch
    // rewriting it mid-browse cannot make the grid jump under the player's thumb; the effects that
    // consume it are one-shot on top of that.
    val resumeAt = remember(host.id) { LibraryPosition.last(context, host.id) }

    if (console) {
        ConsoleLibrary(
            title = title,
            state = state,
            launching = launching,
            navActive = navActive,
            onBack = onBack,
            onLaunch = { identity, game -> launch(identity, game) },
            onCopyLink = { game -> copyLink(game) },
            resumeAt = resumeAt,
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
            resumeAt = resumeAt,
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
    /** The title this shelf last launched — where the coverflow opens. Null on a first visit. */
    resumeAt: String? = null,
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
                // Says the titles below are remembered rather than observed — shown only while
                // that is true, and never as an error: a cached library is a working library, and
                // a host that is still waking is the case this whole path exists to serve.
                (state as? LibState.Ready)?.stale?.note?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.labelMedium,
                        color = ink.fg(0.55f),
                        modifier = Modifier.padding(horizontal = 24.dp, vertical = 4.dp),
                    )
                }
                Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                    when (state) {
                        is LibState.Loading -> LoadingState()
                        is LibState.Message -> MessageState(state.text)
                        is LibState.Ready -> Coverflow(
                            games = state.ordered,
                            loader = state.loader,
                            navActive = navActive && !launching,
                            onFocus = { focused = it },
                            onCopyLink = onCopyLink,
                            onLaunch = { game -> onLaunch(state.identity, game) },
                            running = state.running,
                            resumeAt = resumeAt,
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
    /** The title this shelf last launched — where the grid opens. Null on a first visit. */
    resumeAt: String? = null,
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
            // Says the titles below are remembered rather than observed — shown only while that is
            // true, and never as an error: a cached library is a working library, and a host that
            // is still waking is the case this whole path exists to serve.
            (state as? LibState.Ready)?.stale?.note?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier
                        .semantics { liveRegion = LiveRegionMode.Polite }
                        .padding(horizontal = 20.dp, vertical = 2.dp),
                )
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
                    games = state.ordered,
                    loader = state.loader,
                    onLaunch = { game -> onLaunch(state.identity, game) },
                    onCopyLink = onCopyLink,
                    running = state.running,
                    resumeAt = resumeAt,
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
    /**
     * Which titles the host already has up, keyed by library id — so a tile the player can return
     * to says `Resume` rather than looking like every other one. Empty on an older host, an
     * unreachable one, and while a shelf is being served from cache.
     */
    running: Map<String, RunningGame> = emptyMap(),
    /** The title this shelf last launched — where the grid opens. Null on a first visit. */
    resumeAt: String? = null,
    modifier: Modifier = Modifier,
) {
    val launchers = games.filter { it.isLauncher }
    val titles = games.filter { !it.isLauncher }
    val both = launchers.isNotEmpty() && titles.isNotEmpty()
    val grid = rememberLazyGridState()
    // Put the player back where they were. Leaving a stream re-composes this screen from scratch —
    // a new LazyGridState — so a long library came back at the top every time, and the round trip
    // this shelf exists for (browse → play → quit → browse) lost your place on every lap.
    //
    // Anchored on the TITLE, not an index or a pixel offset: an offset is meaningless across the
    // things that legitimately change between visits (a rotation, a resize, a foldable unfolding,
    // a host that gained titles, the running-first ordering above), and the list is re-ordered at
    // least twice per visit as the cached catalog gives way to the live one and then to `/status`.
    // Keyed on the list so it can still land once the title it names arrives, and one-shot so a
    // later re-order never yanks the grid out from under someone who has started scrolling.
    var restored by rememberSaveable { mutableStateOf(false) }
    LaunchedEffect(games, resumeAt) {
        if (restored || resumeAt == null) return@LaunchedEffect
        val index = flatIndexOf(resumeAt, launchers, titles, both)
        if (index == null) return@LaunchedEffect
        restored = true
        // Without animation: it should simply already be there, not visibly travel.
        grid.scrollToItem(index)
    }
    LazyVerticalGrid(
        columns = GridCells.Adaptive(minSize = 130.dp),
        state = grid,
        modifier = modifier.fillMaxWidth(),
        contentPadding = PaddingValues(16.dp),
        horizontalArrangement = Arrangement.spacedBy(12.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        if (launchers.isNotEmpty()) {
            if (both) item(span = { GridItemSpan(maxLineSpan) }) { TouchGroupHeading("Launchers") }
            items(launchers, key = { "launcher-${it.id}" }) {
                TouchPoster(it, loader, onLaunch, onCopyLink, running[it.id] != null)
            }
        }
        if (titles.isNotEmpty()) {
            if (both) item(span = { GridItemSpan(maxLineSpan) }) { TouchGroupHeading("Games") }
            items(titles, key = { "game-${it.id}" }) {
                TouchPoster(it, loader, onLaunch, onCopyLink, running[it.id] != null)
            }
        }
    }
}

/**
 * Where a title sits in the FLAT item stream `LazyVerticalGrid` scrolls by, headings included.
 *
 * The grid is not one list: design D4 gives the launchers their own section, and each section
 * carries a full-span heading whenever both exist. `scrollToItem` counts those headings, so an
 * index taken from `games` alone lands one or two tiles short — which is exactly the kind of
 * off-by-a-heading that only shows up on a library that happens to have launchers.
 *
 * Null when the title isn't in this grid at all: a host that lost it since, or a shelf filtered
 * down to a group it doesn't belong to.
 */
private fun flatIndexOf(
    id: String,
    launchers: List<GameEntry>,
    titles: List<GameEntry>,
    both: Boolean,
): Int? {
    val heading = if (both) 1 else 0
    launchers.indexOfFirst { it.id == id }.takeIf { it >= 0 }?.let { return heading + it }
    val gamesStart = if (launchers.isEmpty()) 0 else heading + launchers.size
    return titles.indexOfFirst { it.id == id }.takeIf { it >= 0 }?.let { gamesStart + heading + it }
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
    /** Already up on the host, so tapping resumes rather than starts. */
    running: Boolean = false,
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
                // Opposite corner from the store chip, so the two never meet on a narrow tile.
                if (running) {
                    Box(Modifier.fillMaxSize().padding(6.dp), contentAlignment = Alignment.TopEnd) {
                        RunningBadge(compact = true)
                    }
                }
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
    /**
     * Which titles the host already has up, keyed by library id — so a cover the player can
     * return to says `Resume` rather than looking like every other one. Empty on an older host,
     * an unreachable one, and while a shelf is being served from cache.
     */
    running: Map<String, RunningGame> = emptyMap(),
    /** The title this shelf last launched — where the strip opens. Null on a first visit. */
    resumeAt: String? = null,
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
        // Put the player back where they were. Leaving a stream re-composes this screen from
        // scratch — a new PagerState — so a long library came back at page 0 every time, and the
        // round trip this shelf exists for (browse → play → quit → browse) lost your place on
        // every lap.
        //
        // Anchored on the TITLE, not an index: the strip is re-ordered under us at least twice
        // per visit (the cached catalog gives way to the live one, then `/status` moves the
        // running titles to the front), and an index would land on whichever cover happened to
        // slide into that slot. Keyed on `games` so it can still land once the title it names
        // actually arrives, and one-shot so a later re-order never yanks the strip out from
        // under someone who has started browsing.
        var restored by rememberSaveable { mutableStateOf(false) }
        LaunchedEffect(games, resumeAt) {
            if (restored || resumeAt == null) return@LaunchedEffect
            val idx = games.indexOfFirst { it.id == resumeAt }
            if (idx < 0) return@LaunchedEffect
            restored = true
            // Without animation: it should simply already be there, not visibly travel.
            pagerState.scrollToPage(idx)
        }
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
                    running = running[games[page].id] != null,
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

/**
 * "This one is already up on the host" — the Resume affordance, overlaid on a poster.
 *
 * A badge rather than a changed action label because these tiles have no labels to change: the
 * poster *is* the control. It says `Resume` rather than `Running` on purpose — the player does not
 * need a status report, they need to know what tapping it will do.
 *
 * A fixed semantic green rather than the theme's primary: this is a state the HOST reports, not a
 * Punktfunk surface, and it has to stay distinguishable from the launcher chip, which already owns
 * the brand fill one corner away. Fixed also means it survives Material You seeding the touch theme
 * from a wallpaper that happens to be green.
 */
@Composable
private fun RunningBadge(compact: Boolean = false) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .semantics { contentDescription = "Running on the host \u2014 resume" }
            .clip(ConsoleShape.Pill)
            .background(RESUME_GREEN)
            .padding(horizontal = if (compact) 6.dp else 8.dp, vertical = 3.dp),
    ) {
        Icon(
            Icons.Filled.PlayArrow,
            contentDescription = null,
            tint = RESUME_INK,
            modifier = Modifier.size(12.dp),
        )
        // Glyph only on the touch grid, whose tiles go down to ~130 dp wide and already carry the
        // store chip in the opposite corner; at that size "Resume" plus an icon leaves the two
        // badges touching in the middle. The coverflow's covers are several times wider and take
        // the word.
        if (!compact) {
            Text(
                "Resume",
                style = MaterialTheme.typography.labelSmall,
                fontWeight = FontWeight.SemiBold,
                color = RESUME_INK,
                modifier = Modifier.padding(start = 4.dp),
            )
        }
    }
}

/** The host presence green the console palette already uses for "this machine is up". */
private val RESUME_GREEN = Color(0xFF33D64A)

/** Near-black on that green — a fixed pair, so neither theme nor palette can wash it out. */
private val RESUME_INK = Color(0xFF0A1A0D)

/** One cover: walks the art candidates (portrait → header → hero) then a text placeholder. */
@Composable
private fun Poster(
    game: GameEntry,
    loader: ImageLoader,
    running: Boolean = false,
    modifier: Modifier = Modifier,
) {
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
        // Opposite corner from the store chip, so the two never meet in the middle of a cover.
        if (running) {
            Box(Modifier.fillMaxSize().padding(8.dp), contentAlignment = Alignment.TopEnd) {
                RunningBadge()
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
