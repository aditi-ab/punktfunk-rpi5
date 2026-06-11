// Opus ⇄ PCM through CoreAudio's built-in codec (kAudioFormatOpus, macOS 10.13+ / iOS
// 11+) — no bundled libopus. The host's audio plane is raw Opus packets (48 kHz stereo,
// one frame per packet); AVAudioConverter handles them as single-packet
// AVAudioCompressedBuffers with explicit packet descriptions.
//
// Both classes are single-threaded by contract (one per direction, owned by their
// drain/capture pipelines).

import AVFoundation

enum OpusCodecError: Error {
    /// CoreAudio rejected the Opus stream format or had no converter for it.
    case unavailable
    case convertFailed(String)
}

/// 48 kHz stereo float32 interleaved — the PCM side of both converters and the layout
/// of the playback ring buffer.
func opusPCMFormat() -> AVAudioFormat? {
    AVAudioFormat(
        commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2, interleaved: true)
}

/// The compressed side: raw Opus, `framesPerPacket` nominal samples per packet at 48 kHz
/// (240 = the host's 5 ms audio plane; 960 = the 20 ms packets the encoder emits).
private func opusFormat(framesPerPacket: UInt32) -> AVAudioFormat? {
    var desc = AudioStreamBasicDescription(
        mSampleRate: 48_000,
        mFormatID: kAudioFormatOpus,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: framesPerPacket,
        mBytesPerFrame: 0,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 0,
        mReserved: 0)
    return AVAudioFormat(streamDescription: &desc)
}

final class OpusDecoder {
    private let converter: AVAudioConverter
    private let inBuf: AVAudioCompressedBuffer
    private let opus: AVAudioFormat
    let pcmFormat: AVAudioFormat

    /// `framesPerPacket`: the sender's packet duration in samples (host audio = 240).
    init(framesPerPacket: UInt32) throws {
        guard let pcm = opusPCMFormat(), let opus = opusFormat(framesPerPacket: framesPerPacket),
              let converter = AVAudioConverter(from: opus, to: pcm)
        else { throw OpusCodecError.unavailable }
        self.converter = converter
        self.opus = opus
        self.pcmFormat = pcm
        inBuf = AVAudioCompressedBuffer(
            format: opus, packetCapacity: 1, maximumPacketSize: 1500)
    }

    /// Decode one Opus packet into `out` (whose format must be `pcmFormat`); returns the
    /// number of frames written. Empty packets (DTX) decode to 0 frames.
    func decode(_ packet: Data, into out: AVAudioPCMBuffer) throws -> AVAudioFrameCount {
        guard !packet.isEmpty else { return 0 }
        guard packet.count <= Int(inBuf.maximumPacketSize) else {
            throw OpusCodecError.convertFailed("packet larger than maximumPacketSize")
        }
        packet.withUnsafeBytes { raw in
            inBuf.data.copyMemory(from: raw.baseAddress!, byteCount: raw.count)
        }
        inBuf.byteLength = UInt32(packet.count)
        inBuf.packetCount = 1
        inBuf.packetDescriptions![0] = AudioStreamPacketDescription(
            mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: UInt32(packet.count))

        out.frameLength = 0
        var fed = false
        var convError: NSError?
        let status = converter.convert(to: out, error: &convError) { [inBuf] _, outStatus in
            if fed {
                outStatus.pointee = .noDataNow
                return nil
            }
            fed = true
            outStatus.pointee = .haveData
            return inBuf
        }
        if status == .error {
            throw OpusCodecError.convertFailed(convError?.localizedDescription ?? "decode")
        }
        return out.frameLength
    }
}

final class OpusEncoder {
    /// The encoder's packet duration: 960 samples = 20 ms, CoreAudio's default Opus
    /// framing. The host's mic service decodes any Opus frame size up to 120 ms.
    static let framesPerPacket: AVAudioFrameCount = 960

    private let converter: AVAudioConverter
    private let outBuf: AVAudioCompressedBuffer
    let pcmFormat: AVAudioFormat

    init() throws {
        guard let pcm = opusPCMFormat(),
              let opus = opusFormat(framesPerPacket: UInt32(Self.framesPerPacket)),
              let converter = AVAudioConverter(from: pcm, to: opus)
        else { throw OpusCodecError.unavailable }
        converter.bitRate = 96_000
        self.converter = converter
        self.pcmFormat = pcm
        outBuf = AVAudioCompressedBuffer(
            format: opus, packetCapacity: 4, maximumPacketSize: 1500)
    }

    /// Encode exactly `framesPerPacket` frames of `pcmFormat` audio; returns the encoded
    /// packets (normally one).
    func encode(_ pcm: AVAudioPCMBuffer) throws -> [Data] {
        outBuf.byteLength = 0
        outBuf.packetCount = 0
        var fed = false
        var convError: NSError?
        let status = converter.convert(to: outBuf, error: &convError) { _, outStatus in
            if fed {
                outStatus.pointee = .noDataNow
                return nil
            }
            fed = true
            outStatus.pointee = .haveData
            return pcm
        }
        if status == .error {
            throw OpusCodecError.convertFailed(convError?.localizedDescription ?? "encode")
        }
        guard let descs = outBuf.packetDescriptions else { return [] }
        return (0..<Int(outBuf.packetCount)).map { i in
            let d = descs[i]
            return Data(
                bytes: outBuf.data.advanced(by: Int(d.mStartOffset)),
                count: Int(d.mDataByteSize))
        }
    }
}
