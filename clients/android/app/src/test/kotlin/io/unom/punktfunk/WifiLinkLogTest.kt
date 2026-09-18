package io.unom.punktfunk

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

/** When a Wi-Fi reading earns a `pf.wifi` line. */
class WifiLinkLogTest {
    private val base = WifiLink(rssiDbm = -55, txMbps = 866, rxMbps = 780, freqMhz = 5180, standard = 5)

    @Test
    fun theFirstReadingOnWifiLogsAndOffWifiNothingDoes() {
        assertNull(WifiLinkLog().reason(base.copy(freqMhz = -1), 0))
        assertEquals("start", WifiLinkLog().reason(base, 0))
    }

    @Test
    fun aChannelMoveOrLeavingWifiLogsAtOnce() {
        val log = WifiLinkLog()
        log.reason(base, 0)
        assertEquals("channel", log.reason(base.copy(freqMhz = 5500), 1_000))
        assertEquals("channel", log.reason(base.copy(freqMhz = -1), 2_000))
        assertNull("off Wi-Fi is not periodic", log.reason(base.copy(freqMhz = -1), 60_000))
    }

    @Test
    fun onlyAHalvedOrDoubledRateLogsBeforeThePeriod() {
        val log = WifiLinkLog()
        log.reason(base, 0)
        assertNull(log.reason(base.copy(txMbps = 600, rssiDbm = -70), 1_000))
        assertEquals("rate", log.reason(base.copy(txMbps = 433), 2_000))
        assertNull("the baseline moved to 433", log.reason(base.copy(txMbps = 300), 3_000))
        assertEquals("rate", log.reason(base.copy(txMbps = 866), 4_000))
    }

    @Test
    fun aSteadyLinkLogsOncePerPeriod() {
        val log = WifiLinkLog()
        log.reason(base, 0)
        assertNull(log.reason(base, WifiLinkLog.PERIOD_MS - 1))
        assertEquals("periodic", log.reason(base, WifiLinkLog.PERIOD_MS))
        assertNull(log.reason(base, WifiLinkLog.PERIOD_MS + 1_000))
    }
}
