package io.unom.punktfunk.kit.library

import okhttp3.OkHttpClient
import okhttp3.Request
import org.json.JSONArray
import org.json.JSONObject
import java.io.ByteArrayInputStream
import java.security.KeyFactory
import java.security.KeyStore
import java.security.MessageDigest
import java.security.PrivateKey
import java.security.cert.CertificateFactory
import java.security.cert.X509Certificate
import java.security.spec.PKCS8EncodedKeySpec
import java.util.Base64
import java.util.concurrent.TimeUnit
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.HttpsURLConnection
import javax.net.ssl.KeyManagerFactory
import javax.net.ssl.SSLContext
import javax.net.ssl.TrustManager
import javax.net.ssl.TrustManagerFactory
import javax.net.ssl.X509TrustManager

// Android game-library client — the mirror of the Apple client's LibraryClient.swift. Fetches a
// host's unified game library from its management REST API (`GET /api/v1/library`) over **mTLS**: the
// paired client presents its persistent cert/key (the same identity the host paired over QUIC), and
// the host's self-signed cert is pinned by SHA-256(DER). Read-only. Mirrors the GameEntry/Artwork
// schema in crates/punktfunk-host/src/library.rs.

/** The management API's default port — matches `mgmt::DEFAULT_PORT` on the host and the Apple client. */
const val DEFAULT_MGMT_PORT = 47990

/** Cover-art URLs. Steam art arrives as host-relative proxy paths, resolved to absolute by [LibraryClient]. */
data class Artwork(val portrait: String?, val header: String?, val hero: String?) {
    /** Poster preference for a 2:3 tile: portrait capsule → header → hero (near-universal fallbacks). */
    val posterCandidates: List<String> get() = listOfNotNull(portrait, header, hero)
}

/** One title in the unified library. [id] is store-qualified (`steam:<appid>` / `custom:<id>`). */
data class GameEntry(val id: String, val store: String, val title: String, val art: Artwork) {
    val isCustom: Boolean get() = store == "custom"
}

/** Fetch outcome — three states so the UI can guide setup (the common case is "not paired yet"). */
sealed class LibraryResult {
    data class Ok(val games: List<GameEntry>) : LibraryResult()
    data class Unauthorized(val message: String) : LibraryResult()
    data class Error(val message: String) : LibraryResult()
}

object LibraryClient {
    /**
     * `GET https://<address>:<mgmtPort>/api/v1/library`, authenticated by mTLS. [fpHex] is the pinned
     * host-cert SHA-256 (64 hex, from the paired [io.unom.punktfunk.kit.security.KnownHost]); a blank
     * value means the host was never connected/paired, so there's nothing authorized to browse.
     * BLOCKING — call from a background dispatcher.
     */
    fun fetch(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
    ): LibraryResult {
        if (fpHex.isBlank()) {
            return LibraryResult.Unauthorized(
                "Connect to this host once first — the library uses the identity created on pairing to authenticate.",
            )
        }
        val client = try {
            mtlsHttpClient(certPem, keyPem, address, fpHex)
        } catch (e: Exception) {
            return LibraryResult.Error("Couldn't set up the secure connection: ${e.message}")
        }
        val base = "https://$address:$mgmtPort"
        val req = Request.Builder().url("$base/api/v1/library").build()
        return try {
            client.newCall(req).execute().use { resp ->
                when (resp.code) {
                    200 -> LibraryResult.Ok(parse(resp.body?.string().orEmpty(), base))
                    401 -> LibraryResult.Unauthorized(
                        "The host didn't recognize this device. Pair with the host first — it authorizes paired clients by their certificate.",
                    )
                    else -> LibraryResult.Error("The management API returned HTTP ${resp.code}.")
                }
            }
        } catch (e: Exception) {
            LibraryResult.Error(
                "Couldn't reach the host's management API: ${e.message}. It binds the LAN by default, so check the host is updated and reachable.",
            )
        }
    }

    private fun parse(json: String, base: String): List<GameEntry> {
        val arr = JSONArray(json)
        val out = ArrayList<GameEntry>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.getJSONObject(i)
            val art = o.optJSONObject("art") ?: JSONObject()
            out.add(
                GameEntry(
                    id = o.optString("id"),
                    store = o.optString("store"),
                    title = o.optString("title"),
                    art = Artwork(
                        portrait = resolveArt(str(art, "portrait"), base),
                        header = resolveArt(str(art, "header"), base),
                        hero = resolveArt(str(art, "hero"), base),
                    ),
                ),
            )
        }
        return out
    }

    /** A present, non-null, non-blank JSON string field, else null. */
    private fun str(o: JSONObject, key: String): String? =
        if (o.has(key) && !o.isNull(key)) o.optString(key).ifBlank { null } else null

    /** Host-relative art path (`/api/v1/library/art/...`) → absolute against the host; else unchanged. */
    private fun resolveArt(s: String?, base: String): String? =
        if (s != null && s.startsWith("/")) base + s else s
}

/**
 * An OkHttpClient that presents the paired client cert and pins the host's self-signed cert by
 * SHA-256(DER) — reused for BOTH the library fetch and the cover-art loads (so a paired client
 * reaches the host's own art proxy). The pinning trust manager trusts the host by fingerprint and
 * defers to normal public trust for any other origin (an external CDN URL); the hostname verifier
 * accepts the pinned host (whose self-signed cert has no matching SAN) and defers otherwise.
 */
fun mtlsHttpClient(certPem: String, keyPem: String, host: String, fpHex: String): OkHttpClient {
    val clientCert = CertificateFactory.getInstance("X.509")
        .generateCertificate(ByteArrayInputStream(certPem.toByteArray())) as X509Certificate
    val privateKey = parsePrivateKey(keyPem)

    val keyStore = KeyStore.getInstance("PKCS12").apply {
        load(null, null)
        setKeyEntry("client", privateKey, CharArray(0), arrayOf(clientCert))
    }
    val kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm())
    kmf.init(keyStore, CharArray(0))

    // System default trust manager, for non-host (external CDN) origins.
    val sysTmf = TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm())
    sysTmf.init(null as KeyStore?)
    val sysTm = sysTmf.trustManagers.filterIsInstance<X509TrustManager>().first()

    val pinned = fpHex.lowercase()
    val trustManager = object : X509TrustManager {
        override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {}
        override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
            if (sha256Hex(chain[0].encoded) == pinned) return // the pinned host
            sysTm.checkServerTrusted(chain, authType) // external CDN — normal public trust
        }
        override fun getAcceptedIssuers(): Array<X509Certificate> = sysTm.acceptedIssuers
    }

    val ssl = SSLContext.getInstance("TLS")
    ssl.init(kmf.keyManagers, arrayOf<TrustManager>(trustManager), null)

    val defaultVerifier = HttpsURLConnection.getDefaultHostnameVerifier()
    val verifier = HostnameVerifier { hostname, session ->
        hostname == host || defaultVerifier.verify(hostname, session)
    }

    return OkHttpClient.Builder()
        .sslSocketFactory(ssl.socketFactory, trustManager)
        .hostnameVerifier(verifier)
        .connectTimeout(8, TimeUnit.SECONDS)
        .readTimeout(15, TimeUnit.SECONDS)
        .build()
}

/** Parse a PKCS#8 PEM private key (rcgen emits `-----BEGIN PRIVATE KEY-----`), trying EC then RSA/Ed25519. */
private fun parsePrivateKey(pem: String): PrivateKey {
    val body = pem
        .replace(Regex("-----BEGIN [A-Z ]*PRIVATE KEY-----"), "")
        .replace(Regex("-----END [A-Z ]*PRIVATE KEY-----"), "")
        .replace(Regex("\\s"), "")
    val der = Base64.getDecoder().decode(body)
    val spec = PKCS8EncodedKeySpec(der)
    for (alg in listOf("EC", "RSA", "Ed25519")) {
        try {
            return KeyFactory.getInstance(alg).generatePrivate(spec)
        } catch (_: Exception) {
            // try the next algorithm
        }
    }
    throw IllegalArgumentException("unsupported private-key format (not EC/RSA/Ed25519 PKCS#8)")
}

private fun sha256Hex(der: ByteArray): String =
    MessageDigest.getInstance("SHA-256").digest(der).joinToString("") { "%02x".format(it) }
