// Annex-B HEVC → CoreMedia plumbing.
//
// The punktfunk host emits Annex-B access units with in-band VPS/SPS/PPS on every IDR
// (deliberately — the client needs no out-of-band extradata). VideoToolbox wants the AVCC
// flavor instead: a CMVideoFormatDescription built from the parameter sets, and sample
// buffers whose NALs are 4-byte-length-prefixed. This file converts between the two.
//
// SCAFFOLD: written on the Linux host, not yet compiled against Xcode.

import CoreMedia
import Foundation

/// The video codec of the host's elementary stream — negotiated in the Welcome and read via
/// `punktfunk_connection_codec`. Both are Annex-B with in-band parameter sets on every IDR; they
/// differ only in NAL-header layout and which parameter sets exist (HEVC adds a VPS). AV1 is not an
/// Annex-B/NAL codec and isn't handled here (hosts don't emit it on the native path yet).
public enum VideoCodec: Equatable {
    case h264
    case hevc

    /// Resolve from the wire `Welcome.codec` byte (`PUNKTFUNK_CODEC_*`; unknown → HEVC).
    public init(wire: UInt8) {
        self = wire == 0x01 ? .h264 : .hevc // 0x01 = PUNKTFUNK_CODEC_H264
    }
}

public enum AnnexB {
    /// Split an Annex-B stream into NAL units (start codes 00 00 01 / 00 00 00 01 stripped).
    /// All zeros immediately preceding a start code are dropped: they're either the
    /// 4-byte-code prefix or `trailing_zero_8bits` padding, never NAL payload (emulation
    /// prevention keeps 00 00 0x out of conforming NAL bytes) — same policy as ffmpeg.
    public static func nalUnits(in data: Data) -> [Data] {
        var nals: [Data] = []
        let bytes = [UInt8](data)
        var i = 0
        var start = -1
        while i + 2 < bytes.count {
            if bytes[i] == 0, bytes[i + 1] == 0, bytes[i + 2] == 1 {
                var codeStart = i
                while codeStart > 0, bytes[codeStart - 1] == 0 {
                    codeStart -= 1
                }
                if start >= 0, start < codeStart {
                    nals.append(Data(bytes[start..<codeStart]))
                }
                start = i + 3
                i += 3
            } else {
                i += 1
            }
        }
        if start >= 0, start < bytes.count {
            nals.append(Data(bytes[start...]))
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

    /// True if this NAL is a parameter set for `codec` (dropped from AVCC; kept for the format desc).
    /// HEVC: VPS 32 / SPS 33 / PPS 34. H.264: SPS 7 / PPS 8 (no VPS).
    private static func isParameterSet(_ nal: Data, _ codec: VideoCodec) -> Bool {
        switch codec {
        case .hevc: let t = hevcNalType(nal); return t == 32 || t == 33 || t == 34
        case .h264: let t = h264NalType(nal); return t == 7 || t == 8
        }
    }

    /// Build a format description from an IDR AU's in-band parameter sets (HEVC: VPS/SPS/PPS;
    /// H.264: SPS/PPS). Returns nil when the AU carries no parameter sets (non-IDR).
    public static func formatDescription(fromIDR au: Data, codec: VideoCodec)
        -> CMVideoFormatDescription?
    {
        // Collect the parameter-set NALs in the order VideoToolbox wants them (HEVC: VPS,SPS,PPS;
        // H.264: SPS,PPS).
        var vps: Data?, sps: Data?, pps: Data?
        for nal in nalUnits(in: au) {
            switch codec {
            case .hevc:
                switch hevcNalType(nal) {
                case 32: vps = nal
                case 33: sps = nal
                case 34: pps = nal
                default: break
                }
            case .h264:
                switch h264NalType(nal) {
                case 7: sps = nal
                case 8: pps = nal
                default: break
                }
            }
        }
        guard let sps, let pps else { return nil }
        let sets: [Data] = codec == .hevc ? [vps, sps, pps].compactMap { $0 } : [sps, pps]
        guard codec == .h264 || sets.count == 3 else { return nil } // HEVC needs the VPS too

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
            }
        }
        return status == noErr ? format : nil
    }

    /// Re-pack an Annex-B AU as AVCC (4-byte big-endian length before each NAL), dropping
    /// the parameter-set NALs (they live in the format description).
    public static func avcc(from au: Data, codec: VideoCodec) -> Data {
        var out = Data(capacity: au.count + 16)
        for nal in nalUnits(in: au) {
            if isParameterSet(nal, codec) { continue }
            var len = UInt32(nal.count).bigEndian
            withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
            out.append(nal)
        }
        return out
    }

    /// Wrap one AU as a decode-ready CMSampleBuffer.
    public static func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription, codec: VideoCodec
    ) -> CMSampleBuffer? {
        let avccData = avcc(from: au.data, codec: codec)
        var blockBuffer: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil,
            blockLength: avccData.count, blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil, offsetToData: 0, dataLength: avccData.count,
            flags: 0, blockBufferOut: &blockBuffer) == noErr,
            let block = blockBuffer
        else { return nil }
        let copied = avccData.withUnsafeBytes { raw in
            CMBlockBufferReplaceDataBytes(
                with: raw.baseAddress!, blockBuffer: block,
                offsetIntoDestination: 0, dataLength: avccData.count)
        }
        guard copied == noErr else { return nil }

        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: Int64(au.ptsNs), timescale: 1_000_000_000),
            decodeTimeStamp: .invalid)
        var sampleSize = avccData.count
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
