// Runtime 4:4:4 HEVC decode-capability probe.
//
// We advertise `VIDEO_CAP_444` (so the host upgrades to a full-chroma 4:4:4 stream) ONLY when this
// device can decode 4:4:4 HEVC *in hardware* — software 4:4:4 decode works but is far too slow for a
// real-time stream at the negotiated resolution, so a software-only device must keep 4:2:0.
//
// `VTIsHardwareDecodeSupported(HEVC)` and the HEVC-decoder-capabilities dictionary report HEVC HW
// decode but expose nothing about `chroma_format_idc`, so the only reliable signal is to actually
// create a *hardware-required* `VTDecompressionSession` for a tiny synthetic 4:4:4 keyframe and
// confirm it both creates and decodes to the expected biplanar 4:4:4 pixel format. Validated on an
// Apple M3 (HW 4:4:4 8- and 10-bit decode to `444v`/`x444`); a software-only decoder fails the
// hardware-required create and we fall back to 4:2:0.
//
// The probe blobs are 256×256 (above the hardware decoder's minimum-dimension floor — a 16×16 clip
// is rejected for ALL chroma formats, including 4:2:0) HEVC Range-Extensions keyframes generated
// offline with libx265; see scripts notes. Results are cached (device-static) in lazy statics.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

public enum Stage444Probe {
    /// True iff this device hardware-decodes 8-bit 4:4:4 HEVC (the host's current 4:4:4 path —
    /// BT.709 limited `yuv444p`). Cached after first evaluation.
    public static let hwDecode444_8bit: Bool = probeHardware444(
        au: Probe444Blobs.au444_8bit,
        want: kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
        fullRangeSibling: kCVPixelFormatType_444YpCbCr8BiPlanarFullRange)

    /// True iff this device hardware-decodes 10-bit 4:4:4 HEVC (the 4:4:4 ∩ HDR/10-bit intersection).
    /// Cached after first evaluation.
    public static let hwDecode444_10bit: Bool = probeHardware444(
        au: Probe444Blobs.au444_10bit,
        want: kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange,
        fullRangeSibling: kCVPixelFormatType_444YpCbCr10BiPlanarFullRange)

    /// Create a hardware-REQUIRED `VTDecompressionSession` for the synthetic 4:4:4 keyframe and
    /// decode it, returning true only when the decoder produces the expected (video- or full-range)
    /// biplanar 4:4:4 pixel format. Any failure (no hardware path, wrong output format, decode error)
    /// → false → we keep 4:2:0.
    private static func probeHardware444(
        au auBytes: [UInt8], want: OSType, fullRangeSibling: OSType
    ) -> Bool {
        let data = Data(auBytes)
        guard let format = AnnexB.formatDescription(fromIDR: data) else { return false }
        // Require a hardware decoder — a software false-positive would make us advertise 4:4:4 and
        // then decode every real frame on the CPU, blowing the latency budget.
        let spec: [CFString: Any] = [
            kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true,
        ]
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: want,
            kCVPixelBufferMetalCompatibilityKey: true,
        ]
        var session: VTDecompressionSession?
        let created = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault, formatDescription: format,
            decoderSpecification: spec as CFDictionary, imageBufferAttributes: attrs as CFDictionary,
            outputCallback: nil, decompressionSessionOut: &session)
        guard created == noErr, let session else { return false }
        defer { VTDecompressionSessionInvalidate(session) }

        let au = AccessUnit(data: data, ptsNs: 0, frameIndex: 0, flags: 0)
        guard let sample = AnnexB.sampleBuffer(au: au, format: format) else { return false }

        var produced: OSType = 0
        let done = DispatchSemaphore(value: 0)
        let status = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression], infoFlagsOut: nil
        ) { status, _, imageBuffer, _, _ in
            if status == noErr, let imageBuffer {
                produced = CVPixelBufferGetPixelFormatType(imageBuffer)
            }
            done.signal()
        }
        guard status == noErr else { return false }
        VTDecompressionSessionWaitForAsynchronousFrames(session)
        _ = done.wait(timeout: .now() + 1.0)
        return produced == want || produced == fullRangeSibling
    }
}
