package io.unom.punktfunk

import io.unom.punktfunk.kit.SessionAccess
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class StreamUiScrollTest {
    private fun state(initial: Boolean, grants: Int = SessionAccess.ALL, apply: (Boolean) -> Boolean) =
        StreamUi(0, intArrayOf(grants, 0), StatsVerbosity.entries.first(), initial, apply)

    @Test
    fun sessionChoiceStartsFromPresetAndTogglesLive() {
        val sent = mutableListOf<Boolean>()
        val ui = state(true) { sent += it; true }
        assertTrue(ui.invertScroll)
        assertTrue(sent.isEmpty())
        ui.setScrollInverted(false)
        assertFalse(ui.invertScroll)
        ui.setScrollInverted(true)
        ui.setScrollInverted(true)
        assertEquals(listOf(false, true), sent)
    }

    @Test
    fun deniedPointerNeverCallsNativeSetter() {
        val ui = state(false, SessionAccess.KEYBOARD) { error("must not send") }
        ui.setScrollInverted(true)
        assertFalse(ui.invertScroll)
    }

    @Test
    fun failedNativeUpdateKeepsDisplayedValue() {
        val ui = state(false) { false }
        ui.setScrollInverted(true)
        assertFalse(ui.invertScroll)
    }

    @Test
    fun sessionChoicesStayIndependent() {
        val first = state(false) { true }
        val second = state(true) { true }
        first.setScrollInverted(true)
        second.setScrollInverted(false)
        assertTrue(first.invertScroll)
        assertFalse(second.invertScroll)
        assertFalse(state(false) { true }.invertScroll)
    }
}
