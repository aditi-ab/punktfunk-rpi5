package io.unom.punktfunk.kit

import android.view.Surface
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/device-gyro-vectors.json` against [DeviceGyro.remap] and the wire scale — the
 * same rows the Apple `DeviceGyroRemap` runs, so a phone clipped landscape yaws the host's pad
 * the same way on both. Run: `./gradlew :kit:testDebugUnitTest`.
 */
class DeviceGyroVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/kit).
        val file = File("../../shared/device-gyro-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun JSONArray.floats() = FloatArray(length()) { getDouble(it).toFloat() }

    /** The pose is where the device's top points, as the player sees it. */
    private fun rotation(pose: String) = when (pose) {
        "top_up" -> Surface.ROTATION_0
        "top_down" -> Surface.ROTATION_180
        "top_left" -> Surface.ROTATION_90
        "top_right" -> Surface.ROTATION_270
        else -> error("a pose this test does not know: $pose")
    }

    @Test
    fun everySharedRemapAgrees() {
        val rows = vectors.getJSONArray("remap")
        assertTrue("the vector file is the contract; keep it rich", rows.length() >= 12)
        for (i in 0 until rows.length()) {
            val row = rows.getJSONObject(i)
            val pose = row.getString("pose")
            val input = row.getJSONArray("input").floats()
            val want = row.getJSONArray("expect").floats()
            val got = DeviceGyro.remap(rotation(pose), input[0], input[1], input[2])
            for (axis in 0..2) {
                assertEquals("$pose ${input.toList()} axis $axis", want[axis], got[axis], 0f)
            }
        }
    }

    @Test
    fun theWireScaleMatchesTheVector() {
        val units = vectors.getJSONObject("wire_units")
        assertEquals(
            units.getDouble("gyro_lsb_per_rad_s"),
            Gamepad.MOTION_GYRO_LSB_PER_RAD_S.toDouble(),
            1e-3,
        )
        assertEquals(
            units.getDouble("accel_lsb_per_g"),
            Gamepad.MOTION_ACCEL_LSB_PER_G.toDouble(),
            1e-3,
        )
    }
}
