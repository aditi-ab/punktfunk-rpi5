// How the host grid is ordered and grouped.
//
// Pure value logic, deliberately: which host lands where is the kind of thing that reads as
// obviously right and behaves subtly wrong (an unstable sort reshuffling equal rows on every
// redraw, a never-connected host sorting as if it were connected in 1970), so it lives apart from
// the view and is tested. It sits in PunktfunkShared because everything it touches — `StoredHost`,
// `ProfileCatalog` — already does.

import Foundation

/// The order hosts appear in.
public enum HostSort: String, CaseIterable, Identifiable, Sendable {
    /// Oldest first — the order they were added, and what the grid did before it could sort at
    /// all. The default for exactly that reason: an update shouldn't rearrange anyone's grid.
    case added
    case name
    case lastConnected

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .added: return "Date Added"
        case .name: return "Name"
        case .lastConnected: return "Last Connected"
        }
    }

    public var symbol: String {
        switch self {
        case .added: return "calendar"
        case .name: return "textformat.abc"
        case .lastConnected: return "clock.arrow.circlepath"
        }
    }
}

/// What the grid is divided by.
public enum HostGrouping: String, CaseIterable, Identifiable, Sendable {
    case none
    /// The host's DEFAULT profile — the one a plain tap uses. Pinned cards stay with their host:
    /// a pin is presentation of that host, not a host of its own.
    case profile
    case status

    public var id: String { rawValue }

    public var label: String {
        switch self {
        case .none: return "None"
        case .profile: return "Profile"
        case .status: return "Status"
        }
    }

    public var symbol: String {
        switch self {
        case .none: return "square.grid.2x2"
        case .profile: return "slider.horizontal.3"
        case .status: return "wifi"
        }
    }
}

/// One rendered band of the grid. `title` is nil for the ungrouped case, which is what tells the
/// view to draw no header at all rather than an empty one.
public struct HostGroup: Identifiable, Equatable, Sendable {
    public let id: String
    public let title: String?
    /// `#RRGGBB` when the band is a profile — its chip colour, so the header matches the cards.
    public let accent: String?
    public let hosts: [StoredHost]

    public init(id: String, title: String?, accent: String? = nil, hosts: [StoredHost]) {
        self.id = id
        self.title = title
        self.accent = accent
        self.hosts = hosts
    }
}

public enum HostArrangement {
    /// Group, then sort within each group. `online` is the set of host ids currently reachable —
    /// the caller owns that (mDNS plus the reachability probe), this only reads it.
    public static func groups(
        hosts: [StoredHost], catalog: ProfileCatalog, online: Set<UUID>,
        sort: HostSort, grouping: HostGrouping
    ) -> [HostGroup] {
        // The original index is the tie-break for every sort below. `Array.sorted` is NOT stable
        // in Swift, so without it two hosts that compare equal — same name, both never connected —
        // could swap places between redraws.
        let ranked = hosts.enumerated().map { (index: $0.offset, host: $0.element) }

        func ordered(_ slice: [(index: Int, host: StoredHost)]) -> [StoredHost] {
            slice.sorted { lhs, rhs in
                switch sort {
                case .added:
                    // Undated hosts first: no date means saved before the field existed, which
                    // means older than anything carrying one. In a real store those are a PREFIX
                    // (everything from before the upgrade), so they hold their stored order and
                    // the grid looks exactly as it did — which is why this is the default sort.
                    let a = lhs.host.addedAt, b = rhs.host.addedAt
                    if let a, let b, a != b { return a < b }
                    if (a == nil) != (b == nil) { return a == nil }
                case .name:
                    let comparison = lhs.host.displayName.localizedStandardCompare(
                        rhs.host.displayName)
                    if comparison != .orderedSame { return comparison == .orderedAscending }
                case .lastConnected:
                    // Never-connected hosts go last, not first — `.distantPast` would put a host
                    // you have never used at the top of "Last Connected".
                    switch (lhs.host.lastConnected, rhs.host.lastConnected) {
                    case let (a?, b?) where a != b: return a > b
                    case (nil, _?): return false
                    case (_?, nil): return true
                    default: break
                    }
                }
                return lhs.index < rhs.index
            }.map(\.host)
        }

        switch grouping {
        case .none:
            return [HostGroup(id: "all", title: nil, hosts: ordered(ranked))]

        case .profile:
            // Catalog order, so the bands sit in the same order the scope menu lists them, then
            // the unbound hosts. A band with nothing in it isn't drawn.
            var groups: [HostGroup] = catalog.profiles.compactMap { profile in
                let members = ranked.filter { catalog.binding(for: $0.host)?.id == profile.id }
                guard !members.isEmpty else { return nil }
                return HostGroup(
                    id: "profile-\(profile.id)", title: profile.name, accent: profile.accent,
                    hosts: ordered(members))
            }
            // A dangling binding resolves as none (§4.4), so it lands here rather than in a band
            // named after a profile that no longer exists.
            let unbound = ranked.filter { catalog.binding(for: $0.host) == nil }
            if !unbound.isEmpty {
                groups.append(
                    HostGroup(id: "profile-none", title: "No Profile", hosts: ordered(unbound)))
            }
            return groups

        case .status:
            var groups: [HostGroup] = []
            let up = ranked.filter { online.contains($0.host.id) }
            let down = ranked.filter { !online.contains($0.host.id) }
            if !up.isEmpty {
                groups.append(HostGroup(id: "status-online", title: "Online", hosts: ordered(up)))
            }
            if !down.isEmpty {
                groups.append(
                    HostGroup(id: "status-offline", title: "Offline", hosts: ordered(down)))
            }
            return groups
        }
    }
}
