package io.unom.punktfunk.kit

import android.view.KeyEvent
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pure JVM test of [Gamepad.padButtonBit] — the streaming branch's gamepad keycode resolution
 * (`KeyEvent`'s keycode/flag constants are compile-time-inlined ints, so no Android runtime is
 * involved). Run: `./gradlew :kit:testDebugUnitTest`.
 *
 * The regression it pins is a field report: one press of Select disconnected the session. Plenty
 * of pads deliver that button as the plain `KEYCODE_BACK` a remote uses, with no `BUTTON_SELECT`
 * scancode behind it — so it mapped to nothing, fell out of the gamepad branch unconsumed, and
 * reached the activity back stack, which is the deliberate-quit exit. The same gap made
 * [Gamepad.BTN_BACK] unreachable on those pads, and with it every shortcut built on Select: the
 * exit chord `StreamScreen`'s own start banner advertises, the mic mute, the stats tier.
 *
 * Which controller the report came from is not knowable from the logs and does not matter:
 * `KEYCODE_BACK` is the only keycode that reaches the back stack from a SOURCE_GAMEPAD device, so
 * a one-press quit identifies the button's keycode on its own.
 */
class PadButtonBitTest {

    /** The report: Select on an Android-TV pad arrives as BACK and must be the Select bit. */
    @Test
    fun `a pad's BACK is its Select button`() {
        assertEquals(Gamepad.BTN_BACK, Gamepad.padButtonBit(KeyEvent.KEYCODE_BACK, 0))
        // Same bit either spelling reaches us by — a pad that DOES carry BUTTON_SELECT is unchanged.
        assertEquals(
            Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_SELECT, 0),
            Gamepad.padButtonBit(KeyEvent.KEYCODE_BACK, 0),
        )
    }

    /**
     * With Select mapped, the three Select chords are reachable on a pad that has only a BACK
     * keycode — which is the whole point of the mapping, not a side effect of it. Held-state
     * assembly is [GamepadRouter]'s (see `GamepadChordTest`); what is pinned here is that the
     * bits a SHIELD can actually produce cover each chord.
     */
    @Test
    fun `the Select chords are reachable from a BACK-only pad`() {
        val select = Gamepad.padButtonBit(KeyEvent.KEYCODE_BACK, 0)
        val start = Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_START, 0)
        val l1 = Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_L1, 0)
        val r1 = Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_R1, 0)
        val x = Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_X, 0)
        val y = Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_Y, 0)
        assertEquals(GamepadRouter.EXIT_CHORD, select or start or l1 or r1)
        assertEquals(GamepadRouter.STATS_CHORD, select or x)
        assertEquals(GamepadRouter.MIC_CHORD, select or y)
    }

    /**
     * The synthetic BACK the framework raises after an unconsumed `BUTTON_*` press is not a button
     * anyone touched — forwarding it would put a phantom Select on the wire, and one of those
     * landing while Start + L1 + R1 were held would complete the exit chord out of nowhere.
     */
    @Test
    fun `a fallback BACK is not a button press`() {
        assertEquals(0, Gamepad.padButtonBit(KeyEvent.KEYCODE_BACK, KeyEvent.FLAG_FALLBACK))
        // Only BACK is filtered on the flag; a real button keeps its bit whatever rides alongside.
        assertEquals(
            Gamepad.BTN_A,
            Gamepad.padButtonBit(KeyEvent.KEYCODE_BUTTON_A, KeyEvent.FLAG_FALLBACK),
        )
    }

    /** Everything else is [Gamepad.buttonBit] verbatim — BACK is the only row this adds. */
    @Test
    fun `every other keycode is unchanged`() {
        for (code in 0..0x400) {
            if (code == KeyEvent.KEYCODE_BACK) continue
            assertEquals(Gamepad.buttonBit(code), Gamepad.padButtonBit(code, 0))
        }
        // And BACK is genuinely a new row, not one buttonBit already had.
        assertEquals(0, Gamepad.buttonBit(KeyEvent.KEYCODE_BACK))
    }
}
