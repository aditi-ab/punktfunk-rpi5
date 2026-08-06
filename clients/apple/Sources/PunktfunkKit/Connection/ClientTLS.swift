// mTLS for the management REST API. The host now serves the API over HTTPS and authorizes a
// request whose client certificate is in its paired store (host commit b4a85a8) — the SAME
// identity + trust the QUIC data plane uses — so a paired client needs no bearer token.
//
// To present that identity, URLSession needs a SecIdentity (cert + private key pair). The client
// stores its identity as PEM (rcgen ECDSA P-256, PKCS#8 key). We rebuild a SecIdentity natively:
// CryptoKit parses the key → its X9.63 form → a SecKey, the cert PEM → a SecCertificate. From
// there the two platform families diverge because `SecIdentityCreateWithCertificate` — the
// straight-line "pair these two" API — is macOS-only:
//   - macOS: SecIdentityCreateWithCertificate does the pairing directly once the key is in the
//     Keychain (a plain `SecItemAdd`).
//   - iOS/tvOS: that API is unavailable. Instead, add BOTH the key and the certificate to the
//     Keychain (under the same application tag) and query `kSecClassIdentity` — the system
//     correlates a stored cert against a stored key with a matching public key and vends the pair
//     as one `SecIdentity`, no PKCS#12 needed. This is the standard non-macOS technique for
//     "I already have a raw cert + key, not a .p12".

import CryptoKit
import Foundation
import Security

enum ClientTLS {
    enum TLSError: LocalizedError {
        case badKey(String)
        case badCert
        case identity(String)

        var errorDescription: String? {
            switch self {
            case .badKey(let why): return "Couldn't load the client key: \(why)"
            case .badCert: return "Couldn't load the client certificate."
            case .identity(let why): return "Couldn't build the client identity: \(why)"
            }
        }
    }

    /// First PEM block of `type` ("CERTIFICATE" / "PRIVATE KEY") → its DER bytes.
    private static func derFromPEM(_ pem: String, type: String) -> Data? {
        guard let start = pem.range(of: "-----BEGIN \(type)-----"),
              let end = pem.range(of: "-----END \(type)-----", range: start.upperBound..<pem.endIndex)
        else { return nil }
        let b64 = pem[start.upperBound..<end.lowerBound]
            .components(separatedBy: .whitespacesAndNewlines).joined()
        return Data(base64Encoded: b64)
    }

    /// Build a `SecIdentity` from the client's PEM cert + PKCS#8 P-256 key. Pairs them via the
    /// Keychain (stored once under a stable tag, so repeat calls reuse it).
    static func makeIdentity(certPEM: String, keyPEM: String) throws -> SecIdentity {
        // Key: CryptoKit accepts the SEC1 or PKCS#8 PEM; its x963 form is what SecKey wants.
        let priv: P256.Signing.PrivateKey
        do {
            priv = try P256.Signing.PrivateKey(pemRepresentation: keyPEM)
        } catch {
            throw TLSError.badKey(error.localizedDescription)
        }
        var keyError: Unmanaged<CFError>?
        let attrs: [CFString: Any] = [
            kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeyClass: kSecAttrKeyClassPrivate,
            kSecAttrKeySizeInBits: 256,
        ]
        guard let secKey = SecKeyCreateWithData(
            priv.x963Representation as CFData, attrs as CFDictionary, &keyError)
        else {
            throw TLSError.badKey((keyError?.takeRetainedValue()).map { "\($0)" } ?? "SecKeyCreateWithData")
        }

        guard let certDER = derFromPEM(certPEM, type: "CERTIFICATE"),
              let cert = SecCertificateCreateWithData(nil, certDER as CFData)
        else { throw TLSError.badCert }

        let tag = Data("io.unom.punktfunk.library-client-key".utf8)

        #if os(macOS)
        // The key must live in a Keychain for SecIdentityCreateWithCertificate to pair it with the
        // cert. Add it under a stable tag; a duplicate just means a previous fetch already did.
        let add: [CFString: Any] = [
            kSecClass: kSecClassKey,
            kSecAttrApplicationTag: tag,
            kSecValueRef: secKey,
        ]
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess || status == errSecDuplicateItem else {
            throw TLSError.identity("keychain add key failed (OSStatus \(status))")
        }

        var identity: SecIdentity?
        let idStatus = SecIdentityCreateWithCertificate(nil, cert, &identity)
        guard idStatus == errSecSuccess, let identity else {
            throw TLSError.identity("SecIdentityCreateWithCertificate (OSStatus \(idStatus))")
        }
        return identity
        #else
        // Add the key (tagged) and the certificate (matched to it by public key) separately —
        // a duplicate of either just means a previous fetch already added it.
        let addKey: [CFString: Any] = [
            kSecClass: kSecClassKey,
            kSecAttrApplicationTag: tag,
            kSecValueRef: secKey,
        ]
        let keyStatus = SecItemAdd(addKey as CFDictionary, nil)
        guard keyStatus == errSecSuccess || keyStatus == errSecDuplicateItem else {
            throw TLSError.identity("keychain add key failed (OSStatus \(keyStatus))")
        }

        let addCert: [CFString: Any] = [
            kSecClass: kSecClassCertificate,
            kSecValueRef: cert,
        ]
        let certStatus = SecItemAdd(addCert as CFDictionary, nil)
        guard certStatus == errSecSuccess || certStatus == errSecDuplicateItem else {
            throw TLSError.identity("keychain add certificate failed (OSStatus \(certStatus))")
        }

        // The system correlates the just-added cert against the tagged key (matching public key)
        // and vends the pair as a kSecClassIdentity — the tag filter here matches the KEY half.
        var identityRef: CFTypeRef?
        let query: [CFString: Any] = [
            kSecClass: kSecClassIdentity,
            kSecAttrApplicationTag: tag,
            kSecReturnRef: true,
        ]
        let idStatus = SecItemCopyMatching(query as CFDictionary, &identityRef)
        guard idStatus == errSecSuccess, let identityRef else {
            throw TLSError.identity("SecItemCopyMatching(kSecClassIdentity) (OSStatus \(idStatus))")
        }
        // Safe: a kSecClassIdentity query with kSecReturnRef always vends a SecIdentity.
        return (identityRef as! SecIdentity) // swiftlint:disable:this force_cast
        #endif
    }
}

// The URLSession pinning delegate that used to live here is gone: the management API now speaks
// over `MgmtTransport` (Network.framework), which states the same trust rule in a
// `sec_protocol_options_set_verify_block` and — unlike the URL loading system — is not subject to
// App Transport Security. That is what lets ATS stay ON for the cover-art CDN fetches, which are
// the only URLSession traffic left in the app. See MgmtTransport.swift for the full rationale.
