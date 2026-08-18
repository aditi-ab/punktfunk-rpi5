// Cursor arithmetic for the gamepad library's GRID arrangement — the port of `pf-console-ui`'s
// `GridShape` / `grid_step` / `grid_col_hint` (`library.rs`), which is the portable rule set:
//
// ONE shape both the renderer and the cursor read. The overhaul's "focus engine disagreed with
// its own layout" defect came from having two descriptions of the same field, which agreed only
// when `launchers % cols == 0`. So the shape carries the launcher SPLIT — the launcher rows sit
// above the game rows as a prefix with the games restarting at column 0 — and every question
// (which cell is index i, how long is row r, where does row r start) is asked of it.
//
// ONE rule for moves, because an accretion of special cases is how this broke: horizontal moves
// walk the row and refuse at THAT ROW's true ends; vertical moves and pages change row only,
// carrying the remembered column and clamping it into the target row's length. Left/right
// refusing rather than wrapping is the shelf's rule and the one a thumb already knows — a held
// Right that wrapped would scan the whole library, and there are shoulders for that.
//
// This is deliberately NOT `LibraryGridNav`, the touch grid's hardware-keyboard cursor: that one
// runs left/right flat across the section boundary, because a keyboard has no bump or haptic
// vocabulary to say "end of row". Pure values in, pure values out: no SwiftUI.

import Foundation

/// The grid's layout, as the cursor and the renderer both read it.
public struct LibraryGridShape: Hashable, Sendable {
    /// Cells per row. A RENDER fact — it depends on the window — so callers build the shape
    /// from what was actually laid out rather than deriving it twice from two widths.
    public let cols: Int
    /// How many cells there are: the FILTERED count, the one the cursor indexes.
    public let len: Int
    /// Where the games section starts, or 0 when the field is one continuous run.
    public let split: Int

    /// `launchers` is the leading launcher run. The section only exists when BOTH halves do —
    /// an all-launcher or launcher-less field is a plain grid, and giving it a heading band and a
    /// gap it has no second group for would be a rule showing off.
    public init(len: Int, cols: Int, launchers: Int) {
        self.len = max(0, len)
        self.cols = max(0, cols)
        self.split = (launchers > 0 && launchers < len) ? launchers : 0
    }

    private var safeCols: Int { max(1, cols) }

    /// The first row of the games section (meaningless when there is no split).
    public var splitRow: Int { (split + safeCols - 1) / safeCols }

    /// Which cell an index is drawn in.
    public func cell(of i: Int) -> (row: Int, col: Int) {
        if split > 0, i >= split {
            let j = i - split
            return (splitRow + j / safeCols, j % safeCols)
        }
        return (i / safeCols, i % safeCols)
    }

    public var rows: Int {
        if split > 0 {
            return splitRow + (len - split + safeCols - 1) / safeCols
        }
        return (len + safeCols - 1) / safeCols
    }

    /// The index of a row's first cell.
    public func rowStart(_ row: Int) -> Int {
        if split > 0, row >= splitRow {
            return split + (row - splitRow) * safeCols
        }
        return row * safeCols
    }

    /// How many cells a row actually holds — the launcher section's last row stops where the
    /// games section begins, and the field's last row stops at the end of the library.
    public func rowLen(_ row: Int) -> Int {
        let start = rowStart(row)
        let end = (split > 0 && row + 1 == splitRow) ? split : len
        return min(max(0, end - start), safeCols)
    }
}

/// Where the cursor is asked to go.
public enum LibraryGridDirection: Hashable, Sendable {
    case left, right, up, down, pageBack, pageForward
}

/// The answer: a new index, or a refusal (the caller bumps).
public enum LibraryGridStep: Hashable, Sendable {
    case moved(Int)
    case boundary
}

public enum LibraryGridCursor {
    /// L1/R1 page this many rows.
    public static let pageRows = 3

    /// The move — see the file header for the rule. A cursor outside the field is a stale one
    /// (the library shortened under us); reading it as the nearest real cell makes the next
    /// press heal it instead of compounding it.
    public static func step(
        _ cursor: Int, shape: LibraryGridShape, colHint: Int, direction: LibraryGridDirection
    ) -> LibraryGridStep {
        guard shape.len > 0, shape.cols > 0 else { return .boundary }
        let (row, col) = shape.cell(of: min(max(cursor, 0), shape.len - 1))
        func moved(_ i: Int) -> LibraryGridStep { i == cursor ? .boundary : .moved(i) }
        switch direction {
        case .left:
            return col == 0 ? .boundary : moved(shape.rowStart(row) + col - 1)
        case .right:
            return col + 1 >= shape.rowLen(row) ? .boundary : moved(shape.rowStart(row) + col + 1)
        case .up, .down, .pageBack, .pageForward:
            let (delta, paging): (Int, Bool)
            switch direction {
            case .up: (delta, paging) = (-1, false)
            case .down: (delta, paging) = (1, false)
            case .pageBack: (delta, paging) = (-pageRows, true)
            default: (delta, paging) = (pageRows, true)
            }
            let target = min(max(row + delta, 0), shape.rows - 1)
            if target == row {
                // A STEP at the edge refuses. A PAGE is a "take me there", so it lands on the
                // end of the row it is already on — the reading the shoulders have always had.
                guard paging else { return .boundary }
                let c = delta > 0 ? shape.rowLen(row) - 1 : 0
                return moved(shape.rowStart(row) + c)
            }
            let c = min(colHint, shape.rowLen(target) - 1)
            return moved(shape.rowStart(target) + c)
        }
    }

    /// The remembered column after a move. A horizontal step CHOOSES a column; a vertical one
    /// only borrows it — which is what makes crossing a two-wide launcher row and coming back
    /// return to the column the crossing started from.
    public static func colHint(
        shape: LibraryGridShape, previous: Int, direction: LibraryGridDirection, landed: Int
    ) -> Int {
        switch direction {
        case .left, .right: return shape.cell(of: max(landed, 0)).col
        default: return previous
        }
    }
}
