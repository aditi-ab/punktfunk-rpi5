package io.unom.punktfunk

import io.unom.punktfunk.console.ConsoleJson
import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `pinned_key` in `clients/shared/console-vectors.json`: the pinned-card row key this client
 * writes is the one the Skia console splits back to its host.
 */
class ConsolePinnedKeyTest {
    @Test
    fun pinnedKeysMatchTheSharedVectors() {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/console-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        val cases = JSONObject(file.readText()).getJSONArray("pinned_key")
        assertTrue(cases.length() > 0)
        for (i in 0 until cases.length()) {
            val c = cases.getJSONObject(i)
            val key = ConsoleJson.pinnedKey(c.getString("host"), c.getString("preset"))
            assertEquals(c.getString("key"), key)
            assertEquals(c.getString("host"), ConsoleJson.hostKey(key))
        }
    }
}
