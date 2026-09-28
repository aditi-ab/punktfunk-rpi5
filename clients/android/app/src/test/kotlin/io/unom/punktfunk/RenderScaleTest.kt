package io.unom.punktfunk

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/render-scale-vectors.json` against [RenderScale] — the same cases core and the
 * Swift twin run, so every client asks the host for the same `Mode`. Run:
 * `./gradlew :app:testDebugUnitTest`.
 */
class RenderScaleTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/render-scale-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    /** JSON has no NaN; the file writes it as null. */
    private fun JSONObject.num(key: String) = if (isNull(key)) Double.NaN else getDouble(key)

    @Test
    fun everySharedVectorAgrees() {
        val dims = vectors.getJSONArray("max_dimension")
        for (i in 0 until dims.length()) {
            val row = dims.getJSONObject(i)
            val codec = row.getString("codec")
            assertEquals(codec, row.getInt("max"), RenderScale.maxDimension(codec))
        }
        val sanitize = vectors.getJSONArray("sanitize")
        for (i in 0 until sanitize.length()) {
            val row = sanitize.getJSONObject(i)
            assertEquals("$row", row.num("want"), RenderScale.sanitize(row.num("raw")), 0.0)
        }
        val cases = vectors.getJSONArray("apply")
        assertTrue("the vector file is the contract; keep it rich", cases.length() >= 12)
        for (i in 0 until cases.length()) {
            val case = cases.getJSONObject(i)
            val base = case.getJSONArray("base")
            val want = case.getJSONArray("want")
            val got = RenderScale.apply(
                base.getInt(0),
                base.getInt(1),
                case.num("scale"),
                RenderScale.maxDimension(case.getString("codec")),
            )
            assertEquals(case.getString("name"), want.getInt(0) to want.getInt(1), got)
        }
    }
}
