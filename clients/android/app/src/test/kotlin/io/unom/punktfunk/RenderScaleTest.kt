package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the client-side render-scale geometry ([RenderScale]) — the Kotlin twin of
 * `punktfunk-core`'s `render_scale` module. Run: `./gradlew :app:testDebugUnitTest`.
 */
class RenderScaleTest {
    @Test
    fun sanitizeClampsAndDefaults() {
        assertEquals(1.0, RenderScale.sanitize(0.0), 0.0) // absent / zero → Native
        assertEquals(1.0, RenderScale.sanitize(-2.0), 0.0)
        assertEquals(1.0, RenderScale.sanitize(Double.NaN), 0.0)
        assertEquals(0.5, RenderScale.sanitize(0.1), 0.0) // below the floor
        assertEquals(4.0, RenderScale.sanitize(9.0), 0.0) // above the ceiling
        assertEquals(1.5, RenderScale.sanitize(1.5), 0.0)
    }

    @Test
    fun maxDimensionIsCodecAware() {
        assertEquals(4096, RenderScale.maxDimension("h264"))
        assertEquals(8192, RenderScale.maxDimension("hevc"))
        assertEquals(8192, RenderScale.maxDimension("av1"))
        assertEquals(8192, RenderScale.maxDimension("auto"))
    }

    @Test
    fun nativeIsIdentity() {
        assertEquals(1920 to 1080, RenderScale.apply(1920, 1080, 1.0, 8192))
    }

    @Test
    fun supersampleDoubles() {
        assertEquals(3840 to 2160, RenderScale.apply(1920, 1080, 2.0, 8192))
    }

    @Test
    fun underRenderHalves() {
        assertEquals(960 to 540, RenderScale.apply(1920, 1080, 0.5, 8192))
    }

    @Test
    fun resultsAreEven() {
        // 1366×768 × 1.5 = 2049×1152 → even-floored to 2048×1152.
        val (w, h) = RenderScale.apply(1366, 768, 1.5, 8192)
        assertEquals(0, w % 2)
        assertEquals(0, h % 2)
        assertEquals(2048 to 1152, w to h)
    }

    @Test
    fun overCeilingClampsUniformly() {
        // 4K × 4 = 15360×8640; both exceed 8192 → width lands on cap, 16:9 kept (8192×4608).
        val (w, h) = RenderScale.apply(3840, 2160, 4.0, 8192)
        assertTrue(w <= 8192 && h <= 8192)
        assertEquals(8192 to 4608, w to h)
    }

    @Test
    fun h264CeilingIsTighter() {
        // 1080p × 4 = 7680×4320; under H.264's 4096 wall → 4096×2304.
        assertEquals(4096 to 2304, RenderScale.apply(1920, 1080, 4.0, 4096))
    }

    @Test
    fun minimumFloorHonoured() {
        val (w, h) = RenderScale.apply(400, 300, 0.5, 8192)
        assertTrue(w >= 320 && h >= 200)
    }
}
