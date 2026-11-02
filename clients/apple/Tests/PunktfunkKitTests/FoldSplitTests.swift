// Which postures earn the tabletop split, and where it cuts. Twin of the Android client's
// `FoldSplitTest.kt`.

import CoreGraphics
import XCTest

import PunktfunkShared

final class FoldSplitTests: XCTestCase {
    func testACreaseAcrossTheMiddleSplitsWithNoGap() {
        // An iPhone Duo's inner display held like a laptop: 669 × 951 pt, the crease a
        // zero-height line across the whole width.
        let split = FoldSplit(
            hinge: CGRect(x: 0, y: 475.5, width: 669, height: 0),
            in: CGSize(width: 669, height: 951))
        XCTAssertEqual(split, FoldSplit(video: 475.5, hinge: 0))
    }

    func testAHingeWithThicknessKeepsItsStripOutOfBothHalves() {
        let split = FoldSplit(
            hinge: CGRect(x: 0, y: 450, width: 600, height: 28),
            in: CGSize(width: 600, height: 960))
        XCTAssertEqual(split, FoldSplit(video: 450, hinge: 28))
    }

    func testBookPostureDoesNotSplit() {
        // A vertical hinge folds left and right: there is no flat half to rest a pad on.
        XCTAssertNil(FoldSplit(
            hinge: CGRect(x: 475.5, y: 0, width: 0, height: 669),
            in: CGSize(width: 951, height: 669)))
    }

    func testAHingeNearAnEdgeLeavesNothingWorthSplittingInto() {
        let size = CGSize(width: 669, height: 951)
        XCTAssertNil(FoldSplit(hinge: CGRect(x: 0, y: 60, width: 669, height: 0), in: size))
        XCTAssertNil(FoldSplit(hinge: CGRect(x: 0, y: 900, width: 669, height: 0), in: size))
    }

    func testAnUnmeasuredContainerDoesNotSplit() {
        XCTAssertNil(FoldSplit(hinge: CGRect(x: 0, y: 475, width: 669, height: 0), in: .zero))
    }

    func testARegionOffTheIntegerGridStillSpansTheWidth() {
        // Measured on a 3x panel a full-width region can start a fraction inside the edge.
        XCTAssertNotNil(FoldSplit(
            hinge: CGRect(x: 0.33, y: 475.5, width: 668.34, height: 0),
            in: CGSize(width: 669, height: 951)))
    }
}
