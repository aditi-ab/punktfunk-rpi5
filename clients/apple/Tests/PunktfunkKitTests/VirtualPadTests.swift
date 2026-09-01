// The virtual controller's pure parts — preset geometry, the D-pad's angle, the stick's travel,
// the trigger's pull — the Swift twin of the Android `VirtualPadTest`.

import XCTest
@testable import PunktfunkKit
import PunktfunkShared

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

    func testPresetIdsAreTheSchemaIds() {
        XCTAssertEqual(Set(padControls(layout: "full", w: 933, h: 420).map(\.id)),
                       ["lb", "lt", "rb", "rt", "ls", "rs", "dpad", "face", "select", "guide", "start"])
    }

    func testTweaksMoveScaleHideAndClamp() {
        let base = padControls(layout: "full", w: 933, h: 420)
        let ls = base.first { $0.id == "ls" }!
        let out = applyPadTweaks(base, tweaks: [
            "ls": PadTweak(x: 0.5, y: 0.5, scale: 2),
            "start": PadTweak(scale: 9), // clamps to the bound
            "face": PadTweak(hidden: true),
            "select": PadTweak(x: 0, y: 0), // clamps onto the layer
            "nope": PadTweak(x: 0.9), // no such control: ignored
        ], w: 933, h: 420)
        let moved = out.first { $0.id == "ls" }!
        XCTAssertEqual(moved.rect.w, 2 * ls.rect.w, accuracy: 1e-3)
        XCTAssertEqual(moved.rect.x + moved.rect.w / 2, 466.5, accuracy: 1e-3)
        XCTAssertEqual(moved.rect.y + moved.rect.h / 2, 210, accuracy: 1e-3)
        let start = out.first { $0.id == "start" }!
        XCTAssertEqual(start.sc, VirtualPad.tweakScaleRange.upperBound)
        guard case .buttons(let discs) = start.kind, case .buttons(let baseDiscs) = base.first(where: { $0.id == "start" })!.kind else {
            return XCTFail("start is a disc group")
        }
        XCTAssertEqual(discs[0].r, baseDiscs[0].r * VirtualPad.tweakScaleRange.upperBound, accuracy: 1e-3)
        XCTAssertTrue(out.first { $0.id == "face" }!.hidden, "hidden stays in the list, marked")
        let clamped = out.first { $0.id == "select" }!.rect
        XCTAssertTrue(clamped.x >= 0 && clamped.y >= 0, "\(clamped) stays on the layer")
        XCTAssertEqual(out.count, base.count)
    }

    func testAPadConfigPicksTheLayerClassForItsOverrides() {
        let pad = PadConfig(controls: ["face": PadTweak(hidden: true)],
                            controlsNarrow: ["ls": PadTweak(hidden: true)])
        let wide = padControls(pad: pad, w: 933, h: 420)
        XCTAssertTrue(wide.first { $0.id == "face" }!.hidden)
        XCTAssertFalse(wide.first { $0.id == "ls" }!.hidden)
        let narrow = padControls(pad: pad, w: 420, h: 933)
        XCTAssertTrue(narrow.first { $0.id == "ls" }!.hidden)
        XCTAssertFalse(narrow.first { $0.id == "face" }!.hidden)
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
