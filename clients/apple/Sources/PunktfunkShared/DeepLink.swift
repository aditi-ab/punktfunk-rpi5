// The `punktfunk://` deep-link grammar — the single builder/parser shared by the widget (which
// emits links via `widgetURL`/`Link`) and the app (`ContentView.onOpenURL`, which routes them into
// the existing connect path). Keeping both sides on one type means the wire format can't drift.
//
// Grammar (v1):
//   punktfunk://connect/<host-uuid>                       — connect to a stored host
//   punktfunk://connect/<host-uuid>?launch=<GameEntry.id> — connect and ask the host to launch it
//
// `launch` carries a `GameEntry.id` (e.g. "steam:570"); it is percent-encoded on build and decoded
// on parse, so ids with reserved characters survive the round trip.

import Foundation

public enum DeepLink: Equatable {
    /// Connect to a saved host; `launchID` is a `GameEntry.id` to launch on arrival, if any.
    case connect(host: UUID, launchID: String?)

    public static let scheme = "punktfunk"

    /// Build the canonical URL for a route. Non-optional: every route is representable.
    public var url: URL {
        switch self {
        case let .connect(host, launchID):
            var comps = URLComponents()
            comps.scheme = Self.scheme
            comps.host = "connect"
            comps.path = "/\(host.uuidString)"
            if let launchID, !launchID.isEmpty {
                comps.queryItems = [URLQueryItem(name: "launch", value: launchID)]
            }
            // URLComponents percent-encodes the query value; force-unwrap is safe for a URL we
            // fully control (scheme/host/path are all valid).
            return comps.url!
        }
    }

    /// Parse an incoming URL, or nil if it isn't a recognized `punktfunk://` route. Tolerant of
    /// case in the scheme and of a trailing slash on the path.
    public init?(_ url: URL) {
        guard url.scheme?.lowercased() == Self.scheme else { return nil }
        guard let comps = URLComponents(url: url, resolvingAgainstBaseURL: false) else { return nil }

        switch comps.host?.lowercased() {
        case "connect":
            // Path is "/<uuid>"; strip the leading slash and any trailing one.
            let raw = comps.path
                .trimmingCharacters(in: CharacterSet(charactersIn: "/"))
            guard let host = UUID(uuidString: raw) else { return nil }
            let launch = comps.queryItems?
                .first(where: { $0.name == "launch" })?.value
                .flatMap { $0.isEmpty ? nil : $0 }
            self = .connect(host: host, launchID: launch)
        default:
            return nil
        }
    }
}
