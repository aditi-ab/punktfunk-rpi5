package io.unom.punktfunk.screenshots

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onRoot
import com.github.takahirom.roborazzi.captureRoboImage
import com.github.takahirom.roborazzi.captureScreenRoboImage
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * App-store / marketing screenshots of the native Android client, rendered on the JVM by Roborazzi
 * (Robolectric Native Graphics) — no emulator, GPU, host, or JNI core. The scenes (ShotScenes.kt)
 * render the REAL Compose UI with mock state.
 *
 * `sdk = [36]` is mandatory: Robolectric ships android-all jars only up to API 36 (Android 16), and
 * the app's compileSdk is 37. PNGs land in build/outputs/roborazzi/.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [36], qualifiers = "w360dp-h800dp-xxhdpi")
class ScreenshotTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    private val out = "build/outputs/roborazzi"

    // Pausing the animation clock before composing (then advancing once past the entrance animation
    // and freezing) is what makes a text-field-bearing scene capturable: a focused field blinks its
    // cursor via an infinite animation that otherwise keeps Compose perpetually "busy", so
    // setContent's wait-for-idle never returns. Frozen, the capture is also deterministic.

    /** Full-screen content scenes: the compose root fills the device, so a root capture is the shot. */
    private fun shootRoot(name: String, content: @androidx.compose.runtime.Composable () -> Unit) {
        compose.mainClock.autoAdvance = false
        compose.setContent { ShotTheme(content) }
        compose.mainClock.advanceTimeBy(800)
        compose.onRoot().captureRoboImage("$out/phone-$name.png")
    }

    /** Dialog scenes: the AlertDialog is a separate window, so capture the whole screen (all windows). */
    private fun shootScreen(name: String, content: @androidx.compose.runtime.Composable () -> Unit) {
        compose.mainClock.autoAdvance = false
        compose.setContent { ShotTheme(content) }
        compose.mainClock.advanceTimeBy(800)
        captureScreenRoboImage("$out/phone-$name.png")
    }

    @Test
    fun hosts() = shootRoot("hosts") { HostsScene() }

    @Test
    fun settings() = shootRoot("settings") { SettingsScene() }

    // One category page per shot: the sub-section headers, the caption-under-control fields and
    // the "applies from the next session" footers live inside a category, not on the root list.
    @Test
    fun settingsDisplay() = shootRoot("settings-display") {
        SettingsCategoryScene(io.unom.punktfunk.SettingsCategory.Display)
    }

    @Test
    fun settingsInput() = shootRoot("settings-input") {
        SettingsCategoryScene(io.unom.punktfunk.SettingsCategory.Input)
    }

    @Test
    fun settingsProfile() = shootRoot("settings-profile") { SettingsProfileScene() }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi") // landscape — the stream is immersive
    fun stream() = shootRoot("stream") { StreamScene(io.unom.punktfunk.StatsVerbosity.DETAILED) }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamCompact() = shootRoot("stream-compact") { StreamScene(io.unom.punktfunk.StatsVerbosity.COMPACT) }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamNormal() = shootRoot("stream-normal") { StreamScene(io.unom.punktfunk.StatsVerbosity.NORMAL) }

    // The touch flow is a Material dialog over the host grid (a separate window → shootScreen).
    @Test
    fun connecting() = shootScreen("connecting") {
        HostsScene()
        ConnectingScene()
    }

    @Test
    fun waking() = shootScreen("waking") {
        HostsScene()
        WakingScene()
    }

    @Test
    fun wakeTimedOut() = shootScreen("wake-timed-out") {
        HostsScene()
        WakeTimedOutScene()
    }

    // The console flow is the full-screen aurora takeover (a root capture).
    @Test
    fun connectingConsole() = shootRoot("connecting-console") { ConnectConsoleScene() }

    @Test
    fun trust() = shootScreen("trust") {
        HostsScene()
        TrustDialog()
    }

    @Test
    fun pair() = shootScreen("pair") {
        HostsScene()
        PairDialog()
    }
}
