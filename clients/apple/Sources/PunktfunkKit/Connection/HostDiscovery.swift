// LAN auto-discovery of punktfunk/1 hosts over mDNS — the client side of the host's
// `crate::discovery` advert (`_punktfunk._udp`). Browses with NWBrowser (TXT rides in the
// result metadata), resolves each service to a connectable IP:port with a throwaway
// NWConnection, and publishes the live set.
//
// The advertised `fp` (host cert SHA-256) is ADVISORY: mDNS is unauthenticated, so TOFU /
// pinning still verifies the host on connect — it's surfaced only so a picker can show it and
// pre-fill. `pair=required` lets the UI route straight to the pairing ceremony.
//
// iOS/tvOS gate Bonjour browsing on Info.plist `NSBonjourServices` listing `_punktfunk._udp`
// (Config/Info.plist) — without it the system blocks the browse and nothing is returned.

#if canImport(Network)
import Foundation
import Network
import PunktfunkShared

/// A punktfunk/1 host found on the LAN. `fingerprintHex` is advisory (see file header).
public struct DiscoveredHost: Identifiable, Sendable, Equatable {
    /// Stable host id (mDNS `id` TXT); falls back to the Bonjour instance name.
    public let id: String
    /// Bonjour instance name (the host's chosen label).
    public let name: String
    /// Resolved address to hand to `PunktfunkConnection`.
    public let host: String
    public let port: UInt16
    /// Host cert SHA-256 (lowercase hex) the host advertised, or nil if absent.
    public let fingerprintHex: String?
    /// The host advertised `pair=required` — a client must pair before it can stream.
    public let requiresPairing: Bool
    /// The host EXPLICITLY advertised `pair=optional` — only then may the client offer the
    /// reduced-security TOFU "Trust" path. A missing/unknown `pair` field is NOT optional:
    /// pairing is mandatory unless this is true (the policy authority is the host's advert).
    public let allowsTofu: Bool
    /// Wake-on-LAN MAC address(es) the host advertised (mDNS `mac` TXT, comma-separated
    /// `aa:bb:cc:dd:ee:ff`, routed NIC first). Empty when not advertised. A client persists these
    /// onto the saved host so it can wake it after it sleeps; advisory/unauthenticated (a wrong
    /// value only makes a wake fail — the magic packet is inert and the fingerprint still gates
    /// the connection).
    public let macAddresses: [String]
    /// The host's OS-identity chain (mDNS `os` TXT, e.g. `linux/fedora/bazzite`), sanitized
    /// (`sanitizeOsChain`) — drives the host card's OS mark and is persisted like the MACs.
    /// Empty when not advertised (older host). Advisory/unauthenticated like the rest.
    public let osChain: String
}

@MainActor
public final class HostDiscovery: ObservableObject {
    /// Currently-visible hosts, deduped by `id`, sorted by name. Main-actor.
    @Published public private(set) var hosts: [DiscoveredHost] = []

    private var browser: NWBrowser?
    /// Keyed by the service endpoint's description (a stable, Sendable handle we can capture
    /// into the resolve callbacks without smuggling non-Sendable Network types across hops).
    private var resolved: [String: DiscoveredHost] = [:]
    private var connections: [String: NWConnection] = [:]

    public init() {}

    /// Start browsing `_punktfunk._udp`. Idempotent — a second call while live is a no-op.
    public func start() {
        guard browser == nil else { return }
        let browser = NWBrowser(
            for: .bonjourWithTXTRecord(type: "_punktfunk._udp", domain: nil),
            using: NWParameters())
        browser.browseResultsChangedHandler = { results, _ in
            MainActor.assumeIsolated { [weak self] in self?.reconcile(results) }
        }
        browser.stateUpdateHandler = { state in
            // A failed browser never recovers on its own; tear down and re-arm so transient
            // network changes (Wi-Fi flip, VPN) don't leave discovery silently dead.
            MainActor.assumeIsolated { [weak self] in
                if case .failed = state { self?.restart() }
            }
        }
        self.browser = browser
        browser.start(queue: .main)
    }

    /// Stop browsing and drop all discovered state.
    public func stop() {
        browser?.cancel()
        browser = nil
        for conn in connections.values { conn.cancel() }
        connections.removeAll()
        resolved.removeAll()
        if !hosts.isEmpty { hosts = [] }
    }

    deinit {
        browser?.cancel()
        for conn in connections.values { conn.cancel() }
    }

    private func restart() {
        stop()
        start()
    }

    /// Diff the browser's current result set against what we're tracking: drop departed
    /// services, resolve newly-seen ones.
    private func reconcile(_ results: Set<NWBrowser.Result>) {
        let live = Set(results.map { Self.key($0) })
        for key in resolved.keys where !live.contains(key) { resolved[key] = nil }
        for key in connections.keys where !live.contains(key) {
            connections[key]?.cancel()
            connections[key] = nil
        }
        for result in results {
            let key = Self.key(result)
            if resolved[key] == nil, connections[key] == nil { resolve(result) }
        }
        publish()
    }

    /// Resolve one service to IP:port via a short UDP connection (it reaches `.ready` once the
    /// path is established — no data is sent), reading the TXT up front so the callback only
    /// captures Sendable values + the endpoint key.
    private func resolve(_ result: NWBrowser.Result) {
        let key = Self.key(result)
        let name = Self.instanceName(result.endpoint)
        var fp: String?
        var pair: String?
        var id: String?
        var macs: [String] = []
        var osChain = ""
        if case let .bonjour(txt) = result.metadata {
            fp = Self.entry(txt, "fp")
            pair = Self.entry(txt, "pair")
            id = Self.entry(txt, "id")
            macs = (Self.entry(txt, "mac") ?? "")
                .split(separator: ",")
                .map { $0.trimmingCharacters(in: .whitespaces) }
                .filter { !$0.isEmpty }
            osChain = sanitizeOsChain(Self.entry(txt, "os") ?? "")
        }
        // Resolve over IPv4 only: Network.framework prefers IPv6 (RFC 6724), and the host's OS
        // mDNS responder often answers AAAA for its hostname even though the punktfunk host stack
        // (control QUIC + data UDP) binds IPv4 sockets exclusively — a v6-resolved address would
        // produce a host card whose connect always fails in the Rust core (`host:port` parse).
        // Same policy as the Android/desktop clients; lift when the stack speaks IPv6.
        let params = NWParameters.udp
        if let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let conn = NWConnection(to: result.endpoint, using: params)
        connections[key] = conn
        conn.stateUpdateHandler = { state in
            MainActor.assumeIsolated { [weak self] in
                guard let self, let conn = self.connections[key] else { return }
                switch state {
                case .ready:
                    if case let .hostPort(host, port)? = conn.currentPath?.remoteEndpoint,
                       let address = Self.hostString(host) {
                        self.resolved[key] = DiscoveredHost(
                            id: (id?.isEmpty == false) ? id! : name,
                            name: name, host: address, port: port.rawValue,
                            fingerprintHex: fp, requiresPairing: pair == "required",
                            allowsTofu: pair == "optional", macAddresses: macs,
                            osChain: osChain)
                        self.publish()
                    }
                    conn.cancel()
                    self.connections[key] = nil
                case .failed, .cancelled:
                    self.connections[key] = nil
                default:
                    break
                }
            }
        }
        conn.start(queue: .main)
    }

    /// Publish the resolved set, deduped by `id` (a host on several interfaces / re-advertising
    /// collapses to one row), sorted by name.
    private func publish() {
        var byID: [String: DiscoveredHost] = [:]
        for host in resolved.values { byID[host.id] = host }
        let next = byID.values.sorted {
            $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
        if next != hosts { hosts = next }
    }

    private static func key(_ result: NWBrowser.Result) -> String {
        "\(result.endpoint)"
    }

    private static func instanceName(_ endpoint: NWEndpoint) -> String {
        if case let .service(name, _, _, _) = endpoint { return name }
        return "punktfunk host"
    }

    private static func entry(_ txt: NWTXTRecord, _ field: String) -> String? {
        if case let .string(value) = txt.getEntry(for: field), !value.isEmpty { return value }
        return nil
    }

    /// A resolved `NWEndpoint.Host` → a plain address string for `PunktfunkConnection` (the
    /// scope id on a link-local address is stripped — the host+port pair is resolved again on
    /// the Rust side, which can't parse the `%iface` suffix).
    private static func hostString(_ host: NWEndpoint.Host) -> String? {
        switch host {
        case .ipv4(let address):
            return "\(address)".split(separator: "%").first.map(String.init)
        case .ipv6(let address):
            return "\(address)".split(separator: "%").first.map(String.init)
        case .name(let name, _):
            return name
        @unknown default:
            return nil
        }
    }
}
#endif
