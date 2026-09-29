package io.unom.punktfunk.kit

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pins [rumblesOnBody], the rule behind [bodyRumbleFor]: only a controller built into the device
 * with no motor of its own rumbles through the device body.
 * Run: `./gradlew :kit:testDebugUnitTest`.
 */
class BodyRumbleTest {
    @Test
    fun motorlessBuiltInPadRumblesOnTheBody() =
        assertTrue(rumblesOnBody(padHasMotor = false, external = false))

    @Test
    fun aPadWithAMotorKeepsItsOwn() {
        assertFalse(rumblesOnBody(padHasMotor = true, external = false))
        assertFalse(rumblesOnBody(padHasMotor = true, external = true))
    }

    @Test
    fun aMotorlessExternalPadStaysSilent() =
        assertFalse(rumblesOnBody(padHasMotor = false, external = true))
}
