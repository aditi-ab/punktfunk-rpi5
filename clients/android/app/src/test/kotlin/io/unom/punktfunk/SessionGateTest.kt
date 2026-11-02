package io.unom.punktfunk

import io.unom.punktfunk.models.ActiveSession
import org.junit.After
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of [SessionGate] — the guard that makes one launch produce one session (#1068).
 * Run: `./gradlew -PexcludeScreenshots :app:testDebugUnitTest`.
 */
class SessionGateTest {
    @After
    fun reset() {
        endLive()
        SessionGate.release()
    }

    @Test
    fun oneDialAtATime() {
        assertTrue(SessionGate.take())
        // The console's event pump raising a second Launch while the first is still on the host.
        assertFalse(SessionGate.take())
        SessionGate.release()
        assertTrue(SessionGate.take())
    }

    @Test
    fun noDialBehindALiveSession() {
        SessionGate.owner.open(session("host-1"))
        assertFalse(SessionGate.take())
        // Another host is refused too: this app shows one stream, so a second dial replaces the
        // session on screen and strands the first handle on the host.
        assertFalse(SessionGate.take())
        endLive()
        assertTrue(SessionGate.take())
    }

    @Test
    fun aRefusedDialNeverTakesTheGate() {
        SessionGate.owner.open(session(null))
        assertFalse(SessionGate.take())
        endLive()
        // A refusal that had claimed the flag would wedge every later dial.
        assertTrue(SessionGate.take())
    }

    private fun session(hostId: String?) =
        ActiveSession(HANDLE, Settings(), clipboardSync = false, hostId = hostId)

    // Ends it with a screen on it, so nothing closes: the JVM has no native library.
    private fun endLive() {
        SessionGate.owner.attach(HANDLE)
        SessionGate.owner.end(HANDLE)
    }

    private companion object {
        const val HANDLE = 7L
    }
}
