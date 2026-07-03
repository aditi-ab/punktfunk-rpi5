// 4:4:4 decode-path coverage: the hardware-capability probe is stable/cached, and a real 4:4:4 HEVC
// keyframe decodes through VideoDecoder to a biplanar 4:4:4 pixel buffer. Reuses the same synthetic
// 4:4:4 blobs the runtime probe ships with.

import CoreVideo
import VideoToolbox
import XCTest
@testable import PunktfunkKit

private final class FrameBox: @unchecked Sendable {
    let lock = NSLock()
    var frame: ReadyFrame?
    var error: OSStatus?
}

final class Stage444Tests: XCTestCase {
    /// The capability probe is device-static and cached — reading it twice must return the same value
    /// (and must never crash, including where 4:4:4 is unsupported → false).
    func testProbeIsStableAndCached() {
        XCTAssertEqual(Stage444Probe.hwDecode444_8bit, Stage444Probe.hwDecode444_8bit)
        XCTAssertEqual(Stage444Probe.hwDecode444_10bit, Stage444Probe.hwDecode444_10bit)
    }

    /// A real 8-bit 4:4:4 HEVC keyframe (the embedded probe blob) decodes through `VideoDecoder` with
    /// `setChroma444(true)` to a 256×256 biplanar 4:4:4 (`444v`/`444f`) buffer classified SDR.
    /// (4:4:4 sessions require a hardware decoder — skip where there isn't one, which is exactly where
    /// the client wouldn't advertise 4:4:4 anyway.)
    func testVideoDecoderDecodes444() throws {
        try XCTSkipUnless(
            Stage444Probe.hwDecode444_8bit, "no hardware 4:4:4 decode on this device")
        let data = Data(Probe444Blobs.au444_8bit)
        let format = try XCTUnwrap(
            AnnexB.formatDescription(fromIDR: data, codec: .hevc), "the 4:4:4 blob must yield a format description")
        let au = AccessUnit(data: data, ptsNs: 7_000_000, frameIndex: 0, flags: 0, receivedNs: 0)

        let box = FrameBox()
        let done = DispatchSemaphore(value: 0)
        let decoder = VideoDecoder(
            onDecoded: { f in box.lock.lock(); box.frame = f; box.lock.unlock(); done.signal() },
            onDecodeError: { s in box.lock.lock(); box.error = s; box.lock.unlock(); done.signal() })
        decoder.setChroma444(true)

        XCTAssertTrue(decoder.decode(au: au, format: format), "4:4:4 frame submit should succeed")
        XCTAssertEqual(done.wait(timeout: .now() + 10), .success, "the decode callback must fire")
        decoder.reset()

        box.lock.lock(); let frame = box.frame; let error = box.error; box.lock.unlock()
        XCTAssertNil(error.map { "decode error \($0)" })
        let ready = try XCTUnwrap(frame, "a 4:4:4 ReadyFrame must be delivered")
        XCTAssertEqual(CVPixelBufferGetWidth(ready.pixelBuffer), 256)
        XCTAssertEqual(CVPixelBufferGetHeight(ready.pixelBuffer), 256)
        let pf = CVPixelBufferGetPixelFormatType(ready.pixelBuffer)
        XCTAssertTrue(
            pf == kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange
                || pf == kCVPixelFormatType_444YpCbCr8BiPlanarFullRange,
            "expected a biplanar 4:4:4 8-bit buffer, got \(fourCCString(pf))")
        XCTAssertFalse(ready.isHDR, "an 8-bit BT.709 4:4:4 stream is SDR")
        // The chroma plane (plane 1) must be FULL resolution for 4:4:4 (vs half for 4:2:0) — this is
        // what lets the unchanged shader sample chroma at the luma UV.
        XCTAssertEqual(CVPixelBufferGetWidthOfPlane(ready.pixelBuffer, 1), 256)
        XCTAssertEqual(CVPixelBufferGetHeightOfPlane(ready.pixelBuffer, 1), 256)
    }

    private func fourCCString(_ t: OSType) -> String {
        let b = [UInt8(t >> 24 & 0xff), UInt8(t >> 16 & 0xff), UInt8(t >> 8 & 0xff), UInt8(t & 0xff)]
        return String(bytes: b, encoding: .ascii) ?? "\(t)"
    }
}
