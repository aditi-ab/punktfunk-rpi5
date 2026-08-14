package io.unom.punktfunk

import android.widget.Toast
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.PageSize
import androidx.compose.foundation.pager.rememberPagerState
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

// The host game-library browser — the Android mirror of the Apple client's LibraryCoverflowView:
// a gamepad-driven poster coverflow (centered cover flat + prominent, neighbours receding on a 3D
// Y-tilt) fetched from the host's management API over mTLS. Reached with Y from a saved host.

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
) {
    val ink = LocalGamepadInk.current
    BackHandler(onBack = onBack)
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val hazeState = remember { HazeState() }
    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE
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
    LaunchedEffect(host.address, host.port, host.fpHex, host.effectiveMgmtPort) {
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

    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().hazeSource(hazeState)) {
            GamepadAuroraBackground(Modifier.fillMaxSize())
            Column(Modifier.fillMaxSize().consoleSafeArea()) {
                // A pinned card's shelf says so, in the card's own `host · profile` shape: what a
                // launch here will use is a property of the shelf, not something to remember from
                // the tile two screens back.
                ConsoleHeader(
                    if (pinnedProfileId != null && profile != null) {
                        "${host.name} · ${profile.name} — Library"
                    } else {
                        "${host.name} — Library"
                    },
                )
                Box(Modifier.weight(1f).fillMaxWidth(), contentAlignment = Alignment.Center) {
                    when (val s = state) {
                        is LibState.Loading -> LoadingState()
                        is LibState.Message -> MessageState(s.text)
                        is LibState.Ready -> Coverflow(s.games, s.loader, navActive && !launching) { game ->
                            if (!launching) {
                                launching = true
                                scope.launch {
                                    // Dial the host over the same pinned mTLS trust, booting straight
                                    // into this title (the host resolves `launch` = its library id).
                                    val handle = connectToHost(
                                        context, streamSettings, s.identity,
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
                                                // Where to come back to when this game exits —
                                                // this shelf, pin and all, not the host's default one.
                                                launchedFromLibrary = true,
                                                libraryProfileId = pinnedProfileId,
                                            ),
                                        )
                                    }
                                    else Toast.makeText(
                                        context,
                                        "Launch failed — check the host and try again.",
                                        Toast.LENGTH_LONG,
                                    ).show()
                                }
                            }
                        }
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
                    if (state is LibState.Ready) add(PadGlyph.hint('A', "Launch"))
                    add(PadGlyph.hint('B', "Close", onClick = onBack))
                },
                hazeState = hazeState,
            )
        }
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

@Composable
private fun Coverflow(
    games: List<GameEntry>,
    loader: ImageLoader,
    navActive: Boolean,
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

        // Controller nav: the pad drives the coverflow. Left/right steps a coalesced target the pager
        // chases; A launches the centred title; B closes via the screen's BackHandler.
        GamepadNavEffect(
            active = navActive && games.isNotEmpty(),
            onMove = { dir ->
                val t = (navTarget + dir).coerceIn(0, games.lastIndex)
                if (t != navTarget) { navTarget = t; scope.launch { pagerState.animateScrollToPage(t) } }
            },
            onActivate = { games.getOrNull(navTarget)?.let(onLaunch) },
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
