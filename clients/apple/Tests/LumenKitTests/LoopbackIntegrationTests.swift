// Integration: the Swift wrapper against a real lumen/1 host over QUIC + UDP on loopback —
// the Swift twin of lumen-host's m3.rs::c_abi_connection_roundtrip, this time through the
// statically linked xcframework. Driven by clients/apple/test-loopback.sh, which builds and
// starts `lumen-host m3-host --source synthetic` and sets LUMEN_LOOPBACK_PORT.

import XCTest
@testable import LumenKit

final class LoopbackIntegrationTests: XCTestCase {
    func testSyntheticStreamRoundTrip() throws {
        guard let portStr = ProcessInfo.processInfo.environment["LUMEN_LOOPBACK_PORT"],
              let port = UInt16(portStr)
        else {
            throw XCTSkip("needs a running m3-host — use clients/apple/test-loopback.sh")
        }

        let conn = try LumenConnection(
            host: "127.0.0.1", port: port, width: 1280, height: 720, refreshHz: 60)
        XCTAssertEqual(conn.width, 1280)
        XCTAssertEqual(conn.height, 720)
        XCTAssertEqual(conn.refreshHz, 60)

        // Pull 25 synthetic frames and byte-verify the documented pattern:
        // u32 LE frame index, then data[i] = (idx as u8) &+ (i as u8).
        var got = 0
        var lastIndex: UInt32 = 0
        let deadline = Date().addingTimeInterval(30)
        while got < 25 {
            XCTAssertLessThan(Date(), deadline, "timed out after \(got) frames")
            guard let au = try conn.nextAU(timeoutMs: 2000) else { continue }
            let idx = au.data.prefix(4).reversed().reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
            for (i, byte) in au.data.enumerated().dropFirst(4) {
                let expected = UInt8(truncatingIfNeeded: idx) &+ UInt8(truncatingIfNeeded: i)
                if byte != expected {
                    XCTFail("frame \(idx) corrupt at offset \(i)")
                    break
                }
            }
            XCTAssertGreaterThan(au.ptsNs, 0)
            lastIndex = idx
            got += 1
        }
        XCTAssertGreaterThanOrEqual(lastIndex, 24)

        // Input goes the other way (enqueue-only; the host logs the count on close).
        conn.send(.mouseMove(dx: 1, dy: 2))
        conn.send(.key(0x41, down: true))
        conn.send(.key(0x41, down: false))

        conn.close()
        XCTAssertThrowsError(try conn.nextAU(timeoutMs: 10)) { error in
            guard case LumenClientError.closed = error else {
                return XCTFail("expected .closed, got \(error)")
            }
        }
    }

    func testConnectFailureThrows() {
        // Nothing listens on this port; connect must fail within its timeout, not hang.
        XCTAssertThrowsError(
            try LumenConnection(
                host: "127.0.0.1", port: 9, width: 640, height: 480, refreshHz: 30,
                timeoutMs: 2000))
    }
}
