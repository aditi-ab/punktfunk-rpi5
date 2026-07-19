import XCTest

#if canImport(Metal)
import QuartzCore
@testable import PunktfunkKit

/// Stage-3 present pacing: the bounded in-flight `PresentGate` (depth 1 + depth 2), the
/// stage-1/2/3 `PresenterChoice` resolution (setting + PUNKTFUNK_PRESENTER env override + the
/// release-build stage-1 gate), and the per-platform glass-gate depth.
final class PresentPacingTests: XCTestCase {
    // MARK: - PresentGate

    /// The depth-1 invariant: one present in flight. A second acquire while pending must fail
    /// (the frame stays in the ring for the presented handler's re-signal); release reopens.
    func testGateAdmitsOneInFlightPresent() {
        let gate = PresentGate()
        XCTAssertTrue(gate.tryAcquire(now: 0), "an idle gate must admit the first present")
        XCTAssertFalse(gate.tryAcquire(now: 0.001), "a pending present must close the gate")
        gate.release()
        XCTAssertTrue(gate.tryAcquire(now: 0.002), "release must reopen the gate")
        XCTAssertEqual(gate.drainForced(), 0, "no stale present was force-cleared")
    }

    /// Depth 2 (the iOS default): a second present may queue behind the flip scanning out — the
    /// bound only bites at the THIRD. One release (a glass callback) reopens exactly one slot.
    func testGateDepthTwoAdmitsTwoInFlightPresents() {
        let gate = PresentGate(capacity: 2)
        XCTAssertTrue(gate.tryAcquire(now: 0))
        XCTAssertTrue(gate.tryAcquire(now: 0.001), "depth 2 must admit a queued second flip")
        XCTAssertFalse(gate.tryAcquire(now: 0.002), "the third present must wait for glass")
        gate.release()
        XCTAssertTrue(gate.tryAcquire(now: 0.003), "one glass callback frees one slot")
        XCTAssertFalse(gate.tryAcquire(now: 0.004))
        XCTAssertEqual(gate.drainForced(), 0)
    }

    /// Depth 2 staleness anchors to the OLDEST in-flight present: a full gate stays closed while
    /// the oldest is live, force-opens once it ages out, and the younger present keeps its slot.
    func testGateDepthTwoForceOpensOnTheOldestStalePresent() {
        let gate = PresentGate(capacity: 2)
        XCTAssertTrue(gate.tryAcquire(now: 10))
        XCTAssertTrue(gate.tryAcquire(now: 10.05))
        XCTAssertFalse(gate.tryAcquire(now: 10 + PresentGate.staleAfter - 0.01))
        XCTAssertTrue(gate.tryAcquire(now: 10 + PresentGate.staleAfter + 0.01))
        XCTAssertEqual(gate.drainForced(), 1)
        // The 10.05 present is still live, so the gate is full again right after the force-open.
        XCTAssertFalse(gate.tryAcquire(now: 10 + PresentGate.staleAfter + 0.02))
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

    /// The platform default: glass-paced stage-3 where the layer always vsync-latches (iOS,
    /// tvOS — arrival pacing saturates the FIFO image queue there), arrival stage-2 on macOS
    /// (sync-off presents don't queue). No selection / garbage falls back to it.
    func testPresenterChoiceFallsBackToPlatformDefault() {
        #if os(macOS)
        XCTAssertEqual(PresenterChoice.platformDefault, .stage2)
        #else
        XCTAssertEqual(PresenterChoice.platformDefault, .stage3)
        #endif
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: nil, allowStage1: true),
            PresenterChoice.platformDefault)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "garbage", env: nil, allowStage1: true),
            PresenterChoice.platformDefault)
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
    /// builds); a leftover "stage1" value in a release build maps back to the platform default.
    func testPresenterChoiceGatesStage1() {
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage1", env: nil, allowStage1: true), .stage1)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage1", env: nil, allowStage1: false),
            PresenterChoice.platformDefault)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage1", allowStage1: false),
            PresenterChoice.platformDefault)
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

    // MARK: - Glass-gate depth

    /// The per-platform in-flight present budget: 2 on iOS/iPadOS (one flip scanning out + one
    /// queued — the display-latency fix), 1 on tvOS (proven config), 1 pinned on macOS (glass
    /// there is the swapID-panic mitigation — strict serialization is its point, so the env
    /// lever must not widen it). PUNKTFUNK_GATE_DEPTH A/Bs iOS/tvOS; out-of-range/garbage
    /// values are ignored.
    func testGateDepthPlatformDefaultsAndEnvOverride() {
        #if os(macOS)
        XCTAssertEqual(SessionPresenter.gateDepth(env: nil), 1)
        XCTAssertEqual(SessionPresenter.gateDepth(env: "2"), 1, "macOS is pinned to 1")
        #elseif os(tvOS)
        XCTAssertEqual(SessionPresenter.gateDepth(env: nil), 1)
        XCTAssertEqual(SessionPresenter.gateDepth(env: "2"), 2, "the on-device A/B lever")
        #else
        XCTAssertEqual(SessionPresenter.gateDepth(env: nil), 2)
        XCTAssertEqual(SessionPresenter.gateDepth(env: "1"), 1, "the on-device A/B lever")
        #endif
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: "0"), SessionPresenter.gateDepth(env: nil),
            "out-of-range env values fall back to the platform depth")
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: "garbage"), SessionPresenter.gateDepth(env: nil))
    }
}
#endif
