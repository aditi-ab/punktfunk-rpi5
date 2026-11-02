package io.unom.punktfunk.kit

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/gamepad-kind-vectors.json` against [Gamepad.PREFS]: every wire byte the core
 * knows has a row here, at its own index, under the core's name.
 */
class GamepadKindVectorTest {
    @Test
    fun everyKindHasItsRow() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/gamepad-kind-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        val kinds = JSONObject(file.readText()).getJSONArray("kinds")
        assertEquals(kinds.length(), Gamepad.PREFS.size)
        for (i in 0 until kinds.length()) {
            val kind = kinds.getJSONObject(i)
            val pref = Gamepad.PREFS[i]
            assertEquals(kind.getInt("value"), pref.wire)
            assertEquals(kind.getString("name"), pref.name)
        }
    }
}
