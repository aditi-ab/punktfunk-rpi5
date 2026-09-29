package io.unom.punktfunk

import android.hardware.display.DisplayManager
import android.os.ParcelFileDescriptor
import android.os.SystemClock
import androidx.activity.ComponentActivity
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import io.unom.punktfunk.kit.SessionEndReason
import io.unom.punktfunk.models.ActiveSession
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Shape (b) of design/android-dual-screen.md on a simulated second screen the size of an Ayn Thor's
 * lower one: the companion panel comes up there as a Presentation with the stream, its tiles reach
 * the session, and it leaves with the stream. Over a ZERO handle, as [StreamScreenTest].
 */
@RunWith(AndroidJUnit4::class)
class CompanionDisplayTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val ended = mutableListOf<SessionEndReason>()

    private fun shell(cmd: String) {
        val out = InstrumentationRegistry.getInstrumentation().uiAutomation.executeShellCommand(cmd)
        ParcelFileDescriptor.AutoCloseInputStream(out).use { it.readBytes() }
    }

    @Before
    fun attachASecondScreen() {
        shell("settings put global overlay_display_devices 1240x1080/420")
        val dm = compose.activity.getSystemService(DisplayManager::class.java)
        val deadline = SystemClock.uptimeMillis() + 5_000
        while (dm.displays.size < 2 && SystemClock.uptimeMillis() < deadline) SystemClock.sleep(100)
    }

    @After
    fun detachIt() = shell("settings delete global overlay_display_devices")

    @Test
    fun thePanelComesUpOnTheSecondScreenAndLeavesWithTheStream() {
        var streaming by mutableStateOf(true)
        compose.setContent {
            if (streaming) {
                StreamScreen(ActiveSession(handle = 0L, settings = Settings(), clipboardSync = false)) { ended += it }
            }
        }
        compose.waitUntil(5_000) { compose.onAllNodesWithText("Actions").fetchSemanticsNodes().isNotEmpty() }
        compose.onNodeWithText("Actions").performClick()
        compose.onNodeWithText("End stream").performClick()
        compose.waitForIdle()
        assertEquals(emptyList<SessionEndReason>(), ended)
        compose.onNodeWithText("End stream").performClick()
        compose.waitForIdle()
        assertEquals(listOf(SessionEndReason.LOCAL), ended)

        streaming = false
        compose.waitUntil(5_000) { compose.onAllNodesWithText("Actions").fetchSemanticsNodes().isEmpty() }
    }
}
