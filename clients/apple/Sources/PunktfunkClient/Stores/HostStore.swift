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
#if canImport(WidgetKit)
import WidgetKit
#endif

// `StoredHost` (the model + its JSON codec) now lives in PunktfunkShared so the widget extension
// can read the same store; PunktfunkKit re-exports it. The discovery-join helpers below stay here
// because they reference PunktfunkKit's `DiscoveredHost`/`HostDiscovery`.

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

/// The two joins of live mDNS discovery against the saved-host store, shared by the touch grid
/// (HomeView) and the gamepad launcher (GamepadHomeView) so both screens classify hosts the same
/// way. LAN-scoped like the underlying match: a host that isn't advertising here is "not seen",
/// not proven off.
extension HostDiscovery {
    /// A saved host is "online" iff a live advert currently matches it (see `StoredHost.matches`).
    /// Recomputed on every discovery change (the @Published set), so it tracks hosts
    /// appearing/leaving the network live.
    func advertises(_ host: StoredHost) -> Bool {
        hosts.contains { host.matches($0) }
    }

    /// Discovered hosts not already saved — the saved list shows the rest, so this only surfaces
    /// genuinely-new hosts on the network. Same match as `advertises`, so a saved host whose IP
    /// changed (still fingerprint-matched) doesn't also appear as a stranger.
    func unsaved(among saved: [StoredHost]) -> [DiscoveredHost] {
        hosts.filter { d in !saved.contains { $0.matches(d) } }
    }
}

@MainActor
final class HostStore: ObservableObject {
    private static let key = DefaultsKey.hosts

    @Published var hosts: [StoredHost] {
        didSet { persist() }
    }

    /// Saved hosts proven reachable by the periodic QUIC probe (by id) — the mDNS-independent
    /// counterpart to discovery presence, OR'd into the "online" pip so a routed/VPN host that
    /// never advertises still reads Online. Not persisted (it's live reachability, not config).
    @Published var probedOnline: Set<StoredHost.ID> = []

    /// The App-Group suite — shared with the Widget/Live-Activity extension so a launcher widget
    /// sees the same saved hosts. Falls back to `.standard` in an un-entitled process (see
    /// `AppGroup.defaults`).
    private let defaults = AppGroup.defaults

    init() {
        Self.migrateToAppGroupIfNeeded()
        if let data = defaults.data(forKey: Self.key),
           let decoded = try? JSONDecoder().decode([StoredHost].self, from: data) {
            hosts = decoded
        } else {
            hosts = []
        }
    }

    /// One-time move of the saved-host JSON from `UserDefaults.standard` (where every build before
    /// the App Group wrote it) into the shared suite. Idempotent: only fires when the suite has no
    /// hosts yet but standard does. The old value is LEFT in place — during a staged TestFlight
    /// rollout an older build still reads `.standard`, so tombstoning it now would hide hosts from
    /// the not-yet-updated app. Remove the standard copy a release later.
    private static func migrateToAppGroupIfNeeded() {
        let suite = AppGroup.defaults
        let standard = UserDefaults.standard
        guard suite !== standard else { return } // un-entitled fallback: nothing to migrate
        guard suite.data(forKey: key) == nil,
              let legacy = standard.data(forKey: key) else { return }
        suite.set(legacy, forKey: key)
    }

    func add(_ host: StoredHost) {
        var host = host
        // Stamped here rather than in the initializer: a `StoredHost` is also built to describe a
        // host we are only dialing (the dev auto-connect hook, a deep link's confirmation), and
        // those were never added to anything.
        host.addedAt = host.addedAt ?? Date()
        hosts.append(host)
    }

    func remove(_ host: StoredHost) {
        hosts.removeAll { $0.id == host.id }
    }

    /// Replace a saved host in place (the edit sheet) — matched by id, so identity/pin/last-connected
    /// carried on the passed value are preserved.
    func update(_ host: StoredHost) {
        guard let i = hosts.firstIndex(where: { $0.id == host.id }) else { return }
        hosts[i] = host
    }

    func markConnected(_ hostID: UUID) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].lastConnected = Date() // didSet → persist() writes the shared suite + reloads widget
    }

    /// One reachability sweep, driving `probedOnline`: probe every saved host NOT currently
    /// advertising on `discovery` (mDNS already answers for those) off the main actor via a bounded,
    /// trust-agnostic QUIC handshake, then publish the reachable set. This is the mDNS-independent
    /// half of presence — a host reached over a routed network (Tailscale/VPN) never advertises but
    /// answers here. Call in a loop from a home view's `.task` (cancelled on disappear).
    func refreshReachability(discovery: HostDiscovery) async {
        let targets = hosts.filter { !discovery.advertises($0) }
        var online: Set<StoredHost.ID> = []
        for host in targets {
            let reachable = await Task.detached(priority: .utility) {
                PunktfunkConnection.probe(host: host.address, port: host.port)
            }.value
            if reachable { online.insert(host.id) }
        }
        probedOnline = online
    }

    func pin(_ hostID: UUID, fingerprint: Data) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].pinnedSHA256 = fingerprint
    }

    /// Learn/refresh this host's Wake-on-LAN MAC(s) from its live advert (called while the host is
    /// awake, so the client can wake it once it sleeps). No-op when unchanged, so it doesn't churn
    /// UserDefaults on every discovery tick.
    func updateMacs(_ hostID: UUID, macs: [String]) {
        guard !macs.isEmpty,
              let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].macAddresses != macs else { return }
        hosts[i].macAddresses = macs
    }

    /// Learn/refresh this host's OS-identity chain from its live advert — same contract as
    /// [`updateMacs`]: no-op when empty or unchanged, so discovery ticks don't churn UserDefaults.
    func updateOsChain(_ hostID: UUID, chain: String) {
        guard !chain.isEmpty,
              let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].osChain != chain else { return }
        hosts[i].osChain = chain
    }

    /// Learn/refresh this host's management-API port from its live advert — same contract as
    /// `updateMacs`. Until this existed, `StoredHost.mgmtPort` was declared and read but never
    /// written, so `effectiveMgmtPort` always answered 47990 and a host that had moved its mgmt
    /// port simply had no working library here.
    func updateMgmtPort(_ hostID: UUID, port: UInt16?) {
        guard let port, port > 0,
              let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].mgmtPort != port else { return }
        hosts[i].mgmtPort = port
    }

    /// Bind this host to a settings profile, or to "Default settings" (nil) — the ONLY way the
    /// default changes. A one-off "Connect with ▸" deliberately never lands here (§5.2:
    /// predictable, not sticky).
    func setProfile(_ hostID: UUID, profileID: String?) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].profileID = profileID
    }

    /// Pin or unpin a host+profile combo as its own card (§5.2a). Presentation only: it never
    /// touches the default binding or the profile itself. nil stays out of the saved JSON when
    /// nothing is pinned, so the widget contract sees no new key for the common case.
    func setPinned(_ hostID: UUID, profileID: String, pinned: Bool) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        var pins = hosts[i].pinnedProfileIDs ?? []
        pins.removeAll { $0 == profileID }
        if pinned { pins.append(profileID) }
        hosts[i].pinnedProfileIDs = pins.isEmpty ? nil : pins
    }

    /// Drop the pinned identity (e.g. after a legitimate host reinstall). This does NOT downgrade
    /// to TOFU: the next connect re-pairs via the PIN ceremony, unless the host advertises
    /// `pair=optional` (the only case the connect path still offers the trust prompt).
    func forgetIdentity(_ host: StoredHost) {
        guard let i = hosts.firstIndex(where: { $0.id == host.id }) else { return }
        hosts[i].pinnedSHA256 = nil
    }


    private func persist() {
        #if DEBUG
        // The screenshot harness fills a store with mock hosts (ShotMock) purely to render a
        // scene. On a dev Mac that store is the SAME App-Group suite the real app reads, so
        // persisting would replace the tester's saved hosts with "Battlestation" & co.
        if ScreenshotMode.isActive { return }
        #endif
        if let data = try? JSONEncoder().encode(hosts) {
            defaults.set(data, forKey: Self.key)
        }
        reloadHostsWidget() // the widgets read this store; any change refreshes their timelines
    }

    /// Ask WidgetKit to rebuild the launcher widgets' timelines after any store change (add/remove/
    /// pin/last-connected). iOS-only and a no-op where WidgetKit is absent; both widgets use
    /// `.never`-refresh entries and rely on this push.
    private func reloadHostsWidget() {
        #if canImport(WidgetKit) && os(iOS)
        WidgetCenter.shared.reloadTimelines(ofKind: "PunktfunkHosts")
        WidgetCenter.shared.reloadTimelines(ofKind: "PunktfunkLibrary")
        #endif
    }
}
