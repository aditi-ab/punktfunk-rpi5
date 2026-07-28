package io.unom.punktfunk

import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * The profile model — the part of this feature that is wrong-or-right rather than pretty-or-ugly.
 * A profile is a named bundle of OVERRIDES, not a snapshot: an untouched field keeps following the
 * global live, a touched one is recorded even when it equals today's global (a pin), and the only
 * way back to inheriting is an explicit reset. These tests are the Kotlin twin of the Rust
 * `profiles.rs` suite, so the two can't drift.
 *
 * `sdk = [36]` for the same reason the screenshot tests pin it: Robolectric ships android-all jars
 * only up to API 36 while the app compiles against 37.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class ProfilesTest {
    private val base = Settings(
        width = 1920,
        height = 1080,
        bitrateKbps = 20_000,
        codec = "hevc",
        touchMode = TouchMode.TRACKPAD,
        mouseMode = MouseMode.DESKTOP,
    )

    @Test
    fun overlayAppliesOnlyWhatItOverrides() {
        val empty = SettingsOverlay()
        assertTrue(empty.isEmpty())
        assertEquals(base, empty.apply(base))

        val overlay = SettingsOverlay(
            width = 3840,
            height = 2160,
            hz = 120,
            bitrateKbps = 80_000,
            renderScale = 1.5,
            codec = "av1",
            hdrEnabled = false,
            compositor = 4,
            audioChannels = 6,
            micEnabled = true,
            touchMode = TouchMode.POINTER,
            mouseMode = MouseMode.CAPTURE,
            invertScroll = true,
            gamepad = 6,
            statsVerbosity = StatsVerbosity.DETAILED,
            lowLatencyMode = false,
        )
        assertFalse(overlay.isEmpty())
        val out = overlay.apply(base)
        assertEquals(Triple(3840, 2160, 120), Triple(out.width, out.height, out.hz))
        assertEquals(80_000, out.bitrateKbps)
        assertEquals(1.5, out.renderScale, 0.0)
        assertEquals("av1", out.codec)
        assertFalse(out.hdrEnabled)
        assertEquals(4, out.compositor)
        assertEquals(6, out.audioChannels)
        assertTrue(out.micEnabled)
        assertEquals(TouchMode.POINTER, out.touchMode)
        assertEquals(MouseMode.CAPTURE, out.mouseMode)
        assertTrue(out.invertScroll)
        assertEquals(6, out.gamepad)
        assertEquals(StatsVerbosity.DETAILED, out.statsVerbosity)
        assertFalse(out.lowLatencyMode)

        // Device-scope settings are not in the overlay at all, so no profile can move them.
        assertEquals(base.gamepadUiEnabled, out.gamepadUiEnabled)
        assertEquals(base.libraryEnabled, out.libraryEnabled)
        assertEquals(base.autoWakeEnabled, out.autoWakeEnabled)
        assertEquals(base.sc2Capture, out.sc2Capture)
    }

    @Test
    fun anOverrideEqualToTheGlobalIsAPinThatSurvivesTheGlobalMoving() {
        val pin = SettingsOverlay(bitrateKbps = 20_000) // exactly what `base` says today
        assertFalse(pin.isEmpty())
        assertEquals(20_000, pin.apply(base.copy(bitrateKbps = 50_000)).bitrateKbps)
    }

    @Test
    fun absorbRecordsTheTouchedFieldOnly() {
        var o = SettingsOverlay()

        // One control fires: before = what it was showing, after = what the user picked.
        var before = o.apply(base)
        o = o.absorb(before, before.copy(codec = "av1"))
        assertEquals("av1", o.codec)
        assertNull("nothing else may be recorded", o.bitrateKbps)

        // Setting it BACK to the global's value is still an override — the pin case, and the whole
        // difference between this and diffing against the globals at save time.
        before = o.apply(base)
        o = o.absorb(before, before.copy(codec = "hevc"))
        assertEquals("hevc", o.codec)
        assertEquals("hevc", o.apply(base.copy(codec = "h264")).codec)

        // Identical snapshots record nothing.
        before = o.apply(base)
        assertEquals(o, o.absorb(before, before))
    }

    @Test
    fun clearDropsOneOverride() {
        val o = SettingsOverlay(width = 3840, height = 2160, codec = "av1")
        assertEquals(setOf(SettingsOverlay.FIELD_RESOLUTION, "codec"), o.overridden())
        assertNull(o.clear("codec").codec)
        // Width and height are one control, so they reset together.
        val reset = o.clear(SettingsOverlay.FIELD_RESOLUTION)
        assertNull(reset.width)
        assertNull(reset.height)
        assertEquals(o, o.clear("no_such_field")) // unknown names are a no-op, never a crash
    }

    @Test
    fun catalogRoundTripsAndPreservesWhatItCannotRepresent() {
        val store = ProfileStore(RuntimeEnvironment.getApplication())
        val game = newProfile("Game").copy(
            accent = "#ff8800",
            overrides = SettingsOverlay(
                width = 3840,
                height = 2160,
                hz = 120,
                // A codec string this build's picker can't show is still stored and still applied:
                // the host is the component that decides what it can encode.
                codec = "vvc-from-the-future",
                extra = mapOf("some_new_axis" to 7),
            ),
            extra = mapOf("future_profile_key" to "kept"),
        )
        store.save(game)
        store.save(newProfile("Work"))

        val loaded = store.byId(game.id)!!
        assertEquals("Game", loaded.name)
        assertEquals("#ff8800", loaded.accent)
        assertEquals("vvc-from-the-future", loaded.overrides.codec)
        assertEquals(3840, loaded.overrides.width)
        // The don't-clobber rule: an older build must not erase a newer one's keys by opening it.
        assertEquals(mapOf<String, Any>("some_new_axis" to 7), loaded.overrides.extra)
        assertEquals(mapOf<String, Any>("future_profile_key" to "kept"), loaded.extra)
        assertEquals("vvc-from-the-future", loaded.overrides.apply(base).codec)

        // A profile that overrides nothing is the "inherits everything" one a create starts at.
        assertTrue(store.all().first { it.name == "Work" }.overrides.isEmpty())
        assertEquals(listOf("Game", "Work"), store.all().map { it.name })
    }

    @Test
    fun resolvePrefersIdsAndRefusesAmbiguity() {
        val store = ProfileStore(RuntimeEnvironment.getApplication())
        val work = newProfile("Work")
        val work2 = newProfile("work") // saved directly: the UI's name guard is what prevents this
        val game = newProfile("Game")
        listOf(work, work2, game).forEach(store::save)

        assertEquals(ProfileResolution.FOUND, store.resolve(work.id).second)
        assertEquals(work.id, store.resolve(work.id).first!!.id)
        // Two profiles carry this name — refuse rather than pick whichever came first.
        assertEquals(ProfileResolution.AMBIGUOUS, store.resolve("Work").second)
        assertNull(store.resolve("Work").first)
        assertEquals(game.id, store.resolve("GAME").first!!.id) // names match case-insensitively
        assertEquals(ProfileResolution.NOT_FOUND, store.resolve("nope").second)
        assertEquals(ProfileResolution.NOT_FOUND, store.resolve("").second)

        assertTrue(store.nameTaken("GAME"))
        assertFalse(store.nameTaken("GAME", except = game.id)) // renaming in place is allowed
        assertFalse(store.nameTaken("Travel"))
    }

    @Test
    fun profilePrecedenceIsOneOffThenBindingThenNone() {
        val store = ProfileStore(RuntimeEnvironment.getApplication())
        val work = newProfile("Work")
        val game = newProfile("Game")
        listOf(work, game).forEach(store::save)
        val bound = host().copy(profileId = work.id)

        // A plain tap follows the binding…
        assertEquals(work.id, store.resolveFor(bound, oneOff = null)!!.id)
        // …a one-off wins over it, by id or by unique name, and never rebinds anything…
        assertEquals(game.id, store.resolveFor(bound, oneOff = game.id)!!.id)
        assertEquals(game.id, store.resolveFor(bound, oneOff = "game")!!.id)
        assertEquals(work.id, store.resolveFor(bound, oneOff = null)!!.id)
        // …and the empty reference is a real choice — "force the global defaults" — not "unset".
        assertNull(store.resolveFor(bound, oneOff = ""))
        // An unbound host is today's behaviour: the globals.
        assertNull(store.resolveFor(host(), oneOff = null))
        assertNull(store.resolveFor(null, oneOff = null))
    }

    @Test
    fun aDeletedProfileLeavesNoErrorBehind() {
        val store = ProfileStore(RuntimeEnvironment.getApplication())
        val work = newProfile("Work")
        store.save(work)
        val h = host().copy(profileId = work.id, pinnedProfileIds = listOf(work.id, work.id))
        assertEquals(1, store.pinsFor(h).size) // a duplicate pin is one card, not two

        store.delete(work.id)
        // A dangling binding resolves as "no profile" — never an error, never a blocked connect —
        // and its pinned card simply stops rendering.
        assertNull(store.resolveFor(h, oneOff = null))
        assertTrue(store.pinsFor(h).isEmpty())
        assertEquals(base, base.effectiveFor(store.resolveFor(h, oneOff = null)))
    }

    @Test
    fun mintedIdsAreWellFormed() {
        val id = newProfileId()
        assertEquals(12, id.length)
        assertTrue(id.all { it.isDigit() || it in 'a'..'f' })
        assertNotEquals(id, newProfileId())
    }

    private fun host() = KnownHost("192.168.1.42", 9777, "Desk", "a".repeat(64), paired = true)
}
