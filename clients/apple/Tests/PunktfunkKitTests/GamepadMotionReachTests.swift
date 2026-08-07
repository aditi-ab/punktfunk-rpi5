// Whether a given pad's motion can reach the game. The Swift half of punktfunk-core's
// `pad_motion_reaches` — same rows as `config::tests::motion_reach_is_answered_per_pad_not_per_session`,
// because a client that disagrees with the host about this either kills a working gyro or keeps
// streaming ~250 Hz of samples nobody reads, and both failures are silent.

import PunktfunkCore
import XCTest

@testable import PunktfunkKit

final class GamepadMotionReachTests: XCTestCase {
    private typealias Pad = PunktfunkConnection.GamepadType

    func testOnlyTheXboxClassesLackAMotionPlane() {
        for kind: Pad in [.xbox360, .xboxOne] {
            XCTAssertFalse(kind.hasMotion, "\(kind) should have no motion plane")
        }
        for kind: Pad in [
            .dualSense, .dualShock4, .dualSenseEdge, .switchPro,
            .steamController, .steamDeck, .steamController2,
        ] {
            XCTAssertTrue(kind.hasMotion, "\(kind) should carry motion")
        }
        // Unknown must not suppress: an older host that omitted the echo may well have resolved a
        // DualSense, and silently killing its gyro is worse than sending into a void.
        XCTAssertTrue(Pad.auto.hasMotion)
    }

    /// The per-pad question, case by case. Each row is a session a player can actually sit down to;
    /// the comment says which of the three inputs decides it.
    func testMotionReachIsAnsweredPerPadNotPerSession() {
        // The case this predicate exists for, and the one a session-level check gets WRONG:
        // "Automatic" with mixed pads. The handshake carries the active pad's kind (an X-Box pad),
        // so the echo says X-Box 360 — but pad 1 declared a DualSense and the host built it one,
        // with a motion plane. Reading the echo here kills a gyro that works.
        XCTAssertTrue(Pad.motionReaches(declared: .dualSense, asked: .xbox360, resolved: .xbox360))
        // Its mirror: the pad that DID declare the X-Box kind still has nowhere to put motion.
        XCTAssertFalse(Pad.motionReaches(declared: .xbox360, asked: .xbox360, resolved: .xbox360))

        // An explicit Switch Pro against a WINDOWS host, which folds it to X-Box 360. Declared ==
        // asked, so the echo is this pad's answer and catches a fold nothing local could predict.
        XCTAssertFalse(
            Pad.motionReaches(declared: .switchPro, asked: .switchPro, resolved: .xbox360))
        // The same declaration against a Linux host that builds it: unchanged, motion reaches.
        XCTAssertTrue(
            Pad.motionReaches(declared: .switchPro, asked: .switchPro, resolved: .switchPro))

        // A DualSense wish on a host with no usable /dev/uhid degrades the same way.
        XCTAssertFalse(
            Pad.motionReaches(declared: .dualSense, asked: .dualSense, resolved: .xbox360))

        // Nobody connected at dial time, so the handshake asked `.auto` and the host resolved it
        // from its own env. A pad that shows up later declares its own kind and is judged on that.
        XCTAssertTrue(Pad.motionReaches(declared: .dualSense, asked: .auto, resolved: .xbox360))
        XCTAssertFalse(Pad.motionReaches(declared: .xbox360, asked: .auto, resolved: .dualSense))

        // An old host that echoes nothing leaves `.auto`, which must not suppress.
        XCTAssertTrue(Pad.motionReaches(declared: .dualSense, asked: .dualSense, resolved: .auto))
        // Even then the declaration still speaks when it is the thing without a plane.
        XCTAssertFalse(Pad.motionReaches(declared: .xbox360, asked: .dualSense, resolved: .auto))
    }
}
