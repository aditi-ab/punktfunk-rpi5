package io.unom.punktfunk.kit.security

import android.content.Context
import org.json.JSONObject

/**
 * A host the user has trusted (pinned). [fpHex] is the pinned host-cert SHA-256 (64-hex); [paired]
 * is true when trust was established via the SPAKE2 PIN ceremony (vs trust-on-first-use).
 */
data class KnownHost(
    val address: String,
    val port: Int,
    val name: String,
    val fpHex: String,
    val paired: Boolean,
)

/**
 * Persists trusted hosts — the pinned-fingerprint store *and* the saved-hosts list — keyed by
 * `address:port`. Replaces the old fp-only PinStore so a discovered and a manually-typed connection
 * to the same host share one trust record (and so saved hosts can be listed + reconnected). Plain
 * `SharedPreferences` in app-private storage: pinned fingerprints are public host identities, not
 * secrets; the property we need is integrity, which app sandboxing provides.
 */
class KnownHostStore(context: Context) {
    private val prefs =
        context.applicationContext.getSharedPreferences("punktfunk_hosts", Context.MODE_PRIVATE)

    // The pref key is just a unique id; address/port are also stored in the value so an IPv6
    // address (which contains colons) round-trips without parsing the key.
    private fun key(address: String, port: Int) = "$address:$port"

    /** The trusted record for [address]:[port], or `null` if this host has never been trusted. */
    fun get(address: String, port: Int): KnownHost? =
        prefs.getString(key(address, port), null)?.let(::parse)

    /** Pin (or update) a trusted host — upsert by `address:port`. */
    fun save(host: KnownHost) {
        val json = JSONObject()
            .put("addr", host.address)
            .put("port", host.port)
            .put("name", host.name)
            .put("fp", host.fpHex.lowercase())
            .put("paired", host.paired)
        prefs.edit().putString(key(host.address, host.port), json.toString()).apply()
    }

    /** Forget [address]:[port] (the next connect re-pairs / re-TOFUs). */
    fun remove(address: String, port: Int) {
        prefs.edit().remove(key(address, port)).apply()
    }

    /** Set a saved host's display name, keeping its pin + paired flag. No-op if not saved. */
    fun rename(address: String, port: Int, newName: String) {
        val h = get(address, port) ?: return
        save(h.copy(name = newName))
    }

    /** All trusted hosts, name-sorted — backs the saved-hosts list. */
    fun all(): List<KnownHost> =
        prefs.all.values.mapNotNull { (it as? String)?.let(::parse) }.sortedBy { it.name.lowercase() }

    private fun parse(s: String): KnownHost? = runCatching {
        val j = JSONObject(s)
        KnownHost(
            address = j.getString("addr"),
            port = j.getInt("port"),
            name = j.getString("name"),
            fpHex = j.getString("fp"),
            paired = j.optBoolean("paired", false),
        )
    }.getOrNull()
}
