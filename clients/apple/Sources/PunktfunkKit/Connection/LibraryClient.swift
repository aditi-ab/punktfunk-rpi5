// Game library client (experimental, plan step 3). Fetches the host's unified game library
// from the management REST API (`GET /api/v1/library`) — the same payload the web console's
// /library page renders. Read-only on the client for now; launching a chosen title is a later
// step. Gated behind `DefaultsKey.libraryEnabled` in the UI.
//
// The management API serves HTTPS on a port distinct from the punktfunk/1 data plane (default
// 47990, also advertised in the host's mDNS `mgmt` TXT). A paired client is authorized for the
// read-only library route by its **mTLS certificate** — no bearer token. The host binds this read
// surface to the LAN by DEFAULT (the bearer-gated admin surface stays loopback-only), so a paired
// client browses a host's library with no operator step. This mirrors the GameEntry/Artwork/
// LaunchSpec schema in `crates/punktfunk-host/src/library.rs`.

import Foundation
// `punktfunkDefaultMgmtPort` (and StoredHost/DefaultsKey) now live in PunktfunkShared so the
// dependency-free widget extension can share them; PunktfunkKit re-exports the module.
import PunktfunkShared

/// Cover art URLs (the public Steam CDN for Steam titles, user-supplied for custom entries).
public struct Artwork: Codable, Hashable, Sendable {
    public var portrait: String?
    public var hero: String?
    public var logo: String?
    public var header: String?

    /// Preferred order for a poster grid: the 600×900 capsule, falling back to the header
    /// (which is near-universal — many older titles lack a portrait capsule).
    public var posterCandidates: [URL] {
        [portrait, header, hero].compactMap { $0 }.compactMap { URL(string: $0) }
    }
}

/// How the host would launch a title (carried for a later step; the client only displays it).
public struct LaunchSpec: Codable, Hashable, Sendable {
    public var kind: String // "steam_appid" | "command"
    public var value: String
}

/// One title in the unified library. `id` is store-qualified: `steam:<appid>` / `custom:<id>`.
public struct GameEntry: Codable, Hashable, Identifiable, Sendable {
    public var id: String
    public var store: String // "steam" | "custom" | "lutris" | "heroic" | "epic" | "gog" | "xbox"
    public var title: String
    public var art: Artwork
    public var launch: LaunchSpec?
    /// `"game"` (the default, and what an older host omits) or `"launcher"` — an entry that opens
    /// the launcher itself (Steam Big Picture, Heroic) rather than a title. Deliberately a plain
    /// optional String: the host owns the vocabulary, and an unknown future value must never fail
    /// the whole library decode. Anything that isn't `"launcher"` is a game (design D4).
    public var role: String?

    public var isCustom: Bool { store == "custom" }

    /// Whether this entry opens a launcher rather than a game.
    public var isLauncher: Bool { role == "launcher" }

    /// Display name for the store badge — the same table the Rust clients use
    /// (`pf-console-ui::library::store_label`). Before this existed the badge said "Steam" for
    /// every non-custom entry, which a Lutris or GOG title made a lie.
    public var storeLabel: String {
        switch store {
        case "steam": return "Steam"
        case "custom": return "Custom"
        case "heroic": return "Heroic"
        case "lutris": return "Lutris"
        case "epic": return "Epic"
        case "gog": return "GOG"
        case "xbox": return "Xbox"
        default: return "Game"
        }
    }
}

public extension Array where Element == GameEntry {
    /// Design D4: launcher entries lead the shelf, and the host's title order survives within each
    /// group. Applied once where the library is fetched, so no individual view has to remember
    /// the rule — and a library without launcher entries comes back untouched.
    var launchersFirst: [GameEntry] {
        let launchers = filter(\.isLauncher)
        return launchers.isEmpty ? self : launchers + filter { !$0.isLauncher }
    }
}

/// Errors surfaced to the UI so it can guide setup (the common case is "not paired yet").
public enum LibraryError: LocalizedError {
    case unauthorized
    /// The host's certificate didn't hash to the fingerprint pinned at pairing — an impostor, or
    /// a host reinstalled/re-keyed since. Distinct from `unreachable` because the remedy is
    /// completely different: re-pair, don't go hunting the network.
    case pinMismatch
    case http(Int)
    case unreachable(String)

    public var errorDescription: String? {
        switch self {
        case .unauthorized:
            return "The host didn't recognize this device. Pair with the host first — it "
                + "authorizes paired clients by their certificate (no token needed)."
        case .pinMismatch:
            return "The host's certificate doesn't match the one this device paired with. "
                + "If the host was reinstalled, forget it here and pair again."
        case .http(let code):
            return "The management API returned HTTP \(code)."
        case .unreachable(let why):
            // The library rides a DIFFERENT port than the stream (the management API, 47990 by
            // default; the stream is QUIC on 9777), so it can fail while streaming to the same
            // host works perfectly — say that first, because the opposite assumption has sent
            // more than one person hunting the wrong layer. Opening that URL in a browser is the
            // fastest way to tell "port unreachable" apart from anything client-side.
            return "Couldn't reach the host's management API: \(why). The library uses a "
                + "different port than the stream (47990 by default), so streaming can work "
                + "while this doesn't. Check that port is reachable from this device, and that "
                + "the host isn't pinned to `--mgmt-bind 127.0.0.1`, which serves it to the "
                + "host itself only."
        }
    }
}

/// Stateless fetcher for a host's library.
public enum LibraryClient {
    /// `GET https://<address>:<port>/api/v1/library`, authenticated by **mTLS**: the client
    /// presents `identity` (its persistent cert/key PEM — the same identity the host paired over
    /// QUIC), and the host's self-signed cert is pinned by `hostFingerprint` (SHA-256 of its DER,
    /// the value the client already trusts). No bearer token — a paired client is authorized by
    /// its certificate. `hostFingerprint == nil` ⇒ TOFU (accept the presented host cert).
    public static func fetch(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async throws -> [GameEntry] {
        guard let base = URL(string: "\(baseURL(address: address, port: port))/api/v1/library")
        else { throw LibraryError.unreachable("invalid host address") }
        let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let response = try await send(
            path: "/api/v1/library", address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint)
        switch response.status {
        case 200:
            var games = try JSONDecoder().decode([GameEntry].self, from: response.body)
            // Steam art comes back as host-relative proxy paths (`/api/v1/library/art/...`, see
            // the host's `library::steam_art`) so they work the same regardless of which
            // interface/port the client reached the host on. Resolve them against THIS host now,
            // so every other consumer just sees ordinary absolute URLs.
            for i in games.indices {
                games[i].art = games[i].art.resolved(against: base)
            }
            return games
        // 403 joins 401 here: both are the host declining this certificate, and the remedy the
        // user needs is the same one.
        case 401, 403:
            throw LibraryError.unauthorized
        default:
            throw LibraryError.http(response.status)
        }
    }

    /// `https://addr:port`, IPv6 literals bracketed — the mirror of the Rust client's `base_url`.
    static func baseURL(address: String, port: UInt16) -> String {
        let bare = address.hasPrefix("[") && address.hasSuffix("]")
            ? String(address.dropFirst().dropLast()) : address
        return bare.contains(":") ? "https://[\(bare)]:\(port)" : "https://\(bare):\(port)"
    }

    /// Build the paired identity, restating any keychain failure in the UI's vocabulary.
    static func clientIdentity(certPEM: String, keyPEM: String) throws -> SecIdentity {
        do {
            return try ClientTLS.makeIdentity(certPEM: certPEM, keyPEM: keyPEM)
        } catch {
            throw LibraryError.unreachable(
                (error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
    }

    /// One GET against the host, with transport failures mapped onto `LibraryError`.
    static func send(
        path: String, address: String, port: UInt16,
        identity: SecIdentity, hostFingerprint: Data?
    ) async throws -> HTTPResponse {
        do {
            return try await MgmtTransport.get(
                host: address, port: port, path: path,
                identity: identity, pinnedHostFingerprint: hostFingerprint)
        } catch MgmtTransportError.pinMismatch {
            throw LibraryError.pinMismatch
        } catch MgmtTransportError.timedOut {
            throw LibraryError.unreachable("timed out")
        } catch let error as MgmtTransportError {
            throw LibraryError.unreachable(String(describing: error))
        } catch {
            throw LibraryError.unreachable(error.localizedDescription)
        }
    }
}

extension Artwork {
    /// Rewrite any host-relative field (one starting with `/`) into an absolute URL against `base`.
    /// External CDN URLs (GOG/Heroic/Xbox) and `data:` URLs (Lutris) already don't start with `/`,
    /// so they pass through unchanged. `internal` (not `fileprivate`) so `LibraryClientTests` can
    /// exercise it directly without a live host.
    func resolved(against base: URL) -> Artwork {
        func abs(_ s: String?) -> String? {
            guard let s, s.hasPrefix("/") else { return s }
            return URL(string: s, relativeTo: base)?.absoluteString ?? s
        }
        var a = self
        a.portrait = abs(a.portrait)
        a.hero = abs(a.hero)
        a.logo = abs(a.logo)
        a.header = abs(a.header)
        return a
    }
}

/// Loads cover art for the library UI, routing each URL to the transport that suits its origin.
///
/// A `GameEntry`'s art candidates mix two very different things: the host's own art proxy
/// (`/api/v1/library/art/...`, resolved to absolute URLs against this host) and public store CDN
/// URLs carried verbatim on custom/GOG/Heroic entries. Host URLs go over [`MgmtTransport`] with
/// the paired identity and the pinned fingerprint — outside the URL loading system, so App
/// Transport Security can stay ON app-wide. Every other origin keeps ordinary `URLSession` with
/// full system trust evaluation and no client certificate, which is exactly what it should get.
///
/// Built once per library screen and reused across a whole grid's worth of posters.
public final class LibraryArtLoader: @unchecked Sendable {
    private let address: String
    private let port: UInt16
    private let identity: SecIdentity
    private let hostFingerprint: Data?
    /// Third-party origins only. No delegate: these are ordinary public HTTPS URLs and get the
    /// system's normal certificate validation.
    private let cdn = URLSession(configuration: .default)

    public init(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) throws {
        self.address = address
        self.port = port
        self.identity = try LibraryClient.clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        self.hostFingerprint = hostFingerprint
    }

    public func data(for url: URL) async throws -> Data {
        guard isHostOrigin(url) else { return try await cdn.data(from: url).0 }
        var path = url.path.isEmpty ? "/" : url.path
        if let query = url.query { path += "?\(query)" }
        let response = try await LibraryClient.send(
            path: path, address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint)
        guard response.status == 200 else { throw LibraryError.http(response.status) }
        return response.body
    }

    /// Does this URL point at the host's own art proxy? Compared on host + port rather than a
    /// string prefix, so a differently-spelled but equivalent URL still takes the pinned path.
    private func isHostOrigin(_ url: URL) -> Bool {
        guard let host = url.host else { return false }
        let bare = address.hasPrefix("[") && address.hasSuffix("]")
            ? String(address.dropFirst().dropLast()) : address
        let scheme = url.scheme?.lowercased()
        return host.caseInsensitiveCompare(bare) == .orderedSame
            && (url.port ?? (scheme == "http" ? 80 : 443)) == Int(port)
    }
}
