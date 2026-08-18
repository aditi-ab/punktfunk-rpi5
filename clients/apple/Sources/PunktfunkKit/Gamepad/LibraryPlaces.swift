// Where the controller IS inside one library shelf — the desktop console's screen stack for the
// library (shelf → Collections → filtered shelf), kept as a small value type INSIDE the library
// layer rather than as deeper `GamepadShell` screens. The shell's screen enum is derived from
// presentation triggers with depth ≤ 1 by construction, macOS shows the library as a sheet and
// tvOS as a cover, and the desktop's own property is that drilling never touches the model — the
// filter is index-level, the art and the fetch don't change. So the library layer holds a stack
// of PLACES and pushes/pops within itself; the shell's B reaches the layer's `onDismiss` only from
// its root. Pure values in, pure values out — the flows are tested here, not on glass.
//
// The desktop's four flows, verbatim:
//   shelf → Y → collections → A → filtered shelf → B → collections → B → shelf → B → home
//   (start in collections) collections → A → filtered → B → collections → B → home
//   (start in collections) collections → Y "All titles" → unfiltered shelf (DRILLED) → B → collections
//   Y is refused with a pulse on any drilled shelf — the filtered drill-in and the "All titles" way out.

import Foundation

/// One place inside the library layer.
public enum LibraryPlace: Hashable, Sendable {
    /// The shelf (coverflow or grid), optionally filtered to one collated group.
    case shelf(filter: LibraryGroupKey?)
    /// The group-by-platform tiles.
    case collections

    public var isCollections: Bool {
        if case .collections = self { return true }
        return false
    }

    /// The shelf's filter, if this is a shelf.
    public var filter: LibraryGroupKey? {
        if case .shelf(let f) = self { return f }
        return nil
    }
}

/// The library layer's stack of places. Never empty.
public struct LibraryPlaceStack: Hashable, Sendable {
    public private(set) var places: [LibraryPlace]

    public init(root: LibraryPlace) {
        places = [root]
    }

    public var top: LibraryPlace { places[places.count - 1] }
    public var isRoot: Bool { places.count == 1 }

    /// A shelf reached by drilling — the filtered drill-in from Collections, or the "All titles"
    /// way out of a Collections root. Y (Collections) is refused there, and its hint hidden: the
    /// way back is B.
    public var drilled: Bool {
        !isRoot && !top.isCollections
    }

    /// Whether Collections may be pushed from here: only from an unfiltered ROOT shelf. (From a
    /// Collections root, the way to the plain shelf is Y "All titles", which is a different push.)
    public var canOpenCollections: Bool {
        isRoot && top == .shelf(filter: nil)
    }

    /// Whether the "All titles" way out is offered: only on a Collections ROOT — a Collections
    /// place reached from a shelf already has that shelf one B away.
    public var offersAllTitles: Bool {
        isRoot && top.isCollections
    }

    public mutating func push(_ place: LibraryPlace) {
        places.append(place)
    }

    /// Pop the top place; `false` at the root (the caller dismisses the layer instead).
    @discardableResult
    public mutating func pop() -> Bool {
        guard places.count > 1 else { return false }
        places.removeLast()
        return true
    }
}

/// The "start in collections" hand-over — decided ONCE per shelf, from the setting and the
/// library as it stands. The desktop's `collections_upgrade`, minus its epoch: an Apple shelf's
/// view owns its own fetch, so a stale list from another host cannot reach it.
public enum CollectionsHandover {
    public enum Verdict: Equatable, Sendable {
        /// Nothing to decide on yet (the list is loading, empty or errored) — ask again later,
        /// the decision is NOT consumed.
        case wait
        /// Decided: the shelf it is (setting off, drilled, or not worth browsing).
        case shelf
        /// Decided: open on the Collections tiles.
        case collections
    }

    /// - `settingOn`: `punktfunk.libraryCollections`.
    /// - `alreadyDecided`: this shelf decided once already — a later rescan or refresh must not
    ///   yank a browsing user away.
    /// - `drilled`: the place stack is already past its root.
    /// - `ready`: the catalog is Ready — a CACHED catalog counts (the hand-over may fire on it,
    ///   before the host answers; that is what made the cache worth having). Loading / empty /
    ///   error decide nothing.
    /// - `worthBrowsing`: `LibraryCollation.worthBrowsing(games)`.
    public static func decide(
        settingOn: Bool, alreadyDecided: Bool, drilled: Bool, ready: Bool, worthBrowsing: Bool
    ) -> Verdict {
        if alreadyDecided || !settingOn || drilled { return .shelf }
        guard ready else { return .wait }
        return worthBrowsing ? .collections : .shelf
    }
}
