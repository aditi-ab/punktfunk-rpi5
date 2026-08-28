package io.unom.punktfunk

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
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
import androidx.compose.foundation.lazy.grid.GridCells
import androidx.compose.foundation.lazy.grid.GridItemSpan
import androidx.compose.foundation.lazy.grid.LazyVerticalGrid
import androidx.compose.foundation.lazy.grid.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Add
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import io.unom.punktfunk.components.EmptyHostsState
import io.unom.punktfunk.components.HostCard
import io.unom.punktfunk.components.HostMenuItem
import io.unom.punktfunk.components.SectionLabel
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.security.KnownHost
import io.unom.punktfunk.models.HostStatus

/**
 * The touch home: the saved/discovered host grid with the Add-host FAB over it — everything
 * `ConnectScreen` draws when the console UI is off, and the counterpart of [buildHomeTiles] +
 * `GamepadHome` when it is on.
 *
 * Pure display: every action arrives as a callback, because they all end in state the screen owns
 * (a dial in flight, the trust prompt, the host store). What this file DOES own is the arrangement
 * — which sections exist, in what order, and which actions a given card offers — and the two rules
 * that are easy to get wrong from the outside: a pinned card is a shortcut and so withholds the
 * host's destructive actions, and every card in a section reserves the profile chip's space as soon
 * as one of them needs it.
 */
@Composable
internal fun ConnectGrid(
    savedHosts: List<KnownHost>,
    /** Every live advert — the OS mark prefers it over the stored one, and "searching…" reads it. */
    discovered: List<DiscoveredHost>,
    /** Adverts with no saved record behind them, de-duped by the caller (it needs them too). */
    discoveredUnsaved: List<DiscoveredHost>,
    /** Saved hosts answering the QUIC probe, "address:port" — the routed half of "online". */
    reachable: Set<String>,
    profiles: List<StreamProfile>,
    pinsFor: (KnownHost) -> List<StreamProfile>,
    connecting: Boolean,
    /** A confirmation ("75 Mbit/s set in …"); [status] is the failure line. Never the same thing. */
    notice: String?,
    status: String?,
    lnpGranted: Boolean,
    /** Raise the local-network-permission prompt — the banner's "Allow…" and the wake guard. */
    onAskLocalNetwork: () -> Unit,
    /**
     * Dial a saved host. The second argument is `connect`'s one-off profile reference: null follows
     * the host's binding (a plain tap), a profile id forces that profile, and the empty string
     * forces the global defaults — a real, different action on a bound host, which is why it has to
     * survive as a value rather than collapsing into "unset".
     */
    onConnect: (KnownHost, String?) -> Unit,
    onConnectDiscovered: (DiscoveredHost) -> Unit,
    onForget: (KnownHost) -> Unit,
    onEdit: (KnownHost) -> Unit,
    onWake: (KnownHost) -> Unit,
    onSpeedTest: (KnownHost) -> Unit,
    /** Upload this device's recent log to the host — see the menu row's gate below. */
    onSendLogs: (KnownHost) -> Unit,
    /** What each paired host last said this device may do TO it, by fingerprint
     *  (`design/host-actions.md` §7). Absent = no rows. */
    hostActions: Map<String, List<HostActions.Action>>,
    onHostAction: (KnownHost, HostActions.Action) -> Unit,
    onCopyLink: (KnownHost, StreamProfile?) -> Unit,
    onTogglePin: (KnownHost, StreamProfile) -> Unit,
    /** The experimental game-library toggle — off hides "Browse library…" everywhere. */
    libraryEnabled: Boolean,
    /**
     * Open this card's game library. The second argument is the shelf's pinned profile id, exactly
     * as [onConnect] takes the card's one-off: browsing IS this card's connect with a title picked
     * first, so a pinned card's shelf launches with that card's profile.
     */
    onBrowseLibrary: (KnownHost, StreamProfile?) -> Unit,
    onRescan: () -> Unit,
    onAddHost: () -> Unit,
) {
    // The profile rows a card's overflow menu grows. With no profiles at all it stays empty — a
    // user who never wants this feature sees no new clutter anywhere but the settings scope chips.
    // "Connect with" is a ONE-OFF on every card: it never rebinds the host, which is why rebinding
    // lives in the Edit sheet instead.
    fun hostMenu(kh: KnownHost, pin: StreamProfile?): List<HostMenuItem> = buildList {
        // Browsing IS a connect-shaped action — this card's connect with a title picked first — so
        // a PINNED card offers it too, and its shelf launches with that card's profile. Without it
        // the touch home had no route to the library at all: the console shell reaches it with Y
        // from a tile, and a finger has no Y.
        if (libraryEnabled) {
            add(HostMenuItem("Browse library…") { onBrowseLibrary(kh, pin) })
        }
        if (pin == null) {
            add(HostMenuItem("Network speed test") { onSpeedTest(kh) })
        }
        // "Send logs to host" — the same row the console's host menu carries
        // (`pf-console-ui`'s `options.rs`), on the same gate: the upload authenticates with the
        // streaming cert, so it needs a paired identity and a host that is answering. It belongs
        // HERE too and not only in the console: a device whose console never comes up is exactly
        // the one whose logs somebody needs, and the touch home was its only shell.
        if (pin == null && kh.paired && kh.isOnline(discovered, reachable)) {
            add(HostMenuItem("Send logs to host") { onSendLogs(kh) })
        }
        // The host's own actions — sleep, restart, shut it down (`design/host-actions.md` §7),
        // the other half of the Wake-on-LAN round trip. Nothing is decided here: the list is
        // empty unless the host answered AND this device's access carries the grant, so no row
        // appears that the host would refuse. A pinned card is a shortcut to one profile, not a
        // second host, so it offers none — same rule as "Send logs" above.
        if (pin == null) {
            hostActions[kh.fpHex].orEmpty().forEach { a ->
                val label = if (a.available) a.label else "${a.label} (unavailable)"
                add(HostMenuItem(label) { onHostAction(kh, a) })
            }
        }
        add(HostMenuItem("Copy link") { onCopyLink(kh, pin) })
        if (profiles.isEmpty()) return@buildList
        if (pin != null) {
            add(HostMenuItem("Unpin card", startsSection = true) { onTogglePin(kh, pin) })
        }
        add(
            HostMenuItem("Connect with: Default settings", startsSection = true) {
                // The empty reference is "force the defaults", not "unset" — on a bound host that
                // is a real, different action from a plain tap.
                onConnect(kh, "")
            },
        )
        profiles.forEach { p ->
            add(HostMenuItem("Connect with: ${p.name}") { onConnect(kh, p.id) })
        }
        if (pin == null) {
            profiles.forEachIndexed { i, p ->
                val pinned = p.id in kh.pinnedProfileIds
                add(
                    HostMenuItem(
                        if (pinned) "Unpin card: ${p.name}" else "Pin as card: ${p.name}",
                        startsSection = i == 0,
                    ) { onTogglePin(kh, p) },
                )
            }
        }
    }

    // The saved-hosts grid: each host's own card, then one card per profile it has pinned, so a
    // pinned combination is a plain one-click connect instead of a trip through a menu.
    val savedCards = savedHosts.flatMap { kh ->
        listOf(HostCardEntry(kh, null)) + pinsFor(kh).map { HostCardEntry(kh, it) }
    }
    // Cards in one grid row must be the same height (the grid won't stretch them), so as soon as
    // ANY saved card carries a profile chip, they all reserve its space. Nobody who doesn't use
    // profiles ever sees the gap.
    val anyProfileChip = savedCards.any { it.pin != null || it.host.profileId != null }

    Box(Modifier.fillMaxSize()) {
        LazyVerticalGrid(
            columns = GridCells.Adaptive(minSize = 160.dp),
            modifier = Modifier.fillMaxSize(),
            contentPadding = PaddingValues(horizontal = 16.dp, vertical = 16.dp),
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            item(span = { GridItemSpan(maxLineSpan) }) {
                Column(horizontalAlignment = Alignment.CenterHorizontally) {
                    Spacer(Modifier.height(8.dp))
                    Text("Punktfunk", style = MaterialTheme.typography.headlineLarge)
                    Text(
                        "stream a remote desktop",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(24.dp))

                    notice?.let {
                        Surface(
                            color = MaterialTheme.colorScheme.secondaryContainer,
                            shape = MaterialTheme.shapes.medium,
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSecondaryContainer,
                                textAlign = TextAlign.Center,
                                modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                            )
                        }
                        Spacer(Modifier.height(16.dp))
                    }

                    status?.let {
                        // In-flight progress (connecting / waking) is the full-screen ConnectOverlay's
                        // job now, so `status` only ever carries a result/error here — a filled error
                        // container reads as a real failure banner, not just red text lost in the layout.
                        Surface(
                            color = MaterialTheme.colorScheme.errorContainer,
                            shape = MaterialTheme.shapes.medium,
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Text(
                                it,
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onErrorContainer,
                                textAlign = TextAlign.Center,
                                modifier = Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                            )
                        }
                        Spacer(Modifier.height(16.dp))
                    }
                }
            }

            if (!lnpGranted) {
                // Local network access denied: discovery can't ever find anything and every connect
                // would time out — say so at the top, with the fix one tap away, instead of letting
                // the screen look idle/broken.
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Surface(
                        color = MaterialTheme.colorScheme.errorContainer,
                        shape = MaterialTheme.shapes.medium,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Column(
                            Modifier.padding(horizontal = 16.dp, vertical = 12.dp),
                            horizontalAlignment = Alignment.CenterHorizontally,
                        ) {
                            Text(
                                "Local network access is off",
                                style = MaterialTheme.typography.titleSmall,
                                color = MaterialTheme.colorScheme.onErrorContainer,
                            )
                            Text(
                                "Android blocks Punktfunk from finding or reaching hosts until you allow it.",
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onErrorContainer,
                                textAlign = TextAlign.Center,
                            )
                            TextButton(onClick = onAskLocalNetwork) { Text("Allow…") }
                        }
                    }
                    Spacer(Modifier.height(12.dp))
                }
            }

            if (savedHosts.isEmpty() && discoveredUnsaved.isEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    EmptyHostsState()
                }
            }

            if (savedHosts.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    SectionLabel("Saved hosts")
                }
                items(savedCards, key = { it.key }) { entry ->
                    val kh = entry.host
                    val pin = entry.pin
                    val bound = kh.profileId?.let { id -> profiles.firstOrNull { it.id == id } }
                    HostCard(
                        name = kh.name,
                        address = "${kh.address}:${kh.port}",
                        status = if (kh.paired) HostStatus.PAIRED else HostStatus.TOFU,
                        online = kh.isOnline(discovered, reachable),
                        // Live advert preferred (the store lags a discovery tick), else stored.
                        os = discovered.firstOrNull { kh.matches(it) && it.os.isNotEmpty() }?.os
                            ?: kh.os,
                        enabled = !connecting,
                        // A pinned card connects with ITS profile; the host's own card follows the
                        // binding, which is exactly what its chip says it will do.
                        onConnect = { onConnect(kh, pin?.id) },
                        // Edit / Forget / Wake live on the host's own card only: a pinned card is a
                        // shortcut, not a second host, and offering destructive host actions on it
                        // would blur exactly that.
                        onForget = if (pin != null) null else ({ onForget(kh) }),
                        onEdit = if (pin != null) null else ({ onEdit(kh) }),
                        // Explicit wake-only: offered when the host is offline and we have a MAC. The
                        // screen runs it through the WakeController so it shows the "Waking…" overlay
                        // and waits for the host to come online (matched by fingerprint, so a new DHCP
                        // address on a cold boot still counts as "up") rather than firing a single
                        // silent packet.
                        onWake = if (pin == null && kh.mac.isNotEmpty() && !kh.isOnline(discovered, reachable)) {
                            ({ onWake(kh) })
                        } else {
                            null
                        },
                        profileLabel = pin?.name ?: bound?.name,
                        profileProminent = pin != null,
                        accent = accentColor(pin?.accent ?: bound?.accent),
                        menuItems = hostMenu(kh, pin),
                        reserveProfileSlot = anyProfileChip,
                    )
                }
            }

            if (discoveredUnsaved.isNotEmpty()) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Spacer(Modifier.height(12.dp))
                    SectionLabel("Discovered on the network")
                }
                items(discoveredUnsaved, key = { "disc-${it.host}-${it.port}" }) { dh ->
                    HostCard(
                        name = dh.name,
                        address = "${dh.host}:${dh.port}",
                        status = if (dh.pairingRequired) HostStatus.PAIRING else HostStatus.TOFU,
                        online = true, // in the discovered list ⇒ live on mDNS right now
                        os = dh.os,
                        enabled = !connecting,
                        onConnect = { onConnectDiscovered(dh) },
                        onForget = null,
                    )
                }
            }

            // Active-discovery hint: discovery runs whenever this screen is up, so while it's
            // scanning but nothing's turned up yet (and we're not mid-connect), show it's working
            // rather than looking idle/empty. Suppressed while local network access is denied —
            // a spinner would be a lie there (the browse can't receive anything); the banner above
            // owns that state.
            // Scan again is offered whether or not anything turned up: the case that sends people
            // here is ONE expected host missing, not an empty list, and a browse that quietly went
            // deaf (blocked when it started, or backed off to its hour-long re-query) looks
            // exactly like a network without that host on it.
            if (lnpGranted && !connecting) {
                item(span = { GridItemSpan(maxLineSpan) }) {
                    Row(
                        modifier = Modifier.fillMaxWidth().padding(vertical = 12.dp),
                        horizontalArrangement = Arrangement.Center,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        if (discovered.isEmpty()) {
                            CircularProgressIndicator(modifier = Modifier.size(16.dp), strokeWidth = 2.dp)
                            Spacer(Modifier.width(8.dp))
                            Text(
                                "Searching the local network…",
                                style = MaterialTheme.typography.bodyMedium,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                            Spacer(Modifier.width(8.dp))
                        }
                        TextButton(onClick = onRescan) { Text("Scan again") }
                    }
                }
            }

            item(span = { GridItemSpan(maxLineSpan) }) {
                Spacer(Modifier.height(96.dp))
            }
        }

        ExtendedFloatingActionButton(
            onClick = onAddHost,
            icon = { Icon(Icons.Filled.Add, contentDescription = null) },
            text = { Text("Add host") },
            expanded = !connecting,
            modifier = Modifier
                .align(Alignment.BottomEnd)
                .padding(20.dp),
        )
    }
}
