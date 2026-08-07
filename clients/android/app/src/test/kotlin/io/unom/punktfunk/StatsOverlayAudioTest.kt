package io.unom.punktfunk

import androidx.activity.ComponentActivity
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithText
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * The stats HUD's audio line — `audio buffer N ms · a/v ±N ms`, from the live gauges at indexes
 * 33/34 (`design/audio-latency-overhaul.md`).
 *
 * Worth pinning because the whole point of the overhaul's stats half is that the audio plane became
 * OBSERVABLE. Before it, ring depth and A/V offset existed only as a log line, and on a device
 * launched by a game launcher that goes to a pipe nobody can read — so the single number that
 * identifies a deep ring was unobtainable on the exact device reporting the latency, and a field
 * investigation ran to its conclusion without it. A measurement that never reaches a surface is
 * indistinguishable from no measurement, which is what this asserts.
 *
 * `sdk = [36]` for the same reason as the screenshot tests: Robolectric ships android-all jars only
 * up to API 36 while the app's compileSdk is 37.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [36])
class StatsOverlayAudioTest {
    @get:Rule
    val compose = createAndroidComposeRule<ComponentActivity>()

    /**
     * A plausible 35-double window with the audio gauges dialled in. Everything before 33 is the
     * DETAILED-renderable shape the ShotScenes fixture uses; only the last two matter here.
     */
    private fun stats(bufferMs: Double, avOffsetMs: Double, size: Int = 35): DoubleArray {
        val full = doubleArrayOf(
            238.0, 921.4, 1.3, 2.1, 1.0, 1.0, 5120.0, 1440.0, 240.0, 2.0,
            10.0, 9.0, 16.0, 1.0, 0.9, 0.4, 0.6, 0.3,
            2.0, 1.0, 5.0, 238.0,
            1.0, 0.5, 1.8, 2.6,
            0.2, 0.3, 236.0, 1.0,
            0.1, 0.3, 0.0,
            bufferMs, avOffsetMs,
        )
        return full.copyOf(size)
    }

    private fun show(s: DoubleArray, verbosity: StatsVerbosity = StatsVerbosity.DETAILED) {
        compose.setContent { StatsOverlay(s, verbosity = verbosity) }
    }

    @Test
    fun detailedShowsDepthAndOffset() {
        show(stats(bufferMs = 42.0, avOffsetMs = 18.0))
        // Positive = audio playing BEHIND the picture, and the sign is explicit so a glance tells
        // which way the loop still has to move.
        compose.onNodeWithText("audio buffer 42 ms · a/v +18 ms").assertExists()
    }

    @Test
    fun audioAheadOfThePictureReadsNegative() {
        show(stats(bufferMs = 42.0, avOffsetMs = -12.0))
        compose.onNodeWithText("audio buffer 42 ms · a/v -12 ms").assertExists()
    }

    /** Aligned (or not yet measured) drops the offset term; the depth alone is still the triage number. */
    @Test
    fun alignedShowsDepthAlone() {
        show(stats(bufferMs = 42.0, avOffsetMs = 0.0))
        compose.onNodeWithText("audio buffer 42 ms").assertExists()
    }

    /** Nothing queued (audio off, or the ring not yet primed) — the line has nothing to say. */
    @Test
    fun silentPlaneRendersNoLine() {
        show(stats(bufferMs = 0.0, avOffsetMs = 0.0))
        compose.onNodeWithText("audio buffer", substring = true).assertDoesNotExist()
    }

    /** The line is DETAILED-only, like every other per-stage figure. */
    @Test
    fun normalTierOmitsTheLine() {
        show(stats(bufferMs = 42.0, avOffsetMs = 18.0), verbosity = StatsVerbosity.NORMAL)
        compose.onNodeWithText("audio buffer", substring = true).assertDoesNotExist()
    }

    /** An older native lib emits 33 doubles; the overlay must omit the line, not index past the end. */
    @Test
    fun olderNativeLayoutOmitsTheLine() {
        show(stats(bufferMs = 42.0, avOffsetMs = 18.0, size = 33))
        compose.onNodeWithText("audio buffer", substring = true).assertDoesNotExist()
    }
}
