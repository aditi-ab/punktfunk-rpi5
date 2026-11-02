// The rules both VideoToolbox pumps follow per access unit: the wait for parameter sets and which
// post-loss AUs reach the decoder. Which AU is a straggler is the core's rule
// (`PunktfunkConnection.recoveryOnFrame`).

import CoreMedia
import XCTest

@testable import PunktfunkKit

final class AUPumpStateTests: XCTestCase {
    /// A format description of a given size, built the cheap way (a made-up H.264 SPS/PPS pair
    /// would be a second parser to maintain — only the dimensions matter here).
    private func format(_ w: Int32, _ h: Int32) throws -> CMVideoFormatDescription {
        var f: CMVideoFormatDescription?
        let status = CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, codecType: kCMVideoCodecType_HEVC,
            width: w, height: h, extensions: nil, formatDescriptionOut: &f)
        XCTAssertEqual(status, noErr)
        return try XCTUnwrap(f)
    }

    func testFirstIDRSeedsTheFormatAndReportsItsSize() throws {
        var state = AUPumpState()
        let step = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        XCTAssertEqual(step.newSize, .init(width: 1920, height: 1080))
        XCTAssertFalse(state.awaitingIDR)
        XCTAssertNotNil(state.format)
    }

    func testDeltaBeforeAnyFormatAsksForOneExactlyOnce() {
        var state = AUPumpState()
        let first = state.note(frameIndex: 1, idrFormat: nil)
        XCTAssertTrue(first.startedFormatWait, "the first AU with no format starts the wait")
        XCTAssertTrue(state.awaitingIDR)
        let second = state.note(frameIndex: 2, idrFormat: nil)
        XCTAssertFalse(second.startedFormatWait, "the wait is announced once, not per AU")
        XCTAssertTrue(state.awaitingIDR)
    }

    func testSameSizeRecoveryIDRDoesNotReportASize() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        let again = state.note(frameIndex: 2, idrFormat: try format(1920, 1080))
        XCTAssertNil(again.newSize, "a loss-recovery IDR at the same size is not a resize")
    }

    func testModeChangeIDRReportsTheNewSize() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        let resized = state.note(frameIndex: 2, idrFormat: try format(2560, 1440))
        XCTAssertEqual(resized.newSize, .init(width: 2560, height: 1440))
    }

    func testRequireIDRDropsTheFormatUntilParameterSetsReturn() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        state.requireIDR()
        XCTAssertNil(state.format)
        XCTAssertTrue(state.awaitingIDR)
        let recovered = state.note(frameIndex: 2, idrFormat: try format(1920, 1080))
        XCTAssertTrue(recovered.resumed, "the IDR that re-anchors decode ends the wait")
        XCTAssertFalse(state.awaitingIDR)
        XCTAssertNotNil(state.format)
    }

    func testADeltaNeverEndsTheWait() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        state.requireIDR()
        let delta = state.note(frameIndex: 2, idrFormat: nil)
        XCTAssertFalse(delta.resumed)
        XCTAssertTrue(state.awaitingIDR, "only parameter sets can clear this want")
    }

    func testALossWithholdsDeltasUntilTheAnchor() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        _ = state.note(frameIndex: 2, idrFormat: nil)
        // Frame 3 was lost: 4 and 5 reference it and must never reach VideoToolbox.
        let first = state.note(frameIndex: 4, idrFormat: nil, lossAhead: true)
        XCTAssertTrue(first.withhold)
        XCTAssertFalse(first.askKeyframe, "the RFI is in flight — no keyframe ask")
        XCTAssertTrue(state.note(frameIndex: 5, idrFormat: nil).withhold)
        let anchor = state.note(
            frameIndex: 6, idrFormat: nil, flags: PunktfunkConnection.userFlagRecoveryAnchor)
        XCTAssertFalse(anchor.withhold, "the anchor references a pre-loss picture: decode it")
        XCTAssertFalse(state.note(frameIndex: 7, idrFormat: nil).withhold)
    }

    func testAnIDREndsWithholdingToo() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        XCTAssertTrue(state.note(frameIndex: 3, idrFormat: nil, lossAhead: true).withhold)
        XCTAssertFalse(state.note(frameIndex: 4, idrFormat: try format(1920, 1080)).withhold)
        XCTAssertFalse(state.note(frameIndex: 5, idrFormat: nil).withhold)
    }

    func testAConcealedStreamWithholdsNothingAfterALoss() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        // The concealer moved the lost reference: the decoder can take every AU, the wave heals.
        let first = state.note(frameIndex: 3, idrFormat: nil, lossAhead: true, concealed: .decodable)
        XCTAssertFalse(first.withhold)
        XCTAssertFalse(first.askKeyframe)
        let mark = state.note(
            frameIndex: 4, idrFormat: nil, flags: PunktfunkConnection.userFlagRecoveryPoint,
            concealed: .decodable)
        XCTAssertFalse(mark.withhold)
        XCTAssertFalse(mark.askKeyframe)
    }

    func testAnUnrecoverableConcealmentWithholdsAndAsks() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        let step = state.note(frameIndex: 3, idrFormat: nil, lossAhead: true, concealed: .unrecoverable)
        XCTAssertTrue(step.withhold)
        XCTAssertTrue(step.askKeyframe)
        // The concealer resumes at the IDR and says so per AU; the pump follows it.
        XCTAssertFalse(state.note(frameIndex: 4, idrFormat: try format(1920, 1080), concealed: .decodable).withhold)
    }

    func testARecoveryMarkWhileWithholdingAsksForAKeyframe() throws {
        var state = AUPumpState()
        _ = state.note(frameIndex: 1, idrFormat: try format(1920, 1080))
        _ = state.note(frameIndex: 3, idrFormat: nil, lossAhead: true)
        // The host declined the RFI and started an intra-refresh wave. The wave builds on a
        // chain VideoToolbox no longer has, so only an IDR ends this: ask, and keep asking.
        let mark = state.note(
            frameIndex: 4, idrFormat: nil, flags: PunktfunkConnection.userFlagRecoveryPoint)
        XCTAssertTrue(mark.withhold)
        XCTAssertTrue(mark.askKeyframe)
        XCTAssertTrue(state.note(frameIndex: 5, idrFormat: nil).askKeyframe)
    }
}
