// Locks the DualSense raw-HID rumble report layout to the SDL / Linux hid-playstation spec.
// The motors can only be confirmed on a physical pad, but these guard against a silent byte
// error in the offsets, enable flags, lengths, and the Bluetooth CRC32 — the parts most likely
// to regress unnoticed. macOS-only (DualSenseHID isn't compiled elsewhere).

#if os(macOS)
import XCTest

@testable import PunktfunkKit

final class DualSenseHIDTests: XCTestCase {
    func testUSBReportLayout() {
        let r = DualSenseHID.usbReport(low: 0xAA, high: 0xBB)
        XCTAssertEqual(r.count, 48)
        XCTAssertEqual(r[0], 0x02) // report id
        XCTAssertEqual(r[1], 0x03) // flag0: COMPATIBLE_VIBRATION | HAPTICS_SELECT
        XCTAssertEqual(r[2], 0x00) // flag1 (untouched — leaves lightbar/LEDs alone)
        XCTAssertEqual(r[3], 0xBB) // motor_right = high
        XCTAssertEqual(r[4], 0xAA) // motor_left  = low
        XCTAssertEqual(r[39], 0x04) // flag2: COMPATIBLE_VIBRATION2 (payload offset 38 + report id)
    }

    func testBluetoothReportLayoutAndCRC() {
        let r = DualSenseHID.bluetoothReport(low: 0xAA, high: 0xBB)
        XCTAssertEqual(r.count, 78)
        XCTAssertEqual(r[0], 0x31) // report id
        XCTAssertEqual(r[1], 0x00) // seq/tag
        XCTAssertEqual(r[2], 0x10) // magic
        XCTAssertEqual(r[3], 0x03) // flag0
        XCTAssertEqual(r[5], 0xBB) // motor_right = high (payload offset 2 + 3-byte BT header)
        XCTAssertEqual(r[6], 0xAA) // motor_left  = low
        XCTAssertEqual(r[41], 0x04) // flag2 (payload offset 38 + 3)

        // Trailing CRC32 = standard CRC32 over (0xA2 seed + report[0..<74]), little-endian.
        let expected = DualSenseHID.crc32(seed: 0xA2, r[0..<74])
        let stored = UInt32(r[74]) | (UInt32(r[75]) << 8) | (UInt32(r[76]) << 16) | (UInt32(r[77]) << 24)
        XCTAssertEqual(stored, expected)
    }

    func testCRC32MatchesStandardCheckVector() {
        // The canonical CRC32 check value: CRC32("123456789") == 0xCBF43926. Our helper folds a
        // seed byte in first, so feed seed='1' and the rest — proving poly/reflection/init/final.
        let crc = DualSenseHID.crc32(seed: UInt8(ascii: "1"), Array("23456789".utf8))
        XCTAssertEqual(crc, 0xCBF4_3926)
    }
}
#endif
