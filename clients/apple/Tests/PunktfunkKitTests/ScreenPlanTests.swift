#if os(macOS)
import AppKit
import XCTest
@testable import PunktfunkKit

final class ScreenPlanTests: XCTestCase {
    /// A display beside a HiDPI main one sits in the main picture's pixels, y counted down from
    /// the main display's top although AppKit counts up from the bottom.
    func testADisplaySitsInTheFirstPicturesPixels() {
        let first = CGRect(x: 0, y: 0, width: 1512, height: 982)
        let mode: (width: UInt32, height: UInt32) = (3024, 1964)
        let right = NSScreen.offset(
            of: CGRect(x: 1512, y: 0, width: 1920, height: 1080), size: (3840, 2160),
            from: first, mode: mode)
        XCTAssertEqual(right.x, 3024)
        XCTAssertEqual(right.y, -196, "its top stands 98 points above the main display's")
        let above = NSScreen.offset(
            of: CGRect(x: -400, y: 982, width: 1920, height: 1080), size: (3840, 2160),
            from: first, mode: mode)
        XCTAssertEqual(above.x, -800)
        XCTAssertEqual(above.y, -2160)
    }

    /// A 1x display left of or above a 2x main one touches the main picture's edge.
    func testASparserDisplayLeftOrAboveLeavesNoGap() {
        let first = CGRect(x: 0, y: 0, width: 1512, height: 982)
        let mode: (width: UInt32, height: UInt32) = (3024, 1964)
        let left = NSScreen.offset(
            of: CGRect(x: -1920, y: 0, width: 1920, height: 1080), size: (1920, 1080),
            from: first, mode: mode)
        XCTAssertEqual(left.x, -1920, "not -3840")
        XCTAssertEqual(left.y, -196)
        let above = NSScreen.offset(
            of: CGRect(x: 0, y: 982, width: 1920, height: 1080), size: (1920, 1080),
            from: first, mode: mode)
        XCTAssertEqual(above.x, 0)
        XCTAssertEqual(above.y, -1080, "not -2160")
    }

    /// A pointer lands on the pixel the fit draws there, and the bars land nowhere.
    func testAPointerLandsOnThePicture() {
        // A square picture in a 400×200 px view: half scale, 100 px bars left and right.
        let bounds = CGRect(x: 0, y: 0, width: 200, height: 100)
        let middle = StreamLayerView.picturePoint(
            CGPoint(x: 100, y: 50), bounds: bounds, scale: 2, content: (400, 400), fit: .fit)
        XCTAssertEqual(middle?.x, 200)
        XCTAssertEqual(middle?.y, 200)
        let top = StreamLayerView.picturePoint(
            CGPoint(x: 100, y: 99), bounds: bounds, scale: 2, content: (400, 400), fit: .fit)
        XCTAssertEqual(top?.y, 4, "view y counts up, picture y counts down")
        XCTAssertNil(StreamLayerView.picturePoint(
            CGPoint(x: 5, y: 50), bounds: bounds, scale: 2, content: (400, 400), fit: .fit))
        XCTAssertNil(StreamLayerView.picturePoint(
            CGPoint(x: 100, y: 50), bounds: bounds, scale: 2, content: (0, 0), fit: .fit))
    }
}
#endif
