package io.unom.punktfunk

import io.unom.punktfunk.kit.security.KnownHost
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config

/**
 * Where a measured bitrate lands. The measurement itself is the host's job; the decision this code
 * makes is which layer to write — and the long-standing wrong answer (always the global) is exactly
 * what made measuring the slow box downstairs re-tune the desktop too
 * (design/client-settings-profiles.md §5.3).
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class SpeedTestTest {
    private val store get() = ProfileStore(RuntimeEnvironment.getApplication())
    private fun host() = KnownHost("192.168.1.42", 9777, "Desk", "a".repeat(64), paired = true)

    @Test
    fun anUnboundHostTargetsTheGlobalDefault() {
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(host(), null, store))
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(null, null, store))
    }

    @Test
    fun aProfileThatSetsBitrateIsTheLayerThatHostReads() {
        val s = store
        val game = newProfile("Game").copy(overrides = SettingsOverlay(bitrateKbps = 50_000))
        s.save(game)
        val target = SpeedTestTarget.resolve(host().copy(profileId = game.id), null, s)
        assertEquals(game.id, (target as SpeedTestTarget.Profile).profile.id)
    }

    @Test
    fun aProfileThatInheritsBitrateAsksWhichLayer() {
        val s = store
        val work = newProfile("Work") // overrides nothing
        s.save(work)
        val target = SpeedTestTarget.resolve(host().copy(profileId = work.id), null, s)
        // Both layers are defensible here, so the user picks — we don't guess.
        assertEquals(work.id, (target as SpeedTestTarget.Ask).profile.id)
    }

    @Test
    fun theOneOffPickWinsAndTheEmptyOneForcesTheDefaults() {
        val s = store
        val game = newProfile("Game").copy(overrides = SettingsOverlay(bitrateKbps = 50_000))
        val work = newProfile("Work")
        listOf(game, work).forEach(s::save)
        val bound = host().copy(profileId = work.id)

        // Testing from a pinned card measures — and writes — that card's profile.
        assertEquals(game.id, (SpeedTestTarget.resolve(bound, game.id, s) as SpeedTestTarget.Profile).profile.id)
        // "Connect with: Default settings" is a real choice, so its speed test targets the global.
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(bound, "", s))
        // A dangling binding resolves as no profile everywhere else; here too.
        assertEquals(SpeedTestTarget.Global, SpeedTestTarget.resolve(host().copy(profileId = "gone"), null, s))
    }

    @Test
    fun applyingWritesOnlyTheBitrate_andOnlyToTheChosenLayer() {
        val s = store
        val game = newProfile("Game").copy(
            overrides = SettingsOverlay(bitrateKbps = 50_000, width = 3840, height = 2160),
        )
        s.save(game)
        val globals = Settings(bitrateKbps = 20_000, codec = "hevc")
        var savedGlobals: Settings? = null

        val where = applySpeedTestResult(
            kbps = 84_000,
            target = SpeedTestTarget.Profile(game),
            toProfile = true,
            profiles = s,
            settings = globals,
            onGlobalChange = { savedGlobals = it },
        )
        assertEquals("“Game”", where)
        assertNull("the global must not move when a profile was the target", savedGlobals)
        val after = s.byId(game.id)!!.overrides
        assertEquals(84_000, after.bitrateKbps)
        // Nothing else in the overlay is a speed test's business.
        assertEquals(3840, after.width)
        assertEquals(2160, after.height)
    }

    @Test
    fun theAskCaseHonoursWhichButtonWasPressed() {
        val s = store
        val work = newProfile("Work")
        s.save(work)
        val globals = Settings(bitrateKbps = 20_000)
        var savedGlobals: Settings? = null

        // "Set as default" writes the global and leaves the profile inheriting.
        val whereGlobal = applySpeedTestResult(
            42_000, SpeedTestTarget.Ask(work), toProfile = false, profiles = s,
            settings = globals, onGlobalChange = { savedGlobals = it },
        )
        assertEquals("the default bitrate", whereGlobal)
        assertEquals(42_000, savedGlobals!!.bitrateKbps)
        assertNull(s.byId(work.id)!!.overrides.bitrateKbps)

        // "Set in Work" records the override instead — and now that profile stops inheriting.
        savedGlobals = null
        val whereProfile = applySpeedTestResult(
            42_000, SpeedTestTarget.Ask(work), toProfile = true, profiles = s,
            settings = globals, onGlobalChange = { savedGlobals = it },
        )
        assertEquals("“Work”", whereProfile)
        assertNull(savedGlobals)
        assertEquals(42_000, s.byId(work.id)!!.overrides.bitrateKbps)
    }

    @Test
    fun theRecommendationLeavesHeadroom() {
        // 70 % of measured, in the desktop clients' integer order — a stream needs room for the
        // FEC overhead and for the loss a burst measurement doesn't see.
        val done = SpeedTestPhase.Done(throughputKbps = 100_000, lossPct = 0.4, recommendedKbps = 100_000 / 10 * 7)
        assertEquals(70_000, done.recommendedKbps)
        assertEquals(100.0, done.measuredMbps, 0.001)
        assertEquals(70.0, done.recommendedMbps, 0.001)
        assertTrue(done.recommendedKbps < done.throughputKbps)
    }
}
