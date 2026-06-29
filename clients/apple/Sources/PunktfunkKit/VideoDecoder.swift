// Stage-2 presenter, decode half: explicit VideoToolbox decode of the host's HEVC AUs.
//
// Stage-1 hands compressed samples to AVSampleBufferDisplayLayer, which decodes AND presents
// internally with no per-frame callback — so neither decode-completion nor present can be
// stamped, and frames can't be hand-paced. Here we drive VTDecompressionSession ourselves: the
// output callback delivers a decoded CVPixelBuffer, we stamp decode-completion, and push it into
// a ready ring the presenter's display link drains. See docs apple-stage2-presenter.md.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// One decoded frame waiting to be presented. Owns a retained `CVPixelBuffer` until shown.
public struct ReadyFrame: @unchecked Sendable {
    /// Host capture clock (the AU's pts), in nanoseconds.
    public let ptsNs: UInt64
    /// Client `CLOCK_REALTIME` instant decode completed, in nanoseconds.
    public let decodedNs: Int64
    /// The decoded image — 8-bit NV12 biplanar (SDR) or 10-bit P010 biplanar (HDR), Metal-compatible.
    public let pixelBuffer: CVPixelBuffer
    /// True when the stream is HDR (BT.2020 PQ): the buffer is 10-bit P010 and the presenter must
    /// configure EDR + BT.2020 PQ output. Derived from the decoded buffer's pixel format.
    public let isHDR: Bool
}

/// The C output callback can't capture context, so VideoToolbox hands it the refcon we set at
/// session creation — a pointer back to the owning `VideoDecoder`.
private let decoderOutputCallback: VTDecompressionOutputCallback = {
    refcon, _, status, _, imageBuffer, pts, _ in
    guard let refcon else { return }
    Unmanaged<VideoDecoder>.fromOpaque(refcon)
        .takeUnretainedValue()
        .handleDecoded(status: status, imageBuffer: imageBuffer, pts: pts)
}

/// Owns a `VTDecompressionSession` rebuilt whenever the format description changes (every IDR /
/// mode change, the same trigger stage-1 uses). Thread-safe: `decode` runs on the pump thread,
/// the output callback on a VT-managed thread; the only shared mutable state is the session +
/// format, guarded by `lock`. `@unchecked Sendable` — the lock enforces the contract.
public final class VideoDecoder: @unchecked Sendable {
    private let lock = NSLock()
    private var session: VTDecompressionSession?
    private var format: CMVideoFormatDescription?

    /// Called on the VT thread for each successfully decoded frame — stamp + enqueue, don't block.
    private let onDecoded: @Sendable (ReadyFrame) -> Void
    /// Called on the VT thread when a frame fails to decode (bad data / decoder reset) so the
    /// pump can re-gate on the next IDR.
    private let onDecodeError: @Sendable (OSStatus) -> Void

    /// Whether the negotiated stream is full-chroma 4:4:4 (`connection.isChroma444`), set once at
    /// session start before any decode. Selects the 4:4:4 decode pixel format (orthogonal to bit
    /// depth / HDR). Read inside `createSessionLocked` under `lock`.
    private var chroma444 = false

    public init(
        onDecoded: @escaping @Sendable (ReadyFrame) -> Void,
        onDecodeError: @escaping @Sendable (OSStatus) -> Void = { _ in }
    ) {
        self.onDecoded = onDecoded
        self.onDecodeError = onDecodeError
    }

    deinit { teardown() }

    /// Select the chroma subsampling of the decode output (4:2:0 vs full-chroma 4:4:4). Call once at
    /// session start, before decoding, from `connection.isChroma444`. Takes effect on the next
    /// session (re)build. Thread-safe.
    public func setChroma444(_ on: Bool) {
        lock.lock()
        chroma444 = on
        lock.unlock()
    }

    /// Submit one AU for asynchronous decode, (re)creating the session if `format` changed. The
    /// caller resolves `format` from the IDR exactly as stage-1 does (`AnnexB.formatDescription`).
    /// Returns false if the session couldn't be created or the frame couldn't be submitted.
    @discardableResult
    public func decode(au: AccessUnit, format newFormat: CMVideoFormatDescription) -> Bool {
        lock.lock()
        let needsNew: Bool = {
            guard let session, let format else { return true }
            if CMFormatDescriptionEqual(format, otherFormatDescription: newFormat) { return false }
            // A new desc that the live session can still accept (rare for HEVC) avoids a rebuild.
            return !VTDecompressionSessionCanAcceptFormatDescription(session, formatDescription: newFormat)
        }()
        if needsNew, !createSessionLocked(format: newFormat) {
            lock.unlock()
            return false
        }
        // Submit WHILE holding the lock so a concurrent reset()/teardown (main thread) can't
        // invalidate the session between here and DecodeFrame. The VT output callback takes the
        // ring lock, not this one, so there's no re-entrancy. DecodeFrame is async — non-blocking.
        guard let session,
              let sample = AnnexB.sampleBuffer(au: au, format: newFormat)
        else { lock.unlock(); return false }
        var infoOut = VTDecodeInfoFlags()
        let status = VTDecompressionSessionDecodeFrame(
            session,
            sampleBuffer: sample,
            flags: [._EnableAsynchronousDecompression],
            frameRefcon: nil,
            infoFlagsOut: &infoOut)
        lock.unlock()
        if status != noErr {
            onDecodeError(status)
            return false
        }
        return true
    }

    /// Drop the session — the next `decode` rebuilds it. Used on stop and to recover from a
    /// wedged decoder (re-gates on the next in-band parameter sets, like stage-1's flush).
    public func reset() {
        lock.lock()
        teardownLocked()
        lock.unlock()
    }

    private func teardown() {
        lock.lock()
        teardownLocked()
        lock.unlock()
    }

    private func teardownLocked() {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil
    }

    /// True when `newFormat` carries a PQ (SMPTE ST 2084) or HLG transfer function — i.e. the host
    /// is sending HDR (BT.2020). VideoToolbox populates the transfer-function extension from the
    /// HEVC VUI, so this picks the decode bit depth (10-bit P010/x444 vs 8-bit NV12/444v) from the
    /// stream. The present-side HDR config (colorspace/EDR/shader) is latched once per session from
    /// the Welcome (`connection.isHDR`), which the host does NOT flip mid-session — so this predicate
    /// and that config agree for the session (a `#if DEBUG` assert in the presenter guards it).
    static func isHDRFormat(_ format: CMVideoFormatDescription) -> Bool {
        guard
            let tf = CMFormatDescriptionGetExtension(
                format, extensionKey: kCMFormatDescriptionExtension_TransferFunction)
        else { return false }
        let s = tf as? String
        return s == (kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ as String)
            || s == (kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG as String)
    }

    /// `lock` held. Replace the session with one for `newFormat`. SDR streams decode to 8-bit NV12;
    /// HDR streams (BT.2020 PQ) decode to 10-bit P010 so the presenter can drive EDR.
    private func createSessionLocked(format newFormat: CMVideoFormatDescription) -> Bool {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil

        // Decode pixel format is a 2×2 of (chroma, depth/HDR), both biplanar so the presenter binds
        // plane 0 = luma, plane 1 = interleaved chroma uniformly — 4:4:4 just delivers a full-size
        // chroma plane. 10-bit (P010 / `x444`) for HDR (PQ/HLG), 8-bit (NV12 / `444v`) otherwise.
        let hdr = Self.isHDRFormat(newFormat)
        let pixelFormat: OSType = {
            switch (chroma444, hdr) {
            case (false, false): return kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange // NV12
            case (false, true): return kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange // P010
            case (true, false): return kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange // 444v
            case (true, true): return kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange // x444
            }
        }()
        let imageAttrs: [CFString: Any] = [
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferPixelFormatTypeKey: pixelFormat,
        ]
        var callback = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: decoderOutputCallback,
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque())
        // 4:4:4 sessions REQUIRE a hardware decoder: we only advertise 4:4:4 when the hardware probe
        // passed, so a hardware-incapable mode (e.g. a resolution past the HW 4:4:4 ceiling) must fail
        // HERE, synchronously, letting the pump's backstop end the session — rather than silently
        // falling back to a software 4:4:4 decoder far too slow for a real-time stream. 4:2:0 keeps the
        // software fallback (nil spec) as a robustness net.
        let spec: CFDictionary? =
            chroma444
            ? [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true] as CFDictionary
            : nil
        var newSession: VTDecompressionSession?
        let status = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: newFormat,
            decoderSpecification: spec,
            imageBufferAttributes: imageAttrs as CFDictionary,
            outputCallback: &callback,
            decompressionSessionOut: &newSession)
        guard status == noErr, let newSession else { return false }
        session = newSession
        format = newFormat
        return true
    }

    /// VT thread. Stamp decode-completion and enqueue, or report the error.
    fileprivate func handleDecoded(status: OSStatus, imageBuffer: CVImageBuffer?, pts: CMTime) {
        guard status == noErr, let imageBuffer else {
            onDecodeError(status)
            return
        }
        var ts = timespec()
        clock_gettime(CLOCK_REALTIME, &ts)
        let decodedNs = Int64(ts.tv_sec) * 1_000_000_000 + Int64(ts.tv_nsec)
        // pts was stamped at timescale 1e9 (AnnexB.sampleBuffer); normalize defensively.
        let p = CMTimeConvertScale(pts, timescale: 1_000_000_000, method: .default)
        let ptsNs = p.value > 0 ? UInt64(p.value) : 0
        // HDR iff the decoder produced a 10-bit buffer (we only request a 10-bit format for PQ/HLG
        // streams). Covers 4:2:0 (P010) and 4:4:4 (`x444`), video- and full-range, so a 10-bit 4:4:4
        // HDR frame isn't misclassified as SDR. (The mastering metadata is applied to the presenter's
        // CAMetalLayer via CAEDRMetadata, not to this source buffer — a separate-drawable presenter
        // never composites the source buffer's attachments, so attaching them here would be dead.)
        let fmt = CVPixelBufferGetPixelFormatType(imageBuffer)
        let isHDR =
            fmt == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            || fmt == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
            || fmt == kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange
            || fmt == kCVPixelFormatType_444YpCbCr10BiPlanarFullRange
        onDecoded(
            ReadyFrame(ptsNs: ptsNs, decodedNs: decodedNs, pixelBuffer: imageBuffer, isHDR: isHDR))
    }
}
