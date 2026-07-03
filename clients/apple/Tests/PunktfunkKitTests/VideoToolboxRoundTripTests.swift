// Real-bitstream proof of the decode-prep path: VTCompressionSession encodes HEVC, we
// rebuild the host's wire shape (Annex-B AU with in-band VPS/SPS/PPS — exactly what
// punktfunk-host emits on every IDR), run it through AnnexB, and hand the result to a real
// VTDecompressionSession. Pixels out = the whole client decode path is sound.

import AVFoundation
import CoreMedia
import VideoToolbox
import XCTest
@testable import PunktfunkKit

/// Sendable holder for the values the (background-thread) decode callback writes.
private final class FrameBox: @unchecked Sendable {
    let lock = NSLock()
    var frame: ReadyFrame?
    var error: OSStatus?
}

final class VideoToolboxRoundTripTests: XCTestCase {
    private let width = 320
    private let height = 240

    func testEncodeAnnexBDecodeRoundTrip() throws {
        let (formatDesc, avccSample) = try encodeOneHEVCKeyframe()

        // Rebuild the host's wire format: Annex-B AU, parameter sets in-band before the VCL.
        let annexB = try annexBAU(formatDesc: formatDesc, avccSample: avccSample)

        // 1) Parameter-set extraction → format description.
        let rebuilt = try XCTUnwrap(
            AnnexB.formatDescription(fromIDR: annexB, codec: .hevc),
            "in-band VPS/SPS/PPS should yield a format description")
        let dims = CMVideoFormatDescriptionGetDimensions(rebuilt)
        XCTAssertEqual(Int(dims.width), width)
        XCTAssertEqual(Int(dims.height), height)

        // 2) Annex-B → AVCC re-pack must reproduce the encoder's own sample bytes.
        XCTAssertEqual(AnnexB.avcc(from: annexB, codec: .hevc), avccSample)

        // 3) Sample buffer → real decoder → pixels.
        let au = AccessUnit(
            data: annexB, ptsNs: 1_000_000, frameIndex: 0, flags: 0, receivedNs: 0)
        let sample = try XCTUnwrap(AnnexB.sampleBuffer(au: au, format: rebuilt, codec: .hevc))

        var session: VTDecompressionSession?
        XCTAssertEqual(
            VTDecompressionSessionCreate(
                allocator: nil, formatDescription: rebuilt, decoderSpecification: nil,
                imageBufferAttributes: nil, outputCallback: nil,
                decompressionSessionOut: &session),
            noErr)
        let decoder = try XCTUnwrap(session)
        defer { VTDecompressionSessionInvalidate(decoder) }

        var decoded: CVImageBuffer?
        var decodeStatus: OSStatus = -1
        // No async flag → the handler runs before DecodeFrame returns.
        VTDecompressionSessionDecodeFrame(
            decoder, sampleBuffer: sample, flags: [], infoFlagsOut: nil
        ) { status, _, imageBuffer, _, _ in
            decodeStatus = status
            decoded = imageBuffer
        }
        XCTAssertEqual(decodeStatus, noErr)
        let pixels = try XCTUnwrap(decoded) // CVImageBuffer and CVPixelBuffer are the same CF type
        XCTAssertEqual(CVPixelBufferGetWidth(pixels), width)
        XCTAssertEqual(CVPixelBufferGetHeight(pixels), height)
    }

    /// Stage-2 decode half: the same known IDR through `VideoDecoder` — assert its async output
    /// callback fires with a CVPixelBuffer of the right dimensions, the pts and the receipt stamp
    /// round-trip (the latter rides the frame refcon), and decode-completion is stamped.
    func testVideoDecoderAsyncCallbackDeliversPixels() throws {
        let (formatDesc, avccSample) = try encodeOneHEVCKeyframe()
        let annexB = try annexBAU(formatDesc: formatDesc, avccSample: avccSample)
        let format = try XCTUnwrap(AnnexB.formatDescription(fromIDR: annexB, codec: .hevc))
        let au = AccessUnit(
            data: annexB, ptsNs: 42_000_000, frameIndex: 0, flags: 0, receivedNs: 41_000_000)

        let box = FrameBox()
        let done = DispatchSemaphore(value: 0)
        let decoder = VideoDecoder(
            onDecoded: { frame in
                box.lock.lock(); box.frame = frame; box.lock.unlock()
                done.signal()
            },
            onDecodeError: { status in
                box.lock.lock(); box.error = status; box.lock.unlock()
                done.signal()
            })

        XCTAssertTrue(decoder.decode(au: au, format: format), "frame submit should succeed")
        XCTAssertEqual(done.wait(timeout: .now() + 10), .success, "the decode callback must fire")
        decoder.reset()

        box.lock.lock()
        let frame = box.frame
        let error = box.error
        box.lock.unlock()
        XCTAssertNil(error.map { "decode error \($0)" })
        let ready = try XCTUnwrap(frame, "the async output callback must deliver a ReadyFrame")
        XCTAssertEqual(CVPixelBufferGetWidth(ready.pixelBuffer), width)
        XCTAssertEqual(CVPixelBufferGetHeight(ready.pixelBuffer), height)
        XCTAssertEqual(ready.ptsNs, 42_000_000, "pts round-trips through the decoder")
        XCTAssertEqual(
            ready.receivedNs, 41_000_000, "receivedNs round-trips through the frame refcon")
        XCTAssertGreaterThan(ready.decodedNs, 0, "decode-completion is stamped")
    }

    // MARK: - encode helpers

    /// One forced-IDR HEVC frame; returns its format description and raw AVCC sample bytes.
    private func encodeOneHEVCKeyframe() throws -> (CMVideoFormatDescription, Data) {
        var session: VTCompressionSession?
        let rc = VTCompressionSessionCreate(
            allocator: nil, width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_HEVC, encoderSpecification: nil,
            imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &session)
        guard rc == noErr, let encoder = session else {
            throw XCTSkip("no HEVC encoder available (\(rc))")
        }
        defer { VTCompressionSessionInvalidate(encoder) }
        VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        VTSessionSetProperty(
            encoder, key: kVTCompressionPropertyKey_AllowFrameReordering, value: kCFBooleanFalse)

        let lock = NSLock()
        var output: CMSampleBuffer?
        let done = expectation(description: "encoded")
        VTCompressionSessionEncodeFrame(
            encoder, imageBuffer: try gradientPixelBuffer(),
            presentationTimeStamp: CMTime(value: 0, timescale: 30),
            duration: CMTime(value: 1, timescale: 30),
            frameProperties: [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue] as CFDictionary,
            infoFlagsOut: nil
        ) { status, _, sample in
            XCTAssertEqual(status, noErr)
            lock.lock()
            output = sample
            lock.unlock()
            done.fulfill()
        }
        VTCompressionSessionCompleteFrames(encoder, untilPresentationTimeStamp: .invalid)
        wait(for: [done], timeout: 10)

        lock.lock()
        defer { lock.unlock() }
        let sample = try XCTUnwrap(output)
        let desc = try XCTUnwrap(CMSampleBufferGetFormatDescription(sample))
        let block = try XCTUnwrap(CMSampleBufferGetDataBuffer(sample))
        var bytes = Data(count: CMBlockBufferGetDataLength(block))
        try bytes.withUnsafeMutableBytes { raw in
            let rc = CMBlockBufferCopyDataBytes(
                block, atOffset: 0, dataLength: raw.count,
                destination: raw.baseAddress!)
            if rc != noErr { throw NSError(domain: "CMBlockBuffer", code: Int(rc)) }
        }
        return (desc, bytes)
    }

    /// The host's wire shape: 4-byte start codes, VPS/SPS/PPS in-band, then the VCL NALs.
    private func annexBAU(formatDesc: CMVideoFormatDescription, avccSample: Data) throws -> Data {
        var au = Data()

        var psCount = 0
        var nalHeaderLen: Int32 = 0
        XCTAssertEqual(
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                formatDesc, parameterSetIndex: 0, parameterSetPointerOut: nil,
                parameterSetSizeOut: nil, parameterSetCountOut: &psCount,
                nalUnitHeaderLengthOut: &nalHeaderLen),
            noErr)
        XCTAssertEqual(nalHeaderLen, 4, "AnnexB.avcc assumes 4-byte NAL length prefixes")
        for i in 0..<psCount {
            var ptr: UnsafePointer<UInt8>?
            var size = 0
            XCTAssertEqual(
                CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    formatDesc, parameterSetIndex: i, parameterSetPointerOut: &ptr,
                    parameterSetSizeOut: &size, parameterSetCountOut: nil,
                    nalUnitHeaderLengthOut: nil),
                noErr)
            au.append(contentsOf: [0, 0, 0, 1])
            au.append(Data(bytes: try XCTUnwrap(ptr), count: size))
        }

        // AVCC sample (4-byte BE length per NAL) → start codes.
        var i = avccSample.startIndex
        while i + 4 <= avccSample.endIndex {
            let len = avccSample[i..<i + 4].reduce(0) { ($0 << 8) | Int($1) }
            let body = avccSample.index(i, offsetBy: 4)
            guard let end = avccSample.index(body, offsetBy: len, limitedBy: avccSample.endIndex)
            else { break }
            au.append(contentsOf: [0, 0, 0, 1])
            au.append(avccSample[body..<end])
            i = end
        }
        return au
    }

    private func gradientPixelBuffer() throws -> CVPixelBuffer {
        var pb: CVPixelBuffer?
        let attrs = [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary
        XCTAssertEqual(
            CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA, attrs, &pb),
            kCVReturnSuccess)
        let buf = try XCTUnwrap(pb)
        CVPixelBufferLockBaseAddress(buf, [])
        defer { CVPixelBufferUnlockBaseAddress(buf, []) }
        let base = try XCTUnwrap(CVPixelBufferGetBaseAddress(buf))
        let stride = CVPixelBufferGetBytesPerRow(buf)
        for y in 0..<height {
            let row = base.advanced(by: y * stride).assumingMemoryBound(to: UInt8.self)
            for x in 0..<width {
                row[x * 4 + 0] = UInt8(x & 0xFF) // B
                row[x * 4 + 1] = UInt8(y & 0xFF) // G
                row[x * 4 + 2] = UInt8((x ^ y) & 0xFF) // R
                row[x * 4 + 3] = 0xFF
            }
        }
        return buf
    }
}
