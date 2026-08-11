package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The console's route to the two screens that were reachable from touch only — the
 * connected-controllers diagnostics and the open-source notices.
 *
 * "Touch only" reads as a minor gap on a phone and is a dead end on a TV box, where the console IS
 * the interface: there is no touch UI to fall back to, so a screen with no console row could not be
 * opened at all. These pin the rows themselves; `ConsoleSubScreenRoutesTest` drives the real screen.
 */
class ConsoleSubScreenRowsTest {

    private fun rows(
        forwarding: Boolean = true,
        version: String = "1.2.3",
        controllers: () -> Unit = {},
        licenses: () -> Unit = {},
    ): List<GpRow> = buildSettingsRows(
        Settings(gamepadForwarding = forwarding),
        hasBodyVibrator = true,
        hasGyroscope = true,
        av1Capable = true,
        appVersion = version,
        openControllers = controllers,
        openLicenses = licenses,
    ) {}

    private fun row(rows: List<GpRow>, id: String): GpRow = rows.first { it.id == id }

    @Test
    fun `the controllers row opens the diagnostics view from the controller section`() {
        var opened = 0
        val r = row(rows(controllers = { opened++ }), "controllers")
        assertEquals(GpTab.CONTROLLER, r.tab)
        assertEquals("Connected controllers", r.label)
        r.activate()
        assertEquals(1, opened)
    }

    /**
     * It must NOT follow the master forwarding switch, unlike every other row in its section: the
     * screen it opens is what you reach for precisely when forwarding looks broken, and a diagnostic
     * that dims itself when the thing it diagnoses is off is worse than no diagnostic.
     */
    @Test
    fun `the controllers row stays live with forwarding off`() {
        val off = rows(forwarding = false)
        assertTrue(row(off, "controllers").enabled)
        assertNotNull(liveRow(off, off.indexOfFirst { it.id == "controllers" }))
        // Its neighbours in the section still dim, so this is a deliberate exemption and not a
        // forgotten `enabled =`.
        assertFalse(row(off, "sc2").enabled)
    }

    @Test
    fun `the about row opens the notices and states the installed version`() {
        var opened = 0
        val r = row(rows(version = "0.27.0", licenses = { opened++ }), "licenses")
        assertEquals(GpTab.INTERFACE, r.tab)
        assertEquals("About", r.header)
        // The version rides in the value slot — on a TV this row is the whole About page.
        assertEquals("0.27.0", r.value)
        r.activate()
        assertEquals(1, opened)
    }

    /** Both navigate; neither holds a value, so left/right must be refused rather than silently eaten. */
    @Test
    fun `neither row steps a value`() {
        val all = rows()
        for (id in listOf("controllers", "licenses")) {
            val r = row(all, id)
            assertFalse("$id should draw no chevrons", r.adjustable)
            assertFalse("$id must refuse a step", r.adjust(1))
            assertFalse("$id must refuse a step", r.adjust(-1))
        }
    }

    /**
     * The legend follows the ROW. It used to say the literal "Pin to hosts" on every non-adjustable
     * row, because a profile row was the only kind there was — so the moment another one existed,
     * A on it was advertised as pinning something.
     */
    @Test
    fun `an action row advertises what A actually does`() {
        val all = rows()
        assertEquals("Open", row(all, "controllers").actionHint)
        assertEquals("Open", row(all, "licenses").actionHint)
        val profiles = buildProfileRows(listOf(newProfile("Work")), emptyList(), tv = false) {}
        assertEquals("Pin to hosts", profiles.first().actionHint)
    }

    /**
     * The scroll geometry both console sub-screens share. A wall of text has no focusable rows for
     * Compose to keep visible, so these screens move the scroll state themselves — and how far one
     * press travels is the whole of their feel.
     */
    @Test
    fun `a page overlaps what you were reading and a step is shorter still`() {
        val viewport = 1000f
        val page = consoleScrollDelta(viewport, page = true, dir = 1)
        val step = consoleScrollDelta(viewport, page = false, dir = 1)
        assertTrue("a page that skips a whole screenful loses your place", page < viewport)
        assertTrue("a page has to be worth pressing", page > viewport / 2f)
        assertTrue("a D-pad step must be shorter than a shoulder page", step > 0f && step < page)
        assertEquals("the other direction is the other way", -page, consoleScrollDelta(viewport, true, -1), 0.001f)
        // Before the first layout there is no viewport: a press then moves nothing, rather than
        // scrolling by a fraction of zero and reading as a dead button on the way in.
        assertEquals(0f, consoleScrollDelta(0f, page = true, dir = 1), 0f)
    }
}
