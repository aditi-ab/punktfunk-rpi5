import CoreVideo
import XCTest
import simd

@testable import PunktfunkKit

/// Mirrors pf-client-core's `csc_rows` tests (crates/pf-client-core/src/video.rs) — the Swift port
/// must stay in LOCKSTEP with the Rust implementation, so these are the same fixtures with the
/// same tolerances. A divergence here means the two sides would render the same stream
/// differently.
final class CscRowsTests: XCTestCase {
    private func apply(_ u: CscUniform, _ yuv: SIMD3<Float>) -> SIMD3<Float> {
        SIMD3(
            simd_dot(SIMD3(u.r0.x, u.r0.y, u.r0.z), yuv) + u.r0.w,
            simd_dot(SIMD3(u.r1.x, u.r1.y, u.r1.z), yuv) + u.r1.w,
            simd_dot(SIMD3(u.r2.x, u.r2.y, u.r2.z), yuv) + u.r2.w)
    }

    /// 10-bit limited MSB-packed (P010/x444): reference white Y=940, black Y=64, neutral
    /// chroma 512 — sampled as UNORM16 of `code << 6`.
    func testBt2020TenBitLimitedWhiteBlack() {
        let rows = CscRows.rows(.init(matrix: 9, fullRange: false), depth: 10, msbPacked: true)
        func s(_ code: UInt32) -> Float { Float(code << 6) / 65535.0 }
        let white = apply(rows, SIMD3(s(940), s(512), s(512)))
        let black = apply(rows, SIMD3(s(64), s(512), s(512)))
        for i in 0..<3 {
            XCTAssertEqual(white[i], 1.0, accuracy: 0.002, "white \(white)")
            XCTAssertEqual(black[i], 0.0, accuracy: 0.002, "black \(black)")
        }
    }

    /// Reference white (Y=235, U=V=128 limited) → RGB 1.0; reference black (Y=16) → 0.0.
    func testBt709LimitedWhiteBlack() {
        let rows = CscRows.rows(.init(matrix: 1, fullRange: false), depth: 8, msbPacked: false)
        let white = apply(rows, SIMD3(235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0))
        let black = apply(rows, SIMD3(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0))
        for i in 0..<3 {
            XCTAssertEqual(white[i], 1.0, accuracy: 0.005, "white \(white)")
            XCTAssertEqual(black[i], 0.0, accuracy: 0.005, "black \(black)")
        }
    }

    /// Full-range identity points + the 601-vs-709 red excursion (guards the matrix-code
    /// dispatch — the two matrices MUST differ measurably, that difference is the whole bug
    /// class this port fixes).
    func testFullRangeAndRedExcursion() {
        let rows601 = CscRows.rows(.init(matrix: 5, fullRange: true), depth: 8, msbPacked: false)
        let white = apply(rows601, SIMD3(1.0, 0.5, 0.5))
        for i in 0..<3 {
            XCTAssertEqual(white[i], 1.0, accuracy: 1e-5, "\(white)")
        }
        let red601 = apply(rows601, SIMD3(0.0, 0.5, 1.0))
        XCTAssertEqual(red601[0], 2.0 * (1.0 - 0.299) * 0.5, accuracy: 1e-4, "\(red601)")
        let rows709 = CscRows.rows(.init(matrix: 1, fullRange: true), depth: 8, msbPacked: false)
        let red709 = apply(rows709, SIMD3(0.0, 0.5, 1.0))
        XCTAssertEqual(red709[0], 2.0 * (1.0 - 0.2126) * 0.5, accuracy: 1e-4, "\(red709)")
        XCTAssertGreaterThan(abs(red601[0] - red709[0]), 0.05)
    }

    /// Unspecified (2) and unknown matrix codes fall back to BT.709 — the same default as the
    /// Rust side and every punktfunk host's implicit SDR baseline.
    func testUnspecifiedFallsBackTo709() {
        let unspec = CscRows.rows(.init(matrix: 2, fullRange: false), depth: 8, msbPacked: false)
        let bt709 = CscRows.rows(.init(matrix: 1, fullRange: false), depth: 8, msbPacked: false)
        XCTAssertEqual(unspec, bt709)
    }

    /// `signal(of:)` reads the matrix off the buffer's attachment (what VideoToolbox propagates
    /// from the VUI) and the range off the pixel format — a 601-tagged buffer must come back as
    /// matrix 5, an untagged one as unspecified (2), and a full-range sibling as fullRange.
    func testSignalReadsAttachmentAndRange() throws {
        func makeBuffer(_ format: OSType) throws -> CVPixelBuffer {
            var pb: CVPixelBuffer?
            let status = CVPixelBufferCreate(kCFAllocatorDefault, 64, 64, format, nil, &pb)
            guard status == kCVReturnSuccess, let pb else {
                throw XCTSkip("could not allocate a \(format) pixel buffer")
            }
            return pb
        }

        let tagged = try makeBuffer(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        CVBufferSetAttachment(
            tagged, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            .shouldPropagate)
        XCTAssertEqual(CscRows.signal(of: tagged), CscRows.Signal(matrix: 5, fullRange: false))

        let untagged = try makeBuffer(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        XCTAssertEqual(CscRows.signal(of: untagged), CscRows.Signal(matrix: 2, fullRange: false))

        let full = try makeBuffer(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
        CVBufferSetAttachment(
            full, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_2020,
            .shouldPropagate)
        XCTAssertEqual(CscRows.signal(of: full), CscRows.Signal(matrix: 9, fullRange: true))
    }
}
