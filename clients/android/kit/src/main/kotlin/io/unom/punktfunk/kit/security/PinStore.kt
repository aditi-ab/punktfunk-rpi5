package io.unom.punktfunk.kit.security

import android.content.Context

/**
 * Persists the trusted host fingerprint per host id (TOFU pinning / completed pairing). Keyed by the
 * mDNS instance id (`DiscoveredHost.key`) or `"host:port"` for a manually-typed host. Values are
 * lowercase 64-hex SHA-256. Plain `SharedPreferences` in app-private storage — pins are not secrets
 * (they're public host fingerprints); the security property is integrity, which app sandboxing gives.
 */
class PinStore(context: Context) {
    private val prefs =
        context.applicationContext.getSharedPreferences("punktfunk_pins", Context.MODE_PRIVATE)

    /** The pinned fingerprint for [hostId], or `null` if this host has never been trusted. */
    fun get(hostId: String): String? = prefs.getString(hostId, null)

    /** Pin (or re-pin) [hostId] to [fpHex]. Normalizes to lowercase. */
    fun pin(hostId: String, fpHex: String) {
        prefs.edit().putString(hostId, fpHex.lowercase()).apply()
    }

    /** Forget [hostId]'s pin (so the next connect re-TOFUs / re-pairs). */
    fun remove(hostId: String) {
        prefs.edit().remove(hostId).apply()
    }
}
