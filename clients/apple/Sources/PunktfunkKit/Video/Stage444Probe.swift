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

import CoreVideo
import Foundation

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

    /// Decode the synthetic 4:4:4 keyframe on a hardware-REQUIRED session: true only when it
    /// produces the expected (video- or full-range) biplanar 4:4:4 format. Any failure keeps
    /// 4:2:0. A software decode would pass and then run every real frame on the CPU.
    private static func probeHardware444(
        au: [UInt8], want: OSType, fullRangeSibling: OSType
    ) -> Bool {
        guard let buffer = VTOneShot.decode(
            annexB: Data(au), codec: .hevc, pixelFormat: want, requireHardware: true)
        else { return false }
        let produced = CVPixelBufferGetPixelFormatType(buffer)
        return produced == want || produced == fullRangeSibling
    }
}
