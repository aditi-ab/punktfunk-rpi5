// Annex-B HEVC → CoreMedia plumbing.
//
// The lumen host emits Annex-B access units with in-band VPS/SPS/PPS on every IDR
// (deliberately — the client needs no out-of-band extradata). VideoToolbox wants the AVCC
// flavor instead: a CMVideoFormatDescription built from the parameter sets, and sample
// buffers whose NALs are 4-byte-length-prefixed. This file converts between the two.
//
// SCAFFOLD: written on the Linux host, not yet compiled against Xcode.

import CoreMedia
import Foundation

public enum AnnexB {
    /// Split an Annex-B stream into NAL units (start codes 00 00 01 / 00 00 00 01 stripped).
    public static func nalUnits(in data: Data) -> [Data] {
        var nals: [Data] = []
        let bytes = [UInt8](data)
        var i = 0
        var start = -1
        while i + 2 < bytes.count {
            if bytes[i] == 0, bytes[i + 1] == 0, bytes[i + 2] == 1 {
                let codeStart = (i > 0 && bytes[i - 1] == 0) ? i - 1 : i
                if start >= 0 {
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

    /// Build a format description from an IDR AU's in-band VPS(32)/SPS(33)/PPS(34).
    /// Returns nil when the AU carries no parameter sets (non-IDR).
    public static func formatDescription(fromIDR au: Data) -> CMVideoFormatDescription? {
        var vps: Data?, sps: Data?, pps: Data?
        for nal in nalUnits(in: au) {
            switch hevcNalType(nal) {
            case 32: vps = nal
            case 33: sps = nal
            case 34: pps = nal
            default: break
            }
        }
        guard let vps, let sps, let pps else { return nil }

        var format: CMVideoFormatDescription?
        let sets = [vps, sps, pps]
        let status: OSStatus = sets[0].withUnsafeBytes { v in
            sets[1].withUnsafeBytes { s in
                sets[2].withUnsafeBytes { p in
                    let pointers: [UnsafePointer<UInt8>] = [
                        v.bindMemory(to: UInt8.self).baseAddress!,
                        s.bindMemory(to: UInt8.self).baseAddress!,
                        p.bindMemory(to: UInt8.self).baseAddress!,
                    ]
                    let sizes = [vps.count, sps.count, pps.count]
                    return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: kCFAllocatorDefault,
                        parameterSetCount: 3,
                        parameterSetPointers: pointers,
                        parameterSetSizes: sizes,
                        nalUnitHeaderLength: 4,
                        extensions: nil,
                        formatDescriptionOut: &format)
                }
            }
        }
        return status == noErr ? format : nil
    }

    /// Re-pack an Annex-B AU as AVCC (4-byte big-endian length before each NAL), dropping
    /// the parameter-set NALs (they live in the format description).
    public static func avcc(from au: Data) -> Data {
        var out = Data(capacity: au.count + 16)
        for nal in nalUnits(in: au) {
            let t = hevcNalType(nal)
            if t == 32 || t == 33 || t == 34 { continue } // VPS/SPS/PPS
            var len = UInt32(nal.count).bigEndian
            withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
            out.append(nal)
        }
        return out
    }

    /// Wrap one AU as a decode-ready CMSampleBuffer.
    public static func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription
    ) -> CMSampleBuffer? {
        let avccData = avcc(from: au.data)
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
