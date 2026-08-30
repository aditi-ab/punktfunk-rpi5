package io.unom.punktfunk

import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pure JVM test of [videoFitRect] — the picture rect the whole-container gesture layer maps
 * fingers into. Run: `./gradlew :app:testDebugUnitTest`.
 */
class VideoFitRectTest {
    @Test
    fun widerContainerBarsLeftAndRight() {
        // A 16:9 stream on a 20:9 phone in landscape: 2400×1080 → the picture is 1920 wide.
        val r = videoFitRect(IntSize(2400, 1080), 16f / 9f)
        assertEquals(IntRect(240, 0, 2160, 1080), r)
    }

    @Test
    fun tallerContainerBarsTopAndBottom() {
        // The same stream on that phone held in portrait: 1080×2400 → 1080×608, centred.
        val r = videoFitRect(IntSize(1080, 2400), 16f / 9f)
        assertEquals(IntRect(0, 896, 1080, 896 + 608), r)
    }

    @Test
    fun unknownAspectFillsTheContainer() {
        assertEquals(IntRect(0, 0, 1280, 720), videoFitRect(IntSize(1280, 720), 0f))
    }
}
