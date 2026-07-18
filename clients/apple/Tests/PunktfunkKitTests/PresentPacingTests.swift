import XCTest

#if canImport(Metal)
import QuartzCore
@testable import PunktfunkKit

/// Stage-3 present pacing: the one-in-flight `PresentGate` and the stage-1/2/3 `PresenterChoice`
/// resolution (setting + PUNKTFUNK_PRESENTER env override + the release-build stage-1 gate).
final class PresentPacingTests: XCTestCase {
    // MARK: - PresentGate

    /// The core invariant: one present in flight. A second acquire while pending must fail (the
    /// frame stays in the ring for the presented handler's re-signal); release reopens.
    func testGateAdmitsOneInFlightPresent() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 0), "an idle gate must admit the first present")
        XCTAssertFalse(gate.tryAcquire(now: 0.001), "a pending present must close the gate")
        gate.release()
        XCTAssertTrue(gate.tryAcquire(now: 0.002), "release must reopen the gate")
        XCTAssertEqual(gate.drainForced(), 0, "no stale present was force-cleared")
    }

    /// The lost-handler insurance: a present whose handler never fires (the macOS "presents
    /// aren't damage" hazard class) must not freeze the stream — past `staleAfter` the gate
    /// force-opens and counts the event for the PUNKTFUNK_PRESENT_DEBUG `forced` stat.
    func testGateForceOpensAfterStaleTimeout() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 10))
        // Within the stale window the gate stays closed.
        XCTAssertFalse(gate.tryAcquire(now: 10 + PresentGate.staleAfter - 0.01))
        // Past it, the pending present is presumed lost: reopen, and count the force-clear.
        XCTAssertTrue(gate.tryAcquire(now: 10 + PresentGate.staleAfter + 0.01))
        XCTAssertEqual(gate.drainForced(), 1)
        XCTAssertEqual(gate.drainForced(), 0, "drain resets the counter")
    }

    /// Release is idempotent (a late stale-path double-release must be harmless), and an
    /// acquire-then-release with no present (empty ring after arming) leaves the gate clean.
    func testGateReleaseIsIdempotent() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 0))
        gate.release()
        gate.release() // the stale-cleared present's handler firing late
        XCTAssertTrue(gate.tryAcquire(now: 0.001))
        XCTAssertEqual(gate.drainForced(), 0)
    }

    // MARK: - PresenterChoice

    func testPresenterChoiceDefaultsToStage2() {
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: nil, allowStage1: true), .stage2)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "garbage", env: nil, allowStage1: true), .stage2)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage2", env: nil, allowStage1: true), .stage2)
    }

    func testPresenterChoiceResolvesStage3FromSettingAndEnv() {
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage3", env: nil, allowStage1: true), .stage3)
        // The env override wins over the persisted setting (A/B without touching settings)…
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage2", env: "stage3", allowStage1: true), .stage3)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage3", env: "stage2", allowStage1: true), .stage2)
        // …but an EMPTY env var is "unset", not an override.
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage3", env: "", allowStage1: true), .stage3)
    }

    /// Stage-1 (the freeze-prone system-layer diagnostic) resolves only where allowed (DEBUG
    /// builds); a leftover "stage1" value in a release build maps back to stage-2.
    func testPresenterChoiceGatesStage1() {
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage1", env: nil, allowStage1: true), .stage1)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage1", env: nil, allowStage1: false), .stage2)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage1", allowStage1: false), .stage2)
    }

    /// `explicit` is nil exactly when `resolve` would fall back to the platform default — the
    /// distinction the codec-conditional pacing default rides on.
    func testPresenterChoiceExplicitIsNilWithoutASelection() {
        XCTAssertNil(PresenterChoice.explicit(setting: nil, env: nil, allowStage1: true))
        XCTAssertNil(PresenterChoice.explicit(setting: "garbage", env: nil, allowStage1: true))
        XCTAssertNil(PresenterChoice.explicit(setting: "stage1", env: nil, allowStage1: false))
        XCTAssertEqual(
            PresenterChoice.explicit(setting: "stage2", env: nil, allowStage1: true), .stage2)
        XCTAssertEqual(
            PresenterChoice.explicit(setting: nil, env: "stage3", allowStage1: true), .stage3)
    }

    // MARK: - Session pacing (the macOS PyroWave swapID-panic mitigation)

    /// macOS PyroWave sessions under the DEFAULT stage-2 choice must get glass pacing (the
    /// one-in-flight gate is the "mismatched swapID's" kernel-panic mitigation); an EXPLICIT
    /// stage-2 pick must stay a faithful arrival-pacing A/B. Elsewhere the default is unchanged.
    func testPacingDefaultsPyroWaveToGlassOnMacOS() {
        #if os(macOS)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: nil, codec: .pyrowave), .glass,
            "defaulted macOS PyroWave must serialize presents (swapID-panic mitigation)")
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: .stage2, codec: .pyrowave), .arrival,
            "an explicit stage-2 pick must keep arrival pacing (honest A/B)")
        #else
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: nil, codec: .pyrowave), .arrival)
        #endif
        // Non-PyroWave defaults keep arrival pacing under stage-2 everywhere.
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage2, explicit: nil, codec: .hevc), .arrival)
        // Stage-3 means glass regardless of codec or how it was chosen.
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage3, explicit: .stage3, codec: .hevc), .glass)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage3, explicit: nil, codec: .pyrowave), .glass)
    }
}
#endif
