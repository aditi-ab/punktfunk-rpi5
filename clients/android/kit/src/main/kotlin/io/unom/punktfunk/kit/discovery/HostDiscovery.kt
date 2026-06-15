package io.unom.punktfunk.kit.discovery

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.util.Log

private const val TAG = "PunktfunkNsd"

/** DNS-SD service type punktfunk hosts advertise (host: `_punktfunk._udp.local.`). */
const val PUNKTFUNK_SERVICE_TYPE = "_punktfunk._udp"
const val PUNKTFUNK_PROTO = "punktfunk/1"

/** One resolved host fit for the picker. [key] is the stable dedup id. */
data class DiscoveredHost(
    val key: String,
    val name: String,
    val host: String,
    val port: Int,
    val fingerprint: String? = null, // TXT "fp" (host cert SHA-256, advisory — TOFU still verifies)
    val pairingRequired: Boolean = false,
)

/** Parsed TXT fields. Pure — unit-testable without Android (see ParseTxtTest). */
data class TxtFields(
    val proto: String?,
    val fp: String?,
    val pair: String?,
    val id: String?,
) {
    val pairingRequired: Boolean get() = pair == "required"
    val isPunktfunk: Boolean get() = proto == PUNKTFUNK_PROTO
}

/**
 * Pure TXT parser. NSD hands TXT as a `Map<String, ByteArray?>` (a null/empty value = present-but-
 * empty key). Decode UTF-8; missing keys are null, never an error.
 */
fun parseTxt(attrs: Map<String, ByteArray?>): TxtFields {
    fun s(k: String): String? = attrs[k]?.takeIf { it.isNotEmpty() }?.toString(Charsets.UTF_8)
    return TxtFields(proto = s("proto"), fp = s("fp"), pair = s("pair"), id = s("id"))
}

/**
 * Browses `_punktfunk._udp` via NsdManager, resolves each service (the reliable
 * `registerServiceInfoCallback` path on API 34+, legacy `resolveService` on 31–33 where its TXT is
 * often empty), and pushes the live host set to [onChange] (invoked on the main thread).
 *
 * Lifecycle: [start] when the picker appears, [stop] when it leaves / on connect — holds a
 * MulticastLock while running (an OEM Wi-Fi power-save hedge). Note: the Android emulator's SLIRP
 * NAT drops multicast, so on the emulator discovery starts but never finds a LAN host.
 */
class HostDiscovery(context: Context) {
    private val appCtx = context.applicationContext
    private val nsd = appCtx.getSystemService(Context.NSD_SERVICE) as NsdManager

    /** Invoked on the main thread whenever the resolved host set changes. */
    var onChange: ((List<DiscoveredHost>) -> Unit)? = null

    private val resolved = LinkedHashMap<String, DiscoveredHost>() // key -> host
    private var multicastLock: WifiManager.MulticastLock? = null
    private var discoveryListener: NsdManager.DiscoveryListener? = null
    private val infoCallbacks = mutableListOf<NsdManager.ServiceInfoCallback>() // API 34+ registrations
    private var running = false

    @Synchronized
    fun start() {
        if (running) return
        running = true
        acquireMulticastLock()
        val listener = makeDiscoveryListener()
        discoveryListener = listener
        runCatching {
            nsd.discoverServices(PUNKTFUNK_SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
        }.onFailure {
            Log.e(TAG, "discoverServices failed", it)
            stop()
        }
    }

    @Synchronized
    fun stop() {
        if (!running) return
        running = false
        discoveryListener?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        discoveryListener = null
        if (Build.VERSION.SDK_INT >= 34) {
            for (cb in infoCallbacks) runCatching { nsd.unregisterServiceInfoCallback(cb) }
        }
        infoCallbacks.clear()
        releaseMulticastLock()
        resolved.clear()
        onChange?.invoke(emptyList())
    }

    private fun publish() {
        onChange?.invoke(resolved.values.sortedBy { it.name.lowercase() })
    }

    private fun makeDiscoveryListener() = object : NsdManager.DiscoveryListener {
        override fun onDiscoveryStarted(type: String) {
            Log.d(TAG, "discovery started: $type")
        }
        override fun onDiscoveryStopped(type: String) {
            Log.d(TAG, "discovery stopped: $type")
        }
        override fun onStartDiscoveryFailed(type: String, code: Int) {
            Log.e(TAG, "start discovery failed: $code")
            runCatching { nsd.stopServiceDiscovery(this) }
        }
        override fun onStopDiscoveryFailed(type: String, code: Int) {
            Log.e(TAG, "stop discovery failed: $code")
        }

        override fun onServiceFound(info: NsdServiceInfo) {
            Log.d(TAG, "found: ${info.serviceName}")
            resolve(info)
        }
        override fun onServiceLost(info: NsdServiceInfo) {
            Log.d(TAG, "lost: ${info.serviceName}")
            // onServiceLost carries no TXT, so drop by the instance-name fallback key only.
            if (resolved.remove(info.serviceName) != null) publish()
        }
    }

    private fun resolve(found: NsdServiceInfo) {
        if (Build.VERSION.SDK_INT >= 34) resolveViaCallback(found) else resolveViaLegacy(found)
    }

    private fun resolveViaCallback(found: NsdServiceInfo) {
        val cb = object : NsdManager.ServiceInfoCallback {
            override fun onServiceUpdated(info: NsdServiceInfo) = ingest(info)
            override fun onServiceLost() {}
            override fun onServiceInfoCallbackRegistrationFailed(code: Int) {
                Log.e(TAG, "ServiceInfoCallback reg failed: $code")
            }
            override fun onServiceInfoCallbackUnregistered() {}
        }
        runCatching {
            nsd.registerServiceInfoCallback(found, appCtx.mainExecutor, cb)
            infoCallbacks.add(cb)
        }.onFailure { Log.e(TAG, "registerServiceInfoCallback failed", it) }
    }

    private fun resolveViaLegacy(found: NsdServiceInfo) {
        // A ResolveListener can't be reused — allocate one per resolve. TXT may be empty pre-34.
        val listener = object : NsdManager.ResolveListener {
            override fun onServiceResolved(info: NsdServiceInfo) = ingest(info)
            override fun onResolveFailed(info: NsdServiceInfo, code: Int) {
                Log.e(TAG, "resolve failed: $code")
            }
        }
        runCatching { nsd.resolveService(found, listener) }
            .onFailure { Log.e(TAG, "resolveService failed", it) }
    }

    @Suppress("DEPRECATION") // info.host is deprecated at API 34 (replaced by hostAddresses)
    private fun ingest(info: NsdServiceInfo) {
        val txt = parseTxt(info.attributes)
        // Reject an incompatible protocol IF the host advertised one; tolerate empty TXT (pre-34).
        if (txt.proto != null && !txt.isPunktfunk) {
            Log.d(TAG, "skip non-punktfunk proto=${txt.proto}")
            return
        }
        val ip = (if (Build.VERSION.SDK_INT >= 34) info.hostAddresses.firstOrNull() else info.host)
            ?.hostAddress ?: return
        val key = txt.id?.takeIf { it.isNotBlank() } ?: info.serviceName
        resolved[key] = DiscoveredHost(
            key = key,
            name = info.serviceName.removeSuffix("."),
            host = ip,
            port = info.port,
            fingerprint = txt.fp,
            pairingRequired = txt.pairingRequired,
        )
        Log.d(TAG, "resolved: ${resolved[key]}")
        publish()
    }

    private fun acquireMulticastLock() {
        val wifi = appCtx.getSystemService(Context.WIFI_SERVICE) as WifiManager
        multicastLock = wifi.createMulticastLock("punktfunk-nsd").apply {
            setReferenceCounted(true)
            runCatching { acquire() }
        }
    }

    private fun releaseMulticastLock() {
        multicastLock?.takeIf { it.isHeld }?.let { runCatching { it.release() } }
        multicastLock = null
    }
}
