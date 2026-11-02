package io.unom.punktfunk

import io.unom.punktfunk.kit.discovery.DiscoveredHost
import io.unom.punktfunk.kit.discovery.osIconTokens
import io.unom.punktfunk.kit.discovery.sanitizeOsChain
import io.unom.punktfunk.kit.security.KnownHost
import java.io.File
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * `clients/shared/advert-vectors.json` against what the Android client makes of an mDNS advert:
 * which saved host it is ([KnownHost.matches]) and the OS chain it carries — the same rows the
 * Rust `discovery::same_host` / `os` and the Apple client run, so a dual-boot box and a junk
 * `os` TXT read the same on every client.
 */
class AdvertVectorTest {
    private val vectors: JSONObject by lazy {
        // Gradle runs unit tests with the module dir as cwd (clients/android/app).
        val file = File("../../shared/advert-vectors.json")
        assertTrue("the shared vector file must be reachable at ${file.absolutePath}", file.isFile)
        JSONObject(file.readText())
    }

    private fun JSONArray.objects() = List(length()) { getJSONObject(it) }
    private fun JSONArray.strings() = List(length()) { getString(it) }

    @Test
    fun everySharedMatchAgrees() {
        val cases = vectors.getJSONArray("match").objects()
        assertTrue("the vector file is the contract; keep it rich", cases.size >= 10)
        for (case in cases) {
            val name = case.getString("name")
            val saved = case.getJSONObject("saved")
            val advert = case.getJSONObject("advert")
            val host = KnownHost(
                address = saved.getString("addr"),
                port = saved.getInt("port"),
                name = name,
                fpHex = saved.getString("fp"),
                paired = saved.getString("fp").isNotEmpty(),
            )
            val seen = DiscoveredHost(
                key = name,
                name = name,
                host = advert.getString("addr"),
                port = advert.getInt("port"),
                // The native browse leaves the field empty for a host with no pin.
                fingerprint = advert.getString("fp").ifEmpty { null },
            )
            assertEquals(name, case.getBoolean("same"), host.matches(seen))
        }
    }

    @Test
    fun everySharedOsChainAgrees() {
        val chains = vectors.getJSONArray("os_chain").objects()
        assertTrue("the vector file is the contract; keep it rich", chains.size >= 10)
        for (case in chains) {
            val raw = case.getString("raw")
            assertEquals(raw, case.getString("expect"), sanitizeOsChain(raw))
        }
        for (case in vectors.getJSONArray("os_icon_tokens").objects()) {
            val chain = case.getString("chain")
            assertEquals(chain, case.getJSONArray("expect").strings(), osIconTokens(chain))
        }
    }
}
