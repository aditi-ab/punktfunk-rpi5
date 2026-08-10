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

    // Both banner texts, in the stream's own landscape geometry — it is bottom-centre, so the
    // aspect is load-bearing.
    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamBannerPad() = shootRoot("stream-banner-pad") { StreamBannerScene(pad = true) }

    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun streamBannerTouch() = shootRoot("stream-banner-touch") { StreamBannerScene(pad = false) }

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
    fun consoleSettings() = shootRoot("console-settings") { ConsoleSettingsScene() }

    /** A PALE palette: the whole UI flips to dark ink on white frost, which only a shot proves. */
    @Test
    fun consoleSettingsLight() =
        shootRoot("console-settings-light") { ConsoleSettingsScene(paletteId = "holo") }

    // The console home, the screen the living backdrop is most of. The default sdk (36) draws the
    // real AGSL MESH field; the paired API-31 shot below draws the blob fallback, so the two
    // renderings of the same palette can be compared rather than assumed equivalent.
    @Test
    fun consoleHome() = shootRoot("console-home") { ConsoleHomeScene() }

    @Test
    fun consoleHomeLight() = shootRoot("console-home-light") { ConsoleHomeScene(paletteId = "holo") }

    /**
     * Landscape — the orientation the console UI actually runs in, and the only one wide enough to
     * show the carousel's NEIGHBOURS, which is where the projected turn (`CARD_TURN_RAD`) lives.
     */
    @Test
    @Config(sdk = [36], qualifiers = "w800dp-h360dp-xxhdpi")
    fun consoleHomeLandscape() = shootRoot("console-home-landscape") { ConsoleHomeScene() }

    /**
     * The API 31/32 field. `RuntimeShader` is API 33+, so everything below it keeps the four
     * drifting blobs — an honest approximation rather than an emulation, and the thing this shot
     * exists to keep honest.
     */
    @Test
    @Config(sdk = [31], qualifiers = "w360dp-h800dp-xxhdpi")
    fun consoleHomeBlobFallback() = shootRoot("console-home-blobs") { ConsoleHomeScene() }

    // The two screens the console reached for the first time in WP8.3. Each is shot on a dark AND a
    // pale palette, because the console draws them through a ColorScheme derived from the palette's
    // ink — and the pale one is the only place a grey-on-pastel slip can show up.
    @Test
    fun consoleLicenses() = shootRoot("console-licenses") { ConsoleLicensesScene() }

    @Test
    fun consoleLicensesLight() =
        shootRoot("console-licenses-light") { ConsoleLicensesScene(paletteId = "holo") }

    @Test
    fun consoleControllers() = shootRoot("console-controllers") { ConsoleControllersScene() }

    @Test
    fun consoleControllersLight() =
        shootRoot("console-controllers-light") { ConsoleControllersScene(paletteId = "holo") }

    @Test
    fun trust() = shootScreen("trust") {
        HostsScene()
        TrustDialog()
    }

    @Test
    fun newProfile() = shootRoot("new-profile") { NewProfileScene() }

    @Test
    fun speedTest() = shootScreen("speed-test") {
        HostsScene()
        SpeedTestScene()
    }

    @Test
    fun pair() = shootScreen("pair") {
        HostsScene()
        PairDialog()
    }
}
