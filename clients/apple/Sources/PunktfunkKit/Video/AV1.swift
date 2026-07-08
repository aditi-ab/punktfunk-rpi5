// AV1 (low-overhead OBU bitstream) → CoreMedia plumbing — the AV1 sibling of AnnexB.swift.
//
// The punktfunk host emits AV1 access units as low-overhead temporal units (the raw encoder
// output every other client feeds ffmpeg): a temporal-delimiter OBU, then — on every keyframe,
// per the same in-band-config policy as the NAL codecs — a sequence-header OBU, then the frame
// OBUs. VideoToolbox instead wants the ISOBMFF 'av01' flavor: a CMVideoFormatDescription
// carrying an `av1C` configuration record (built from the sequence header), and sample buffers
// holding the temporal unit with the temporal delimiter stripped and every OBU size-fielded.
// This file converts between the two.
//
// HOT PATH: like AnnexB, both pumps run `formatDescription(fromKeyframe:)` +
// `sampleBuffer(au:format:)` once per AU, so everything is built on `forEachOBU` — a zero-copy
// scan over the AU's bytes (ranges, not materialized Datas). A delta AU (no sequence header)
// costs a few OBU-header reads; the sample repack leaves exactly one copy (source → block
// buffer), mirroring AnnexB.sampleBuffer.
//
// The full sequence-header parse (AV1 spec 5.5.1) runs only when a keyframe actually carries
// one — it exists to fill the `av1C` record fields (profile/level/tier/depth/chroma) and the
// colorimetry extensions (so VideoDecoder.isHDRFormat and the presenter's color handling work
// identically across codecs). The host currently gates 10-bit and 4:4:4 to HEVC, so an AV1
// stream is 8-bit 4:2:0 today; the parser still reads depth/chroma/color faithfully so nothing
// here needs touching when that gate lifts.

import CoreMedia
import Foundation
import VideoToolbox

public enum AV1 {
    /// True when this device can hardware-decode AV1 (M3-class Macs, A17 Pro-class iPhones,
    /// current iPads; false on every Apple TV to date). VideoToolbox has no software AV1
    /// decoder, so this is the advertisement gate: a client must never invite a stream it
    /// can't decode in real time.
    public static let hardwareDecodeSupported: Bool =
        VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)

    // MARK: - OBU walking

    /// OBU types (AV1 spec 6.2.2) — only the ones this file dispatches on.
    enum OBUType {
        static let sequenceHeader: UInt8 = 1
        static let temporalDelimiter: UInt8 = 2
        static let padding: UInt8 = 15
    }

    /// Walk the OBUs of a low-overhead temporal unit without copying: `body` receives the buffer
    /// base, each OBU's header range (header byte + optional extension byte + size field, i.e.
    /// everything before the payload), payload range, and type — and returns false to stop early.
    /// The walk ends at the first malformed OBU (forbidden bit set, truncated header, or a size
    /// field overrunning the buffer): a torn AU decodes as garbage anyway and the pumps' keyframe
    /// recovery re-anchors, so bailing beats guessing at boundaries. An OBU with
    /// `obu_has_size_field == 0` extends to the end of the buffer (legal only for the last one).
    /// The base pointer is only valid inside `body`.
    static func forEachOBU(
        in data: Data,
        _ body: (
            _ base: UnsafePointer<UInt8>, _ header: Range<Int>, _ payload: Range<Int>,
            _ type: UInt8
        ) -> Bool
    ) {
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            let count = raw.count
            var i = 0
            while i < count {
                let start = i
                let h = base[i]
                guard h & 0x80 == 0 else { return } // obu_forbidden_bit — not an OBU stream
                let type = (h >> 3) & 0x0F
                let hasExtension = h & 0x04 != 0
                let hasSize = h & 0x02 != 0
                i += 1
                if hasExtension {
                    guard i < count else { return }
                    i += 1
                }
                let payloadLen: Int
                if hasSize {
                    guard let (size, sizeLen) = leb128(base: base, at: i, count: count)
                    else { return }
                    i += sizeLen
                    payloadLen = size
                } else {
                    payloadLen = count - i // no size field: extends to the end (must be last)
                }
                guard i + payloadLen <= count else { return }
                if !body(base, start..<i, i..<(i + payloadLen), type) { return }
                i += payloadLen
            }
        }
    }

    /// Decode a leb128 value at `at` (AV1 spec 4.10.5). Returns (value, encoded length) or nil
    /// on truncation / a value past 32 bits (sizes beyond that are nonsense for an OBU).
    private static func leb128(
        base: UnsafePointer<UInt8>, at: Int, count: Int
    ) -> (Int, Int)? {
        var value: UInt64 = 0
        for i in 0..<8 {
            guard at + i < count else { return nil }
            let byte = base[at + i]
            value |= UInt64(byte & 0x7F) << (7 * i)
            if byte & 0x80 == 0 {
                guard value <= UInt64(UInt32.max) else { return nil }
                return (Int(value), i + 1)
            }
        }
        return nil
    }

    /// leb128-encoded byte length of `value`.
    private static func leb128Length(_ value: Int) -> Int {
        var v = UInt32(value)
        var n = 1
        while v >= 0x80 {
            v >>= 7
            n += 1
        }
        return n
    }

    /// Encode `value` as leb128 into `dst`; returns the byte count written.
    private static func putLeb128(_ value: Int, into dst: UnsafeMutableRawPointer) -> Int {
        var v = UInt32(value)
        var n = 0
        repeat {
            var byte = UInt8(v & 0x7F)
            v >>= 7
            if v != 0 { byte |= 0x80 }
            dst.storeBytes(of: byte, toByteOffset: n, as: UInt8.self)
            n += 1
        } while v != 0
        return n
    }

    // MARK: - Sequence header

    /// The sequence-header fields the `av1C` record and the colorimetry extensions need
    /// (AV1 spec 5.5; color codes are ITU-T H.273, shared with the HEVC VUI).
    struct SequenceHeader {
        var profile: UInt8 = 0
        var levelIdx0: UInt8 = 0
        var tier0: UInt8 = 0
        var highBitdepth = false
        var twelveBit = false
        var monochrome = false
        var subsamplingX = true
        var subsamplingY = true
        var chromaSamplePosition: UInt8 = 0
        /// H.273 codes; 2 = unspecified (the spec default when no color description is coded).
        var colorPrimaries: UInt8 = 2
        var transferCharacteristics: UInt8 = 2
        var matrixCoefficients: UInt8 = 2
        var fullRange = false
        var maxWidth = 0
        var maxHeight = 0
    }

    /// MSB-first bit reader over the sequence-header payload. Every read is bounds-checked and
    /// returns nil on overrun — the parser guard-lets each field so a truncated header yields
    /// nil rather than garbage.
    private struct BitReader {
        private let bytes: UnsafePointer<UInt8>
        private let bitCount: Int
        private var pos = 0

        init(bytes: UnsafePointer<UInt8>, count: Int) {
            self.bytes = bytes
            self.bitCount = count * 8
        }

        mutating func f(_ n: Int) -> UInt32? {
            guard n <= 32, pos + n <= bitCount else { return nil }
            var v: UInt32 = 0
            for _ in 0..<n {
                let bit = (bytes[pos >> 3] >> (7 - UInt8(pos & 7))) & 1
                v = (v << 1) | UInt32(bit)
                pos += 1
            }
            return v
        }

        mutating func flag() -> Bool? { f(1).map { $0 == 1 } }

        /// uvlc() (spec 4.10.3) — only `num_ticks_per_picture_minus_1` uses it here.
        mutating func uvlc() -> UInt32? {
            var leadingZeros = 0
            while true {
                guard let b = f(1) else { return nil }
                if b == 1 { break }
                leadingZeros += 1
                if leadingZeros >= 32 { return nil }
            }
            if leadingZeros == 0 { return 0 }
            guard let v = f(leadingZeros) else { return nil }
            return v + (1 << leadingZeros) - 1
        }
    }

    /// Parse a sequence-header OBU payload (spec 5.5.1 — the full walk down to color_config,
    /// which is what `av1C` + the colorimetry extensions are built from). Returns nil on any
    /// truncation or spec violation.
    static func parseSequenceHeader(_ payload: Data) -> SequenceHeader? {
        payload.withUnsafeBytes { (raw: UnsafeRawBufferPointer) -> SequenceHeader? in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return nil }
            var r = BitReader(bytes: base, count: raw.count)
            var sh = SequenceHeader()

            guard let profile = r.f(3), profile <= 2 else { return nil }
            sh.profile = UInt8(profile)
            guard r.flag() != nil else { return nil } // still_picture
            guard let reduced = r.flag() else { return nil }

            var decoderModelInfoPresent = false
            var bufferDelayLengthMinus1 = 0
            if reduced {
                guard let level = r.f(5) else { return nil }
                sh.levelIdx0 = UInt8(level)
                sh.tier0 = 0
            } else {
                guard let timingInfoPresent = r.flag() else { return nil }
                if timingInfoPresent {
                    guard r.f(32) != nil, r.f(32) != nil, // num_units_in_display_tick, time_scale
                          let equalPictureInterval = r.flag()
                    else { return nil }
                    if equalPictureInterval, r.uvlc() == nil { return nil }
                    guard let dmip = r.flag() else { return nil }
                    decoderModelInfoPresent = dmip
                    if decoderModelInfoPresent {
                        guard let bdl = r.f(5), r.f(32) != nil, r.f(5) != nil, r.f(5) != nil
                        else { return nil }
                        bufferDelayLengthMinus1 = Int(bdl)
                    }
                }
                guard let initialDisplayDelayPresent = r.flag(),
                      let opCountMinus1 = r.f(5)
                else { return nil }
                for i in 0...Int(opCountMinus1) {
                    guard r.f(12) != nil, let level = r.f(5) else { return nil }
                    var tier: UInt32 = 0
                    if level > 7 {
                        guard let t = r.f(1) else { return nil }
                        tier = t
                    }
                    if i == 0 {
                        sh.levelIdx0 = UInt8(level)
                        sh.tier0 = UInt8(tier)
                    }
                    if decoderModelInfoPresent {
                        guard let present = r.flag() else { return nil }
                        if present {
                            let n = bufferDelayLengthMinus1 + 1
                            guard r.f(n) != nil, r.f(n) != nil, r.f(1) != nil else { return nil }
                        }
                    }
                    if initialDisplayDelayPresent {
                        guard let present = r.flag() else { return nil }
                        if present, r.f(4) == nil { return nil }
                    }
                }
            }

            guard let widthBitsMinus1 = r.f(4), let heightBitsMinus1 = r.f(4),
                  let maxWidthMinus1 = r.f(Int(widthBitsMinus1) + 1),
                  let maxHeightMinus1 = r.f(Int(heightBitsMinus1) + 1)
            else { return nil }
            sh.maxWidth = Int(maxWidthMinus1) + 1
            sh.maxHeight = Int(maxHeightMinus1) + 1

            if !reduced {
                guard let frameIdNumbersPresent = r.flag() else { return nil }
                if frameIdNumbersPresent {
                    guard r.f(4) != nil, r.f(3) != nil else { return nil }
                }
            }
            // use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
            guard r.f(3) != nil else { return nil }
            if !reduced {
                // enable_interintra_compound … enable_dual_filter
                guard r.f(4) != nil, let enableOrderHint = r.flag() else { return nil }
                if enableOrderHint {
                    guard r.f(2) != nil else { return nil } // jnt_comp, ref_frame_mvs
                }
                guard let chooseScreenContentTools = r.flag() else { return nil }
                let forceScreenContentTools: UInt32
                if chooseScreenContentTools {
                    forceScreenContentTools = 2 // SELECT_SCREEN_CONTENT_TOOLS
                } else {
                    guard let v = r.f(1) else { return nil }
                    forceScreenContentTools = v
                }
                if forceScreenContentTools > 0 {
                    guard let chooseIntegerMv = r.flag() else { return nil }
                    if !chooseIntegerMv, r.f(1) == nil { return nil }
                }
                if enableOrderHint, r.f(3) == nil { return nil } // order_hint_bits_minus_1
            }
            // enable_superres, enable_cdef, enable_restoration
            guard r.f(3) != nil else { return nil }

            // color_config() (spec 5.5.2)
            guard let highBitdepth = r.flag() else { return nil }
            sh.highBitdepth = highBitdepth
            if sh.profile == 2, highBitdepth {
                guard let twelveBit = r.flag() else { return nil }
                sh.twelveBit = twelveBit
            }
            if sh.profile == 1 {
                sh.monochrome = false
            } else {
                guard let mono = r.flag() else { return nil }
                sh.monochrome = mono
            }
            guard let colorDescriptionPresent = r.flag() else { return nil }
            if colorDescriptionPresent {
                guard let cp = r.f(8), let tc = r.f(8), let mc = r.f(8) else { return nil }
                sh.colorPrimaries = UInt8(cp)
                sh.transferCharacteristics = UInt8(tc)
                sh.matrixCoefficients = UInt8(mc)
            }
            if sh.monochrome {
                guard let fullRange = r.flag() else { return nil }
                sh.fullRange = fullRange
                sh.subsamplingX = true
                sh.subsamplingY = true
                sh.chromaSamplePosition = 0
                return sh
            }
            if sh.colorPrimaries == 1, sh.transferCharacteristics == 13,
               sh.matrixCoefficients == 0 {
                // BT.709 + sRGB + identity forces full-range 4:4:4.
                sh.fullRange = true
                sh.subsamplingX = false
                sh.subsamplingY = false
                return sh
            }
            guard let fullRange = r.flag() else { return nil }
            sh.fullRange = fullRange
            switch sh.profile {
            case 0:
                sh.subsamplingX = true
                sh.subsamplingY = true
            case 1:
                sh.subsamplingX = false
                sh.subsamplingY = false
            default: // profile 2
                if sh.highBitdepth, sh.twelveBit {
                    guard let ssx = r.flag() else { return nil }
                    sh.subsamplingX = ssx
                    if ssx {
                        guard let ssy = r.flag() else { return nil }
                        sh.subsamplingY = ssy
                    } else {
                        sh.subsamplingY = false
                    }
                } else {
                    sh.subsamplingX = true
                    sh.subsamplingY = false
                }
            }
            if sh.subsamplingX, sh.subsamplingY {
                guard let csp = r.f(2) else { return nil }
                sh.chromaSamplePosition = UInt8(csp)
            }
            return sh
        }
    }

    // MARK: - Format description

    /// Build a format description from a keyframe AU's in-band sequence header — the AV1
    /// equivalent of `AnnexB.formatDescription(fromIDR:)`. Returns nil when the AU carries no
    /// sequence-header OBU (a delta frame): the pumps latch the previous description exactly as
    /// they do for the NAL codecs. The description carries the `av1C` record (with the sequence
    /// header as its configOBUs) plus colorimetry extensions mapped from color_config, so
    /// `VideoDecoder.isHDRFormat` and the presenter treat AV1 like any other stream.
    public static func formatDescription(fromKeyframe au: Data) -> CMVideoFormatDescription? {
        // The sequence-header OBU, re-emitted with a size field (encoders size-field everything
        // in practice; the rewrap also covers a last-OBU-without-size corner).
        var seqHeaderOBU: Data?
        var seqHeaderPayload: Data?
        forEachOBU(in: au) { base, header, payload, type in
            guard type == OBUType.sequenceHeader else { return true }
            var obu = Data(capacity: 2 + leb128Length(payload.count) + payload.count)
            obu.append(base[header.lowerBound] | 0x02) // has_size_field set
            if base[header.lowerBound] & 0x04 != 0 { // extension byte rides along
                obu.append(base[header.lowerBound + 1])
            }
            var lenBuf = [UInt8](repeating: 0, count: 8)
            let lenLen = lenBuf.withUnsafeMutableBytes {
                putLeb128(payload.count, into: $0.baseAddress!)
            }
            obu.append(contentsOf: lenBuf[0..<lenLen])
            obu.append(UnsafeBufferPointer(start: base + payload.lowerBound, count: payload.count))
            seqHeaderOBU = obu
            seqHeaderPayload = Data(bytes: base + payload.lowerBound, count: payload.count)
            return false
        }
        guard let seqHeaderOBU, let seqHeaderPayload,
              let sh = parseSequenceHeader(seqHeaderPayload),
              sh.maxWidth > 0, sh.maxHeight > 0
        else { return nil }

        // AV1CodecConfigurationRecord (AV1-ISOBMFF §2.3): 4 fixed bytes + configOBUs.
        var av1C = Data(capacity: 4 + seqHeaderOBU.count)
        av1C.append(0x81) // marker=1, version=1
        av1C.append((sh.profile << 5) | sh.levelIdx0)
        av1C.append(
            (sh.tier0 << 7)
                | ((sh.highBitdepth ? 1 : 0) << 6)
                | ((sh.twelveBit ? 1 : 0) << 5)
                | ((sh.monochrome ? 1 : 0) << 4)
                | ((sh.subsamplingX ? 1 : 0) << 3)
                | ((sh.subsamplingY ? 1 : 0) << 2)
                | sh.chromaSamplePosition)
        av1C.append(0) // no initial_presentation_delay
        av1C.append(seqHeaderOBU)

        // Colorimetry from color_config's H.273 codes; unspecified (2) falls back to BT.709 —
        // the host's SDR default, same policy the presenter applies elsewhere.
        let primaries: CFString = {
            switch sh.colorPrimaries {
            case 9: return kCMFormatDescriptionColorPrimaries_ITU_R_2020
            case 6: return kCMFormatDescriptionColorPrimaries_SMPTE_C
            case 5: return kCMFormatDescriptionColorPrimaries_EBU_3213
            default: return kCMFormatDescriptionColorPrimaries_ITU_R_709_2
            }
        }()
        let transfer: CFString = {
            switch sh.transferCharacteristics {
            case 16: return kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ
            case 18: return kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG
            case 13: return kCMFormatDescriptionTransferFunction_sRGB
            case 8: return kCMFormatDescriptionTransferFunction_Linear
            default: return kCMFormatDescriptionTransferFunction_ITU_R_709_2
            }
        }()
        let matrix: CFString = {
            switch sh.matrixCoefficients {
            case 9, 10: return kCMFormatDescriptionYCbCrMatrix_ITU_R_2020
            case 5, 6: return kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4
            default: return kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2
            }
        }()
        let extensions: [CFString: Any] = [
            kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: ["av1C": av1C],
            kCMFormatDescriptionExtension_ColorPrimaries: primaries,
            kCMFormatDescriptionExtension_TransferFunction: transfer,
            kCMFormatDescriptionExtension_YCbCrMatrix: matrix,
            kCMFormatDescriptionExtension_FullRangeVideo: sh.fullRange,
        ]
        var format: CMVideoFormatDescription?
        let status = CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault,
            codecType: kCMVideoCodecType_AV1,
            width: Int32(sh.maxWidth), height: Int32(sh.maxHeight),
            extensions: extensions as CFDictionary,
            formatDescriptionOut: &format)
        return status == noErr ? format : nil
    }

    // MARK: - Sample buffers

    /// Wrap one temporal unit as a decode-ready CMSampleBuffer in the ISOBMFF 'av01' sample
    /// format: the temporal-delimiter (and padding) OBUs are dropped, every remaining OBU is
    /// re-emitted with a size field, and — mirroring AnnexB.sampleBuffer — the result is packed
    /// straight into the CMBlockBuffer's allocation (sized by a first cheap scan). The sequence
    /// header stays in-band (spec-legal: it's bit-identical to the one in `av1C`, which is
    /// rebuilt from the same keyframe), preserving the host's self-contained-keyframe policy.
    public static func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription
    ) -> CMSampleBuffer? {
        // Pass 1: byte scan only — total repacked size of the kept OBUs.
        var total = 0
        forEachOBU(in: au.data) { base, header, payload, type in
            if type == OBUType.temporalDelimiter || type == OBUType.padding { return true }
            let headerLen = base[header.lowerBound] & 0x04 != 0 ? 2 : 1
            total += headerLen + leb128Length(payload.count) + payload.count
            return true
        }
        // Nothing decodable (a delimiter-only AU — our host never sends one): drop it rather
        // than hand the decoder an empty sample.
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
        // Pass 2: the single copy — header (+extension) byte, size field, payload per OBU.
        let dst = UnsafeMutableRawPointer(dstPtr)
        var off = 0
        forEachOBU(in: au.data) { base, header, payload, type in
            if type == OBUType.temporalDelimiter || type == OBUType.padding { return true }
            dst.storeBytes(
                of: base[header.lowerBound] | 0x02, toByteOffset: off, as: UInt8.self)
            off += 1
            if base[header.lowerBound] & 0x04 != 0 {
                dst.storeBytes(
                    of: base[header.lowerBound + 1], toByteOffset: off, as: UInt8.self)
                off += 1
            }
            off += putLeb128(payload.count, into: dst.advanced(by: off))
            dst.advanced(by: off)
                .copyMemory(from: base + payload.lowerBound, byteCount: payload.count)
            off += payload.count
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

extension VideoCodec {
    /// Codec-dispatching format-description refresh: the AV1 path keys on an in-band sequence
    /// header, the NAL codecs on in-band parameter sets — one call site in each pump.
    public func formatDescription(fromKeyframe au: Data) -> CMVideoFormatDescription? {
        self == .av1
            ? AV1.formatDescription(fromKeyframe: au)
            : AnnexB.formatDescription(fromIDR: au, codec: self)
    }

    /// Codec-dispatching sample wrap (see `formatDescription(fromKeyframe:)`).
    public func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription
    ) -> CMSampleBuffer? {
        self == .av1
            ? AV1.sampleBuffer(au: au, format: format)
            : AnnexB.sampleBuffer(au: au, format: format, codec: self)
    }
}
