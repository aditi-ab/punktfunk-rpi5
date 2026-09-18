package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the aspect-family table ([Resolutions]) — the Kotlin twin of `punktfunk-core`'s
 * `resolutions` module. Run: `./gradlew :app:testDebugUnitTest`.
 */
class ResolutionsTest {
    @Test
    fun everyListedSizeMapsBackToItsFamily() {
        Resolutions.ASPECTS.forEachIndexed { i, a ->
            for ((w, h) in a.sizes) {
                assertEquals("${w}x$h → ${a.label}", i, Resolutions.aspectOf(w, h))
                assertTrue("${w}x$h has an odd side", w % 2 == 0 && h % 2 == 0)
            }
            assertEquals("${a.label} ascending", a.sizes.map { it.second }.sorted(), a.sizes.map { it.second })
        }
    }

    @Test
    fun shapeNotMembership() {
        assertEquals(4, Resolutions.aspectOf(1500, 1000)) // a custom 3:2
        assertEquals(1, Resolutions.aspectOf(3456, 2234)) // a MacBook panel reads 16:10
        assertNull(Resolutions.aspectOf(2556, 1179)) // a phone panel is nobody's
        assertNull(Resolutions.aspectOf(0, 0)) // native
        assertNull(Resolutions.aspectOf(SAFE_AREA_MODE, SAFE_AREA_MODE))
    }

    @Test
    fun nearestFollowsHeightAndNativeMeans1080() {
        assertEquals(1920 to 1080, Resolutions.nearest(0, 0))
        assertEquals(1920 to 1200, Resolutions.nearest(1, 1080))
        assertEquals(3440 to 1440, Resolutions.nearest(2, 1440))
        assertEquals(7680 to 2160, Resolutions.nearest(3, 2160))
        assertEquals(2160 to 1440, Resolutions.nearest(4, 800))
        assertEquals(1600 to 1200, Resolutions.nearest(5, 1000))
    }

    /** A OnePlus 9 Pro: 3216×1440, 127 px of cutout on one side. Twin of the core test. */
    @Test
    fun aPhoneLeadsWithItsScreenAndSafeArea() {
        val f = Resolutions.families(3216 to 1440, 3088 to 1440)
        assertEquals(listOf("Screen", "Safe area", "16:9"), f.take(3).map { it.label })
        assertEquals(listOf(1608 to 720, 2412 to 1080, 3216 to 1440), f[0].sizes)
        assertEquals(listOf(1544 to 720, 2316 to 1080, 3088 to 1440), f[1].sizes)
        assertEquals(0, Resolutions.familyOf(f, 2412, 1080))
        assertEquals(1, Resolutions.familyOf(f, 2316, 1080))
        assertEquals(2, Resolutions.familyOf(f, 1920, 1080))
        // Native and the safe-area mode list their own entries; a 2412 × 1080 pick is a preset.
        assertEquals(0, Settings(width = 0, height = 0).resolutionFamily(f))
        assertEquals(1, Settings(width = SAFE_AREA_MODE, height = SAFE_AREA_MODE).resolutionFamily(f))
        assertTrue(!Settings(width = 2412, height = 1080).isCustomResolution(f))
        // A standard screen adds nothing.
        assertEquals(Resolutions.ASPECTS.size, Resolutions.families(1920 to 1080, 1920 to 1080).size)
    }

    @Test
    fun customIsWhatNoFamilyLists() {
        assertTrue(Settings(width = 1500, height = 1000).isCustomResolution())
        assertTrue(!Settings(width = 1280, height = 800).isCustomResolution()) // the Deck, 16:10
        assertTrue(!Settings(width = 0, height = 0).isCustomResolution())
    }
}
