package io.unom.punktfunk.kit.discovery

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Handler
import android.os.Looper
import android.util.Log
import io.unom.punktfunk.kit.NativeBridge

private const val TAG = "PunktfunkMdns"

/** One resolved host fit for the picker. [key] is the stable dedup id. */
data class DiscoveredHost(
    val key: String,
    val name: String,
    val host: String,
    val port: Int,
    val fingerprint: String? = null, // TXT "fp" (host cert SHA-256, advisory — TOFU still verifies)
    val pairingRequired: Boolean = false,
    val mac: List<String> = emptyList(), // TXT "mac" (wake-capable NIC MAC(s), for Wake-on-LAN)
    val os: String = "", // TXT "os" (OS-identity chain, e.g. "linux/fedora/bazzite"); "" on older hosts
)

/** Field separator the native browse uses inside one record (ASCII Unit Separator). */
private const val FIELD_SEP = '\u001F'

/**
 * Parse one record from [NativeBridge.nativeDiscoveryPoll] (`key␟name␟addr␟port␟fp␟pair␟mac␟os`),
 * or null if it's malformed. Fields past the 6th are optional — an older native lib omits them
 * (`mac` 7th, `os` 8th). Pure — unit-tested without Android (see ParseRecordTest). The native side
 * already applied the protocol gate and address selection, so this is just field marshaling.
 */
fun parseHostRecord(record: String): DiscoveredHost? {
    val f = record.split(FIELD_SEP)
    if (f.size < 6) return null
    val addr = f[2]
    val port = f[3].toIntOrNull() ?: return null
    if (addr.isBlank() || port !in 1..65535) return null
    return DiscoveredHost(
        key = f[0].ifBlank { "$addr:$port" },
        name = f[1].ifBlank { addr },
        host = addr,
        port = port,
        fingerprint = f[4].ifBlank { null },
        pairingRequired = f[5] == "required",
        mac = if (f.size > 6) f[6].split(",").map { it.trim() }.filter { it.isNotEmpty() }
        else emptyList(),
        os = if (f.size > 7) sanitizeOsChain(f[7]) else "",
    )
}

/**
 * Reduce a raw `os` TXT value to the trusted grammar (pf-client-core's `sanitize_os`, mirrored):
 * lowercase slash-separated tokens of `[a-z0-9._-]`, each ≤ 32 chars, at most 5. mDNS is
 * unauthenticated input; a value that sanitizes to nothing becomes "" (no icon, like an older host).
 */
fun sanitizeOsChain(raw: String): String =
    raw.lowercase()
        .split('/')
        .map { token -> token.filter { it in 'a'..'z' || it in '0'..'9' || it in "._-" }.take(32) }
        .filter { it.isNotEmpty() }
        .take(5)
        .joinToString("/")

/**
 * The icon-lookup order for a chain: sanitized tokens most-specific-first, brand aliases applied
 * (`macos` → `apple` art, `steamos` → `steam` art) — pf-client-core's `os_icon_tokens`, mirrored.
 * The UI takes the first token it has art for; empty means "no OS icon" (older host / garbage).
 */
fun osIconTokens(chain: String): List<String> =
    sanitizeOsChain(chain)
        .split('/')
        .filter { it.isNotEmpty() }
        .reversed()
        .map {
            when (it) {
                "macos" -> "apple"
                "steamos" -> "steam"
                else -> it
            }
        }

/**
 * Browses `_punktfunk._udp` for punktfunk/1 hosts via the native `mdns-sd` core (the same browse the
 * Linux/Windows clients use), exposed over JNI — *not* `NsdManager`, whose per-OEM system daemon
 * made discovery "mostly broken". [start] spins up the native browse and polls it ~1 Hz on the main
 * thread, pushing the live host set to [onChange] (also on the main thread, only when it changes);
 * [stop] tears it down.
 *
 * We hold a Wi-Fi [WifiManager.MulticastLock] for the browse lifetime — raw multicast *reception*
 * needs it. (The Android emulator's SLIRP NAT drops multicast, so on the emulator discovery starts
 * but never finds a LAN host — same as before; that's the network, not the API.)
 */
class HostDiscovery(context: Context) {
    private val appCtx = context.applicationContext

    /** Invoked on the main thread whenever the resolved host set changes. */
    var onChange: ((List<DiscoveredHost>) -> Unit)? = null

    private val handler = Handler(Looper.getMainLooper())
    private var multicastLock: WifiManager.MulticastLock? = null
    private var nativeHandle = 0L
    private var running = false
    private var last: List<DiscoveredHost> = emptyList()

    private val poll = object : Runnable {
        override fun run() {
            if (!running) return
            val hosts = snapshot()
            if (hosts != last) {
                last = hosts
                onChange?.invoke(hosts)
            }
            handler.postDelayed(this, POLL_MS)
        }
    }

    fun start() {
        if (running) return
        acquireMulticastLock()
        val h = runCatching { NativeBridge.nativeDiscoveryStart() }
            .onFailure { Log.e(TAG, "nativeDiscoveryStart threw", it) }
            .getOrDefault(0L)
        if (h == 0L) {
            Log.e(TAG, "native mDNS discovery failed to start")
            releaseMulticastLock()
            return
        }
        nativeHandle = h
        running = true
        last = emptyList()
        handler.post(poll)
    }

    /**
     * Tear the browse down and start a fresh one. This is the manual rescan, and the recovery path
     * for a browse that started while blocked (permission not yet granted, multicast filtered) or
     * that never started at all ([start] gives up when `nativeDiscoveryStart` returns 0, and
     * nothing else would ever retry it).
     *
     * It also puts a query back on the wire: `mdns-sd` re-queries on a doubling backoff that caps
     * at an hour, so a long-lived browse is effectively passive — a host that appeared since, or
     * whose announcement was lost to multicast, may never be asked for again.
     *
     * The currently-shown host set is left alone across the swap (rather than blinking empty via
     * [stop]'s notification); the first poll of the new browse publishes the fresh set.
     */
    fun restart() {
        val keep = onChange
        onChange = null
        stop()
        onChange = keep
        start()
    }

    fun stop() {
        if (!running && nativeHandle == 0L) return
        running = false
        handler.removeCallbacks(poll)
        val h = nativeHandle
        nativeHandle = 0L
        if (h != 0L) runCatching { NativeBridge.nativeDiscoveryStop(h) }
            .onFailure { Log.e(TAG, "nativeDiscoveryStop threw", it) }
        releaseMulticastLock()
        last = emptyList()
        onChange?.invoke(emptyList())
    }

    private fun snapshot(): List<DiscoveredHost> {
        val h = nativeHandle
        if (h == 0L) return emptyList()
        // getOrNull (not getOrDefault): the JNI returns a platform String!, so a (near-impossible)
        // native null is a *success* value here — coalesce it so the main-thread poll can't NPE.
        val blob = runCatching { NativeBridge.nativeDiscoveryPoll(h) }
            .onFailure { Log.e(TAG, "nativeDiscoveryPoll threw", it) }
            .getOrNull() ?: ""
        if (blob.isEmpty()) return emptyList()
        return blob.split('\n')
            .filter { it.isNotBlank() }
            .mapNotNull { parseHostRecord(it) }
            .associateBy { it.key } // dedup by stable key (id, or addr:port)
            .values
            .sortedBy { it.name.lowercase() }
    }

    private fun acquireMulticastLock() {
        val wifi = appCtx.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("punktfunk-mdns").apply {
            setReferenceCounted(true)
            runCatching { acquire() }
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.takeIf { it.isHeld }?.let { runCatching { it.release() } }
        multicastLock = null
    }

    private companion object {
        const val POLL_MS = 1000L
    }
}
