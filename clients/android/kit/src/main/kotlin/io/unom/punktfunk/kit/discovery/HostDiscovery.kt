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
)

/** Field separator the native browse uses inside one record (ASCII Unit Separator). */
private const val FIELD_SEP = '\u001F'

/**
 * Parse one record from [NativeBridge.nativeDiscoveryPoll] (`key␟name␟addr␟port␟fp␟pair`), or null
 * if it's malformed. Pure — unit-tested without Android (see ParseRecordTest). The native side
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
    )
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
