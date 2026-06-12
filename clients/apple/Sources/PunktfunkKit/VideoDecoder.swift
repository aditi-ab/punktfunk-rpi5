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
    /// The decoded image (NV12 biplanar, Metal-compatible).
    public let pixelBuffer: CVPixelBuffer
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

    public init(
        onDecoded: @escaping @Sendable (ReadyFrame) -> Void,
        onDecodeError: @escaping @Sendable (OSStatus) -> Void = { _ in }
    ) {
        self.onDecoded = onDecoded
        self.onDecodeError = onDecodeError
    }

    deinit { teardown() }

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

    /// `lock` held. Replace the session with one for `newFormat`. NV12 video-range, Metal-
    /// compatible output (10-bit/HDR is a later tie-in — see the plan).
    private func createSessionLocked(format newFormat: CMVideoFormatDescription) -> Bool {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        session = nil
        format = nil

        let imageAttrs: [CFString: Any] = [
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferPixelFormatTypeKey:
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        ]
        var callback = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: decoderOutputCallback,
            decompressionOutputRefCon: Unmanaged.passUnretained(self).toOpaque())
        var newSession: VTDecompressionSession?
        let status = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: newFormat,
            decoderSpecification: nil, // hardware by default
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
        onDecoded(ReadyFrame(ptsNs: ptsNs, decodedNs: decodedNs, pixelBuffer: imageBuffer))
    }
}
