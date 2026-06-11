// Table-driven coverage of the DualSense trigger-effect parser: every supported mode
// byte, the packed 10-zone decoding, and the it-must-never-trap guarantee for garbage.
// Pure data → data, no controller needed.

import XCTest

@testable import PunktfunkKit

final class DualSenseTriggerEffectTests: XCTestCase {
    /// Build an 11-byte block: mode + up to 10 params (zero-padded).
    private func block(_ mode: UInt8, _ params: [UInt8] = []) -> [UInt8] {
        var b = [mode] + params
        while b.count < 11 { b.append(0) }
        return b
    }

    /// Pack a 10-zone effect: active-zone bitmask (p0/p1) + 3-bit values (p2...p5).
    private func zones(_ mode: UInt8, values: [UInt8], extra: [UInt8] = []) -> [UInt8] {
        precondition(values.count == 10)
        var mask: UInt16 = 0
        var packed: UInt32 = 0
        for (i, v) in values.enumerated() where v > 0 {
            mask |= 1 << UInt16(i)
            packed |= UInt32(v & 0x07) << (3 * UInt32(i))
        }
        var p: [UInt8] = [
            UInt8(mask & 0xFF), UInt8(mask >> 8),
            UInt8(packed & 0xFF), UInt8((packed >> 8) & 0xFF),
            UInt8((packed >> 16) & 0xFF), UInt8((packed >> 24) & 0xFF),
        ]
        p += extra
        return block(mode, p)
    }

    func testOffModes() {
        XCTAssertEqual(DualSenseTriggerEffect.parse(block(0x00)), .off)
        XCTAssertEqual(DualSenseTriggerEffect.parse(block(0x05, [9, 9, 9])), .off)
        XCTAssertEqual(DualSenseTriggerEffect.parse(block(0xFC)), .off)
    }

    func testUnknownAndGarbageNeverTrap() {
        XCTAssertEqual(DualSenseTriggerEffect.parse([]), .off)
        XCTAssertEqual(DualSenseTriggerEffect.parse([0x99]), .off)
        XCTAssertEqual(DualSenseTriggerEffect.parse([0x42, 0xFF]), .off)
        // Short blocks of known modes parse with zero-padded params.
        XCTAssertEqual(
            DualSenseTriggerEffect.parse([0x01, 128]),
            .feedback(start: 128.0 / 255, strength: 1.0 / 8))
        // Every possible mode byte with random-ish params returns *something*.
        for mode in UInt8.min...UInt8.max {
            _ = DualSenseTriggerEffect.parse(block(mode, [255, 255, 255, 255, 255, 255, 255, 255, 255, 255]))
        }
    }

    func testLegacySimpleModes() {
        // 0x01: continuous resistance (start, force).
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(block(0x01, [51, 255])),
            .feedback(start: 51.0 / 255, strength: 1))
        // Zero force still resists faintly (an active effect is never strength 0).
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(block(0x01, [0, 0])),
            .feedback(start: 0, strength: 1.0 / 8))
        // 0x02: section between start and end.
        guard case let .weapon(start, end, strength) =
            DualSenseTriggerEffect.parse(block(0x02, [51, 204])) else {
            return XCTFail("0x02 should parse as weapon")
        }
        XCTAssertEqual(start, 51.0 / 255, accuracy: 0.001)
        XCTAssertEqual(end, 204.0 / 255, accuracy: 0.001)
        XCTAssertEqual(strength, 1)
        // 0x06: vibration — Nielk1's Simple_Vibration order is (frequency, amplitude, position).
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(block(0x06, [30, 128, 51])),
            .vibration(start: 51.0 / 255, amplitude: 128.0 / 255, frequency: 30.0 / 255))
    }

    func testFeedback0x21UniformSuffixSimplifies() {
        // Zones 3...9 all at strength 4 → one simple feedback call from zone 3.
        let b = zones(0x21, values: [0, 0, 0, 4, 4, 4, 4, 4, 4, 4])
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(b),
            .feedback(start: 3.0 / 9, strength: 5.0 / 8))
    }

    func testFeedback0x21MixedGoesPositional() {
        let b = zones(0x21, values: [0, 0, 2, 0, 0, 7, 0, 0, 0, 1])
        guard case let .positionalFeedback(strengths) = DualSenseTriggerEffect.parse(b) else {
            return XCTFail("mixed zones should parse positional")
        }
        XCTAssertEqual(strengths.count, 10)
        XCTAssertEqual(strengths[2], 3.0 / 8) // 3-bit value 2 → (2+1)/8
        XCTAssertEqual(strengths[5], 8.0 / 8)
        XCTAssertEqual(strengths[9], 2.0 / 8)
        XCTAssertEqual(strengths[0], 0) // inactive zone
        XCTAssertEqual(strengths[3], 0)
    }

    func testFeedback0x21EmptyMaskIsOff() {
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(zones(0x21, values: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
            .off)
    }

    func testWeapon0x25() {
        // Nielk1 Weapon = mode 0x25: start zone 2, end zone 7 from the mask; p2 = strength.
        var b = zones(0x25, values: [0, 0, 1, 0, 0, 0, 0, 1, 0, 0])
        b[3] = 6 // p2 (block[3] = params[2]): strength, stored minus one
        guard case let .weapon(start, end, strength) = DualSenseTriggerEffect.parse(b) else {
            return XCTFail("0x25 should parse as weapon")
        }
        XCTAssertEqual(start, 2.0 / 9, accuracy: 0.001)
        XCTAssertEqual(end, 7.0 / 9, accuracy: 0.001)
        XCTAssertEqual(strength, 7.0 / 8)
        // A single active zone can't form a section.
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(zones(0x25, values: [0, 0, 1, 0, 0, 0, 0, 0, 0, 0])),
            .off)
    }

    func testBow0x22BecomesSlope() {
        // Nielk1 Bow = mode 0x22, draw + snap packed as a 3-bit pair in p2 (p3 always 0).
        var b = zones(0x22, values: [0, 1, 0, 0, 0, 0, 0, 0, 1, 0])
        b[3] = (7 & 0x07) | ((3 & 0x07) << 3) // p2: draw (low) | snap (bits 3-5)
        guard case let .slope(start, end, from, to) = DualSenseTriggerEffect.parse(b) else {
            return XCTFail("0x22 should parse as slope")
        }
        XCTAssertEqual(start, 1.0 / 9, accuracy: 0.001)
        XCTAssertEqual(end, 8.0 / 9, accuracy: 0.001)
        XCTAssertEqual(from, 8.0 / 8)
        XCTAssertEqual(to, 4.0 / 8)
    }

    func testVibration0x26() {
        var b = zones(0x26, values: [0, 0, 0, 0, 0, 5, 5, 5, 5, 5])
        b[9] = 40 // p8 (block[9]): frequency Hz
        guard case let .positionalVibration(amps, freq) = DualSenseTriggerEffect.parse(b) else {
            return XCTFail("0x26 should parse as positional vibration")
        }
        XCTAssertEqual(amps[4], 0)
        XCTAssertEqual(amps[5], 6.0 / 8)
        XCTAssertEqual(amps[9], 6.0 / 8)
        XCTAssertEqual(freq, 40.0 / 255, accuracy: 0.001)
        XCTAssertEqual(
            DualSenseTriggerEffect.parse(zones(0x26, values: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
            .off)
    }

    func testGalloping0x23() {
        // Nielk1 Galloping = mode 0x23: zone range + p3 frequency (p2 is foot timing).
        var b = zones(0x23, values: [0, 0, 1, 0, 0, 0, 1, 0, 0, 0])
        b[4] = 20 // p3 (block[4]): frequency
        guard case let .positionalVibration(amps, freq) = DualSenseTriggerEffect.parse(b) else {
            return XCTFail("0x23 should parse as positional vibration")
        }
        XCTAssertEqual(amps[1], 0)
        XCTAssertEqual(amps[2], 0.5)
        XCTAssertEqual(amps[6], 0.5)
        XCTAssertEqual(amps[7], 0)
        XCTAssertEqual(freq, 20.0 / 255, accuracy: 0.001)
    }

    func testMachine0x27() {
        // Nielk1 Machine = mode 0x27: zone range, p2 = two raw 3-bit amplitudes, p3 = frequency.
        var b = zones(0x27, values: [0, 0, 0, 1, 0, 0, 0, 0, 1, 0])
        b[3] = (2 & 0x07) | ((6 & 0x07) << 3) // p2: amplitude A = 2, B = 6
        b[4] = 90 // p3: frequency
        guard case let .positionalVibration(amps, freq) = DualSenseTriggerEffect.parse(b) else {
            return XCTFail("0x27 should parse as positional vibration")
        }
        XCTAssertEqual(amps[2], 0)
        XCTAssertEqual(amps[3], 6.0 / 7, accuracy: 0.001) // the stronger leg
        XCTAssertEqual(amps[8], 6.0 / 7, accuracy: 0.001)
        XCTAssertEqual(amps[9], 0)
        XCTAssertEqual(freq, 90.0 / 255, accuracy: 0.001)
        // Zero amplitudes render as off, whatever the mask says.
        var z = zones(0x27, values: [0, 0, 0, 1, 0, 0, 0, 0, 1, 0])
        z[3] = 0
        XCTAssertEqual(DualSenseTriggerEffect.parse(z), .off)
    }

    /// The host's PUNKTFUNK_TEST_FEEDBACK burst sends [0x21, 1, 2, 3, ...] — make sure the
    /// loopback test's expected shape stays a real parse result.
    func testScriptedLoopbackBlockParses() {
        let scripted: [UInt8] = [0x21, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
        if case .off = DualSenseTriggerEffect.parse(scripted) {
            XCTFail("the scripted test block should parse as a feedback effect")
        }
    }
}
