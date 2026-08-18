import XCTest

@testable import PunktfunkKit

/// The gamepad grid's shape and cursor, against the desktop's `library.rs` grid tests — the same
/// nine cases by name, over the same shapes, so a rule that moves there is a red test here.
final class LibraryGridCursorTests: XCTestCase {
    /// (len, cols, launchers) — the desktop's `SHAPES` fixture.
    private let shapes: [(Int, Int, Int)] = [
        (11, 4, 0), (40, 5, 0), (30, 7, 2), (20, 4, 6), (4, 4, 2), (9, 3, 3), (7, 3, 7),
        (1, 3, 1), (13, 1, 2),
    ]

    private func shape(_ t: (Int, Int, Int)) -> LibraryGridShape {
        LibraryGridShape(len: t.0, cols: t.1, launchers: t.2)
    }

    private func step(
        _ cursor: Int, _ s: LibraryGridShape, _ hint: Int, _ dir: LibraryGridDirection
    ) -> LibraryGridStep {
        LibraryGridCursor.step(cursor, shape: s, colHint: hint, direction: dir)
    }

    /// The invariant the two old layout models broke: a cell's coordinates and its row's
    /// extent come from one shape, and every index is in exactly one row.
    func testGridRowsTileTheFieldExactlyOnce() {
        for t in shapes {
            let s = shape(t)
            var next = 0
            for row in 0..<s.rows {
                let n = s.rowLen(row)
                XCTAssertTrue((1...t.1).contains(n), "\(t) row \(row) holds \(n) cells")
                for col in 0..<n {
                    let i = s.rowStart(row) + col
                    XCTAssertEqual(i, next, "\(t) row \(row) does not follow the one above")
                    XCTAssertEqual(s.cell(of: i).row, row, "\(t) disagrees about index \(i)")
                    XCTAssertEqual(s.cell(of: i).col, col, "\(t) disagrees about index \(i)")
                    next += 1
                }
            }
            XCTAssertEqual(next, t.0, "\(t) left cells in no row at all")
        }
    }

    /// A row END refuses (like the shelf), and it is the row's TRUE end — not `cols`, which
    /// is a different number in every partial row and in the whole games section.
    func testGridHorizontalMovesWalkTheRowAndRefuseItsTrueEnds() {
        for t in shapes {
            let s = shape(t)
            for i in 0..<t.0 {
                let (row, col) = s.cell(of: i)
                for hint in 0..<t.1 {
                    XCTAssertEqual(
                        step(i, s, hint, .left), col == 0 ? .boundary : .moved(i - 1),
                        "\(t) Left from \(i)")
                    XCTAssertEqual(
                        step(i, s, hint, .right),
                        col + 1 == s.rowLen(row) ? .boundary : .moved(i + 1),
                        "\(t) Right from \(i)")
                }
            }
        }
    }

    /// Vertical moves change the ROW and nothing else, arriving in the remembered column
    /// clamped into whatever that row can hold.
    func testGridVerticalMovesChangeRowAndCarryTheColumn() {
        let vertical: [(LibraryGridDirection, Int)] = [
            (.up, -1), (.down, 1), (.pageBack, -LibraryGridCursor.pageRows),
            (.pageForward, LibraryGridCursor.pageRows),
        ]
        for t in shapes {
            let s = shape(t)
            for i in 0..<t.0 {
                let row = s.cell(of: i).row
                for hint in 0..<t.1 {
                    for (dir, d) in vertical {
                        let wantRow = min(max(row + d, 0), s.rows - 1)
                        let what = "\(t) \(dir) from \(i) with hint \(hint)"
                        switch step(i, s, hint, dir) {
                        case .moved(let to):
                            let (r, c) = s.cell(of: to)
                            XCTAssertEqual(r, wantRow, "\(what) landed in row \(r)")
                            if r != row {
                                XCTAssertEqual(c, min(hint, s.rowLen(r) - 1), "\(what) column")
                            }
                        case .boundary:
                            XCTAssertEqual(wantRow, row, "\(what) refused")
                        }
                    }
                }
            }
        }
    }

    /// Both sections stay reachable from anywhere in the other, in a bounded number of presses.
    func testEveryRowIsReachableByStepping() {
        for t in shapes {
            let s = shape(t)
            for i in 0..<t.0 {
                for (dir, end) in [(LibraryGridDirection.up, 0), (.down, s.rows - 1)] {
                    var cursor = i
                    for _ in 0...s.rows {
                        if case .moved(let to) = step(cursor, s, 0, dir) { cursor = to } else { break }
                    }
                    XCTAssertEqual(s.cell(of: cursor).row, end, "\(t) \(dir) from \(i) stalled")
                }
            }
        }
    }

    /// The Deck, exactly: 1280×800 gives seven columns, and a host with the usual two launchers
    /// puts them alone on row 0 with the games restarting at column 0 under them.
    func testTheLauncherRowSitsSquarelyAboveTheGames() {
        let s = LibraryGridShape(len: 30, cols: 7, launchers: 2)
        XCTAssertEqual(s.rows, 5)
        XCTAssertEqual(s.rowLen(0), 2)
        XCTAssertEqual(s.rowLen(1), 7)
        XCTAssertEqual(step(0, s, 0, .down), .moved(2))
        XCTAssertEqual(step(1, s, 1, .down), .moved(3))
        XCTAssertEqual(step(2, s, 0, .up), .moved(0))
        XCTAssertEqual(step(3, s, 1, .up), .moved(1))
        XCTAssertEqual(step(6, s, 4, .up), .moved(1))
        XCTAssertEqual(step(6, s, 4, .right), .moved(7))
        XCTAssertEqual(step(7, s, 5, .left), .moved(6))
        XCTAssertEqual(step(8, s, 6, .right), .boundary)
        XCTAssertEqual(step(2, s, 0, .left), .boundary)
    }

    /// The remembered column is what makes a crossing reversible.
    func testACrossingReturnsToTheColumnItStartedFrom() {
        let s = LibraryGridShape(len: 30, cols: 7, launchers: 2)
        func walk(_ start: Int, _ dirs: [LibraryGridDirection]) -> Int {
            var cursor = start
            var hint = s.cell(of: max(start, 0)).col
            for dir in dirs {
                if case .moved(let to) = step(cursor, s, hint, dir) {
                    hint = LibraryGridCursor.colHint(shape: s, previous: hint, direction: dir, landed: to)
                    cursor = to
                }
            }
            return cursor
        }
        XCTAssertEqual(walk(0, [.down, .right, .right, .right, .right]), 6)
        XCTAssertEqual(walk(0, [.down, .right, .right, .right, .right, .up]), 1)
        XCTAssertEqual(walk(0, [.down, .right, .right, .right, .right, .up, .down]), 6)
        XCTAssertEqual(walk(0, [.down, .right, .right, .right, .right, .up, .down, .up, .down]), 6)
    }

    func testGridPagesByRowsAndLandsOnTheEnds() {
        let s = LibraryGridShape(len: 40, cols: 5, launchers: 0)
        XCTAssertEqual(step(0, s, 0, .pageForward), .moved(15))
        XCTAssertEqual(step(35, s, 0, .pageForward), .moved(39))
        XCTAssertEqual(step(39, s, 4, .pageForward), .boundary)
        XCTAssertEqual(step(3, s, 3, .pageBack), .moved(0))
        XCTAssertEqual(step(0, s, 0, .pageBack), .boundary)
    }

    /// The launcher-less grid, unchanged: rows of 4, 4, 3.
    func testGridRowsRefuseAtTheirEndsButTheTailRowClamps() {
        let s = LibraryGridShape(len: 11, cols: 4, launchers: 0)
        XCTAssertEqual(step(1, s, 1, .right), .moved(2))
        XCTAssertEqual(step(2, s, 2, .left), .moved(1))
        XCTAssertEqual(step(3, s, 3, .right), .boundary)
        XCTAssertEqual(step(4, s, 0, .left), .boundary)
        XCTAssertEqual(step(1, s, 1, .down), .moved(5))
        XCTAssertEqual(step(7, s, 3, .down), .moved(10))
        XCTAssertEqual(step(10, s, 3, .down), .boundary)
        XCTAssertEqual(step(2, s, 2, .up), .boundary)
        XCTAssertEqual(step(6, s, 2, .up), .moved(2))
    }

    func testGridStepIsSafeOnADegenerateGrid() {
        let empty = LibraryGridShape(len: 0, cols: 4, launchers: 0)
        XCTAssertEqual(step(0, empty, 0, .right), .boundary)
        let colless = LibraryGridShape(len: 5, cols: 0, launchers: 0)
        XCTAssertEqual(step(0, colless, 0, .right), .boundary)
        let thin = LibraryGridShape(len: 5, cols: 1, launchers: 0)
        XCTAssertEqual(step(1, thin, 0, .right), .boundary)
        XCTAssertEqual(step(1, thin, 0, .down), .moved(2))
        let s = LibraryGridShape(len: 6, cols: 3, launchers: 2)
        XCTAssertEqual(step(99, s, 0, .up), .moved(2))
        XCTAssertEqual(step(-4, s, 0, .right), .moved(1))
    }
}
