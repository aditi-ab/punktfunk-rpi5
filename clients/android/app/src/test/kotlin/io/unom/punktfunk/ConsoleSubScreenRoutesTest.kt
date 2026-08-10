package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The console route to the two sub-screens, driven through the REAL settings screen — the rows
 * themselves are pinned by `ConsoleSubScreenRowsTest`; what needs the Compose runtime is the trip:
 * that a press on the row reaches the shell, and that coming back lands where you left rather than
 * at the top of the first section (the shell's `AnimatedContent` discards a screen's state the
 * moment it stops being the target, so the place has to travel out and back).
 *
 * Rows are activated by TAP for the same reason `GamepadSettingsLayoutTest` does it: the pad path
 * needs a `MainActivity` for its probes, and both routes end in the same `activate`.
 *
 * `sdk = [36]` for the reason every Robolectric test here pins it: android-all jars stop at 36 while
 * the app compiles against 37.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class ConsoleSubScreenRoutesTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    @Test
    fun openingTheControllersRowNavigatesAndReportsWhereItWas() {
        var opened = 0
        var place: GpSettingsPlace? = null
        compose.setContent {
            GamepadSettingsScreen(
                initial = Settings(),
                onChange = {},
                onBack = {},
                onOpenControllers = { opened++ },
                // Entering as if we had just come back from it, which is also what puts the cursor
                // on the row — so a single tap ACTIVATES rather than merely focusing.
                resume = GpSettingsPlace(GpTab.CONTROLLER, "controllers"),
                onPlace = { place = it },
            )
        }
        compose.waitForIdle()

        compose.onNodeWithText("Connected controllers").performClick()
        compose.waitForIdle()

        assertEquals("the console never reached the diagnostics screen", 1, opened)
        assertEquals(
            "the place has to leave before the row does — this screen is gone the next frame",
            GpSettingsPlace(GpTab.CONTROLLER, "controllers"),
            place,
        )
    }

    /**
     * Back from a sub-screen lands on the section it was opened from, with the row on screen. The
     * cursor is restored by row ID rather than index, so it survives a section whose length follows
     * the hardware.
     */
    @Test
    fun comingBackFromTheNoticesLandsOnTheRowThatOpenedThem() {
        compose.setContent {
            GamepadSettingsScreen(
                initial = Settings(),
                onChange = {},
                onBack = {},
                resume = GpSettingsPlace(GpTab.INTERFACE, "licenses"),
            )
        }
        compose.waitForIdle()

        compose.onNodeWithText("Open-source licenses").assertIsDisplayed()
        // Not back at the top of the first section — "Resolution" leads the Stream tab, which is
        // where a screen that forgot its place would be.
        compose.onNodeWithText("Resolution").assertDoesNotExist()
        // And the legend describes THIS row's A. It said the literal "Pin to hosts" on every
        // non-adjustable row back when profiles were the only ones.
        compose.onNodeWithText("Open").assertIsDisplayed()
        compose.onNodeWithText("Pin to hosts").assertDoesNotExist()
    }

    /**
     * The notices screen stands on its own on the console's field: no Scaffold or Surface above it
     * (the shell has neither), its own backdrop, and a legend that says how to leave. Composing it
     * is most of the assertion — a screen that only ever ran inside the touch Scaffold takes its
     * content colour from one.
     */
    @Test
    fun theConsoleNoticesScreenStandsOnItsOwn() {
        compose.setContent { ConsoleLicensesScreen(onBack = {}) }
        compose.waitForIdle()

        compose.onNodeWithText("Open-source licenses").assertIsDisplayed()
        compose.onNodeWithText("Scroll").assertIsDisplayed()
        compose.onNodeWithText("Close").assertIsDisplayed()
    }
}
