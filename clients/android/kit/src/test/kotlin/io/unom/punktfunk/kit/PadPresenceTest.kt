package io.unom.punktfunk.kit

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The truth table behind "is a controller attached" — the question the console UI's
 * "With a controller" mode is answered by. A false positive here is not cosmetic: it pins the
 * console UI on with no pad in the room, and no setting short of turning the whole thing off can
 * dismiss it, because the phantom pad never disconnects.
 */
class PadPresenceTest {

    /** A real pad: the source class plus hardware behind it, in either of the two shapes. */
    @Test
    fun realPadsCount() {
        assertTrue(
            Gamepad.looksLikeController(
                padSource = true, virtual = false, hasStick = true, hasFaceButtons = true,
            ),
        )
        // An arcade stick / d-pad-only pad — buttons, no analog stick.
        assertTrue(
            Gamepad.looksLikeController(
                padSource = true, virtual = false, hasStick = false, hasFaceButtons = true,
            ),
        )
        // A wheel or flight stick — axes, no A/B.
        assertTrue(
            Gamepad.looksLikeController(
                padSource = true, virtual = false, hasStick = true, hasFaceButtons = false,
            ),
        )
    }

    /** The gaming-phone shoulder triggers and OEM game-mode overlays: a virtual device wearing the
     * gamepad source class. This is the field report — the console UI that could not be dismissed. */
    @Test
    fun virtualDevicesAreNotControllers() {
        assertFalse(
            Gamepad.looksLikeController(
                padSource = true, virtual = true, hasStick = true, hasFaceButtons = true,
            ),
        )
    }

    /** A device that claims a pad source with nothing behind it is not a pad either. */
    @Test
    fun aSourceClaimWithoutHardwareIsNotAController() {
        assertFalse(
            Gamepad.looksLikeController(
                padSource = true, virtual = false, hasStick = false, hasFaceButtons = false,
            ),
        )
    }

    /** And a keyboard/mouse with sticks it never reports on the joystick source stays out. */
    @Test
    fun nonPadSourcesNeverCount() {
        assertFalse(
            Gamepad.looksLikeController(
                padSource = false, virtual = false, hasStick = true, hasFaceButtons = true,
            ),
        )
    }
}
