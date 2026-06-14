// mTLS for the management REST API. The host now serves the API over HTTPS and authorizes a
// request whose client certificate is in its paired store (host commit b4a85a8) — the SAME
// identity + trust the QUIC data plane uses — so a paired client needs no bearer token.
//
// To present that identity, URLSession needs a SecIdentity (cert + private key pair). The client
// stores its identity as PEM (rcgen ECDSA P-256, PKCS#8 key). We rebuild a SecIdentity natively:
// CryptoKit parses the key → its X9.63 form → a SecKey, the cert PEM → a SecCertificate, and
// SecIdentityCreateWithCertificate pairs them via the Keychain. This is macOS-only
// (SecIdentityCreateWithCertificate is unavailable on iOS — that path will need a PKCS#12); the
// client library is macOS-first today.

import CryptoKit
import Foundation
import Security
import os

private let tlsLog = Logger(subsystem: "io.unom.punktfunk", category: "library-tls")

enum ClientTLS {
    enum TLSError: LocalizedError {
        case unsupportedPlatform
        case badKey(String)
        case badCert
        case identity(String)

        var errorDescription: String? {
            switch self {
            case .unsupportedPlatform:
                return "Library mTLS is supported on macOS only right now."
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
    /// Keychain (the key is stored once under a stable tag, so repeat calls reuse it).
    static func makeIdentity(certPEM: String, keyPEM: String) throws -> SecIdentity {
        #if os(macOS)
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

        // The key must live in a Keychain for SecIdentityCreateWithCertificate to pair it with the
        // cert. Add it under a stable tag; a duplicate just means a previous fetch already did.
        let tag = Data("io.unom.punktfunk.library-client-key".utf8)
        let add: [CFString: Any] = [
            kSecClass: kSecClassKey,
            kSecAttrApplicationTag: tag,
            kSecValueRef: secKey,
        ]
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess || status == errSecDuplicateItem else {
            throw TLSError.identity("keychain add failed (OSStatus \(status))")
        }

        var identity: SecIdentity?
        let idStatus = SecIdentityCreateWithCertificate(nil, cert, &identity)
        guard idStatus == errSecSuccess, let identity else {
            throw TLSError.identity("SecIdentityCreateWithCertificate (OSStatus \(idStatus))")
        }
        return identity
        #else
        throw TLSError.unsupportedPlatform
        #endif
    }
}

/// URLSession delegate that pins the host's self-signed cert (by the fingerprint the client
/// already trusts) and presents the client identity for the mTLS client-cert challenge.
final class LibraryTLSDelegate: NSObject, URLSessionDelegate {
    private let identity: SecIdentity
    private let pinnedHostFingerprint: Data? // SHA-256 of the host cert DER; nil = accept any (TOFU)

    init(identity: SecIdentity, pinnedHostFingerprint: Data?) {
        self.identity = identity
        self.pinnedHostFingerprint = pinnedHostFingerprint
    }

    func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        switch challenge.protectionSpace.authenticationMethod {
        case NSURLAuthenticationMethodServerTrust:
            // Pin the host cert by fingerprint — the host is self-signed (the client trusts it the
            // same way the QUIC session does). No pin yet (TOFU) → accept the presented leaf.
            guard let trust = challenge.protectionSpace.serverTrust,
                  let leaf = (SecTrustCopyCertificateChain(trust) as? [SecCertificate])?.first
            else {
                completionHandler(.cancelAuthenticationChallenge, nil)
                return
            }
            let der = SecCertificateCopyData(leaf) as Data
            let fp = Data(SHA256.hash(data: der))
            if let pinned = pinnedHostFingerprint, pinned != fp {
                tlsLog.warning("library: host cert fingerprint mismatch — refusing")
                completionHandler(.cancelAuthenticationChallenge, nil)
                return
            }
            completionHandler(.useCredential, URLCredential(trust: trust))

        case NSURLAuthenticationMethodClientCertificate:
            completionHandler(.useCredential,
                URLCredential(identity: identity, certificates: nil, persistence: .forSession))

        default:
            completionHandler(.performDefaultHandling, nil)
        }
    }
}
