package io.unom.punktfunk

import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.security.KnownHost

/**
 * The console home's tiles, in carousel order: every saved host with its pinned host+profile cards
 * immediately behind it, then the hosts seen on the network but not yet saved, then Add Host.
 *
 * Pure, and deliberately not a composable. The half of this that can be WRONG is the ordering and
 * what a tile claims — a pin drifting away from the host it belongs to, a discovered host offered a
 * second time next to the saved record it already is, a chip naming a profile the press won't
 * actually use. None of that needs a display to be checked, and `HomeTilesTest` checks it without
 * one; the console home itself needs the live JNI core to compose at all.
 *
 * [isOnline] and [pinsFor] arrive as lambdas rather than as the discovery lists and the profile
 * store behind them: "online" means advertising on mDNS OR answering a QUIC probe (the routed
 * Tailscale/VPN case), which is a rule belonging to the screen that does the probing, not to a list
 * builder.
 */
internal fun buildHomeTiles(
    savedHosts: List<KnownHost>,
    /** The live catalog — resolves each host's binding into the name and colour its chip wears. */
    profiles: List<StreamProfile>,
    pinsFor: (KnownHost) -> List<StreamProfile>,
    /** Already de-duped against [savedHosts] by the caller: a saved host is not also "discovered". */
    discoveredUnsaved: List<DiscoveredHost>,
    isOnline: (KnownHost) -> Boolean,
    /**
     * Dial a saved host. The second argument is `connect`'s one-off profile reference: null on a
     * host's own tile (follow whatever the host is bound to), the pinned profile's id on a pin tile.
     */
    onConnect: (KnownHost, String?) -> Unit,
    onConnectDiscovered: (DiscoveredHost) -> Unit,
    onAddHost: () -> Unit,
): List<HomeTile> = buildList {
    savedHosts.forEach { kh ->
        val bound = kh.profileId?.let { id -> profiles.firstOrNull { it.id == id } }
        add(
            HomeTile(
                id = "saved-${kh.id}",
                title = kh.name,
                subtitle = "${kh.address}:${kh.port}",
                filled = true,
                online = isOnline(kh),
                paired = kh.paired,
                knownHost = kh,
                // The binding is what a press will actually do, so the tile says so — the console
                // can't edit profiles, but it must never lie about which one it uses. It rides in
                // the card's own chip now rather than as a "· Name" tail on the address, which is
                // where it read as an afterthought.
                profileName = bound?.name,
                profileAccent = accentColor(bound?.accent),
                activate = { onConnect(kh, null) },
            ),
        )
        // Pinned host+profile combinations, right after their host: one focus-and-press each,
        // which is the affordance a controller surface does well (menus are not).
        pinsFor(kh).forEach { p ->
            add(
                HomeTile(
                    id = "pin-${kh.id}-${p.id}",
                    title = kh.name,
                    // The address, like every other card — the PROFILE is what makes this card
                    // different, and it now says so in the chip instead of standing in for the
                    // subtitle, which left a pin card unable to say where it pointed.
                    subtitle = "${kh.address}:${kh.port}",
                    filled = true,
                    online = isOnline(kh),
                    paired = kh.paired,
                    knownHost = kh,
                    pinnedProfileId = p.id,
                    profileName = p.name,
                    profileAccent = accentColor(p.accent),
                    activate = { onConnect(kh, p.id) },
                ),
            )
        }
    }
    discoveredUnsaved.forEach { dh ->
        add(
            HomeTile(
                id = "disc-${dh.host}:${dh.port}",
                title = dh.name,
                subtitle = "${dh.host}:${dh.port}",
                online = true,
                activate = { onConnectDiscovered(dh) },
            ),
        )
    }
    add(
        HomeTile(
            id = "add",
            title = "Add Host",
            subtitle = "Register a host by address",
            isAdd = true,
            activate = onAddHost,
        ),
    )
}
