package io.unom.punktfunk

import androidx.compose.ui.graphics.Color
import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The console home's tile list ([buildHomeTiles]). Pure JVM — the carousel itself needs the live
 * JNI core to compose, so its ORDER and what each tile claims had no cover at all until now, and
 * both are exactly the kind of thing that survives a refactor looking fine and behaving wrong.
 *
 * Run: `./gradlew :app:testDebugUnitTest --tests 'io.unom.punktfunk.HomeTilesTest'`.
 */
class HomeTilesTest {
    private fun host(
        name: String,
        address: String,
        fp: String = "",
        profileId: String? = null,
        pins: List<String> = emptyList(),
    ) = KnownHost(
        address = address,
        port = 9777,
        name = name,
        fpHex = fp,
        paired = true,
        id = "id-$name",
        profileId = profileId,
        pinnedProfileIds = pins,
    )

    private fun advert(name: String, address: String, fp: String? = null) = DiscoveredHost(
        key = "$address:9777",
        name = name,
        host = address,
        port = 9777,
        fingerprint = fp,
    )

    private val work = StreamProfile(id = "p-work", name = "Work", accent = "#3B82F6")
    private val travel = StreamProfile(id = "p-travel", name = "Travel")

    /** The builder with nothing plugged in — every list empty, every callback a no-op. */
    private fun tiles(
        savedHosts: List<KnownHost> = emptyList(),
        profiles: List<StreamProfile> = emptyList(),
        pins: Map<String, List<StreamProfile>> = emptyMap(),
        discoveredUnsaved: List<DiscoveredHost> = emptyList(),
        online: Set<String> = emptySet(),
        onConnect: (KnownHost, String?) -> Unit = { _, _ -> },
        onConnectDiscovered: (DiscoveredHost) -> Unit = {},
        onAddHost: () -> Unit = {},
    ) = buildHomeTiles(
        savedHosts = savedHosts,
        profiles = profiles,
        pinsFor = { kh -> pins[kh.id].orEmpty() },
        discoveredUnsaved = discoveredUnsaved,
        isOnline = { it.name in online },
        onConnect = onConnect,
        onConnectDiscovered = onConnectDiscovered,
        onAddHost = onAddHost,
    )

    /**
     * A pin belongs to the host above it. Ordering is the whole affordance: on a controller a pin is
     * reached by walking one tile past its host, and a builder that grouped all the pins at the end
     * would still LOOK right in a screenshot of any single tile.
     */
    @Test
    fun pinnedCardsFollowTheirOwnHost() {
        val living = host("living", "192.168.1.42", pins = listOf(work.id, travel.id))
        val studio = host("studio", "192.168.1.61", pins = listOf(work.id))
        val ids = tiles(
            savedHosts = listOf(living, studio),
            profiles = listOf(work, travel),
            pins = mapOf(living.id to listOf(work, travel), studio.id to listOf(work)),
        ).map { it.id }
        assertEquals(
            listOf(
                "saved-id-living",
                "pin-id-living-p-work",
                "pin-id-living-p-travel",
                "saved-id-studio",
                "pin-id-studio-p-work",
                "add",
            ),
            ids,
        )
    }

    /** Add Host is the last tile, always — including on a device with nothing saved or seen. */
    @Test
    fun theAddTileIsAlwaysLast() {
        val empty = tiles()
        assertEquals(listOf("add"), empty.map { it.id })
        assertTrue(empty.single().isAdd)

        val populated = tiles(
            savedHosts = listOf(host("living", "192.168.1.42")),
            discoveredUnsaved = listOf(advert("studio", "192.168.1.61")),
        )
        assertEquals(listOf("saved-id-living", "disc-192.168.1.61:9777", "add"), populated.map { it.id })
        assertTrue(populated.last().isAdd)
        // The Add tile is not a host: no library, no options menu, nothing to wake.
        assertNull(populated.last().knownHost)
    }

    /**
     * A host that is both saved and advertising appears ONCE. The de-dupe is the caller's
     * ([KnownHost.matches], which the screen applies before handing the list over) — checked here
     * because the rule that matters is the fingerprint one: a host that came back on a new DHCP
     * address is the same machine, and matching on address alone would offer it a second time as a
     * stranger, next to the record that already holds its trust.
     */
    @Test
    fun aSavedHostSeenOnTheNetworkIsNotListedTwice() {
        val fp = "ab12cd34"
        val living = host("living", "192.168.1.42", fp = fp)
        // Same host, new address after a cold boot, plus a genuine stranger.
        val adverts = listOf(advert("living", "192.168.1.77", fp = fp), advert("stranger", "192.168.1.99"))
        val unsaved = adverts.filter { dh -> listOf(living).none { it.matches(dh) } }
        val ids = tiles(savedHosts = listOf(living), discoveredUnsaved = unsaved).map { it.id }
        assertEquals(listOf("saved-id-living", "disc-192.168.1.99:9777", "add"), ids)
    }

    /**
     * The chip says which profile a press will connect with — the host's binding on its own tile,
     * the pinned profile on a pin tile. The console cannot EDIT profiles, so this claim is the only
     * thing standing between a user and a stream with settings they didn't choose.
     */
    @Test
    fun theChipNamesTheProfileThePressWillUse() {
        val living = host("living", "192.168.1.42", profileId = work.id, pins = listOf(travel.id))
        val result = tiles(
            savedHosts = listOf(living),
            profiles = listOf(work, travel),
            pins = mapOf(living.id to listOf(travel)),
        )
        val own = result[0]
        assertEquals("Work", own.profileName)
        assertEquals(Color(0xFF3B82F6), own.profileAccent)
        assertNull(own.pinnedProfileId)

        val pin = result[1]
        assertEquals("Travel", pin.profileName)
        assertEquals(travel.id, pin.pinnedProfileId)
        // Travel set no accent: a chip with no colour, not a crash and not a stray default.
        assertNull(pin.profileAccent)

        // A binding whose profile was deleted resolves to nothing — the tile stays silent rather
        // than naming an id that resolves to nobody.
        val dangling = tiles(savedHosts = listOf(host("ghost", "10.0.0.5", profileId = "p-gone")))
        assertNull(dangling[0].profileName)
    }

    /** Both address and the subtitle: a pin card says where it points, like every other card. */
    @Test
    fun everySavedTileSaysWhereItPoints() {
        val living = host("living", "192.168.1.42", pins = listOf(work.id))
        val result = tiles(
            savedHosts = listOf(living),
            profiles = listOf(work),
            pins = mapOf(living.id to listOf(work)),
            online = setOf("living"),
        )
        result.take(2).forEach {
            assertEquals("192.168.1.42:9777", it.subtitle)
            assertEquals("living", it.title)
            assertTrue(it.filled)
            assertTrue(it.online)
            assertTrue(it.paired)
            assertNotNull(it.knownHost)
        }
        // Both tiles reach the library (Y): a pin card opens its OWN shelf, whose launches carry
        // the pinned profile — the library is a way to start a card, not a host-level action.
        assertTrue(result[0].hasLibrary)
        assertTrue(result[1].hasLibrary)
    }

    /**
     * What a press DOES. A host's own tile dials with no one-off reference so the host's binding is
     * followed; a pin tile forces its own profile. Passing the pin's id as the binding (or the
     * other way round) is invisible until someone streams at the wrong bitrate.
     */
    @Test
    fun activationCarriesTheRightProfileReference() {
        val living = host("living", "192.168.1.42", pins = listOf(work.id))
        val dialled = mutableListOf<Pair<String, String?>>()
        val discovered = mutableListOf<String>()
        var addOpened = false
        val result = tiles(
            savedHosts = listOf(living),
            profiles = listOf(work),
            pins = mapOf(living.id to listOf(work)),
            discoveredUnsaved = listOf(advert("stranger", "192.168.1.99")),
            onConnect = { kh, oneOff -> dialled += kh.name to oneOff },
            onConnectDiscovered = { dh -> discovered += dh.host },
            onAddHost = { addOpened = true },
        )
        result.forEach { it.activate() }
        assertEquals(listOf("living" to null, "living" to work.id), dialled)
        assertEquals(listOf("192.168.1.99"), discovered)
        assertTrue(addOpened)
    }
}
