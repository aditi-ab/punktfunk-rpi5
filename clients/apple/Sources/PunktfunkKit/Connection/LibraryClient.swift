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
    public var store: String // "steam" | "custom"
    public var title: String
    public var art: Artwork
    public var launch: LaunchSpec?

    public var isCustom: Bool { store == "custom" }
}

/// Errors surfaced to the UI so it can guide setup (the common case is "not paired yet").
public enum LibraryError: LocalizedError {
    case unauthorized
    case http(Int)
    case unreachable(String)

    public var errorDescription: String? {
        switch self {
        case .unauthorized:
            return "The host didn't recognize this device. Pair with the host first — it "
                + "authorizes paired clients by their certificate (no token needed)."
        case .http(let code):
            return "The management API returned HTTP \(code)."
        case .unreachable(let why):
            return "Couldn't reach the host's management API: \(why). It binds the LAN by default, "
                + "so check the host is updated and reachable (a host pinned to "
                + "`--mgmt-bind 127.0.0.1` is loopback-only and can't be browsed remotely)."
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
        guard let url = URL(string: "https://\(address):\(port)/api/v1/library") else {
            throw LibraryError.unreachable("invalid host address")
        }
        let identity: SecIdentity
        do {
            identity = try ClientTLS.makeIdentity(certPEM: certPEM, keyPEM: keyPEM)
        } catch {
            throw LibraryError.unreachable(
                (error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
        let delegate = LibraryTLSDelegate(
            identity: identity, pinnedHostFingerprint: hostFingerprint, host: address, port: port)
        let session = URLSession(configuration: .ephemeral, delegate: delegate, delegateQueue: nil)
        defer { session.finishTasksAndInvalidate() }

        let req = URLRequest(url: url, timeoutInterval: 10)
        let (data, response): (Data, URLResponse)
        do {
            (data, response) = try await session.data(for: req)
        } catch {
            throw LibraryError.unreachable(error.localizedDescription)
        }
        guard let http = response as? HTTPURLResponse else {
            throw LibraryError.unreachable("not an HTTP response")
        }
        switch http.statusCode {
        case 200:
            var games = try JSONDecoder().decode([GameEntry].self, from: data)
            // Steam art now comes back as host-relative proxy paths (`/api/v1/library/art/...`,
            // see the host's `library::steam_art`) so they work the same regardless of which
            // interface/port the client reached the host on. Resolve them against THIS host now,
            // so every other consumer just sees ordinary absolute URLs.
            let base = url
            for i in games.indices {
                games[i].art = games[i].art.resolved(against: base)
            }
            return games
        case 401:
            throw LibraryError.unauthorized
        default:
            throw LibraryError.http(http.statusCode)
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

/// Builds the authenticated `URLSession` the library UI uses to fetch cover-art images — the same
/// paired identity + host pinning as [`LibraryClient.fetch`], reused across a whole grid's worth of
/// poster loads (this session is NOT one-shot: callers own its lifetime and should invalidate it
/// when the view goes away). Safe to use for every candidate URL a `GameEntry`'s `Artwork` carries:
/// `LibraryTLSDelegate` only pins/presents-cert for the host itself, deferring to normal system
/// trust + no client cert for any other origin (an external CDN URL).
public enum LibraryImageLoader {
    public static func session(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) throws -> URLSession {
        let identity = try ClientTLS.makeIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let delegate = LibraryTLSDelegate(
            identity: identity, pinnedHostFingerprint: hostFingerprint, host: address, port: port)
        return URLSession(configuration: .default, delegate: delegate, delegateQueue: nil)
    }
}
