package io.unom.punktfunk

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** [isMouseSideKey]: which Back keys reach the host as X1 instead of opening the ring. */
class MouseSideKeyTest {
    private fun claim(
        tv: Boolean = false,
        external: Boolean = true,
        pad: Boolean = false,
        fallback: Boolean = false,
        mouse: Boolean = true,
        dpad: Boolean = false,
        mousePresent: Boolean = false,
        gestureIsKey: Boolean = false,
    ) = isMouseSideKey(tv, external, pad, fallback, mouse, dpad, mousePresent, gestureIsKey)

    @Test
    fun offTvEveryExternalBackIsTheHosts() {
        assertTrue("plain mouse", claim())
        assertTrue("keyboard-and-mouse combo", claim(dpad = true))
        assertTrue("consumer-page node without a mouse source", claim(mouse = false))
    }

    @Test
    fun theRingKeepsTheGestureAndThePad() {
        assertFalse("nav bar or gesture Back", claim(external = false))
        assertFalse("a pad's Select-as-Back", claim(pad = true))
        assertFalse("the framework's fallback duplicate", claim(fallback = true))
    }

    /** A side button the system maps to Back arrives as a Back from one of its own devices. */
    @Test
    fun anInternalBackIsTheMousesWhileOneIsAttached() {
        val virtual = { present: Boolean, tv: Boolean ->
            claim(tv = tv, external = false, mouse = false, dpad = true, mousePresent = present)
        }
        assertTrue("mouse attached", virtual(true, false))
        assertFalse("no mouse: the nav bar's Back", virtual(false, false))
        assertFalse("a TV keeps its Back", virtual(true, true))
        // Before Android 16 the gesture is this same key, and it opens the ring.
        assertFalse(
            "gesture as a key",
            claim(external = false, mouse = false, dpad = true, mousePresent = true, gestureIsKey = true),
        )
    }

    @Test
    fun onTvARemoteKeepsItsBack() {
        assertTrue("mouse", claim(tv = true))
        assertFalse("air-mouse remote", claim(tv = true, dpad = true))
        assertFalse("D-pad remote", claim(tv = true, mouse = false, dpad = true))
    }
}
