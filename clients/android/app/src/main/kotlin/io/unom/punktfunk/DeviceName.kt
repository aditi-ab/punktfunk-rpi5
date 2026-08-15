package io.unom.punktfunk

import android.content.Context
import android.os.Build
import android.provider.Settings

/**
 * The name the user knows this device by — what a host shows in its pending-approval list (the web
 * console's outstanding-pairings view and the dialog that approves a knock) and files the device
 * under once approved.
 *
 * `Settings.Global.DEVICE_NAME` is the name the user typed in Settings ("Enrico's Pixel", "TV im
 * Wohnzimmer"); it is what every other protocol on the network already calls this device. Only when
 * it is unset does this fall back to [Build.MODEL], which names the *product* and so reads
 * identically on every unit of it — two of the same tablet pending approval are indistinguishable.
 * Available unconditionally here: `DEVICE_NAME` landed in API 25 and this app's floor is 28.
 */
internal fun deviceName(context: Context): String {
    val userNamed = runCatching {
        Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
    }.getOrNull()
    return userNamed?.trim()?.takeIf { it.isNotEmpty() }
        ?: Build.MODEL?.trim()?.takeIf { it.isNotEmpty() }
        ?: "Android"
}
