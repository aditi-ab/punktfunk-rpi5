// Where the arrow keys go in the library's plain poster grid (LibraryView's touch layout on
// iOS/iPadOS/macOS) — the model behind "select games with keyboard arrows, enter to launch".
//
// The grid is up to TWO sections (launcher entries above titles), each rendered as its own
// `LazyVGrid`. A single flat index across both would step by the wrong amount at the boundary
// whenever the first section's last row is partial — up from the second section's first row would
// land in the middle of the first section rather than on the row above. So moves happen WITHIN a
// section, with an explicit hand-off at its edges that preserves the column.
//
// Lives in PunktfunkKit rather than beside the view because this is arithmetic with edge cases —
// partial rows, section hand-offs, empty sections — and PunktfunkKit is the target the tests can
// reach (the app is an executable target). Pure values in, pure value out: no SwiftUI.

import Foundation

public struct LibraryGridNav {
    /// Game ids per RENDERED section, in display order. Callers drop empty sections before
    /// constructing this, so `sections` never contains one.
    public let sections: [[String]]
    /// How many columns the grid actually laid out — the caller derives it from the measured
    /// width using `.adaptive`'s own fitting rule, so a vertical move is exactly one visual row.
    public let columns: Int

    public init(sections: [[String]], columns: Int) {
        self.sections = sections
        // A zero or negative count would divide by zero below; one column is the degenerate grid.
        self.columns = max(1, columns)
    }

    /// The id `direction` leads to from `current`, or nil when there is nowhere to go (so the
    /// caller leaves the cursor where it is). A nil `current` — nothing selected yet — lands on
    /// the very first tile, so the first arrow press always produces a visible cursor rather than
    /// appearing to do nothing.
    public func move(from current: String?, _ direction: GamepadMenuInput.Direction) -> String? {
        guard !sections.isEmpty else { return nil }
        guard let (s, i) = locate(current) else { return sections[0].first }
        switch direction {
        case .left:
            if i > 0 { return sections[s][i - 1] }
            return s > 0 ? sections[s - 1].last : nil
        case .right:
            if i + 1 < sections[s].count { return sections[s][i + 1] }
            return s + 1 < sections.count ? sections[s + 1].first : nil
        case .up:
            if i >= columns { return sections[s][i - columns] }
            // Off the top of this section: the section above, same column, its LAST row —
            // clamped, because that row may be partial.
            guard s > 0 else { return nil }
            let above = sections[s - 1]
            let lastRowStart = ((above.count - 1) / columns) * columns
            return above[min(lastRowStart + (i % columns), above.count - 1)]
        case .down:
            if i + columns < sections[s].count { return sections[s][i + columns] }
            // Off the bottom: the section below, same column, its first row.
            if s + 1 < sections.count {
                let below = sections[s + 1]
                return below[min(i % columns, below.count - 1)]
            }
            // Nothing below. A press from a full row above the last (partial) one still settles
            // on the final tile rather than refusing — the row IS down from here, just short.
            let lastRowStart = ((sections[s].count - 1) / columns) * columns
            return i < lastRowStart ? sections[s].last : nil
        }
    }

    /// (section, index within it) for an id, or nil when it isn't in the grid any more.
    private func locate(_ id: String?) -> (Int, Int)? {
        guard let id else { return nil }
        for (s, section) in sections.enumerated() {
            if let i = section.firstIndex(of: id) { return (s, i) }
        }
        return nil
    }
}
