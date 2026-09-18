package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The touch Bitrate row's custom entry: an off-menu rate is custom and reads as its number. */
class CustomBitrateTest {
    @Test
    fun anOffMenuRateIsCustomAndNeverAutomatic() {
        assertFalse(Settings().isCustomBitrate())
        assertFalse(Settings(bitrateKbps = 20_000).isCustomBitrate())
        // The speed test stores 70 % of the measurement, which is rarely a menu entry.
        assertTrue(Settings(bitrateKbps = 14_000).isCustomBitrate())
        assertEquals("14 Mbps", bitrateLabel(14_000))
        assertTrue(BITRATE_OPTIONS.none { it.first == CUSTOM_BITRATE })
    }
}
