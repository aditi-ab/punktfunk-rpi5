// Arrow-key navigation over the library's two-section poster grid. The cases that matter are the
// ones a flat index gets wrong: a PARTIAL last row, and the hand-off between the launcher section
// and the titles below it.

import XCTest
@testable import PunktfunkKit

final class LibraryGridNavTests: XCTestCase {
    /// Two sections, 3 columns:
    ///   launchers  L0 L1          (one partial row)
    ///   titles     T0 T1 T2
    ///              T3 T4
    private let nav = LibraryGridNav(
        sections: [["L0", "L1"], ["T0", "T1", "T2", "T3", "T4"]], columns: 3)

    func testFirstPressSelectsTheFirstTile() {
        XCTAssertEqual(nav.move(from: nil, .right), "L0")
        XCTAssertEqual(nav.move(from: nil, .down), "L0")
    }

    func testHorizontalMovesWithinARow() {
        XCTAssertEqual(nav.move(from: "T0", .right), "T1")
        XCTAssertEqual(nav.move(from: "T1", .left), "T0")
    }

    /// Left/right run through the whole grid in display order, crossing the section boundary —
    /// the launchers are simply the first tiles.
    func testHorizontalCrossesTheSectionBoundary() {
        XCTAssertEqual(nav.move(from: "L1", .right), "T0")
        XCTAssertEqual(nav.move(from: "T0", .left), "L1")
    }

    func testVerticalMovesOneRowWithinASection() {
        XCTAssertEqual(nav.move(from: "T0", .down), "T3")
        XCTAssertEqual(nav.move(from: "T3", .up), "T0")
    }

    /// Down from the launcher row lands in the titles' first row at the SAME column — this is the
    /// move a flat index gets wrong, because the launcher row is partial.
    func testDownFromLaunchersKeepsTheColumn() {
        XCTAssertEqual(nav.move(from: "L0", .down), "T0")
        XCTAssertEqual(nav.move(from: "L1", .down), "T1")
    }

    /// Up out of the titles' first row lands in the launcher row, clamped to what is actually
    /// there: column 2 has no launcher above it, so it settles on the last one rather than
    /// running off the end.
    func testUpIntoAPartialLauncherRowClamps() {
        XCTAssertEqual(nav.move(from: "T0", .up), "L0")
        XCTAssertEqual(nav.move(from: "T1", .up), "L1")
        XCTAssertEqual(nav.move(from: "T2", .up), "L1")
    }

    /// Down from a full row into a SHORTER last row still moves — landing on the final tile — but
    /// there is nothing below the last row itself.
    func testDownIntoAPartialLastRow() {
        XCTAssertEqual(nav.move(from: "T2", .down), "T4") // column 2 has no T5
        XCTAssertNil(nav.move(from: "T4", .down))
    }

    func testEdgesRefuseRatherThanWrap() {
        XCTAssertNil(nav.move(from: "L0", .left))
        XCTAssertNil(nav.move(from: "L0", .up))
        XCTAssertNil(nav.move(from: "T4", .right))
    }

    /// A library with no launcher entries renders ONE section — the common case, and it must
    /// behave like a plain grid.
    func testSingleSectionGrid() {
        let single = LibraryGridNav(sections: [["A", "B", "C", "D"]], columns: 2)
        XCTAssertEqual(single.move(from: "A", .down), "C")
        XCTAssertEqual(single.move(from: "D", .up), "B")
        XCTAssertNil(single.move(from: "A", .up))
    }

    /// An id that is no longer in the grid (the list reloaded under the cursor) re-seeds rather
    /// than returning nil forever.
    func testStaleCursorReseeds() {
        XCTAssertEqual(nav.move(from: "gone", .down), "L0")
    }

    func testEmptyGridHasNowhereToGo() {
        let empty = LibraryGridNav(sections: [], columns: 3)
        XCTAssertNil(empty.move(from: nil, .down))
    }

    /// A degenerate column count must not divide by zero.
    func testZeroColumnsIsClampedToOne() {
        let single = LibraryGridNav(sections: [["A", "B"]], columns: 0)
        XCTAssertEqual(single.columns, 1)
        XCTAssertEqual(single.move(from: "A", .down), "B")
    }
}
