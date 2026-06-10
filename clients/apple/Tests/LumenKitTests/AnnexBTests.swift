// Unit tests for the Annex-B ⇄ AVCC plumbing (pure byte-level; no codec involved —
// VideoToolboxRoundTripTests covers the real-bitstream path).

import XCTest
@testable import LumenKit

final class AnnexBTests: XCTestCase {
    /// NAL with the given HEVC type in bits 1..6 of the first header byte.
    private func nal(type: UInt8, payload: [UInt8]) -> Data {
        Data([type << 1, 0x01] + payload)
    }

    private let start4: [UInt8] = [0, 0, 0, 1]
    private let start3: [UInt8] = [0, 0, 1]

    func testSplitMixedStartCodes() {
        let a = nal(type: 32, payload: [0xAA])
        let b = nal(type: 33, payload: [0xBB, 0xBC])
        let c = nal(type: 19, payload: [0xCC, 0xCD, 0xCE])
        var au = Data(start4)
        au.append(a)
        au.append(contentsOf: start3)
        au.append(b)
        au.append(contentsOf: start4)
        au.append(c)

        let nals = AnnexB.nalUnits(in: au)
        XCTAssertEqual(nals, [a, b, c])
        XCTAssertEqual(nals.map(AnnexB.hevcNalType), [32, 33, 19])
    }

    func testSplitSingleNalNoTrailingCode() {
        let v = nal(type: 34, payload: [1, 2, 3])
        let au = Data(start3) + v
        XCTAssertEqual(AnnexB.nalUnits(in: au), [v])
    }

    func testSplitEmptyAndGarbage() {
        XCTAssertEqual(AnnexB.nalUnits(in: Data()), [])
        // No start code at all → no NALs.
        XCTAssertEqual(AnnexB.nalUnits(in: Data([9, 8, 7, 6])), [])
    }

    func testSplitDropsTrailingZeroPadding() {
        // trailing_zero_8bits between NALs (and >2 zeros forming a long separator) must
        // not leak into the preceding NAL.
        let a = nal(type: 33, payload: [0xAA])
        let b = nal(type: 19, payload: [0xBB])
        var au = Data(start4)
        au.append(a)
        au.append(contentsOf: [0, 0, 0, 0, 0, 1]) // padding + start code
        au.append(b)
        XCTAssertEqual(AnnexB.nalUnits(in: au), [a, b])
    }

    func testAvccDropsParameterSetsAndPrefixesLengths() {
        let vps = nal(type: 32, payload: [0xAA])
        let sps = nal(type: 33, payload: [0xBB])
        let pps = nal(type: 34, payload: [0xCC])
        let idr = nal(type: 19, payload: [0xDD, 0xDE, 0xDF, 0xE0])
        var au = Data()
        for n in [vps, sps, pps, idr] {
            au.append(contentsOf: start4)
            au.append(n)
        }

        let avcc = AnnexB.avcc(from: au)
        // Only the IDR survives: 4-byte BE length, then the NAL bytes.
        var expected = Data([0, 0, 0, UInt8(idr.count)])
        expected.append(idr)
        XCTAssertEqual(avcc, expected)
    }

    func testFormatDescriptionNilWithoutParameterSets() {
        let idr = nal(type: 19, payload: [0xDD])
        let au = Data(start4) + idr
        XCTAssertNil(AnnexB.formatDescription(fromIDR: au))
    }
}
