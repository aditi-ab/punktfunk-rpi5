// First light, headless: the full client pipeline against a REAL remote host — QUIC
// handshake over the LAN, NVENC HEVC AUs through FEC + AES-GCM, AnnexB conversion, and a
// real VTDecompressionSession turning them into pixels. Everything the GUI does except
// putting the layer on glass.
//
// Run (host side, on the Linux box):
//   PUNKTFUNK_COMPOSITOR=gamescope PUNKTFUNK_GAMESCOPE_APP=vkcube PUNKTFUNK_ZEROCOPY=1 \
//   punktfunk-host m3-host --source virtual --seconds 120
// Then here:
//   PUNKTFUNK_REMOTE_HOST=192.168.1.70 swift test --filter RemoteFirstLightTests

import CoreMedia
import VideoToolbox
import XCTest
@testable import PunktfunkKit

final class RemoteFirstLightTests: XCTestCase {
    /// The pairing ceremony over the real LAN, exactly as the app runs it: fresh identity,
    /// SPAKE2 with the host's arming PIN, then a pinned + identified session. Needs the
    /// host armed (--allow-pairing) and its logged PIN in PUNKTFUNK_REMOTE_PIN. Heads-up:
    /// every run durably adds one throwaway "remote-test" identity to the host's
    /// ~/.config/punktfunk/punktfunk1-paired.json — prune those entries at will.
    func testRemotePairingThenPinnedStream() throws {
        let env = ProcessInfo.processInfo.environment
        guard let host = env["PUNKTFUNK_REMOTE_HOST"], let pin = env["PUNKTFUNK_REMOTE_PIN"]
        else {
            throw XCTSkip("set PUNKTFUNK_REMOTE_HOST + PUNKTFUNK_REMOTE_PIN "
                + "(host armed with --allow-pairing)")
        }
        let port = env["PUNKTFUNK_REMOTE_PORT"].flatMap(UInt16.init) ?? 9777

        let identity = try generateIdentity()
        let fingerprint = try pair(
            host: host, port: port, identity: identity, pin: pin, name: "remote-test")
        XCTAssertEqual(fingerprint.count, 32)

        let conn = try PunktfunkConnection(
            host: host, port: port, width: 1280, height: 720, refreshHz: 60,
            pinSHA256: fingerprint, identity: identity)
        defer { conn.close() }
        XCTAssertEqual(conn.hostFingerprint, fingerprint)
        var got = 0
        let deadline = Date().addingTimeInterval(20)
        while got < 10, Date() < deadline {
            if try conn.nextAU(timeoutMs: 2000) != nil { got += 1 }
        }
        XCTAssertGreaterThanOrEqual(got, 10, "paired + pinned session must stream")
    }

    func testRemoteStreamDecodesToPixels() throws {
        let env = ProcessInfo.processInfo.environment
        guard let host = env["PUNKTFUNK_REMOTE_HOST"] else {
            throw XCTSkip("set PUNKTFUNK_REMOTE_HOST (and start m3-host --source virtual there)")
        }
        let port = env["PUNKTFUNK_REMOTE_PORT"].flatMap(UInt16.init) ?? 9777
        let width: UInt32 = 1280
        let height: UInt32 = 720

        let conn = try PunktfunkConnection(
            host: host, port: port, width: width, height: height, refreshHz: 60)
        defer { conn.close() }
        XCTAssertEqual(conn.width, width)
        XCTAssertEqual(conn.height, height)

        var format: CMVideoFormatDescription?
        var decoder: VTDecompressionSession?
        defer { decoder.map { VTDecompressionSessionInvalidate($0) } }
        var received = 0
        var decoded = 0
        var firstPtsNs: UInt64 = 0
        var lastPtsNs: UInt64 = 0
        let deadline = Date().addingTimeInterval(30)

        while decoded < 60, Date() < deadline {
            guard let au = try conn.nextAU(timeoutMs: 2000) else { continue }
            received += 1
            if firstPtsNs == 0 { firstPtsNs = au.ptsNs }
            lastPtsNs = au.ptsNs

            if let f = AnnexB.formatDescription(fromIDR: au.data) {
                format = f
                if decoder == nil {
                    let dims = CMVideoFormatDescriptionGetDimensions(f)
                    XCTAssertEqual(UInt32(dims.width), width)
                    XCTAssertEqual(UInt32(dims.height), height)
                    var session: VTDecompressionSession?
                    XCTAssertEqual(
                        VTDecompressionSessionCreate(
                            allocator: nil, formatDescription: f, decoderSpecification: nil,
                            imageBufferAttributes: nil, outputCallback: nil,
                            decompressionSessionOut: &session),
                        noErr)
                    decoder = session
                }
            }
            guard let f = format, let dec = decoder,
                  let sample = AnnexB.sampleBuffer(au: au, format: f)
            else { continue }

            var gotPixels = false
            VTDecompressionSessionDecodeFrame(
                dec, sampleBuffer: sample, flags: [], infoFlagsOut: nil
            ) { status, _, imageBuffer, _, _ in
                gotPixels = status == noErr && imageBuffer != nil
            }
            if gotPixels { decoded += 1 }
        }

        XCTAssertGreaterThanOrEqual(decoded, 60, "decoded \(decoded)/\(received) received AUs")
        // The host stamps pts with its capture wall clock — 60 frames should span ~1 s.
        let spanMs = Double(lastPtsNs &- firstPtsNs) / 1_000_000
        print("first light: \(decoded) frames decoded, \(received) received, pts span \(Int(spanMs)) ms")
    }
}
