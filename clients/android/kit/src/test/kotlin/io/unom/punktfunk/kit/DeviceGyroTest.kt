package io.unom.punktfunk.kit

import android.view.Surface
import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * Pins the phone-gyro mirror's device→controller frame remap and its wire-unit constants
 * ([DeviceGyro]). Pure JVM: [Surface]'s ROTATION_* are compile-time constants and remap is
 * plain math. The matrix is derived (like the wire scale constants) — if on-glass says an axis
 * is wrong, fix [DeviceGyro.remap] AND these expectations together.
 * Run: `./gradlew :kit:testDebugUnitTest`.
 */
class DeviceGyroTest {
    /** A distinct value per axis so a swapped or flipped component can't cancel out. */
    private fun remap(rotation: Int) = DeviceGyro.remap(rotation, 1f, 2f, 3f).toList()

    @Test
    fun naturalPortraitIsIdentity() = assertEquals(listOf(1f, 2f, 3f), remap(Surface.ROTATION_0))

    @Test
    fun upsideDownFlipsInPlane() = assertEquals(listOf(-1f, -2f, 3f), remap(Surface.ROTATION_180))

    /** ROTATION_90 = device turned counter-clockwise, top to the player's LEFT:
     *  player-right = device-bottom (−y), player-up = device-right (+x); z never changes. */
    @Test
    fun rotation90TopLeft() = assertEquals(listOf(-2f, 1f, 3f), remap(Surface.ROTATION_90))

    /** ROTATION_270 = top to the player's RIGHT: player-right = +y, player-up = −x. */
    @Test
    fun rotation270TopRight() = assertEquals(listOf(2f, -1f, 3f), remap(Surface.ROTATION_270))

    /** Every remap stays a proper (right-handed) rotation: x̂ × ŷ = ẑ after mapping. */
    @Test
    fun handednessPreserved() {
        for (r in listOf(
            Surface.ROTATION_0, Surface.ROTATION_90, Surface.ROTATION_180, Surface.ROTATION_270,
        )) {
            val x = DeviceGyro.remap(r, 1f, 0f, 0f)
            val y = DeviceGyro.remap(r, 0f, 1f, 0f)
            assertEquals("left-handed remap at rotation $r", 1f, x[0] * y[1] - x[1] * y[0], 0f)
        }
    }

    /** The wire contract, shared with pf-client-core / the Swift client and now with every other
     *  Android motion sender ([Gamepad.motionGyroWire]): 20 LSB/°·s means 1 rad/s ⇒ ~1145.9 raw;
     *  1 g ⇒ 10000 raw. */
    @Test
    fun wireUnitConstants() {
        assertEquals(20f * 180f / Math.PI.toFloat(), Gamepad.MOTION_GYRO_LSB_PER_RAD_S, 0f)
        assertEquals(1145.9156f, Gamepad.MOTION_GYRO_LSB_PER_RAD_S, 0.001f)
        assertEquals(10_000, Gamepad.MOTION_ACCEL_LSB_PER_G)
    }
}
