// The client's persistent identity + the SPAKE2 PIN pairing ceremony — the trust
// bootstrap that precedes any pinned PunktfunkConnection.

import Foundation
import PunktfunkCore

/// This client's persistent self-signed identity. Generate ONCE with `generateIdentity()`,
/// store both PEMs (Keychain), present on every connect — the certificate's fingerprint is
/// how hosts recognize this client after pairing.
public struct ClientIdentity: Sendable {
    public let certPEM: String
    public let keyPEM: String
    public init(certPEM: String, keyPEM: String) {
        self.certPEM = certPEM
        self.keyPEM = keyPEM
    }
}

/// Generate a fresh client identity (self-signed cert + key, PEM).
public func generateIdentity() throws -> ClientIdentity {
    var cert = [CChar](repeating: 0, count: 4096)
    var key = [CChar](repeating: 0, count: 4096)
    let rc = punktfunk_generate_identity(&cert, UInt(cert.count), &key, UInt(key.count))
    guard rc == PUNKTFUNK_STATUS_OK.rawValue else {
        throw PunktfunkClientError.status(rc)
    }
    return ClientIdentity(certPEM: String(cString: cert), keyPEM: String(cString: key))
}

/// Run the PIN pairing ceremony: the host displays a 4-digit PIN (its log/UI), the user
/// types it here. On success the host stores this client's identity and the returned
/// fingerprint is the host's now-VERIFIED identity — persist it and pass it as `pinSHA256`
/// to every later connect. Throws `.wrongPIN` when the proof is rejected.
public func pair(
    host: String, port: UInt16 = 9777,
    identity: ClientIdentity, pin: String, name: String,
    timeoutMs: UInt32 = 90_000
) throws -> Data {
    var observed = [UInt8](repeating: 0, count: 32)
    // The C header types PunktfunkStatus as a bare int32 (C17, no enum import), so the ABI
    // functions return Int32 directly — compare against the enum constants' rawValue, the
    // same bridging the connection methods use (statusOK etc.).
    let rc = host.withCString { cs in
        identity.certPEM.withCString { cert in
            identity.keyPEM.withCString { key in
                pin.withCString { p in
                    name.withCString { n in
                        punktfunk_pair(cs, port, cert, key, p, n, &observed, timeoutMs)
                    }
                }
            }
        }
    }
    switch rc {
    case PUNKTFUNK_STATUS_OK.rawValue: return Data(observed)
    case PUNKTFUNK_STATUS_CRYPTO.rawValue: throw PunktfunkClientError.wrongPIN
    default: throw PunktfunkClientError.status(rc)
    }
}
