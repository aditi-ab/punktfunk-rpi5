package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the safe-area stream geometry ([SafeArea]) and the sentinel that selects it —
 * the width-only inset that keeps the picture clear of the cutout.
 * Run: `./gradlew -PexcludeScreenshots :app:testDebugUnitTest`.
 */
class SafeAreaTest {
    @Test
    fun aHoleOnOneSideIsPaidForOnceInEitherRotation() {
        // OnePlus 9 Pro (#1068): the window reports the punch-hole as 126 px, on top in portrait and
        // on one side in landscape. Both readings charge 126 once, never on both sides.
        val portrait = SafeArea.landscapeInset(left = 0, top = 126, right = 0, bottom = 0)
        val landscape = SafeArea.landscapeInset(left = 126, top = 0, right = 0, bottom = 0)
        assertEquals(126, portrait)
        assertEquals(126, landscape)
        assertEquals(3090, SafeArea.insetWidth(3216, portrait))
        // A 127 px hole leaves an odd width, so the host gets the even neighbour.
        assertEquals(3088, SafeArea.insetWidth(3216, 127))
    }

    @Test
    fun housingOnBothEdgesAddsUp() {
        // A notch on top plus a chin cutout on the bottom land on both landscape sides.
        assertEquals(96 + 40, SafeArea.landscapeInset(left = 0, top = 96, right = 0, bottom = 40))
        // A probe that raced a rotation sees both pairs; the housing is the larger one.
        assertEquals(126, SafeArea.landscapeInset(left = 126, top = 63, right = 0, bottom = 0))
        assertEquals(0, SafeArea.landscapeInset(-5, -5, -5, -5))
    }

    @Test
    fun insetWidthStaysHostValid() {
        assertEquals(2400 - 96 * 2, SafeArea.insetWidth(2400, 96 * 2))
        // Odd results even-floor — the host rejects odd dimensions outright.
        assertEquals(0, SafeArea.insetWidth(2401, 95) % 2)
        // No cutout → the native width, unchanged.
        assertEquals(2400, SafeArea.insetWidth(2400, 0))
    }

    @Test
    fun absurdInsetsCannotDriveTheModeUnderTheHostFloor() {
        assertEquals(SafeArea.MIN_WIDTH, SafeArea.insetWidth(1280, 10000))
        // A negative reading is treated as no inset rather than widening past the panel.
        assertEquals(1280, SafeArea.insetWidth(1280, -80))
    }

    @Test
    fun theSentinelIsAPresetAndNeverReadsAsCustom() {
        // The safe-area mode is a stored preset, not a typed size: `isCustomResolution` must be
        // false for it, or the touch settings would open the custom width/height fields on it and
        // the gamepad screen would prepend a bogus "Custom · -2 × -2" row.
        val s = Settings(width = SAFE_AREA_MODE, height = SAFE_AREA_MODE)
        assertTrue(!s.isCustomResolution())
        // And it must be distinct from the UI's own "Custom…" sentinel (-1).
        assertTrue(SAFE_AREA_MODE != -1)
        assertTrue(NATIVE_RESOLUTION_OPTIONS.any { it.first == SAFE_AREA_MODE && it.second == SAFE_AREA_MODE })
    }
}
