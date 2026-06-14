// Game library client (experimental, plan step 3). Fetches the host's unified game library
// from the management REST API (`GET /api/v1/library`) — the same payload the web console's
// /library page renders. Read-only on the client for now; launching a chosen title is a later
// step. Gated behind `DefaultsKey.libraryEnabled` in the UI.
//
// The management API is HTTP on a port distinct from the punktfunk/1 data plane (default 47990),
// binds loopback unless started with a token, and REQUIRES a bearer token for any non-loopback
// bind. So to browse a host's library remotely the host must expose the mgmt API on the LAN with
// `--mgmt-token`; the client carries that token per host. This mirrors the GameEntry/Artwork/
// LaunchSpec schema in `crates/punktfunk-host/src/library.rs`.

import Foundation

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
            return "Couldn't reach the host's management API: \(why). The host must expose it on "
                + "the LAN (serve --mgmt-bind 0.0.0.0)."
        }
    }
}

/// The management API's default port — adjacent to the GameStream block; matches
/// `mgmt::DEFAULT_PORT` on the host.
public let punktfunkDefaultMgmtPort: UInt16 = 47990

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
        let delegate = LibraryTLSDelegate(identity: identity, pinnedHostFingerprint: hostFingerprint)
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
            return try JSONDecoder().decode([GameEntry].self, from: data)
        case 401:
            throw LibraryError.unauthorized
        default:
            throw LibraryError.http(http.statusCode)
        }
    }
}
