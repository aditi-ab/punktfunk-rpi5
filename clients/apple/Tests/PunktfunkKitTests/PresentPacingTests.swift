import XCTest

#if canImport(Metal)
import QuartzCore
@testable import PunktfunkKit

/// Present pacing: the stage-3 bounded in-flight `PresentGate`, the stage-4 `LatestBox`
/// drawable hand-off, the stage-1/2/3/4 `PresenterChoice` resolution (setting +
/// PUNKTFUNK_PRESENTER env override + the release-build stage-1 gate + the iOS/tvOS-only
/// stage-4 gate), and the per-platform glass-gate depth.
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

    /// Depth 2 (the PUNKTFUNK_GATE_DEPTH ladder rung — no longer a default; see
    /// `SessionPresenter.gateDepth`'s standing-queue post-mortem): a second present may queue
    /// behind the flip scanning out — the bound only bites at the THIRD. One release (a glass
    /// callback) reopens exactly one slot.
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

    // MARK: - LatestBox (stage-4's drawable hand-off)

    /// Newest-wins hand-off: `put` replaces (an unpresented older drawable returns to the
    /// layer's pool by release), `take` empties the slot.
    func testLatestBoxNewestWins() {
        let box = LatestBox<Int>()
        XCTAssertNil(box.take())
        box.put(1)
        box.put(2)
        XCTAssertEqual(box.take(), 2, "a fresher put replaces the unpresented value")
        XCTAssertNil(box.take(), "take empties the slot")
    }

    /// `putBack` fills only an EMPTY slot: the render thread returning a drawable it took but
    /// didn't present must never clobber a fresher one the link vended in between.
    func testLatestBoxPutBackNeverClobbersAFresherPut() {
        let box = LatestBox<Int>()
        box.put(1)
        let stale = box.take()
        XCTAssertEqual(stale, 1)
        box.putBack(stale!)
        XCTAssertEqual(box.take(), 1, "putBack into a still-empty slot restores the value")
        box.put(2)
        let taken = box.take()
        box.put(3) // the link vends a fresher drawable while the render thread holds `taken`
        box.putBack(taken!)
        XCTAssertEqual(box.take(), 3, "the fresher vend wins over the stale return")
    }

    // MARK: - PresenterChoice

    /// The platform default: deadline-paced stage-4 on iOS/iPadOS (the vsync-latching platform
    /// where any bounded-FIFO pacing keeps a standing queue), glass stage-3 on tvOS (proven —
    /// stage-4 A/Bs first), arrival stage-2 on macOS (sync-off presents don't queue). No
    /// selection / garbage falls back to it.
    func testPresenterChoiceFallsBackToPlatformDefault() {
        #if os(iOS)
        XCTAssertEqual(PresenterChoice.platformDefault, .stage4)
        #elseif os(tvOS)
        XCTAssertEqual(PresenterChoice.platformDefault, .stage3)
        #else
        XCTAssertEqual(PresenterChoice.platformDefault, .stage2)
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

    /// "stage4" (deadline pacing) resolves only on iOS/tvOS. On macOS — whose present path is
    /// entangled with the sync-off/DCP-panic saga — a synced-over "stage4" value maps back to
    /// the platform default instead of engaging an unvalidated pacing.
    func testPresenterChoiceGatesStage4ToVsyncLatchPlatforms() {
        #if os(macOS)
        XCTAssertNil(PresenterChoice.explicit(setting: "stage4", env: nil, allowStage1: true))
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage4", env: nil, allowStage1: true), .stage2)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage4", allowStage1: true), .stage2)
        #else
        XCTAssertEqual(
            PresenterChoice.explicit(setting: "stage4", env: nil, allowStage1: true), .stage4)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: nil, env: "stage4", allowStage1: true), .stage4)
        // The env override wins over the persisted setting, both directions.
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage4", env: "stage3", allowStage1: true), .stage3)
        XCTAssertEqual(
            PresenterChoice.resolve(setting: "stage2", env: "stage4", allowStage1: true), .stage4)
        #endif
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
        // Stage-4 means deadline regardless of codec or how it was chosen.
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage4, explicit: nil, codec: .hevc), .deadline)
        XCTAssertEqual(
            SessionPresenter.pacing(for: .stage4, explicit: .stage4, codec: .pyrowave), .deadline)
    }

    // MARK: - Glass-gate depth

    /// The in-flight present budget is 1 EVERYWHERE: any deeper gate keeps a standing queue —
    /// the 2026-07 iPad depth-2 experiment regressed display latency 14→22–28 ms (see
    /// `SessionPresenter.gateDepth`'s post-mortem). macOS additionally pins the env lever (glass
    /// there is the swapID-panic mitigation — strict serialization is its point);
    /// PUNKTFUNK_GATE_DEPTH still reproduces the standing-queue ladder on iOS/tvOS.
    /// Out-of-range/garbage values are ignored.
    func testGateDepthPlatformDefaultsAndEnvOverride() {
        #if os(macOS)
        XCTAssertEqual(SessionPresenter.gateDepth(env: nil), 1)
        XCTAssertEqual(SessionPresenter.gateDepth(env: "2"), 1, "macOS is pinned to 1")
        #else
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: nil), 1,
            "any depth >1 is a standing queue — one refresh of display latency per slot")
        XCTAssertEqual(SessionPresenter.gateDepth(env: "2"), 2, "the on-device ladder lever")
        #endif
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: "0"), SessionPresenter.gateDepth(env: nil),
            "out-of-range env values fall back to the platform depth")
        XCTAssertEqual(
            SessionPresenter.gateDepth(env: "garbage"), SessionPresenter.gateDepth(env: nil))
    }
}
#endif
