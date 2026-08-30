// The virtual controller's pure parts — preset geometry, the D-pad's angle, the stick's travel,
// the trigger's pull — the Swift twin of the Android `VirtualPadTest`.

import XCTest
@testable import PunktfunkKit

final class VirtualPadTests: XCTestCase {
    private let sizes: [(Float, Float)] = [(933, 420), (420, 933), (1024, 768)]

    func testEveryPresetFitsItsLayerWithNoTwoControlsOverlapping() {
        for layout in ["full", "sticks", "dpad", ""] {
            for (w, h) in sizes {
                let ctls = padControls(layout: layout, w: w, h: h)
                for c in ctls {
                    let r = c.rect
                    XCTAssertTrue(r.x >= 0 && r.y >= 0 && r.x + r.w <= w && r.y + r.h <= h,
                                  "\(layout) \(c.label) inside \(w)x\(h): \(r)")
                }
                for i in ctls.indices {
                    for j in (i + 1)..<ctls.count {
                        XCTAssertFalse(ctls[i].rect.overlaps(ctls[j].rect),
                                       "\(layout) \(ctls[i].label) overlaps \(ctls[j].label) at \(w)x\(h)")
                    }
                }
            }
        }
    }

    func testPresetsCarryTheControlsTheDesignNames() {
        func labels(_ layout: String) -> Set<String> { Set(padControls(layout: layout, w: 933, h: 420).map(\.label)) }
        let full = labels("full")
        XCTAssertEqual(full.count, 11)
        XCTAssertTrue(full.isSuperset(of: ["Left stick", "Right stick", "D-pad", "Face buttons", "Left trigger", "Right bumper", "Start"]))
        let sticks = labels("sticks")
        XCTAssertTrue(sticks.isSuperset(of: ["Left stick", "Right stick", "Left bumper", "Right trigger"]))
        XCTAssertFalse(sticks.contains("D-pad") || sticks.contains("Face buttons"))
        let dpad = labels("dpad")
        XCTAssertTrue(dpad.isSuperset(of: ["D-pad", "Face buttons"]))
        XCTAssertFalse(dpad.contains("Left stick") || dpad.contains("Left trigger"))
        XCTAssertEqual(full, labels("bogus"))
    }

    func testDpadReadsEightWaysWithADeadCentre() {
        XCTAssertEqual(dpadBits(dx: 3, dy: -3, dead: 10), 0)
        XCTAssertEqual(dpadBits(dx: 0, dy: -40, dead: 10), GamepadWire.dpadUp)
        XCTAssertEqual(dpadBits(dx: 0, dy: 40, dead: 10), GamepadWire.dpadDown)
        XCTAssertEqual(dpadBits(dx: -40, dy: 0, dead: 10), GamepadWire.dpadLeft)
        XCTAssertEqual(dpadBits(dx: 40, dy: 0, dead: 10), GamepadWire.dpadRight)
        XCTAssertEqual(dpadBits(dx: 30, dy: -30, dead: 10), GamepadWire.dpadUp | GamepadWire.dpadRight)
        XCTAssertEqual(dpadBits(dx: -30, dy: 30, dead: 10), GamepadWire.dpadDown | GamepadWire.dpadLeft)
    }

    func testStickIsNeutralInsideTheDeadZoneAndFullAtTheRadius() {
        func wire(_ dx: Float, _ dy: Float) -> [Int32] {
            let r = stickWire(dx: dx, dy: dy, radius: 100, dead: 6)
            return [r.x, r.y]
        }
        XCTAssertEqual(wire(0, 0), [0, 0])
        XCTAssertEqual(wire(4, -4), [0, 0])
        XCTAssertEqual(wire(100, 0), [32767, 0])
        XCTAssertEqual(wire(250, 0), [32767, 0])
        // Screen +y down is wire +y up.
        XCTAssertEqual(wire(0, -100), [0, 32767])
        // Half way past the dead zone is half deflection (16383.5 rounds up).
        XCTAssertEqual(wire(53, 0), [16384, 0])
    }

    func testTriggerPullsFromNothingAtTheTopToFullAtTheBottom() {
        XCTAssertEqual(triggerWire(y: -5, h: 100), 0)
        XCTAssertEqual(triggerWire(y: 0, h: 100), 0)
        XCTAssertEqual(triggerWire(y: 50, h: 100), 128)
        XCTAssertEqual(triggerWire(y: 100, h: 100), 255)
        XCTAssertEqual(triggerWire(y: 140, h: 100), 255)
    }
}
