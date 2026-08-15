package io.unom.punktfunk

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * [sc2BluetoothGrantOffered] is pure — table-tested over its inputs.
 *
 * The rule exists because a Steam Controller 2 paired over Bluetooth is invisible to this client
 * until `BLUETOOTH_CONNECT` is granted (the bonded list answers "nothing is paired" rather than
 * refusing), and nothing ever asked for it — so the pad silently never engaged while the same
 * controller over USB worked. The offer has to reach those users without becoming a Bluetooth
 * prompt for everyone else, which is the whole content of these assertions.
 */
class Sc2BluetoothGrantTest {

    /** The reported case: an SC2 sitting in lizard mode that we cannot capture. */
    @Test
    fun offeredWhenAnSc2IsAttachedButBluetoothIsNot() {
        assertTrue(
            sc2BluetoothGrantOffered(
                permissionGranted = false,
                usbSc2 = false,
                sc2Attached = true,
                anyPadDetected = false,
            ),
        )
        // Still offered next to other working pads — the SC2 is the one we can't reach.
        assertTrue(
            sc2BluetoothGrantOffered(
                permissionGranted = false,
                usbSc2 = false,
                sc2Attached = true,
                anyPadDetected = true,
            ),
        )
    }

    /**
     * The probe reads an SC2's USB identity, which we cannot assume a BLE stack reports. When it
     * misses, "no controller detected" is exactly when a blind spot is worth naming.
     */
    @Test
    fun offeredWhenNothingWasDetectedAtAll() {
        assertTrue(
            sc2BluetoothGrantOffered(
                permissionGranted = false,
                usbSc2 = false,
                sc2Attached = false,
                anyPadDetected = false,
            ),
        )
    }

    /** Never a prompt for someone with working controllers and no sign of an SC2. */
    @Test
    fun notOfferedToUsersWithNoSignOfAnSc2() {
        assertFalse(
            sc2BluetoothGrantOffered(
                permissionGranted = false,
                usbSc2 = false,
                sc2Attached = false,
                anyPadDetected = true,
            ),
        )
    }

    /** Granting it changes nothing that is already captured over USB — wired and Puck alike. */
    @Test
    fun notOfferedWhenTheSc2IsOnUsb() {
        assertFalse(
            sc2BluetoothGrantOffered(
                permissionGranted = false,
                usbSc2 = true,
                sc2Attached = true,
                anyPadDetected = false,
            ),
        )
    }

    /** Nothing to ask for once it is held — including on releases that grant it at install time. */
    @Test
    fun notOfferedOncePermitted() {
        for (attached in listOf(true, false)) {
            for (pads in listOf(true, false)) {
                assertFalse(
                    sc2BluetoothGrantOffered(
                        permissionGranted = true,
                        usbSc2 = false,
                        sc2Attached = attached,
                        anyPadDetected = pads,
                    ),
                )
            }
        }
    }
}
