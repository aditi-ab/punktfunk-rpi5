// Annex-B (HEVC / H.264) → CoreMedia plumbing.
//
// The punktfunk host emits Annex-B access units with in-band parameter sets on every IDR
// (deliberately — the client needs no out-of-band extradata). VideoToolbox wants the AVCC
// flavor instead: a CMVideoFormatDescription built from the parameter sets, and sample
// buffers whose NALs are 4-byte-length-prefixed. This file converts between the two, for
// the codec the host resolved in the Welcome (`connection.videoCodec`) — HEVC and H.264
// differ only in NAL-header layout and which parameter sets exist (HEVC adds a VPS). AV1
// is not an Annex-B/NAL codec and isn't handled here — its OBU flavor of the same plumbing
// lives in AV1.swift, and the pumps reach both through `VideoCodec`'s dispatching
// `formatDescription(fromKeyframe:)` / `sampleBuffer(au:format:)`, so nothing below is ever
// called with `.av1`.
//
// HOT PATH: both pumps run `formatDescription(fromIDR:codec:)` + `sampleBuffer(au:format:codec:)`
// once per AU, so the conversion is built on `forEachNAL` — a zero-copy scan over the AU's bytes
// (ranges, not materialized Datas) — and `sampleBuffer` packs the AVCC form straight into
// the CMBlockBuffer's own allocation. Per AU that leaves exactly one copy here (source →
// block buffer) instead of the naive scan-copy-slice-repack chain.

import CoreMedia
import Foundation

/// The video codec of the host's elementary stream — negotiated in the Welcome and read via
/// `punktfunk_connection_codec`.
public enum VideoCodec: Equatable {
    case h264
    case hevc
    case av1

    /// Resolve from the wire `Welcome.codec` byte (`PUNKTFUNK_CODEC_*`; unknown → HEVC).
    public init(wire: UInt8) {
        switch wire {
        case 0x01: self = .h264 // PUNKTFUNK_CODEC_H264
        case 0x04: self = .av1 // PUNKTFUNK_CODEC_AV1
        default: self = .hevc // PUNKTFUNK_CODEC_HEVC — the default / older-host codec
        }
    }

    /// NAL unit type from a NAL's first byte. HEVC: bits 1..6; H.264: bits 0..4.
    fileprivate func nalType(_ first: UInt8) -> UInt8 {
        self == .hevc ? (first >> 1) & 0x3F : first & 0x1F
    }

    /// True for a parameter-set NAL (dropped from AVCC; kept for the format description).
    /// HEVC: VPS 32 / SPS 33 / PPS 34. H.264: SPS 7 / PPS 8 (no VPS).
    fileprivate func isParameterSet(_ first: UInt8) -> Bool {
        let t = nalType(first)
        return self == .hevc ? (32...34).contains(t) : t == 7 || t == 8
    }

    /// True for a VCL (slice) NAL — in a conforming AU no parameter set follows the first one,
    /// so the format-description scan can stop there.
    fileprivate func isVCL(_ first: UInt8) -> Bool {
        let t = nalType(first)
        return self == .hevc ? t <= 31 : (1...5).contains(t)
    }
}

public enum AnnexB {
    /// Walk the NAL units of `data` without copying: `body` receives the buffer base and each
    /// NAL's byte range (start codes 00 00 01 / 00 00 00 01 excluded), and returns false to
    /// stop the walk early (e.g. at the first VCL NAL). All zeros immediately preceding a
    /// start code are dropped: they're either the 4-byte-code prefix or `trailing_zero_8bits`
    /// padding, never NAL payload (emulation prevention keeps 00 00 0x out of conforming NAL
    /// bytes) — same policy as ffmpeg. The base pointer is only valid inside `body`.
    static func forEachNAL(
        in data: Data, _ body: (_ base: UnsafePointer<UInt8>, _ range: Range<Int>) -> Bool
    ) {
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            let count = raw.count
            var i = 0
            var start = -1
            while i + 2 < count {
                if base[i] == 0, base[i + 1] == 0, base[i + 2] == 1 {
                    var codeStart = i
                    while codeStart > 0, base[codeStart - 1] == 0 {
                        codeStart -= 1
                    }
                    if start >= 0, start < codeStart, !body(base, start..<codeStart) { return }
                    start = i + 3
                    i += 3
                } else {
                    i += 1
                }
            }
            if start >= 0, start < count {
                _ = body(base, start..<count)
            }
        }
    }

    /// Split an Annex-B stream into NAL units (start codes stripped — see `forEachNAL` for
    /// the boundary policy). Materializes a Data per NAL; the streaming paths use
    /// `forEachNAL` directly instead.
    public static func nalUnits(in data: Data) -> [Data] {
        var nals: [Data] = []
        forEachNAL(in: data) { base, range in
            nals.append(Data(bytes: base + range.lowerBound, count: range.count))
            return true
        }
        return nals
    }

    /// HEVC NAL unit type (bits 1..6 of the first byte).
    public static func hevcNalType(_ nal: Data) -> UInt8 {
        guard let first = nal.first else { return 0xFF }
        return (first >> 1) & 0x3F
    }

    /// H.264 NAL unit type (bits 0..4 of the first byte).
    public static func h264NalType(_ nal: Data) -> UInt8 {
        guard let first = nal.first else { return 0xFF }
        return first & 0x1F
    }

    /// Build a format description from an IDR AU's in-band parameter sets (HEVC: VPS/SPS/PPS;
    /// H.264: SPS/PPS). Returns nil when the AU carries no parameter sets (non-IDR). Runs per
    /// AU on the pump thread: parameter sets precede the first VCL NAL in a conforming AU, so
    /// the scan stops there — a delta frame (no leading parameter sets) costs a few byte
    /// compares, no copies.
    public static func formatDescription(
        fromIDR au: Data, codec: VideoCodec
    ) -> CMVideoFormatDescription? {
        var vps: Data?, sps: Data?, pps: Data?
        forEachNAL(in: au) { base, range in
            let first = base[range.lowerBound]
            switch codec.nalType(first) {
            case 32 where codec == .hevc:
                vps = Data(bytes: base + range.lowerBound, count: range.count)
            case 33 where codec == .hevc, 7 where codec == .h264:
                sps = Data(bytes: base + range.lowerBound, count: range.count)
            case 34 where codec == .hevc, 8 where codec == .h264:
                pps = Data(bytes: base + range.lowerBound, count: range.count)
            default:
                if codec.isVCL(first) { return false } // no parameter sets can follow
                // AUD/SEI/… may precede the slices; keep scanning.
            }
            return true
        }
        guard let sps, let pps else { return nil }
        // In the order VideoToolbox wants them: HEVC VPS,SPS,PPS (VPS required); H.264 SPS,PPS.
        let sets: [Data]
        switch codec {
        case .hevc:
            guard let vps else { return nil }
            sets = [vps, sps, pps]
        case .h264:
            sets = [sps, pps]
        case .av1:
            return nil // OBU stream, no parameter-set NALs — handled in AV1.swift, never here
        }

        var format: CMVideoFormatDescription?
        // Pin every parameter set's bytes for the duration of the create call, then hand
        // VideoToolbox parallel pointer/size arrays.
        var pointers: [UnsafePointer<UInt8>] = []
        var sizes: [Int] = []
        func withAll(_ i: Int, _ body: () -> Void) {
            if i == sets.count { body(); return }
            sets[i].withUnsafeBytes { raw in
                pointers.append(raw.bindMemory(to: UInt8.self).baseAddress!)
                sizes.append(sets[i].count)
                withAll(i + 1, body)
            }
        }
        var status: OSStatus = -1
        withAll(0) {
            switch codec {
            case .hevc:
                status = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: pointers.count,
                    parameterSetPointers: pointers,
                    parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4,
                    extensions: nil,
                    formatDescriptionOut: &format)
            case .h264:
                status = CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: pointers.count,
                    parameterSetPointers: pointers,
                    parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &format)
            case .av1:
                break // unreachable — the .av1 arm above already returned
            }
        }
        return status == noErr ? format : nil
    }

    /// Re-pack an Annex-B AU as AVCC (4-byte big-endian length before each NAL), dropping
    /// the parameter-set NALs (they live in the format description).
    public static func avcc(from au: Data, codec: VideoCodec) -> Data {
        var out = Data(capacity: au.count + 16)
        forEachNAL(in: au) { base, range in
            if codec.isParameterSet(base[range.lowerBound]) { return true }
            var len = UInt32(range.count).bigEndian
            withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
            out.append(UnsafeBufferPointer(start: base + range.lowerBound, count: range.count))
            return true
        }
        return out
    }

    /// Wrap one AU as a decode-ready CMSampleBuffer. The AVCC form is packed directly into
    /// the CMBlockBuffer's allocation (sized by a first cheap scan) — no intermediate Data.
    public static func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription, codec: VideoCodec
    ) -> CMSampleBuffer? {
        // Pass 1: byte scan only — total AVCC size of the payload (non-parameter-set) NALs.
        var total = 0
        forEachNAL(in: au.data) { base, range in
            if !codec.isParameterSet(base[range.lowerBound]) { total += 4 + range.count }
            return true
        }
        // Nothing decodable (a parameter-set-only AU — our host never sends one): drop it
        // rather than hand the decoder an empty sample.
        guard total > 0 else { return nil }

        var blockBuffer: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil,
            blockLength: total, blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil, offsetToData: 0, dataLength: total,
            flags: kCMBlockBufferAssureMemoryNowFlag, blockBufferOut: &blockBuffer) == noErr,
            let block = blockBuffer
        else { return nil }
        var dstLen = 0
        var dstPtr: UnsafeMutablePointer<CChar>?
        guard CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &dstLen,
            dataPointerOut: &dstPtr) == noErr,
            dstLen == total, let dstPtr
        else { return nil }
        // Pass 2: the single copy — length prefix + payload per NAL, straight into the block.
        let dst = UnsafeMutableRawPointer(dstPtr)
        var off = 0
        forEachNAL(in: au.data) { base, range in
            if codec.isParameterSet(base[range.lowerBound]) { return true }
            var len = UInt32(range.count).bigEndian
            withUnsafeBytes(of: &len) {
                dst.advanced(by: off).copyMemory(from: $0.baseAddress!, byteCount: 4)
            }
            dst.advanced(by: off + 4)
                .copyMemory(from: base + range.lowerBound, byteCount: range.count)
            off += 4 + range.count
            return true
        }

        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: Int64(au.ptsNs), timescale: 1_000_000_000),
            decodeTimeStamp: .invalid)
        var sampleSize = total
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreate(
            allocator: kCFAllocatorDefault, dataBuffer: block, dataReady: true,
            makeDataReadyCallback: nil, refcon: nil, formatDescription: format,
            sampleCount: 1, sampleTimingEntryCount: 1, sampleTimingArray: &timing,
            sampleSizeEntryCount: 1, sampleSizeArray: &sampleSize,
            sampleBufferOut: &sample) == noErr
        else { return nil }
        // Low-latency display: render on arrival, don't wait for a clock.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample!, createIfNecessary: true) {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(
                dict,
                Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        }
        return sample
    }
}
