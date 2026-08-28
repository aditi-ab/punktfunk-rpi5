package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pure JVM test of [SessionAccess] — the bit values are an ABI contract with
 * `punktfunk_core::quic::access` (wire == store == this mirror), and the preset labels are the
 * §3.2 naming rule the Access chip renders from: three levels people reason about, "Custom" for
 * anything else, derived from the mask so they cannot drift. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class SessionAccessTest {

    /** Bit-for-bit the core vocabulary — a reorder here would mislabel every session. */
    @Test
    fun `bits mirror punktfunk-core`() {
        assertEquals(1, SessionAccess.GAMEPAD)
        assertEquals(2, SessionAccess.POINTER)
        assertEquals(4, SessionAccess.KEYBOARD)
        assertEquals(8, SessionAccess.CLIPBOARD)
        assertEquals(16, SessionAccess.MIC)
        assertEquals(32, SessionAccess.LAUNCH)
        assertEquals(64, SessionAccess.POWER)
        assertEquals(0x7F, SessionAccess.ALL)
    }

    @Test
    fun `preset labels derive from the mask`() {
        assertEquals("Full control", SessionAccess.label(SessionAccess.ALL))
        assertEquals("Controller only", SessionAccess.label(SessionAccess.GAMEPAD))
        assertEquals("View only", SessionAccess.label(0))
        // Any other combination is Custom — including controller + clipboard, the design's
        // media-remote example.
        assertEquals(
            "Custom",
            SessionAccess.label(SessionAccess.GAMEPAD or SessionAccess.CLIPBOARD),
        )
        assertEquals("Custom", SessionAccess.label(SessionAccess.ALL and SessionAccess.LAUNCH.inv()))
        // The legacy-full read rule (host-actions §4.3): an old host's pre-power "Full control"
        // (exactly 0x3F) still labels Full, never Custom.
        assertEquals("Full control", SessionAccess.label(0x3F))
    }

    @Test
    fun `remaining label is compact and never empty`() {
        assertEquals("1 h 58 m", SessionAccess.remainingLabel(7130))
        assertEquals("2 h", SessionAccess.remainingLabel(7200))
        assertEquals("12 m", SessionAccess.remainingLabel(725))
        assertEquals("45 s", SessionAccess.remainingLabel(45))
        assertEquals("0 s", SessionAccess.remainingLabel(0))
    }
}
