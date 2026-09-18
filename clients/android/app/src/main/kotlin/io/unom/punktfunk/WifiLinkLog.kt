package io.unom.punktfunk

import android.net.wifi.WifiManager
import android.os.Build

/** One Wi-Fi link reading. `-1` is unknown; a frequency ≤ 0 means off Wi-Fi. */
internal data class WifiLink(
    val rssiDbm: Int,
    val txMbps: Int,
    val rxMbps: Int,
    val freqMhz: Int,
    val standard: Int,
)

/**
 * Which readings earn a `pf.wifi` line: the first on Wi-Fi, a frequency change (the access point
 * moved channel, the device roamed, or it left Wi-Fi), a transmit rate that halved or doubled since
 * the last line, and one every [PERIOD_MS] while on Wi-Fi. A line costs a log entry, a reading
 * does not, so readings can come often enough that a change lands within a second.
 */
internal class WifiLinkLog {
    private var logged: WifiLink? = null
    private var loggedAtMs = 0L

    /** Why [link] earns a line, or null; a non-null answer becomes the new baseline. */
    fun reason(link: WifiLink, nowMs: Long): String? {
        val last = logged
        val why = when {
            last == null -> if (link.freqMhz > 0) "start" else null
            link.freqMhz != last.freqMhz -> "channel"
            last.txMbps > 0 && link.txMbps > 0 &&
                (link.txMbps * 2 <= last.txMbps || link.txMbps >= last.txMbps * 2) -> "rate"
            link.freqMhz > 0 && nowMs - loggedAtMs >= PERIOD_MS -> "periodic"
            else -> null
        } ?: return null
        logged = link
        loggedAtMs = nowMs
        return why
    }

    companion object {
        const val PERIOD_MS = 10_000L
    }
}

/**
 * The link as the radio reports it now. `connectionInfo` is deprecated, but it still returns live
 * signal, rate and frequency without a location grant; SSID and BSSID come back redacted and are
 * not read.
 */
@Suppress("DEPRECATION")
internal fun WifiManager.readLink(): WifiLink {
    val info = connectionInfo ?: return WifiLink(-1, -1, -1, -1, 0)
    val q = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q
    return WifiLink(
        rssiDbm = info.rssi,
        txMbps = if (q) info.txLinkSpeedMbps else info.linkSpeed,
        rxMbps = if (q) info.rxLinkSpeedMbps else -1,
        freqMhz = info.frequency,
        standard = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) info.wifiStandard else 0,
    )
}
