// The Swift twin of pf-client-core's `overlay_actions` tests — the same blobs, the same
// outcomes, so the two parsers cannot drift.

import XCTest
@testable import PunktfunkShared

final class OverlayActionsTests: XCTestCase {
    private let full = """
        {"v":2,
         "ring":["end_stream","shortcut:s1","host:power.sleep","stats",null,"pad"],
         "shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]}],
         "pad":{"layout":"sticks","opacity":0.3,"scale":1.2}}
        """

    func testRoundTripsThroughJSON() {
        let cfg = OverlayConfig.parse(full)
        XCTAssertEqual(cfg.ring[1], .shortcut("s1"))
        XCTAssertEqual(cfg.ring[2], .host("power.sleep"))
        XCTAssertNil(cfg.ring[4])
        XCTAssertEqual(cfg.pad.layout, "sticks")
        XCTAssertEqual(cfg.shortcut("s1")?.keys, ["ctrl", "shift", "escape"])
        XCTAssertEqual(OverlayConfig.parse(cfg.toJSON()), cfg)
    }

    func testShortRingsPadAndLongRingsTruncate() {
        let short = OverlayConfig.parse(#"{"ring":["mic"]}"#, platform: .desktop)
        XCTAssertEqual(short.ring.count, 6)
        XCTAssertEqual(short.ring[0], .mic)
        XCTAssertTrue(short.ring.dropFirst().allSatisfy { $0 == nil })
        let long = OverlayConfig.parse(
            #"{"ring":["mic","mic","mic","mic","mic","mic","stats","stats"]}"#, platform: .desktop)
        XCTAssertEqual(long.ring.count, 6)
        XCTAssertTrue(long.ring.allSatisfy { $0 == .mic })
    }

    func testUnknownIdsAndDanglingShortcutsAreEmptySlots() {
        let cfg = OverlayConfig.parse(#"{"ring":["teleport","shortcut:nope","host:","stats"]}"#)
        XCTAssertNil(cfg.ring[0], "a newer client's id degrades to empty")
        XCTAssertNil(cfg.ring[1], "no such shortcut")
        XCTAssertNil(cfg.ring[2], "a host id needs a name")
        XCTAssertEqual(cfg.ring[3], .stats)
    }

    func testEmptyOrBrokenBlobsAreThePlatformDefault() {
        let touch = OverlayConfig.platformDefault(.touch)
        let desktop = OverlayConfig.platformDefault(.desktop)
        XCTAssertEqual(OverlayConfig.parse(""), touch)
        XCTAssertEqual(OverlayConfig.parse(nil), touch)
        XCTAssertEqual(OverlayConfig.parse("{not json", platform: .desktop), desktop)
        XCTAssertEqual(touch.ring[5], .pad)
        XCTAssertEqual(desktop.ring[5], .sendText)
        let cfg = OverlayConfig.parse(#"{"v":2,"ring":[]}"#)
        XCTAssertEqual(cfg.pad, PadConfig())
        XCTAssertTrue(cfg.ring.allSatisfy { $0 == nil })
    }

    /// The Swift port of `pad_control_tweaks_round_trip_and_carry_unknown_ids`.
    func testPadControlTweaksRoundTripAndCarryUnknownIds() {
        let blob = """
            {"v":2,"pad":{"layout":"full","opacity":0.45,"scale":1.0,
             "controls":{"ls":{"x":0.1,"y":0.8,"scale":1.5},"weird":{"hidden":true}},
             "controls_narrow":{"face":{"scale":0.75}}}}
            """
        let cfg = OverlayConfig.parse(blob)
        XCTAssertEqual(cfg.pad.controls["ls"], PadTweak(x: 0.1, y: 0.8, scale: 1.5))
        XCTAssertEqual(cfg.pad.controls["weird"]?.hidden, true, "an unknown id is data, not an error")
        XCTAssertEqual(cfg.pad.controlsNarrow["face"]?.scale, 0.75)
        let json = cfg.toJSON()
        XCTAssertTrue(json.contains("weird"), "a rewrite keeps what it does not know")
        XCTAssertEqual(OverlayConfig.parse(json), cfg)
        let plain = OverlayConfig.platformDefault(.touch).toJSON()
        XCTAssertFalse(plain.contains("controls"), "an untouched pad keeps its blob clean")
        let sparse = OverlayConfig.parse(#"{"pad":{"controls":{"rs":{"x":0.5}}}}"#).toJSON()
        XCTAssertTrue(sparse.contains(#""rs":{"x":0.5}"#), "absent fields stay absent: \(sparse)")
    }

    func testKeyNamesMapToWindowsVKs() {
        XCTAssertEqual(keyVk("ctrl"), 0x11)
        XCTAssertEqual(keyVk("Shift"), 0x10)
        XCTAssertEqual(keyVk("escape"), 0x1B)
        XCTAssertEqual(keyVk("tab"), 0x09)
        XCTAssertEqual(keyVk("a"), 0x41)
        XCTAssertEqual(keyVk("z"), 0x5A)
        XCTAssertEqual(keyVk("0"), 0x30)
        XCTAssertEqual(keyVk("f1"), 0x70)
        XCTAssertEqual(keyVk("f12"), 0x7B)
        XCTAssertNil(keyVk("f25"))
        XCTAssertNil(keyVk("hyper"))
        XCTAssertNil(keyVk(""))
        XCTAssertEqual(chordChip(["ctrl", "shift", "escape"]), "Ctrl+Shift+Esc")
        XCTAssertEqual(keyLegend("win"), "Win")
        XCTAssertEqual(keyLegend("pageup"), "PgUp")
        XCTAssertEqual(keyLegend("f4"), "F4")
        XCTAssertEqual(keyLegend("left"), "←")
    }

    func testSlotIdsAreStableStrings() {
        for id in [
            "end_stream", "disconnect_linger", "touch_mode", "keyboard", "stats", "mic", "pad",
            "send_text", "host:power.reboot", "shortcut:s2",
        ] {
            XCTAssertEqual(SlotId.parse(id)?.id, id)
        }
    }
}
