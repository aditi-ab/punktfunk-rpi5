package io.unom.punktfunk

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** [movedElsewhere]: when the client hands the pointer back to the host. */
class HostCursorTest {
    @Test
    fun handsBackOnlyWhenSomethingElseMovedAStillMouse() {
        assertFalse("still on the mouse's spot", movedElsewhere(100, 100, 100, 100, 1_000))
        assertFalse("rounding between window and frame", movedElsewhere(102, 98, 100, 100, 1_000))
        assertFalse("the host still trails the mouse", movedElsewhere(400, 100, 100, 100, 100))
        assertTrue("touch or a pad moved it", movedElsewhere(400, 100, 100, 100, 250))
    }
}
