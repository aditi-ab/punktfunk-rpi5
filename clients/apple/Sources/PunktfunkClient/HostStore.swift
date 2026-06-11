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

    var displayName: String { name.isEmpty ? address : name }
}

@MainActor
final class HostStore: ObservableObject {
    private static let key = "punktfunk.hosts"

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

    /// Drop the pinned identity (e.g. after a legitimate host reinstall) — the next
    /// connect goes through the trust prompt again.
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
