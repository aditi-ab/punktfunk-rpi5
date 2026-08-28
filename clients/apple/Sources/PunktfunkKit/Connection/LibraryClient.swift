// Game library client. Fetches the host's unified game library from the management REST API
// (`GET /api/v1/library`) — the same payload the web console's /library page renders — and what it
// currently has running (`GET /api/v1/status`), so a title the player can return to can be marked
// as such. Gated behind `DefaultsKey.libraryEnabled` in the UI.
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
    /// The token for this entry's brand mark (`"steam"`, `"heroic"`) — never art, never a URL.
    /// `nil` on every older host and on every ordinary title. See `launcherIconImage`.
    public var icon: String?
    /// The platform the host filed this title under ("PC", "PS3", "SNES" — free-form, the host's
    /// `GameMeta.platform`, populated by the rom-manager plugin and left `nil` by the store
    /// scanners). What the library's collections group by; a `nil` buckets under the STORE, never
    /// under "Unknown" (see `LibraryCollation.bucket`). Also on the detail band as
    /// `STORE · PLATFORM`.
    public var platform: String?

    public var isCustom: Bool { store == "custom" }

    /// Whether this entry opens a launcher rather than a game.
    public var isLauncher: Bool { role == "launcher" }

    /// The brand-icon token, re-validated rather than taken on trust.
    ///
    /// The host checks the shape on the way in, so this can only fire for a host older than that
    /// check or one that isn't ours. The value reaches `Image(named:)`, and "the peer promised" is
    /// not the standard a name lookup deserves.
    public var iconToken: String? {
        guard let t = icon, !t.isEmpty, t.count <= 32,
              let first = t.first, first.isASCII, first.isLowercase,
              t.allSatisfy({ $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-") })
        else { return nil }
        return t
    }

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

/// One game the host currently has launched, from `/api/v1/status`.
///
/// A deliberately partial mirror of the host's `ActiveGame`: only the fields a client can act on.
/// The console's own view of this payload carries more (which session, which plane, the grace
/// countdown), and none of that is a player's business from the library screen.
public struct RunningGame: Codable, Hashable, Sendable {
    /// Store-qualified library id (`steam:570`) — the key that lines this up with a `GameEntry`.
    /// Absent for an operator-typed GameStream command, which has no catalog entry behind it.
    public var appID: String?
    public var title: String
    /// `launching` | `running` | `exited` | `untracked` | `grace`. A plain String on purpose: the
    /// host owns the vocabulary and adds to it (`untracked` arrived in 0.30), so an unknown value
    /// must never fail the decode of the whole list.
    public var state: String

    private enum CodingKeys: String, CodingKey {
        case appID = "app_id"
        case title
        case state
    }

    /// Is this title *up on the host right now* — i.e. would picking it take the player back into
    /// it rather than start it?
    ///
    /// `untracked` counts: the host cannot follow that process, but it did launch it and has no
    /// evidence it stopped. `grace` counts too — its session is gone but the game is still running,
    /// which is precisely the case where getting back in promptly matters most. Only a confirmed
    /// `exited` does not.
    public var isUp: Bool { state != "exited" }
}

/// One action a host offers THIS device, from `/api/v1/actions` (`design/host-actions.md` §3.2)
/// — v1: sleep, restart, shut down.
///
/// Unknown ids are expected and fine: the client renders ``title`` verbatim for anything it has
/// no local wording for, which is what lets a later host add actions with no client release.
public struct HostAction: Codable, Hashable, Sendable, Identifiable {
    /// Stable id: `power.sleep`, `power.reboot`, `power.shutdown` today.
    public var id: String
    /// The host's own display title — the fallback label for an id this client doesn't know.
    public var title: String
    /// Action group (`power` for the built-ins).
    public var group: String
    /// Confirm before running: the action loses whatever is on that machine (restart, shut down).
    public var danger: Bool
    /// Whether the host can run it right now (a machine that cannot suspend, a foreign inhibitor).
    public var available: Bool
    /// Why not, when it can't — shown rather than hidden, so "greyed out" always has a reason.
    public var unavailableReason: String?
    /// Whether THIS device's access covers it (the host's Host-power grant). The client only ever
    /// keeps the permitted ones.
    public var permitted: Bool

    private enum CodingKeys: String, CodingKey {
        case id, title, group, danger, available, permitted
        case unavailableReason = "unavailable_reason"
    }

    /// This client's wording for a known id, else the host's own title — so a familiar action is
    /// worded the way the rest of this app words it, without hiding an unfamiliar one.
    public var label: String {
        switch id {
        case "power.sleep": return "Sleep Host"
        case "power.reboot": return "Restart Host"
        case "power.shutdown": return "Shut Down Host"
        default: return title
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

    /// What the host currently has running, from `GET /api/v1/status`.
    ///
    /// Same lane, same identity, no new host work: `/status` is already on the paired-certificate
    /// allowlist (the host's `mgmt::auth::cert_may_access`) alongside `/library`, and has carried a
    /// `games[]` array since the session⇄game lifetime work. The client simply never read it — so a
    /// player had no way to see, from the device they browse on, that something was already up.
    ///
    /// Best-effort by contract: an older host, an unreachable one, or a shape we don't recognize
    /// yields an empty list rather than an error. Nothing here is worth failing a library screen
    /// over — the worst case is a Resume badge that doesn't appear.
    public static func running(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async -> [RunningGame] {
        guard let identity = try? clientIdentity(certPEM: certPEM, keyPEM: keyPEM),
              let response = try? await send(
                  path: "/api/v1/status", address: address, port: port,
                  identity: identity, hostFingerprint: hostFingerprint),
              response.status == 200,
              let status = try? JSONDecoder().decode(HostStatus.self, from: response.body)
        else { return [] }
        return status.games ?? []
    }

    /// Upload this client's recent log (`ClientLogRing`) to the host — `POST /api/v1/client-logs`,
    /// the one WRITE a paired certificate may make (the host's `mgmt/client_logs.rs`). Same lane
    /// and identity as the library; the host files the bundle under this device and shows it on
    /// its web console's Logs page next to its own log. Returns the stored bundle id (empty for a
    /// host that predates the id in the reply).
    ///
    /// Why it exists: on an Apple TV (or a phone, for anyone who is not a developer) there is no
    /// way to get the client's log off the device, so every fault report arrived with only the
    /// host's half of the story. `hostFingerprint` is required, not optional: this is an outbound
    /// write carrying the device's diagnostics, and it goes to the host the user paired with.
    public static func sendLogs(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data
    ) async throws -> String {
        let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let body = Data(ClientLogRing.render(header: ClientLogRing.header()).utf8)
        let response = try await send(
            path: "/api/v1/client-logs", address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint,
            body: (body, "text/plain; charset=utf-8"))
        switch response.status {
        case 200, 201:
            let json = try? JSONSerialization.jsonObject(with: response.body) as? [String: Any]
            return json?["id"] as? String ?? ""
        case 401, 403:
            throw LibraryError.unauthorized
        default:
            throw LibraryError.http(response.status)
        }
    }

    /// What this host lets THIS device do to it — sleep, restart, shut it down
    /// (`design/host-actions.md` §7) — from `GET /api/v1/actions`.
    ///
    /// Only the PERMITTED rows come back: the host is the only judge of whether this device's
    /// access carries the Host-power grant, and a row it would refuse is not this client's to
    /// render. Best-effort by contract, like ``running(address:port:certPEM:keyPEM:hostFingerprint:)``
    /// — an older host (no such route), an unreachable one, or a shape we don't recognise yields
    /// an empty list. A missing menu row costs a menu row; a thrown error would cost the screen.
    public static func actions(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async -> [HostAction] {
        guard let identity = try? clientIdentity(certPEM: certPEM, keyPEM: keyPEM),
              let response = try? await send(
                  path: "/api/v1/actions", address: address, port: port,
                  identity: identity, hostFingerprint: hostFingerprint),
              response.status == 200,
              let list = try? JSONDecoder().decode(HostActionList.self, from: response.body)
        else { return [] }
        return list.actions.filter(\.permitted)
    }

    /// Invoke one host action by id (`POST /api/v1/actions/{id}`, empty body).
    ///
    /// Returning normally means the host ACCEPTED it (202) — it now ends every session and acts
    /// about a second later, so this is the last word the client will get. A refusal throws
    /// with the host's own sentence ("another device is streaming from this host right now"),
    /// which tells a person what to do where a bare status code would not.
    ///
    /// The body stays empty by design: the id is the whole request, and no request field ever
    /// reaches the host's privileged path.
    public static func invokeAction(
        id: String,
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data
    ) async throws {
        let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let escaped = id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? id
        let response = try await send(
            path: "/api/v1/actions/\(escaped)", address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint,
            body: (Data(), "application/json"))
        switch response.status {
        case 200, 202:
            return
        case 401, 403:
            throw LibraryError.unauthorized
        default:
            // The `ApiError` envelope carries the host's reason; prefer it over the code.
            let json = try? JSONSerialization.jsonObject(with: response.body) as? [String: Any]
            if let why = json?["error"] as? String, !why.isEmpty {
                throw LibraryError.unreachable(why)
            }
            throw LibraryError.http(response.status)
        }
    }

    /// Just the slice of `/status` this client reads. Everything else on that payload is the
    /// operator console's business, and decoding only what we use keeps an unrelated schema change
    /// on the host from breaking the library screen.
    private struct HostStatus: Decodable {
        var games: [RunningGame]?
    }

    private struct HostActionList: Decodable {
        var actions: [HostAction]
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

    /// One request against the host — a GET, or a POST when `body` is given — with transport
    /// failures mapped onto `LibraryError`.
    static func send(
        path: String, address: String, port: UInt16,
        identity: SecIdentity, hostFingerprint: Data?,
        body: (data: Data, contentType: String)? = nil
    ) async throws -> HTTPResponse {
        do {
            if let body {
                return try await MgmtTransport.post(
                    host: address, port: port, path: path, body: body.data,
                    contentType: body.contentType,
                    identity: identity, pinnedHostFingerprint: hostFingerprint)
            }
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

/// Anything that answers poster bytes for a cover-art URL. The production implementation is
/// [`LibraryArtLoader`]; the screenshot harness substitutes a canned source so store frames carry
/// artwork without a host on the network.
public protocol LibraryArtSource: Sendable {
    func data(for url: URL) async throws -> Data
    /// Release pooled connections when the owning screen goes away. Sources without connections
    /// have nothing to do.
    func close() async
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
/// Posters are cached on disk (`ArtCache`), so a second visit to a library costs no network at
/// all — and the connections behind a first visit are pooled and kept alive rather than paying a
/// TLS handshake per tile.
///
/// Built once per library screen and reused across a whole grid's worth of posters.
public final class LibraryArtLoader: LibraryArtSource, @unchecked Sendable {
    private let address: String
    private let port: UInt16
    private let identity: SecIdentity
    private let hostFingerprint: Data?
    /// Third-party origins only. No delegate: these are ordinary public HTTPS URLs and get the
    /// system's normal certificate validation.
    private let cdn = URLSession(configuration: .default)
    /// nil when the caches directory is unavailable — then we simply always fetch.
    private let cache = ArtCache.standard()

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
        if let cache, let cached = await cache.data(for: url) { return cached }
        let fetched = try await fetch(url)
        if let cache { await cache.store(fetched, for: url) }
        return fetched
    }

    /// Release this host's pooled connections — call when the library screen goes away, so we
    /// don't sit on open TLS sockets the user is finished with.
    public func close() async {
        await MgmtConnectionPool.shared.closeAll(
            matching: "\(MgmtTransport.unbracketed(address)):\(port):")
    }

    private func fetch(_ url: URL) async throws -> Data {
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
