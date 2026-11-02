#if os(macOS)
import AppKit
import XCTest
@testable import PunktfunkKit

final class RawMotionTests: XCTestCase {
    /// A captured move carries the mouse's own counts, not the accelerated delta beside them.
    func testCapturedMotionIsTheUnacceleratedCounts() throws {
        let cg = try XCTUnwrap(CGEvent(
            mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: .zero,
            mouseButton: .left))
        cg.setIntegerValueField(.mouseEventDeltaX, value: 14)
        cg.setIntegerValueField(.mouseEventDeltaY, value: -9)
        cg.setIntegerValueField(.eventUnacceleratedPointerMovementX, value: 5)
        cg.setIntegerValueField(.eventUnacceleratedPointerMovementY, value: -3)
        let event = try XCTUnwrap(NSEvent(cgEvent: cg))
        XCTAssertEqual(event.deltaX, 14)

        let counts = StreamLayerView.rawMotion(of: event)
        XCTAssertEqual(counts.dx, 5)
        XCTAssertEqual(counts.dy, -3)
    }
}
#endif
