package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pure JVM test of the safe-area stream geometry ([SafeArea]) and the sentinel that selects it —
 * the width-only inset that keeps the picture clear of the cutout and the rounded corners.
 * Run: `./gradlew :app:testDebugUnitTest`.
 */
class SafeAreaTest {
    @Test
    fun insetsBothSidesAndStaysHostValid() {
        // A punch-hole phone: 2400 px wide, 96 px of unsafe edge per side → 2208.
        assertEquals(2400 - 96 * 2, SafeArea.insetWidth(2400, 96))
        // Odd results even-floor — the host rejects odd dimensions outright, and an inset
        // subtraction lands odd about half the time.
        assertEquals(0, SafeArea.insetWidth(2401, 95) % 2)
        // No cutout and square corners → the native width, unchanged.
        assertEquals(2400, SafeArea.insetWidth(2400, 0))
    }

    @Test
    fun absurdInsetsCannotDriveTheModeUnderTheHostFloor() {
        assertEquals(SafeArea.MIN_WIDTH, SafeArea.insetWidth(1280, 5000))
        // A negative reading is treated as no inset rather than widening past the panel.
        assertEquals(1280, SafeArea.insetWidth(1280, -40))
    }

    @Test
    fun safeModeIsNarrowerThanNativeWheneverThereIsAnInset() {
        val native = 2556
        assertTrue(SafeArea.insetWidth(native, 60) < native)
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
        assertTrue(RESOLUTION_OPTIONS.any { it.first == SAFE_AREA_MODE && it.second == SAFE_AREA_MODE })
    }
}
