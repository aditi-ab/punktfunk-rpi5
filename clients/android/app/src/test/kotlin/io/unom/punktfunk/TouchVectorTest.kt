package io.unom.punktfunk

import java.io.File
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/touch-vectors.json` against [TouchTuning] and [pointerAccel] — the tuning the
 * Apple gesture layer runs too, so a flick crosses the screen the same distance on both.
 */
class TouchVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/touch-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    @Test
    fun theTuningMatchesTheVector() {
        val t = vectors.getJSONObject("tuning")
        assertEquals(t.getLong("tap_drag_ms"), TouchTuning.tapDragMs)
        assertEquals(t.getLong("long_press_ms"), TouchTuning.longPressMs)
        val floats = mapOf(
            "pointer_sens" to TouchTuning.pointerSens,
            "accel_gain" to TouchTuning.accelGain,
            "accel_speed_floor" to TouchTuning.accelSpeedFloor,
            "accel_max" to TouchTuning.accelMax,
            "keyboard_swipe_fraction" to TouchTuning.keyboardSwipeFraction,
            "dial_arm_deg" to TouchTuning.dialArmDeg,
            "dial_commit_deg" to TouchTuning.dialCommitDeg,
            "dial_slop_in_tap_slops" to TouchTuning.dialSlopInTapSlops,
        )
        for ((key, got) in floats) {
            assertEquals(key, t.getDouble(key), got.toDouble(), 1e-6)
        }
    }

    @Test
    fun everySharedGainAgrees() {
        val rows = vectors.getJSONArray("accel")
        assertTrue("the vector file is the contract; keep it rich", rows.length() >= 8)
        for (i in 0 until rows.length()) {
            val row = rows.getJSONObject(i)
            val speed = row.getDouble("speed").toFloat()
            assertEquals("speed $speed", row.getDouble("expect"), pointerAccel(speed).toDouble(), 1e-4)
        }
    }
}
