import GameController
import XCTest

@testable import PunktfunkKit

/// The escape chord's mask and its GameController alias list have to describe the same four
/// buttons. `GamepadCapture.openSlot` claims the system gesture of every element while forwarding
/// is on, but only of `escapeChordElements` while it is off — so if the alias list ever stops
/// covering the mask, the missing button's press stays the system's and the chord never completes.
///
/// That matters most on tvOS, where this chord is the only controller way out of a stream: the
/// symptom is a session nobody can leave with the pad in their hands, and nothing logs or crashes.
/// Hence a test on the invariant rather than trusting the comment beside it.
@MainActor
final class GamepadEscapeChordTests: XCTestCase {

    /// The intended alias↔bit pairing, spelled out independently of the implementation.
    private let pairing: [(alias: String, bit: UInt32)] = [
        (GCInputLeftShoulder, GamepadWire.leftShoulder),
        (GCInputRightShoulder, GamepadWire.rightShoulder),
        (GCInputButtonMenu, GamepadWire.start),
        (GCInputButtonOptions, GamepadWire.back),
    ]

    func testChordMaskIsExactlyTheFourPairedButtons() {
        XCTAssertEqual(
            pairing.reduce(UInt32(0)) { $0 | $1.bit },
            GamepadCapture.escapeChord,
            "the chord mask and the alias pairing describe different buttons")
    }

    func testEveryChordBitHasAnElementToClaim() {
        // One alias per bit — a mask that grew a fifth button without a matching alias would
        // leave that button's gesture with the OS while forwarding is off.
        XCTAssertEqual(
            GamepadCapture.escapeChordElements.count,
            GamepadCapture.escapeChord.nonzeroBitCount,
            "alias list and chord mask differ in size")
        XCTAssertEqual(GamepadCapture.escapeChordElements, pairing.map(\.alias))
    }

    /// The claim list is a strict subset of what a forwarding slot takes — it is a NARROWING of
    /// the full sweep, never an extra grab, and it must not be empty (that would be "skip", which
    /// is the behaviour this deliberately avoids).
    func testClaimListIsNonEmptyAndAllDistinct() {
        XCTAssertFalse(GamepadCapture.escapeChordElements.isEmpty)
        XCTAssertEqual(
            Set(GamepadCapture.escapeChordElements).count,
            GamepadCapture.escapeChordElements.count,
            "a repeated alias would mean a chord bit has no element")
    }
}
