import XCTest

@testable import PunktfunkKit

/// Pins the rumble renderer's pure scheduling/mapping decisions and the relations between its
/// tuning constants that the design depends on (see `RumbleRenderer`'s invariants). No
/// CHHapticEngine or physical pad involved.
final class RumbleTuningTests: XCTestCase {
    func testAmplitudeMapsWireRangeToUnitInterval() {
        XCTAssertEqual(RumbleTuning.amplitude(0), 0)
        XCTAssertEqual(RumbleTuning.amplitude(0xFFFF), 1)
        XCTAssertEqual(RumbleTuning.amplitude(0x8000), Float(0x8000) / 65535, accuracy: 1e-6)
        // Monotonic — a stronger wire value can never render weaker.
        XCTAssertLessThan(RumbleTuning.amplitude(0x1000), RumbleTuning.amplitude(0x2000))
    }

    func testHidByteMapsWireRangeToPadRange() {
        XCTAssertEqual(RumbleTuning.hidByte(0), 0)
        XCTAssertEqual(RumbleTuning.hidByte(0xFFFF), 255)
        XCTAssertEqual(RumbleTuning.hidByte(0x8000), 0x80)
    }

    func testCombinedActuatorRendersStrongerMotor() {
        XCTAssertEqual(RumbleTuning.combined(low: 0x4000, high: 0x8000), 0x8000)
        XCTAssertEqual(RumbleTuning.combined(low: 0x8000, high: 0x4000), 0x8000)
        XCTAssertEqual(RumbleTuning.combined(low: 0, high: 0), 0)
    }

    func testLevelDedupeEpsilon() {
        // An identical host refresh (and LSB jitter) is the same level — no player rebuild.
        XCTAssertTrue(RumbleTuning.sameLevel(0.5, 0.5))
        XCTAssertTrue(RumbleTuning.sameLevel(0.5, 0.5 + RumbleTuning.levelEpsilon))
        // A real level change is not.
        XCTAssertFalse(RumbleTuning.sameLevel(0.5, 0.5 + RumbleTuning.levelEpsilon * 3))
        XCTAssertFalse(RumbleTuning.sameLevel(0, 1))
    }

    func testRearmDecision() {
        let ends: TimeInterval = 100
        XCTAssertFalse(
            RumbleTuning.shouldRearm(endsAt: ends, now: ends - RumbleTuning.rearmHeadroom - 0.1))
        XCTAssertTrue(
            RumbleTuning.shouldRearm(endsAt: ends, now: ends - RumbleTuning.rearmHeadroom + 0.1))
        // Even a segment already past its end re-arms (the gap already happened; recover).
        XCTAssertTrue(RumbleTuning.shouldRearm(endsAt: ends, now: ends + 1))
    }

    func testHandoffStartsAtSegmentEndNeverInThePast() {
        // Successor starts exactly at the predecessor's end...
        XCTAssertEqual(RumbleTuning.handoffStart(endsAt: 100, now: 99.5), 100)
        // ...unless that instant already passed — then start immediately, not in the past.
        XCTAssertEqual(RumbleTuning.handoffStart(endsAt: 100, now: 100.5), 100.5)
    }

    func testPolicies() {
        // The session policy ties motor life to wire liveness; the manual (test-panel) policy
        // holds a level indefinitely.
        XCTAssertNotNil(RumbleRenderer.Policy.session.staleAfter)
        XCTAssertNil(RumbleRenderer.Policy.manual.staleAfter)
    }

    /// Exercise the renderer's queue/ticker machinery without a physical pad: a wire-rate call
    /// storm, an audible target left to the ticker (watchdog path), then `stop()` — which runs
    /// `queue.sync` against the same serial queue the ticker fires on and must not deadlock.
    func testRendererSurvivesCallStormAndTeardownWithoutController() {
        let renderer = RumbleRenderer(policy: .session)
        renderer.retarget(nil)
        for i in 0..<500 {
            renderer.apply(
                low: i % 2 == 0 ? 0x8000 : 0, high: UInt16(truncatingIfNeeded: i &* 37))
        }
        // Leave a nonzero target long enough for the ticker to spin a few times.
        renderer.apply(low: 0x4000, high: 0x4000)
        Thread.sleep(forTimeInterval: 0.2)
        renderer.stop()
    }

    func testLeaseSecondsInterpretsWireTTL() {
        // The legacy no-lease sentinel → nil (fall back to the staleness watchdog).
        XCTAssertNil(RumbleTuning.leaseSeconds(ttlMs: RumbleTuning.noTTL))
        XCTAssertEqual(RumbleTuning.noTTL, UInt32.max)
        // A real lease → its duration in seconds (non-nil for any ttl != noTTL).
        XCTAssertEqual(RumbleTuning.leaseSeconds(ttlMs: 400) ?? .nan, 0.4, accuracy: 1e-9)
        XCTAssertEqual(RumbleTuning.leaseSeconds(ttlMs: 0) ?? .nan, 0, accuracy: 1e-9)
        XCTAssertEqual(RumbleTuning.leaseSeconds(ttlMs: 150) ?? .nan, 0.15, accuracy: 1e-9)
    }

    func testEnvelopeLeaseBoundsMotorLifeTighterThanTheLegacyWatchdog() {
        // The whole point of v2: a host-supplied lease silences the motor faster than the
        // legacy staleness watchdog ever could (which needs sessionStaleSeconds of silence). The
        // default 400 ms TTL is well under that, on every platform.
        let defaultTTL = RumbleTuning.leaseSeconds(ttlMs: 400)
        XCTAssertNotNil(defaultTTL)
        XCTAssertLessThan(defaultTTL!, RumbleTuning.sessionStaleSeconds)
        // The ticker must be able to observe an expired lease promptly (well within one TTL).
        XCTAssertLessThan(RumbleTuning.tickSeconds, defaultTTL!)
    }

    /// A v2 envelope with a short TTL, left unrenewed, must self-silence — the renderer's core
    /// promise. Drive the real queue/ticker (no physical pad) and confirm it doesn't wedge.
    func testEnvelopeExpiresWhenUnrenewed() {
        let renderer = RumbleRenderer(policy: .session)
        renderer.retarget(nil)
        // A 100 ms lease, then no renewal — the ticker (50 ms) must silence it on its own.
        renderer.apply(low: 0x8000, high: 0x8000, ttlMs: 100)
        Thread.sleep(forTimeInterval: 0.3)
        // No assertion on private state; this exercises the expiry path + serial-queue teardown
        // without deadlock (the ticker fires on the same queue stop() sync-hops onto).
        renderer.stop()
    }

    func testTuningRelationsTheDesignDependsOn() {
        // The watchdog must tolerate a couple of lost 500 ms host refreshes (heals, not gaps)
        // but trip well before a stuck rumble reads as "still going".
        XCTAssertGreaterThan(RumbleTuning.sessionStaleSeconds, 2 * 0.5)
        XCTAssertLessThanOrEqual(RumbleTuning.sessionStaleSeconds, 2.5)
        // Re-arm headroom must clear several ticker periods, or a steady rumble could miss the
        // segment boundary and gap.
        XCTAssertGreaterThanOrEqual(
            RumbleTuning.rearmHeadroom, 4 * RumbleTuning.tickSeconds)
        // The headroom must fit inside a segment, or re-arm would trigger instantly forever.
        XCTAssertLessThan(RumbleTuning.rearmHeadroom, RumbleTuning.segmentSeconds)
        // The rebake throttle must be far under the host refresh period, or refreshed level
        // changes would queue behind it; and under a frame at 30 fps so ramps stay smooth.
        XCTAssertLessThan(RumbleTuning.minRebakeSeconds, 1.0 / 30)
        // The ticker (which lands throttled levels) must outpace the HID keepalive and the
        // watchdog, or those deadlines could be overshot by a full period.
        XCTAssertLessThan(RumbleTuning.tickSeconds, RumbleTuning.hidKeepaliveSeconds)
        XCTAssertLessThan(RumbleTuning.tickSeconds, RumbleTuning.sessionStaleSeconds)
    }
}
