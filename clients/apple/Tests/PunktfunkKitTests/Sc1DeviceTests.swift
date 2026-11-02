// The 2015 Steam Controller parser, against `clients/shared/sc1-vectors.json`, which Android's
// `Sc1DeviceTest` and the host's typed fallback replay too.

import XCTest

@testable import PunktfunkKit

final class Sc1DeviceTests: XCTestCase {
    /// A 64-byte state frame: `word` at 8..11, the left trigger at 11, then 16..24.
    private func state(_ word: UInt32, lt: UInt8 = 0, _ axes: [Int: Int16] = [:]) -> [UInt8] {
        var r = [UInt8](repeating: 0, count: 64)
        r[0] = 1
        r[2] = Sc1Device.typeState
        r[8] = UInt8(word & 0xFF)
        r[9] = UInt8(word >> 8 & 0xFF)
        r[10] = UInt8(word >> 16 & 0xFF)
        r[11] = lt
        for (o, v) in axes {
            r[o] = UInt8(UInt16(bitPattern: v) & 0xFF)
            r[o + 1] = UInt8(UInt16(bitPattern: v) >> 8)
        }
        return r
    }

    func testButtonsMatchTheSharedVectors() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/sc1-vectors.json")
        let root = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let rows = try XCTUnwrap(root["buttons"] as? [[String: Any]])
        XCTAssertFalse(rows.isEmpty)
        for row in rows {
            let sc1 = UInt32(try XCTUnwrap(row["sc1"] as? Int))
            let wire = UInt32(try XCTUnwrap(row["wire"] as? Int))
            // A finger on the left pad keeps a pad click from reading as a stick click.
            let got = Sc2Device.wireButtons(Sc1Device.sc2Buttons(sc1 | 0x08_0000))
            XCTAssertEqual(got, wire, "\(row["name"] ?? "?")")
        }
    }

    func testLeftPadAndStickShareTheirBytes() {
        var s = Sc2Device.State()
        XCTAssertTrue(Sc1Device.parseState(state(0, [16: 1000, 18: -2000]), into: &s))
        XCTAssertEqual(s.lsX, 1000)
        XCTAssertEqual(s.lsY, -2000)
        // Pad touched while the stick rests: the bytes are the pad, the stick centres.
        _ = Sc1Device.parseState(state(0x08_0000, [16: 5000]), into: &s)
        XCTAssertEqual(s.lsX, 0)
        // Pad touched and the stick interleaved: the stick keeps its last reading.
        _ = Sc1Device.parseState(state(0, [16: 3000]), into: &s)
        _ = Sc1Device.parseState(state(0x88_0000, [16: 9000]), into: &s)
        XCTAssertEqual(s.lsX, 3000)
    }

    func testRightPadIsTheRightStickOnlyWhileTouched() {
        var s = Sc2Device.State()
        _ = Sc1Device.parseState(state(0x10_0000, [20: -700, 22: 800]), into: &s)
        XCTAssertEqual(s.rsX, -700)
        XCTAssertEqual(s.rsY, 800)
        _ = Sc1Device.parseState(state(0, [20: -700]), into: &s)
        XCTAssertEqual(s.rsX, 0)
    }

    func testFramesAreToldApartByType() {
        var s = Sc2Device.State()
        _ = Sc1Device.parseState(state(0, lt: 255), into: &s)
        XCTAssertEqual(s.lt, 255)
        XCTAssertFalse(Sc1Device.parseState([1, 0, 3, 1, 1], into: &s))
        XCTAssertTrue(Sc1Device.isWirelessDisconnect([1, 0, 3, 1, 1]))
        XCTAssertFalse(Sc1Device.isWirelessDisconnect([1, 0, 3, 1, 2]))
        XCTAssertFalse(Sc1Device.isFrame([0x42, 0, 1], type: Sc1Device.typeState))
        for f in Sc1Device.lizardOff + Sc1Device.lizardOn {
            XCTAssertEqual(f.count, 65)
            XCTAssertEqual(f[0], 0)
        }
    }

    /// The Bluetooth link's three core calls: features out as segments, input in as wired
    /// frames, replies rebuilt with report id 0 first.
    func testBluetoothSegmentsGoThroughCore() {
        let segments = PunktfunkConnection.sc1BleSegments(frame: Sc1Device.lizardOff[2])
        XCTAssertEqual(segments.count, 1)
        XCTAssertEqual(Array(segments[0][..<4]), [0x03, 0xC0, 0x87, 12])

        var state = PunktfunkConnection.sc1BleState()
        var segment = [UInt8](repeating: 0, count: 20)
        segment[..<5] = [0x03, 0xC0, 0x14, 0x00, 0x80] // a chunked state: A held
        let frame = PunktfunkConnection.sc1BleInput(state: &state, segment: segment)
        XCTAssertEqual(frame.map { Array($0[..<4]) }, [0x01, 0x00, 0x01, 0x3C])
        var parsed = Sc2Device.State()
        XCTAssertTrue(Sc1Device.parseState(frame ?? [], into: &parsed))
        XCTAssertEqual(parsed.buttons, Sc2Device.btnA)

        let reply = PunktfunkConnection.sc1BleReply(
            segments: [0x03, 0xC0, 0x83, 0x00] + [UInt8](repeating: 0, count: 16))
        XCTAssertEqual(reply.map { Array($0[..<3]) }, [0x00, 0x83, 0x00])
    }

    func testIdentityQueriesComeFromCore() {
        XCTAssertEqual(PunktfunkConnection.sc1IdentityRequests(), [
            [0x00, 0x83, 0x00], [0x00, 0xAE, 0x15, 0x00], [0x00, 0xAE, 0x15, 0x01],
        ])
    }
}
