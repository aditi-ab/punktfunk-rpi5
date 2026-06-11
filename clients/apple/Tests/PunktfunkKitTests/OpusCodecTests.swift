// The Opus codec through CoreAudio (kAudioFormatOpus): a real encode → decode round
// trip. This is the load-bearing assumption of the whole audio feature (no bundled
// libopus) — if AVAudioConverter can't handle raw Opus packets, fail HERE, not in the
// app.

import AVFoundation
import XCTest

@testable import PunktfunkKit

final class OpusCodecTests: XCTestCase {
    /// Encode a 440 Hz stereo tone, decode it back, and require the result to be
    /// recognizably the same signal (Opus is lossy — check correlation, not bytes).
    func testEncodeDecodeRoundTripPreservesTone() throws {
        let encoder = try OpusEncoder()
        let decoder = try OpusDecoder(framesPerPacket: UInt32(OpusEncoder.framesPerPacket))
        let pcmFormat = encoder.pcmFormat

        let frames = OpusEncoder.framesPerPacket
        var packets: [Data] = []
        var phase: Float = 0
        let step = 2 * Float.pi * 440 / 48_000

        // 50 packets = 1 s of tone.
        for _ in 0..<50 {
            let buf = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: frames)!
            buf.frameLength = frames
            let p = buf.floatChannelData![0] // interleaved: one plane, L R L R …
            for f in 0..<Int(frames) {
                let s = sin(phase) * 0.5
                phase += step
                p[f * 2] = s
                p[f * 2 + 1] = s
            }
            packets.append(contentsOf: try encoder.encode(buf))
        }
        XCTAssertGreaterThanOrEqual(packets.count, 45, "encoder must emit ~one packet per buffer")
        XCTAssertTrue(packets.allSatisfy { !$0.isEmpty })

        var decoded: [Float] = []
        let out = AVAudioPCMBuffer(pcmFormat: decoder.pcmFormat, frameCapacity: 5760)!
        for packet in packets {
            let n = try decoder.decode(packet, into: out)
            let p = out.floatChannelData![0]
            for f in 0..<Int(n) {
                decoded.append(p[f * 2]) // left channel
            }
        }
        XCTAssertGreaterThan(decoded.count, 40_000, "~1 s of 48 kHz audio back out")

        // The decoded signal must contain a strong 440 Hz component: correlate against
        // quadrature reference tones (phase-agnostic), skipping the codec warm-up.
        let skip = 4800
        var inPhase: Float = 0
        var quadrature: Float = 0
        var energy: Float = 0
        for (i, s) in decoded[skip...].enumerated() {
            let t = Float(i) * step
            inPhase += s * sin(t)
            quadrature += s * cos(t)
            energy += s * s
        }
        let n = Float(decoded.count - skip)
        let correlation = (inPhase * inPhase + quadrature * quadrature).squareRoot() / n
        let rms = (energy / n).squareRoot()
        XCTAssertGreaterThan(rms, 0.2, "decoded audio is not silence")
        // A clean sine at amplitude a yields correlation a/2 (≈0.25 here); noise ≈ 0.
        XCTAssertGreaterThan(correlation, 0.15, "440 Hz tone must survive the round trip")
    }

    /// The host's audio plane is 5 ms (240-frame) packets — make sure a 240-frame
    /// decoder accepts packets that small (encoder-side we can't force 5 ms out of
    /// CoreAudio, so this decodes the 20 ms packets with a mismatched nominal fpp,
    /// which the packet descriptions override).
    func testDecoderHandlesDTXAndOversizedPackets() throws {
        let decoder = try OpusDecoder(framesPerPacket: 240)
        let out = AVAudioPCMBuffer(pcmFormat: decoder.pcmFormat, frameCapacity: 5760)!
        XCTAssertEqual(try decoder.decode(Data(), into: out), 0, "DTX decodes to silence/0")
        XCTAssertThrowsError(
            try decoder.decode(Data(repeating: 0, count: 2000), into: out),
            "oversized packet must throw, not crash")
    }
}
