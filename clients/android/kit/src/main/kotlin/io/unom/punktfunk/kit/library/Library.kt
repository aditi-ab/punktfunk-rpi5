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

/**
 * One title in the unified library. [id] is store-qualified (`steam:<appid>` / `custom:<id>`).
 *
 * [role] is `"game"` (the default, and what an older host omits) or `"launcher"` — an entry that
 * opens the launcher itself (Steam Big Picture, Heroic) rather than a title. Kept a plain nullable
 * String on purpose: the host owns the vocabulary, and an unknown future value must degrade to a
 * game rather than break the decode (design D4).
 *
 * [icon] is the token for the entry's brand mark (`"steam"`, `"heroic"`) — never art, never a URL.
 * Null on every older host and on every ordinary title.
 */
data class GameEntry(
    val id: String,
    val store: String,
    val title: String,
    val art: Artwork,
    val role: String? = null,
    val icon: String? = null,
    /**
     * The host's platform tag (`platform` in the catalog — a ROM manager's console name; Steam
     * sets none). What the console's Collections group by; carried through verbatim.
     */
    val platform: String? = null,
) {
    val isCustom: Boolean get() = store == "custom"

    /** Whether this entry opens a launcher rather than a game. */
    val isLauncher: Boolean get() = role == "launcher"

    /**
     * The brand-icon token, re-validated rather than taken on trust.
     *
     * The host checks the shape on the way in, so this only fires for a host older than that
     * check or one that isn't ours. It costs a scan of a short string and means no consumer has
     * to wonder what it is about to look up.
     */
    val iconToken: String? get() = icon?.takeIf { t ->
        t.isNotEmpty() && t.length <= 32 && t[0] in 'a'..'z' &&
            t.all { it in 'a'..'z' || it in '0'..'9' || it == '-' }
    }

    /**
     * Display name for the store badge — the same table the other clients use
     * (`pf-console-ui::library::store_label`). Before this the UI said "Steam" for every non-custom
     * entry, which a Lutris or GOG title made a lie.
     */
    val storeLabel: String get() = when (store) {
        "steam" -> "Steam"
        "custom" -> "Custom"
        "heroic" -> "Heroic"
        "lutris" -> "Lutris"
        "epic" -> "Epic"
        "gog" -> "GOG"
        "xbox" -> "Xbox"
        else -> "Game"
    }
}

/**
 * Design D4: launcher entries lead the shelf, keeping the host's title order within each group.
 * Applied once where the library is fetched, so no screen has to remember the rule — and a library
 * without launcher entries comes back untouched.
 */
fun List<GameEntry>.launchersFirst(): List<GameEntry> {
    val launchers = filter { it.isLauncher }
    return if (launchers.isEmpty()) this else launchers + filterNot { it.isLauncher }
}

/** Fetch outcome — three states so the UI can guide setup (the common case is "not paired yet"). */
sealed class LibraryResult {
    data class Ok(val games: List<GameEntry>) : LibraryResult()
    data class Unauthorized(val message: String) : LibraryResult()
    data class Error(val message: String) : LibraryResult()

    /**
     * Is this the "can't reach it" failure — the only one worth waiting out?
     *
     * A rejected certificate does not become acceptable by retrying, and asking an unpaired host
     * twelve times only delays telling the user what is actually wrong. Lives here rather than at
     * the call site so the retry loop and the error copy can never disagree about which failures
     * are transient.
     */
    val isTransient: Boolean get() = this is Error
}

/**
 * One game the host currently has launched, from `GET /api/v1/status`.
 *
 * A deliberately partial mirror of the host's `ActiveGame`: only the fields a client can act on.
 * The web console's view of this payload carries more (which session, which plane, the grace
 * countdown), and none of that is a player's business from the library shelf.
 */
data class RunningGame(
    /**
     * Store-qualified library id (`steam:570`) — the key that lines this up with a [GameEntry].
     * Null for an operator-typed GameStream command, which has no catalog entry behind it.
     */
    val appId: String?,
    val title: String,
    /**
     * `launching` | `running` | `exited` | `untracked` | `grace`. A plain String on purpose: the
     * host owns the vocabulary and adds to it (`untracked` arrived in 0.30), so an unknown value
     * must never fail the decode of the whole list.
     */
    val state: String,
) {
    /**
     * Is this title *up on the host right now* — i.e. would picking it take the player back into
     * it rather than start it?
     *
     * `untracked` counts: the host cannot follow that process, but it did launch it and has no
     * evidence it stopped. `grace` counts too — its session is gone but the game is still running,
     * which is precisely the case where getting back in promptly matters most. Only a confirmed
     * `exited` does not.
     */
    val isUp: Boolean get() = state != "exited"
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

    /**
     * What the host currently has running, from `GET /api/v1/status`.
     *
     * Same lane, same identity, no new host work: `/status` is already on the paired-certificate
     * allowlist (the host's `mgmt::auth::cert_may_access`) alongside `/library`, and has carried a
     * `games[]` array since the session⇄game lifetime work. This client simply never read it — so
     * a player had no way to see, from the device they browse on, that something was already up.
     *
     * **Best-effort by contract**: an older host, an unreachable one, or a shape we don't recognize
     * yields an empty list rather than an error. Nothing here is worth failing a library screen
     * over — the worst case is a Resume badge that doesn't appear. BLOCKING; call from IO.
     */
    fun fetchRunning(
        address: String,
        mgmtPort: Int = DEFAULT_MGMT_PORT,
        certPem: String,
        keyPem: String,
        fpHex: String,
    ): List<RunningGame> {
        if (fpHex.isBlank()) return emptyList()
        return try {
            val client = mtlsHttpClient(certPem, keyPem, address, fpHex)
            val req = Request.Builder().url("https://$address:$mgmtPort/api/v1/status").build()
            client.newCall(req).execute().use { resp ->
                if (resp.code != 200) return emptyList()
                parseRunning(resp.body?.string().orEmpty())
            }
        } catch (_: Exception) {
            emptyList()
        }
    }

    /** Just the `games[]` slice of `/status`; everything else on that payload is the console's. */
    private fun parseRunning(json: String): List<RunningGame> {
        val arr = JSONObject(json).optJSONArray("games") ?: return emptyList()
        val out = ArrayList<RunningGame>(arr.length())
        for (i in 0 until arr.length()) {
            val o = arr.optJSONObject(i) ?: continue
            out.add(
                RunningGame(
                    appId = str(o, "app_id"),
                    title = o.optString("title"),
                    state = o.optString("state"),
                ),
            )
        }
        return out
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
                    role = str(o, "role"),
                    icon = str(o, "icon"),
                    platform = str(o, "platform"),
                ),
            )
        }
        return out.launchersFirst()
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
 * defers to normal public trust for any other origin (an external CDN URL).
 *
 * The two checks are only sound TOGETHER, and the composition is the point: the trust manager
 * cannot fail closed on its own (it has no hostname, so it must let a CDN chain through), so the
 * hostname verifier is what makes the pinned host pin-only. Loosen either and a publicly-trusted
 * certificate for any name is accepted for the host — which is exactly what 2026-08-05 review M-2
 * found. The host's own cert is self-signed with no matching SAN, so it can never satisfy the
 * default verifier; the pin is its only credential, on purpose.
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
        if (hostname == host) {
            // The PINNED host fails closed: only the pinned leaf is acceptable for this name.
            //
            // This used to be a bare `hostname == host`, which composed with the trust manager's
            // system-CA fall-through into "any publicly-trusted certificate, for any name, is
            // accepted for the pinned host" — the pin was decorative (2026-08-05 review M-2). A
            // MITM with any free CA-issued cert intercepted the connection, received the client's
            // mTLS IDENTITY certificate, and served attacker-chosen library JSON and art URLs.
            // The Rust (`pf-client-core`) and Apple (`ClientTLS`) paths already fail closed here;
            // only Android did not.
            try {
                sha256Hex((session.peerCertificates.firstOrNull() as? X509Certificate)?.encoded ?: return@HostnameVerifier false) == pinned
            } catch (_: Exception) {
                false
            }
        } else {
            // Any other origin (an external CDN art URL) is ordinary public trust: the system
            // trust manager validated the chain, and this checks the name against it.
            defaultVerifier.verify(hostname, session)
        }
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
