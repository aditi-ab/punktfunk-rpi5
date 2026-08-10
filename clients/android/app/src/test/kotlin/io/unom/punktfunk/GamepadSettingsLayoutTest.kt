package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.getBoundsInRoot
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.unit.Dp
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The console settings list must not MOVE under the cursor. This is the regression net for the
 * layout instability the visual refresh fixed, and it needs the real Compose runtime because the
 * bug was entirely a layout one — every value the model held was correct throughout.
 *
 * What used to happen: the focused row unfolded its description in place
 * (`AnimatedVisibility` + `expandVertically`), so every step of the cursor shrank one row and grew
 * another and shifted every row below the focus point — on a list that is simultaneously being
 * scrolled to keep the focused row visible, whose target therefore moved mid-animation. The
 * description now renders in the screen's floating `ConsoleDetailBand`, which is an overlay and
 * cannot displace anything. Sideways, the value's `AnimatedContent` animated its own WIDTH on every
 * step, walking the ‹ chevron and the label's right edge back and forth.
 *
 * Focus is moved by TAP here rather than by pad: the pad path needs a `MainActivity` for its input
 * probes, and the screen routes both to the same `focus` state — the geometry under test is the
 * same either way.
 *
 * `sdk = [36]` for the reason every Robolectric test here pins it: android-all jars stop at 36
 * while the app compiles against 37.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class GamepadSettingsLayoutTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private fun settings() {
        compose.setContent {
            GamepadSettingsScreen(initial = Settings(), onChange = {}, onBack = {})
        }
    }

    /** A Dp compared at hairline tolerance — a rounding difference is not a layout shift. */
    private fun assertSame(what: String, expected: Dp, actual: Dp) {
        assertEquals(what, expected.value.toDouble(), actual.value.toDouble(), 0.5)
    }

    /**
     * Moving the cursor down the list leaves every OTHER row exactly where it was. The rows below
     * the new focus are the ones the old in-row detail pushed around, so they are the assertion
     * that matters; the row above proves the shrink half.
     */
    @Test
    fun focusingARowMovesNoOtherRow() {
        settings()
        // Entry focus is the first row (Resolution), so "Refresh rate" starts unfocused and
        // "Compositor" sits below both candidates.
        val refreshBefore = compose.onNodeWithText("Refresh rate").getBoundsInRoot()
        val compositorBefore = compose.onNodeWithText("Compositor").getBoundsInRoot()

        // One tap on an unfocused row focuses it (a second would activate it — see the screen).
        compose.onNodeWithText("Bitrate").performClick()
        compose.waitForIdle()

        val refreshAfter = compose.onNodeWithText("Refresh rate").getBoundsInRoot()
        val compositorAfter = compose.onNodeWithText("Compositor").getBoundsInRoot()
        assertSame("row above the cursor moved", refreshBefore.top, refreshAfter.top)
        assertSame("row below the cursor moved", compositorBefore.top, compositorAfter.top)
    }

    /**
     * Stepping a value leaves the row's own geometry alone. The label's right edge is the probe:
     * it is what the widening value slot used to shove, and it is stable for any value that fits
     * the slot (which every shipped Bitrate label does).
     */
    @Test
    fun steppingAValueMovesNoLabel() {
        settings()
        compose.onNodeWithText("Bitrate").performClick() // focus it
        compose.waitForIdle()
        val labelBefore = compose.onNodeWithText("Bitrate").getBoundsInRoot()

        compose.onNodeWithText("Bitrate").performClick() // now activates → cycles the value
        compose.waitForIdle()

        val labelAfter = compose.onNodeWithText("Bitrate").getBoundsInRoot()
        assertSame("label moved sideways under a value step", labelBefore.left, labelAfter.left)
        assertSame("label moved sideways under a value step", labelBefore.right, labelAfter.right)
        assertSame("row changed height under a value step", labelBefore.top, labelAfter.top)
    }

    /**
     * The focused row's description is on screen — in the floating band, not inside the row. Proves
     * the detail did not simply get dropped when it left the row: it is still what the cursor
     * explains itself with.
     */
    @Test
    fun theFocusedRowsDetailIsShown() {
        settings()
        compose.onNodeWithText("Refresh rate").performClick()
        compose.waitForIdle()
        compose.onNodeWithText("Frame rate the host renders and streams at.").assertExists()
    }
}
