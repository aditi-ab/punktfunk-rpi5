package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the overlay-scale arithmetic ([OsdScale]) — the Kotlin twin of
 * `punktfunk-core`'s `osd_scale` module. Run: `./gradlew :app:testDebugUnitTest`.
 */
class OsdScaleTest {
    @Test
    fun autoIsTheZeroSentinelAndSurvivesSanitize() {
        assertTrue(OsdScale.isAuto(OsdScale.AUTO))
        assertTrue(OsdScale.isAuto(-1.0))
        assertTrue(OsdScale.isAuto(Double.NaN))
        assertFalse(OsdScale.isAuto(1.0))
        assertEquals(OsdScale.AUTO, OsdScale.sanitize(OsdScale.AUTO), 0.0)
        assertEquals(OsdScale.AUTO, OsdScale.sanitize(Double.NaN), 0.0)
    }

    @Test
    fun manualValuesClampIntoRange() {
        assertEquals(OsdScale.MIN_SCALE, OsdScale.sanitize(0.1), 0.0)
        assertEquals(OsdScale.MAX_SCALE, OsdScale.sanitize(9.0), 0.0)
        assertEquals(1.25, OsdScale.sanitize(1.25), 0.0)
    }

    @Test
    fun onlyTvDepartsFromNativeSize() {
        assertEquals(1.0, OsdScale.autoScale(OsdScale.DeviceClass.HANDHELD), 0.0)
        assertEquals(1.0, OsdScale.autoScale(OsdScale.DeviceClass.TABLET), 0.0)
        assertEquals(1.0, OsdScale.autoScale(OsdScale.DeviceClass.DESKTOP), 0.0)
        assertEquals(1.75, OsdScale.autoScale(OsdScale.DeviceClass.TV), 0.0)
    }

    @Test
    fun resolvePrefersTheManualValueOverTheClass() {
        assertEquals(1.75, OsdScale.resolve(OsdScale.AUTO, OsdScale.DeviceClass.TV), 0.0)
        assertEquals(1.0, OsdScale.resolve(1.0, OsdScale.DeviceClass.TV), 0.0)
        assertEquals(2.0, OsdScale.resolve(2.0, OsdScale.DeviceClass.HANDHELD), 0.0)
    }

    @Test
    fun resolveIsAlwaysDrawable() {
        val prefs = listOf(OsdScale.AUTO, Double.NaN, -5.0, 0.01, 99.0, 1.5)
        for (pref in prefs) {
            for (cls in OsdScale.DeviceClass.entries) {
                val scale = OsdScale.resolve(pref, cls)
                assertTrue("$pref on $cls is not finite", scale.isFinite())
                assertTrue("$pref on $cls → $scale", scale >= OsdScale.MIN_SCALE && scale <= OsdScale.MAX_SCALE)
            }
        }
    }

    @Test
    fun percentRoundTrips() {
        for (p in OsdScale.PRESETS) {
            assertEquals(p, OsdScale.fromPercent(OsdScale.toPercent(p)), 0.0)
        }
        assertEquals(175, OsdScale.toPercent(1.75))
        assertEquals(1.25, OsdScale.fromPercent(125), 0.0)
    }

    @Test
    fun typedPercentClampsButZeroMeansAuto() {
        assertEquals(OsdScale.AUTO, OsdScale.fromPercent(0), 0.0)
        assertEquals(OsdScale.MIN_SCALE, OsdScale.fromPercent(5), 0.0)
        assertEquals(OsdScale.MAX_SCALE, OsdScale.fromPercent(500), 0.0)
    }

    @Test
    fun presetsAreOrdered25ApartAndInRange() {
        OsdScale.PRESETS.zipWithNext { a, b ->
            assertEquals(25, OsdScale.toPercent(b) - OsdScale.toPercent(a))
        }
        assertEquals(OsdScale.MIN_SCALE, OsdScale.PRESETS.first(), 0.0)
        assertTrue(OsdScale.PRESETS.all { it >= OsdScale.MIN_SCALE && it <= OsdScale.MAX_SCALE })
        assertTrue(OsdScale.PRESETS.contains(1.0))
    }

    @Test
    fun presetsSurviveTheFloatRoundTripStorageUses() {
        // `SharedPreferences` stores this as a Float; the picker matches on Double equality, so
        // every preset must come back bit-identical.
        for (p in OsdScale.PRESETS) {
            assertEquals(p, p.toFloat().toDouble(), 0.0)
        }
    }

    @Test
    fun stepWalksAutomaticAndThePresetsAndWraps() {
        assertEquals(1.25, OsdScale.step(1.0, 1), 0.0)
        assertEquals(0.75, OsdScale.step(1.0, -1), 0.0)
        assertEquals(1.0, OsdScale.step(1.0, 0), 0.0)
        assertEquals(OsdScale.PRESETS.first(), OsdScale.step(OsdScale.AUTO, 1), 0.0)
        assertEquals(OsdScale.AUTO, OsdScale.step(OsdScale.PRESETS.first(), -1), 0.0)
        assertEquals(OsdScale.AUTO, OsdScale.step(OsdScale.PRESETS.last(), 1), 0.0)
        // A custom entry has no rung; the first step snaps to Automatic.
        assertEquals(OsdScale.AUTO, OsdScale.step(1.6, 1), 0.0)
        assertEquals(OsdScale.AUTO, OsdScale.step(1.6, -1), 0.0)
    }

    @Test
    fun labelsNameTheAutoValue() {
        assertEquals("Automatic (175%)", OsdScale.label(OsdScale.AUTO, OsdScale.DeviceClass.TV))
        assertEquals("Automatic (100%)", OsdScale.label(OsdScale.AUTO, OsdScale.DeviceClass.DESKTOP))
        assertEquals("125%", OsdScale.label(1.25, OsdScale.DeviceClass.TV))
    }

    @Test
    fun theCustomSentinelCannotCollideWithAStoredValue() {
        assertTrue(OsdScale.isAuto(OSD_SCALE_CUSTOM))
        assertTrue(OsdScale.PRESETS.none { it == OSD_SCALE_CUSTOM })
    }
}
