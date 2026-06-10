// This client's persistent punktfunk/1 identity: a self-signed certificate + key (PEM),
// generated once and stored in the login Keychain. The certificate's fingerprint is how
// hosts recognize this client after PIN pairing — losing the key un-pairs this Mac from
// every host, so the pair is presented on every connect but never regenerated once
// stored. That invariant drives the error handling below: a Keychain that *refuses
// access* (locked, ACL denied) is an error, not a first run — minting a replacement
// would silently shadow the durable identity and break every existing pairing.

import Foundation
import PunktfunkKit
import Security

final class ClientIdentityStore: @unchecked Sendable {
    static let shared = ClientIdentityStore()

    enum IdentityError: Error {
        /// The Keychain refused access (locked, ACL denied, …) — an identity may exist.
        case keychain(OSStatus)
        /// The identity lives only in memory (Keychain write failed); good enough to
        /// present on a connect, not good enough to pair against.
        case notPersisted
    }

    private let lock = NSLock()
    private var cached: (identity: ClientIdentity, persisted: Bool)?

    /// The identity to present when connecting, generating + persisting it on first run.
    /// `persisted == false` means the Keychain write failed and it lives only in memory —
    /// fine for a session, see `loadForPairing()` for the strict variant. Blocking
    /// (Keychain + key generation) — call off the main actor.
    func load() throws -> (identity: ClientIdentity, persisted: Bool) {
        lock.lock()
        defer { lock.unlock() }
        if let cached { return cached }

        switch copyStored() {
        case .found(let identity):
            let hit = (identity, true)
            cached = hit
            return hit
        case .absent:
            break // genuine first run — mint below
        case .corrupt:
            // Our own item, undecodable: the pairings it backed are unusable either
            // way, so deliberately self-heal by replacing it.
            SecItemDelete(Self.query as CFDictionary)
        case .denied(let status):
            throw IdentityError.keychain(status)
        }

        let fresh = try generateIdentity()
        let entry: (ClientIdentity, Bool)
        switch add(fresh) {
        case errSecSuccess:
            entry = (fresh, true)
        case errSecDuplicateItem:
            // Lost a first-run race with another instance — the stored identity is the
            // durable one, never overwrite it.
            if case .found(let identity) = copyStored() {
                entry = (identity, true)
            } else {
                entry = (fresh, false)
            }
        default:
            entry = (fresh, false)
        }
        cached = entry
        return entry
    }

    /// Pairing variant: the host is about to durably trust this identity, so it must be
    /// durable on our side too — a memory-only identity would evaporate on relaunch and
    /// strand the pairing.
    func loadForPairing() throws -> ClientIdentity {
        let (identity, persisted) = try load()
        guard persisted else { throw IdentityError.notPersisted }
        return identity
    }

    private struct Stored: Codable {
        var certPEM: String
        var keyPEM: String
    }

    private enum ReadResult {
        case found(ClientIdentity)
        case absent
        case corrupt
        case denied(OSStatus)
    }

    private static let query: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: "io.unom.punktfunk",
        kSecAttrAccount as String: "client-identity",
    ]

    private func copyStored() -> ReadResult {
        var query = Self.query
        query[kSecReturnData as String] = true
        var out: CFTypeRef?
        switch SecItemCopyMatching(query as CFDictionary, &out) {
        case errSecSuccess:
            guard let data = out as? Data,
                  let stored = try? JSONDecoder().decode(Stored.self, from: data)
            else { return .corrupt }
            return .found(ClientIdentity(certPEM: stored.certPEM, keyPEM: stored.keyPEM))
        case errSecItemNotFound:
            return .absent
        case let status:
            return .denied(status)
        }
    }

    private func add(_ identity: ClientIdentity) -> OSStatus {
        guard let data = try? JSONEncoder().encode(
            Stored(certPEM: identity.certPEM, keyPEM: identity.keyPEM))
        else { return errSecParam }
        var add = Self.query
        add[kSecValueData as String] = data
        return SecItemAdd(add as CFDictionary, nil)
    }
}
