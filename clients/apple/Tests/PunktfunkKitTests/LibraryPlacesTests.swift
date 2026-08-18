import XCTest

@testable import PunktfunkKit

/// The library layer's place stack and the start-in-collections hand-over — the desktop's
/// screen-stack flows for the library, as values.
final class LibraryPlacesTests: XCTestCase {
    /// shelf → Y → collections → A → filtered shelf → B → collections → B → shelf → B → dismiss.
    func testYPushesCollectionsAPushesAFilteredShelfAndBReturnsTheWayItCame() {
        var stack = LibraryPlaceStack(root: .shelf(filter: nil))
        XCTAssertTrue(stack.canOpenCollections)
        XCTAssertFalse(stack.drilled)
        XCTAssertFalse(stack.offersAllTitles)

        stack.push(.collections)
        XCTAssertEqual(stack.top, .collections)
        XCTAssertFalse(stack.offersAllTitles, "a Collections place under a shelf has that shelf one B away")
        XCTAssertFalse(stack.drilled)

        stack.push(.shelf(filter: .platform("PS3")))
        XCTAssertTrue(stack.drilled)
        XCTAssertFalse(stack.canOpenCollections, "Y is refused on a drilled shelf")
        XCTAssertEqual(stack.top.filter, .platform("PS3"))

        XCTAssertTrue(stack.pop())
        XCTAssertEqual(stack.top, .collections)
        XCTAssertTrue(stack.pop())
        XCTAssertEqual(stack.top, .shelf(filter: nil))
        XCTAssertTrue(stack.isRoot)
        XCTAssertFalse(stack.pop(), "at the root, B is the layer's dismiss")
        XCTAssertEqual(stack.places.count, 1)
    }

    /// The way to "All titles" exists only where there is no shelf underneath.
    func testTheWayToAllTitlesExistsOnlyWhereThereIsNoShelf() {
        var stack = LibraryPlaceStack(root: .collections)
        XCTAssertTrue(stack.offersAllTitles)
        XCTAssertFalse(stack.canOpenCollections)

        stack.push(.shelf(filter: nil))
        XCTAssertTrue(stack.drilled, "the All-titles shelf is a drill-in — Y is refused there too")
        XCTAssertFalse(stack.canOpenCollections)
        XCTAssertFalse(stack.offersAllTitles)
        stack.pop()
        XCTAssertTrue(stack.offersAllTitles)
    }

    func testTheHandoverIsDecidedOnceFromTheSettingAndTheLibrary() {
        typealias H = CollectionsHandover
        // Off: the shelf, whatever else.
        XCTAssertEqual(H.decide(settingOn: false, alreadyDecided: false, drilled: false, ready: true, worthBrowsing: true), .shelf)
        // Decided already: never again for this shelf.
        XCTAssertEqual(H.decide(settingOn: true, alreadyDecided: true, drilled: false, ready: true, worthBrowsing: true), .shelf)
        // Drilled: refused outright.
        XCTAssertEqual(H.decide(settingOn: true, alreadyDecided: false, drilled: true, ready: true, worthBrowsing: true), .shelf)
        // Not ready (loading / empty / error): decide nothing, decision NOT consumed.
        XCTAssertEqual(H.decide(settingOn: true, alreadyDecided: false, drilled: false, ready: false, worthBrowsing: true), .wait)
        // Ready — a cached catalog counts — and worth browsing: open on the tiles.
        XCTAssertEqual(H.decide(settingOn: true, alreadyDecided: false, drilled: false, ready: true, worthBrowsing: true), .collections)
        // Ready but one group: the shelf stays.
        XCTAssertEqual(H.decide(settingOn: true, alreadyDecided: false, drilled: false, ready: true, worthBrowsing: false), .shelf)
    }
}
