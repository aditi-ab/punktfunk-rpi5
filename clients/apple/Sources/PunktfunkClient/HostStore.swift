// Saved hosts + their pinned identities, persisted as JSON in UserDefaults.
//
// Trust model (client side of punktfunk/1): the host serves a persistent certificate and
// logs its SHA-256 fingerprint at startup. The pin lands here one of two ways — the
// trust-on-first-use prompt (user compares the observed fingerprint against the host's
// log) or the SPAKE2 PIN pairing ceremony (PairSheet; mutually verified, and the host
// stores our identity from ClientIdentityStore in return). Every later connect passes
// the pin into punktfunk-core, which refuses a host whose identity changed. Hosts running
// --require-pairing only admit paired clients, so for them pairing is the only way in.

import Foundation
import PunktfunkKit
import SwiftUI

struct StoredHost: Identifiable, Codable, Hashable {
    var id = UUID()
    var name: String
    var address: String
    var port: UInt16 = 9777
    /// SHA-256 of the host's certificate, set after the user explicitly trusted it.
    var pinnedSHA256: Data?
    /// Last time a streaming session actually started (nil until the first one).
    var lastConnected: Date?
    /// Management-API port for the library browser (distinct from the data-plane `port`). Optional
    /// (NOT a defaulted non-optional) so older saved hosts — whose JSON lacks this key — still
    /// decode: synthesized Decodable ignores property defaults but treats a missing Optional as
    /// nil. Resolve via `effectiveMgmtPort`. (Auth is mTLS by the pinned identity — no token.)
    var mgmtPort: UInt16?

    var displayName: String { name.isEmpty ? address : name }
    var effectiveMgmtPort: UInt16 { mgmtPort ?? punktfunkDefaultMgmtPort }
}

extension StoredHost {
    /// True when a live mDNS advert (`DiscoveredHost`) describes THIS saved host — drives the
    /// "online" indicator and de-dupes the discovered section. Matched by certificate
    /// fingerprint when both sides carry it (so it survives a DHCP address change), otherwise
    /// by address:port. Online detection is LAN-scoped: a host not advertising on this network
    /// (off, or a remote/cross-subnet address) simply won't match — "not seen", not proven off.
    func matches(_ discovered: DiscoveredHost) -> Bool {
        if let pin = pinnedSHA256, let fp = discovered.fingerprintHex,
           pin.hexLower == fp.lowercased() {
            return true
        }
        return address == discovered.host && port == discovered.port
    }
}

private extension Data {
    /// Lowercase hex, no separators — to compare a pinned fingerprint against the mDNS `fp`.
    var hexLower: String { map { String(format: "%02x", $0) }.joined() }
}

@MainActor
final class HostStore: ObservableObject {
    private static let key = DefaultsKey.hosts

    @Published var hosts: [StoredHost] {
        didSet { persist() }
    }

    init() {
        if let data = UserDefaults.standard.data(forKey: Self.key),
           let decoded = try? JSONDecoder().decode([StoredHost].self, from: data) {
            hosts = decoded
        } else {
            hosts = []
        }
    }

    func add(_ host: StoredHost) {
        hosts.append(host)
    }

    func remove(_ host: StoredHost) {
        hosts.removeAll { $0.id == host.id }
    }

    func markConnected(_ hostID: UUID) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].lastConnected = Date()
    }

    func pin(_ hostID: UUID, fingerprint: Data) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].pinnedSHA256 = fingerprint
    }

    /// Drop the pinned identity (e.g. after a legitimate host reinstall). This does NOT downgrade
    /// to TOFU: the next connect re-pairs via the PIN ceremony, unless the host advertises
    /// `pair=optional` (the only case the connect path still offers the trust prompt).
    func forgetIdentity(_ host: StoredHost) {
        guard let i = hosts.firstIndex(where: { $0.id == host.id }) else { return }
        hosts[i].pinnedSHA256 = nil
    }


    private func persist() {
        if let data = try? JSONEncoder().encode(hosts) {
            UserDefaults.standard.set(data, forKey: Self.key)
        }
    }
}
