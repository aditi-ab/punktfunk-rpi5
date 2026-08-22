package io.unom.punktfunk.kit

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The router's instant button chords — mic mute (Select + Y) and the stats-tier cycle
 * (Select + X) — pinned at the one place a JVM test can reach them: [GamepadRouter.completesChord],
 * the shared edge rule both fire on. A [GamepadRouter] itself needs an InputManager, a main Looper
 * and live InputDevices behind it, so driving real KeyEvents through it is not a unit test; the
 * rule below is the whole of what those two `if`s decide.
 *
 * The Apple client pins the same chords from its own side (`GamepadStatsChordTests`), and the two
 * suites exist for the same reason: a chord that stops completing fails INVISIBLY — the buttons
 * still reach the game, nothing logs, and the couch simply finds that a shortcut it was told about
 * does nothing. On a TV the stats chord is the only route to the overlay at all.
 */
class GamepadChordTest {

    /** The two chords `slotButton` tests on every press, in the order it tests them. */
    private val instantChords = listOf(GamepadRouter.MIC_CHORD, GamepadRouter.STATS_CHORD)

    /**
     * One pad's held-button set, driven exactly the way `slotButton` drives a slot's: the chord
     * test reads the state from BEFORE the press, then the bit joins `held`. [press] returns the
     * chords that completed on it — an empty list means the press was silent.
     */
    private inner class Pad {
        var held = 0
            private set

        fun press(bit: Int): List<Int> {
            val wasHeld = held
            held = held or bit
            return instantChords.filter { GamepadRouter.completesChord(wasHeld, bit, it) }
        }

        /** An auto-repeat DOWN: Android re-delivers a held button, so `wasHeld` already has it. */
        fun repeat(bit: Int): List<Int> = press(bit)

        fun release(bit: Int) {
            held = held and bit.inv()
        }
    }

    /**
     * Select + X, the same pair as the Apple client's `GamepadCapture.statsChord`
     * (`GamepadWire.back | GamepadWire.x`). A per-platform shortcut is worse than none, so the
     * literal bits are spelled out here rather than derived from the constant under test.
     */
    @Test
    fun `the stats chord is Select plus X`() {
        assertEquals(0x0020 or 0x4000, GamepadRouter.STATS_CHORD)
        assertEquals(Gamepad.BTN_BACK or Gamepad.BTN_X, GamepadRouter.STATS_CHORD)
        assertEquals(Gamepad.BTN_BACK or Gamepad.BTN_Y, GamepadRouter.MIC_CHORD)
    }

    /**
     * The three chords must not be reachable through one another: pressing toward the exit chord
     * may not cycle the overlay or mute the mic on the way, and neither instant chord may arm a
     * disconnect. Select is the one button they share by design — everything else is disjoint, and
     * no chord is a subset of another (a subset would complete whenever its superset did).
     */
    @Test
    fun `the chords meet only on Select`() {
        val chords = mapOf(
            "exit" to GamepadRouter.EXIT_CHORD,
            "mic" to GamepadRouter.MIC_CHORD,
            "stats" to GamepadRouter.STATS_CHORD,
        )
        for ((aName, a) in chords) {
            for ((bName, b) in chords) {
                if (aName == bName) continue
                assertEquals("$aName and $bName share a button other than Select", Gamepad.BTN_BACK, a and b)
                assertNotEquals("$aName is a subset of $bName", a and b, a)
                assertNotEquals("$bName is a subset of $aName", a and b, b)
            }
        }
    }

    /**
     * One cycle per chord, not one per press: the completing button fires it, an auto-repeat of
     * that same button does not, and a third button pressed on top of the held chord finds the mask
     * already complete.
     */
    @Test
    fun `the chord fires once, on the button that completes it`() {
        val pad = Pad()
        assertEquals("Select alone is not a chord", emptyList<Int>(), pad.press(Gamepad.BTN_BACK))
        assertEquals(listOf(GamepadRouter.STATS_CHORD), pad.press(Gamepad.BTN_X))
        assertEquals("auto-repeat re-fired the chord", emptyList<Int>(), pad.repeat(Gamepad.BTN_X))
        assertEquals("a press on top re-fired the chord", emptyList<Int>(), pad.press(Gamepad.BTN_A))
        assertEquals(emptyList<Int>(), pad.press(Gamepad.BTN_B))
    }

    /**
     * Lifting either member re-arms the chord — pressing it again is a fresh completion. Both
     * directions matter: a couch user cycling tiers taps X with Select still down, and one who
     * lifted Select instead taps Select again with X still down.
     */
    @Test
    fun `either member re-arms the chord when released`() {
        val pad = Pad()
        pad.press(Gamepad.BTN_BACK)
        assertEquals(listOf(GamepadRouter.STATS_CHORD), pad.press(Gamepad.BTN_X))
        pad.release(Gamepad.BTN_X)
        assertEquals(listOf(GamepadRouter.STATS_CHORD), pad.press(Gamepad.BTN_X))
        pad.release(Gamepad.BTN_BACK)
        assertEquals(listOf(GamepadRouter.STATS_CHORD), pad.press(Gamepad.BTN_BACK))
    }

    /** A partial mask fires nothing — either member alone, or with a non-member alongside it. */
    @Test
    fun `a partial chord never fires`() {
        for (opening in listOf(Gamepad.BTN_BACK, Gamepad.BTN_X, Gamepad.BTN_Y)) {
            val pad = Pad()
            assertEquals(emptyList<Int>(), pad.press(opening))
            for (other in listOf(Gamepad.BTN_A, Gamepad.BTN_B, Gamepad.BTN_LB, Gamepad.BTN_DPAD_UP)) {
                assertEquals(emptyList<Int>(), pad.press(other))
            }
        }
    }

    /**
     * Walking into the exit chord (Select + Start + L1 + R1, in any order) must pass through
     * neither instant chord: the disconnect hold is the one gesture where a stray mute or a
     * changed overlay would land while the user is looking at the "hold to quit" hint.
     */
    @Test
    fun `reaching the exit chord fires nothing on the way`() {
        val exit = listOf(Gamepad.BTN_BACK, Gamepad.BTN_START, Gamepad.BTN_LB, Gamepad.BTN_RB)
        for (order in exit.permutations()) {
            val pad = Pad()
            for (bit in order) {
                assertEquals("$order fired a chord at $bit", emptyList<Int>(), pad.press(bit))
            }
            assertEquals(GamepadRouter.EXIT_CHORD, pad.held)
        }
    }

    /**
     * X and Y held, then Select: ONE press completes BOTH chords. That is the honest reading of
     * "the button that completes the mask", it is what the Apple client does too, and the
     * alternative — first match wins — would make the same press mean different things depending
     * on which chord the router happened to test first. Pinned so the behaviour is a decision
     * rather than a surprise; both outcomes are visible and reversible on screen.
     */
    @Test
    fun `a shared Select can complete both chords at once`() {
        val pad = Pad()
        pad.press(Gamepad.BTN_X)
        pad.press(Gamepad.BTN_Y)
        assertEquals(instantChords, pad.press(Gamepad.BTN_BACK))
    }

    /**
     * A pad's own mute button (a DualSense's) is a second trigger for the mic toggle, and
     * `slotButton` reads it through the SAME edge rule expressed as a one-button chord.
     *
     * That is not decoration. `onButton` deliberately still calls `slotButton(down = true)` on
     * auto-repeat and suppresses only the wire send (its repeatCount guard), so a plain
     * `bit == BTN_MISC1` would toggle the mic on every repeat — hold the button and the mic
     * flaps. `completesChord` against a single-bit mask is exactly "a fresh press of it".
     *
     * The other half is which buttons must NOT reach it. `0x13e` is R3 on every pad but a
     * driverless Sony one, so a mapping that leaked touchpad/mute meanings outside
     * [Gamepad.PadButtons.GENERIC_SONY] would put the mic toggle on every R3 press in the house.
     *
     * `slotButton` ANDs this rule with `Slot.hasMuteButton`, because BTN_MISC1 is the wire's
     * misc/QAM bit and a Steam Controller 2's QAM button rides it too. That term needs a live
     * `Slot`, which needs an InputManager and a main Looper, so it is out of reach from here —
     * the edge rule below is the half a unit test can hold.
     */
    @Test
    fun `the mute button toggles the mic once per press`() {
        fun fires(wasHeld: Int, bit: Int) =
            GamepadRouter.completesChord(wasHeld, bit, Gamepad.BTN_MISC1)

        assertTrue("a fresh press must toggle", fires(0, Gamepad.BTN_MISC1))
        assertFalse("auto-repeat re-fired the toggle", fires(Gamepad.BTN_MISC1, Gamepad.BTN_MISC1))
        assertTrue(
            "a press while other buttons are held is still a fresh press",
            fires(Gamepad.BTN_A or Gamepad.BTN_BACK, Gamepad.BTN_MISC1),
        )
        for (other in listOf(
            Gamepad.BTN_A, Gamepad.BTN_X, Gamepad.BTN_Y, Gamepad.BTN_BACK,
            Gamepad.BTN_LS_CLICK, Gamepad.BTN_RS_CLICK, Gamepad.BTN_GUIDE, Gamepad.BTN_TOUCHPAD,
        )) {
            assertFalse("$other toggled the mic", fires(0, other))
            assertFalse("$other toggled the mic under a held mute", fires(Gamepad.BTN_MISC1, other))
        }
    }

    /** The chord bits are the wire's, so they must stay inside the 32-bit button mask. */
    @Test
    fun `chord masks are wire button bits`() {
        for (chord in instantChords + GamepadRouter.EXIT_CHORD) {
            assertTrue("chord $chord has no bits", chord != 0)
            assertEquals("a chord bit is not a known BTN_*", chord, chord and ALL_BUTTONS)
        }
    }

    private companion object {
        /** Every button bit `Gamepad` defines — the universe a chord may draw from. */
        val ALL_BUTTONS = listOf(
            Gamepad.BTN_DPAD_UP, Gamepad.BTN_DPAD_DOWN, Gamepad.BTN_DPAD_LEFT, Gamepad.BTN_DPAD_RIGHT,
            Gamepad.BTN_START, Gamepad.BTN_BACK, Gamepad.BTN_LS_CLICK, Gamepad.BTN_RS_CLICK,
            Gamepad.BTN_LB, Gamepad.BTN_RB, Gamepad.BTN_GUIDE,
            Gamepad.BTN_A, Gamepad.BTN_B, Gamepad.BTN_X, Gamepad.BTN_Y,
            Gamepad.BTN_PADDLE1, Gamepad.BTN_PADDLE2, Gamepad.BTN_PADDLE3, Gamepad.BTN_PADDLE4,
            Gamepad.BTN_TOUCHPAD, Gamepad.BTN_MISC1,
        ).fold(0) { acc, bit -> acc or bit }

        /** Every ordering of a chord's buttons — presses arrive in whatever order the hands do. */
        fun <T> List<T>.permutations(): List<List<T>> =
            if (size <= 1) listOf(this)
            else flatMap { head -> (this - head).permutations().map { listOf(head) + it } }
    }
}
