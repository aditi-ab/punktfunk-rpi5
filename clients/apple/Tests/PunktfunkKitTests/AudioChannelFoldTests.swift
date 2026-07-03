// Multi-channel input → mono fold (SessionAudio.foldToMono): the fix for a mic on one channel of
// a multi-channel interface. AVAudioConverter's default N→stereo downmix grabs channels 0/1 — dead
// silence when the mic sits higher up — so we fold ourselves. This pins the fiddly bits (the
// interleaved stride, channel pinning, the sum-clamp) against regressions without needing hardware.

#if !os(tvOS)
import XCTest

@testable import PunktfunkKit

final class AudioChannelFoldTests: XCTestCase {
    /// Drive `foldToMono` over channel data expressed as `[[Float]]`, mirroring the two
    /// `floatChannelData` layouts:
    ///   - deinterleaved: each inner array is one channel (all `frames` long).
    ///   - interleaved: a single inner array already interleaved (c0f0, c1f0, …), with the real
    ///     channel count passed separately.
    private func fold(
        _ planes: [[Float]], frames: Int, channels: Int, interleaved: Bool, pinned: Int?
    ) -> [Float] {
        // One C buffer per plane + a table of pointers to them — the shape of floatChannelData.
        let buffers: [UnsafeMutablePointer<Float>] = planes.map { plane in
            let p = UnsafeMutablePointer<Float>.allocate(capacity: plane.count)
            for i in 0..<plane.count { p[i] = plane[i] }
            return p
        }
        let table = UnsafeMutablePointer<UnsafeMutablePointer<Float>>.allocate(
            capacity: buffers.count)
        for (i, b) in buffers.enumerated() { table[i] = b }
        let out = UnsafeMutablePointer<Float>.allocate(capacity: frames)
        defer {
            buffers.forEach { $0.deallocate() }
            table.deallocate()
            out.deallocate()
        }
        SessionAudio.foldToMono(
            input: table, frames: frames, channels: channels,
            interleaved: interleaved, pinned: pinned, out: out)
        return (0..<frames).map { out[$0] }
    }

    // A pinned channel is copied verbatim — the exact fix: mic on a HIGH channel, not 0/1.
    func testPinsHigherChannelDeinterleaved() {
        let result = fold(
            [[0, 0, 0], [0, 0, 0], [0.1, 0.2, 0.3], [0, 0, 0]],
            frames: 3, channels: 4, interleaved: false, pinned: 2)
        XCTAssertEqual(result, [0.1, 0.2, 0.3])
    }

    // Same signal, interleaved layout: [c0f0,c1f0,c2f0,c3f0, c0f1,…]. Guards the `i*ch + c` stride.
    func testPinsHigherChannelInterleaved() {
        let interleaved: [Float] = [
            0, 0, 0.1, 0,
            0, 0, 0.2, 0,
            0, 0, 0.3, 0,
        ]
        let result = fold([interleaved], frames: 3, channels: 4, interleaved: true, pinned: 2)
        XCTAssertEqual(result, [0.1, 0.2, 0.3])
    }

    // Auto (pinned: nil): a lone hot channel amid silence passes at FULL level, never attenuated.
    func testAutoSumsAllChannelsSoALoneMicSurvives() {
        let result = fold(
            [[0, 0], [0.4, -0.4], [0, 0]],
            frames: 2, channels: 3, interleaved: false, pinned: nil)
        XCTAssertEqual(result, [0.4, -0.4])
    }

    // Two simultaneously-hot channels sum past the unit range → clamped, never wraps/overflows.
    func testAutoSumClampsToUnitRange() {
        let result = fold(
            [[0.8, -0.8], [0.9, -0.9]],
            frames: 2, channels: 2, interleaved: false, pinned: nil)
        XCTAssertEqual(result, [1.0, -1.0])
    }

    // A plain mono device is passed through untouched (no clamp, no attenuation).
    func testMonoIsIdentity() {
        let result = fold(
            [[0.25, -0.5, 0.75]], frames: 3, channels: 1, interleaved: false, pinned: nil)
        XCTAssertEqual(result, [0.25, -0.5, 0.75])
    }

    // Belt-and-suspenders: an out-of-range pin (the tap already guards, but the setting is
    // persisted) is ignored by foldToMono's own `ch < channels` guard, which sums instead of
    // reading past the buffer.
    func testOutOfRangePinFallsBackToSum() {
        let result = fold(
            [[0, 0], [0.3, 0.3]],
            frames: 2, channels: 2, interleaved: false, pinned: 2)
        XCTAssertEqual(result, [0.3, 0.3])
    }
}
#endif
