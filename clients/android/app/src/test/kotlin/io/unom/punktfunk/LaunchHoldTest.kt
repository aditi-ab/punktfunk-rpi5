package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [launchGaveUp] — which of the hold's exits gets a message (#1072).
 * Run: `./gradlew -PexcludeScreenshots :app:testDebugUnitTest`.
 */
class LaunchHoldTest {
    @Test
    fun aLaunchStillComingUpIsNeverGivenUpOn() {
        // The host lists nothing yet: a launch resolving is worth 15 seconds.
        assertNull(launchGaveUp("Quail", null, 0.0))
        assertNull(launchGaveUp("Quail", null, 14.9))
        // A cold Steam boot with shader work is `launching` for minutes.
        assertNull(launchGaveUp("Quail", "launching", 119.0))
        // Launches that worked: the caller shows the stream, with nothing to say.
        assertNull(launchGaveUp("Quail", "running", 300.0))
        assertNull(launchGaveUp("Quail", "untracked", 300.0))
        assertNull(launchGaveUp("Quail", "grace", 300.0))
        // A word the host adds later is not a failure either.
        assertNull(launchGaveUp("Quail", "window", 300.0))
    }

    @Test
    fun aRunningGameHoldsOnlyWhileItsWindowIsStillToCome() {
        assertTrue(launchStillHeld("running", awaitingWindow = true, elapsed = 60.0))
        assertFalse(launchStillHeld("running", awaitingWindow = false, elapsed = 1.0))
        assertFalse(launchStillHeld("window", awaitingWindow = false, elapsed = 1.0))
        // No window by the cap: show the stream rather than hold forever.
        assertFalse(launchStillHeld("running", awaitingWindow = true, elapsed = 120.0))
    }

    @Test
    fun eachGivingUpExitSaysWhatDidNotHappen() {
        assertEquals(
            "The host didn't start Quail — nothing is running for it.",
            launchGaveUp("Quail", null, 15.0),
        )
        assertEquals(
            "Quail is still starting after 2 minutes.",
            launchGaveUp("Quail", "launching", 120.0),
        )
        // Straight away: the host un-launches the record the moment it sees this.
        assertEquals(
            "Quail closed right after starting.",
            launchGaveUp("Quail", "exited", 0.4),
        )
    }
}
