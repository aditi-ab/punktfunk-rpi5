package io.unom.punktfunk.kit.security

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import android.util.Log
import io.unom.punktfunk.kit.NativeBridge
import java.io.File
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

private const val TAG = "PunktfunkIdentity"

/** The delimiter the JNI uses to join the two PEMs; collision-free (PEM bodies never contain it). */
private const val PEM_DELIM = "\n-----PUNKTFUNK-KEY-----\n"

/** This device's persistent punktfunk identity (presented to hosts via TLS client auth). */
data class ClientIdentity(val certPem: String, val privateKeyPem: String)

/** Result of [IdentityStore.load] — four states so the caller never mints over a *recoverable* error. */
sealed interface IdentityLoad {
    data class Ok(val identity: ClientIdentity) : IdentityLoad

    /** Genuine first run (no blob on disk) — mint a new identity here, and only here. */
    object Absent : IdentityLoad

    /** A blob exists but can't be decrypted (Keystore key gone, corruption). NEVER shadow-mint. */
    data class Unrecoverable(val reason: String, val cause: Throwable?) : IdentityLoad
}

class IdentityUnrecoverableException(message: String, cause: Throwable?) : Exception(message, cause)

/** Split the JNI's joined "<cert>\n-----PUNKTFUNK-KEY-----\n<key>" blob; `null` if malformed. */
fun splitGenerated(joined: String): ClientIdentity? {
    val i = joined.indexOf(PEM_DELIM)
    if (i < 0) return null
    return ClientIdentity(
        certPem = joined.substring(0, i),
        privateKeyPem = joined.substring(i + PEM_DELIM.length),
    )
}

/**
 * Load the device identity, minting *once* on genuine first run. NEVER mints over an error state:
 * an [IdentityLoad.Unrecoverable] surfaces as a throw so the UI can tell the user (re-pair) rather
 * than silently swapping in a new identity (which would change our fingerprint everywhere).
 */
fun obtainIdentity(store: IdentityStore): ClientIdentity =
    when (val r = store.load()) {
        is IdentityLoad.Ok -> r.identity
        IdentityLoad.Absent -> {
            val joined = NativeBridge.nativeGenerateIdentity()
            val id = splitGenerated(joined)
                ?: throw IdentityUnrecoverableException("nativeGenerateIdentity returned empty", null)
            store.persist(id)
            id
        }
        is IdentityLoad.Unrecoverable ->
            throw IdentityUnrecoverableException(r.reason, r.cause)
    }

/**
 * Persists the identity PEM blob to app-private storage, wrapped with an AndroidKeyStore AES-256-GCM
 * key (never exportable; StrongBox-backed where available, TEE otherwise). On-disk layout:
 * `[12-byte IV][GCM ciphertext+tag]`. The wrapping key never leaves the secure element, and Keystore
 * keys don't survive backup/restore — so a restored device reads [IdentityLoad.Absent] (the blob is
 * excluded from backup; see the manifest) and re-mints, rather than carrying a dead identity.
 */
class IdentityStore(context: Context) {
    private val appCtx = context.applicationContext
    private val file = File(appCtx.filesDir, "pf_identity.bin")
    private val alias = "punktfunk_identity_v1"

    fun load(): IdentityLoad {
        if (!file.exists()) return IdentityLoad.Absent
        return try {
            val blob = file.readBytes()
            if (blob.size <= IV_LEN) {
                return IdentityLoad.Unrecoverable("identity blob truncated (${blob.size} B)", null)
            }
            val key = (keyStore().getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.secretKey
                ?: return IdentityLoad.Unrecoverable("blob present but Keystore key missing", null)
            val iv = blob.copyOfRange(0, IV_LEN)
            val ct = blob.copyOfRange(IV_LEN, blob.size)
            val cipher = Cipher.getInstance(TRANSFORM)
            cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(GCM_TAG_BITS, iv))
            val plain = String(cipher.doFinal(ct), Charsets.UTF_8)
            splitGenerated(plain)?.let { IdentityLoad.Ok(it) }
                ?: IdentityLoad.Unrecoverable("decrypted identity blob malformed", null)
        } catch (e: Exception) {
            // Decrypt/Keystore failure: the identity is unrecoverable. Do NOT mint a shadow identity.
            Log.e(TAG, "identity load failed", e)
            IdentityLoad.Unrecoverable("identity decrypt failed: ${e.javaClass.simpleName}", e)
        }
    }

    fun persist(identity: ClientIdentity) {
        val key = getOrCreateKey()
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.ENCRYPT_MODE, key)
        val iv = cipher.iv // GCM: a fresh random 12-byte IV per encryption
        val plain = (identity.certPem + PEM_DELIM + identity.privateKeyPem).toByteArray(Charsets.UTF_8)
        val ct = cipher.doFinal(plain)
        // Write to a temp file then rename, so a crash mid-write can't leave a torn (unrecoverable) blob.
        val tmp = File(file.parentFile, "${file.name}.tmp")
        tmp.writeBytes(iv + ct)
        if (!tmp.renameTo(file)) {
            file.writeBytes(iv + ct)
            tmp.delete()
        }
    }

    private fun keyStore(): KeyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }

    private fun getOrCreateKey(): SecretKey {
        val ks = keyStore()
        (ks.getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        // Prefer a StrongBox-backed key; fall back to TEE where StrongBox is absent (e.g. the emulator).
        return try {
            generateKey(strongBox = true)
        } catch (e: StrongBoxUnavailableException) {
            Log.i(TAG, "StrongBox unavailable — using TEE-backed key", e)
            generateKey(strongBox = false)
        }
    }

    private fun generateKey(strongBox: Boolean): SecretKey {
        val spec = KeyGenParameterSpec.Builder(
            alias,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setIsStrongBoxBacked(strongBox)
            .build()
        val kg = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        kg.init(spec)
        return kg.generateKey()
    }

    private companion object {
        const val TRANSFORM = "AES/GCM/NoPadding"
        const val IV_LEN = 12
        const val GCM_TAG_BITS = 128
    }
}
