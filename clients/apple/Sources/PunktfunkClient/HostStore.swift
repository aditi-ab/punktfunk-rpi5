// Saved hosts + their pinned identities, persisted as JSON in UserDefaults.
//
// Trust model (client side of punktfunk/1): the host serves a persistent certificate and
// logs its SHA-256 fingerprint at startup. First connect is trust-on-first-use — the user
// explicitly confirms the observed fingerprint against the host's log, and we pin it here.
// Every later connect passes the pin into punktfunk-core, which refuses a host whose
// identity changed. (Host→client authorization — a pairing PIN — is a roadmap item; today
// the host accepts any client that can reach its port.)

import Foundation
import SwiftUI

struct StoredHost: Identifiable, Codable, Hashable {
    var id = UUID()
    var name: String
    var address: String
    var port: UInt16 = 9777
    /// SHA-256 of the host's certificate, set after the user explicitly trusted it.
    var pinnedSHA256: Data?

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
