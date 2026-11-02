package io.unom.punktfunk

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** [advertiseHdr] is pure: the panel's HDR10 listing, gated by the OS headroom signal when it has one. */
class AdvertiseHdrTest {

    /** A built-in panel the OS can light above SDR white (the Nothing Phone 3) streams HDR. */
    @Test
    fun aPanelWithHeadroomAdvertises() {
        assertTrue(advertiseHdr(hdr10 = true, headroom = true))
    }

    /** A panel that lists HDR10 with no headroom (the S20+ on LineageOS) would show PQ dark. */
    @Test
    fun aPanelWithoutHeadroomStaysSdr() {
        assertFalse(advertiseHdr(hdr10 = true, headroom = false))
    }

    /** No signal (API < 34, a TV, an external screen) keeps the panel's own listing. */
    @Test
    fun noSignalKeepsTheListing() {
        assertTrue(advertiseHdr(hdr10 = true, headroom = null))
        assertFalse(advertiseHdr(hdr10 = false, headroom = null))
    }

    /** Headroom never makes an SDR panel advertise HDR. */
    @Test
    fun headroomAloneIsNotHdr() {
        assertFalse(advertiseHdr(hdr10 = false, headroom = true))
    }
}
