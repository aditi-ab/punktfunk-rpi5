import GameController
import XCTest

@testable import PunktfunkKit

/// The stats chord (Select + X) has the same drift hazard as the escape chord it sits beside: its
/// mask and its GameController alias list must describe the same buttons, and every element some
/// chord reads has to appear in the list a NON-forwarding slot claims — otherwise that button's
/// press stays the system's and the chord silently never completes.
///
/// It matters most on tvOS, where this is the only way to the statistics overlay at all (no
/// keyboard for ⌃⌥⇧S, no touchscreen for the three-finger tap). The failure looks like nothing
/// happening, so it is pinned here rather than left to the comments.
@MainActor
final class GamepadStatsChordTests: XCTestCase {

    /// The intended alias↔bit pairing, spelled out independently of the implementation.
    private let pairing: [(alias: String, bit: UInt32)] = [
        (GCInputButtonOptions, GamepadWire.back),
        (GCInputButtonX, GamepadWire.x),
    ]

    func testChordMaskIsExactlyTheTwoPairedButtons() {
        XCTAssertEqual(
            pairing.reduce(UInt32(0)) { $0 | $1.bit },
            GamepadCapture.statsChord,
            "the chord mask and the alias pairing describe different buttons")
    }

    func testAliasListMirrorsTheMask() {
        XCTAssertEqual(
            GamepadCapture.statsChordElements.count,
            GamepadCapture.statsChord.nonzeroBitCount,
            "alias list and chord mask differ in size")
        XCTAssertEqual(GamepadCapture.statsChordElements, pairing.map(\.alias))
    }

    /// The two chords must not be reachable through one another: pressing toward the exit chord
    /// may not cycle the overlay on the way, and holding the stats chord may not arm a disconnect.
    /// Select is the one button they share by design — everything else has to be disjoint.
    func testChordsOverlapOnlyOnSelect() {
        XCTAssertEqual(
            GamepadCapture.statsChord & GamepadCapture.escapeChord,
            GamepadWire.back,
            "the stats and escape chords share a button other than Select")
        // Neither is a subset of the other, so completing one can never complete the other.
        XCTAssertNotEqual(
            GamepadCapture.statsChord & GamepadCapture.escapeChord, GamepadCapture.statsChord)
        XCTAssertNotEqual(
            GamepadCapture.statsChord & GamepadCapture.escapeChord, GamepadCapture.escapeChord)
    }

    /// `chordElements` is what `openSlot` claims when forwarding is OFF. It must cover BOTH
    /// chords' aliases and repeat none of them (a duplicate would mean a bit with no element).
    func testClaimListCoversBothChordsWithoutDuplicates() {
        let claim = GamepadCapture.chordElements
        for alias in GamepadCapture.escapeChordElements + GamepadCapture.statsChordElements {
            XCTAssertTrue(claim.contains(alias), "\(alias) is read by a chord but never claimed")
        }
        XCTAssertEqual(Set(claim).count, claim.count, "a repeated alias in the claim list")
        // Shared Select means the union is one shorter than the two lists laid end to end.
        XCTAssertEqual(
            claim.count,
            GamepadCapture.escapeChordElements.count + GamepadCapture.statsChordElements.count - 1)
    }

    /// A cycle is a pure rotation through the four tiers — the chord fires `StatsVerbosity.cycle`,
    /// and a tier that dead-ended would strand a tvOS user with no other way back.
    func testCycleReachesEveryTierAndReturns() {
        var tier = StatsVerbosity.off
        var seen: [StatsVerbosity] = []
        for _ in 0..<StatsVerbosity.allCases.count {
            seen.append(tier)
            tier = tier.next()
        }
        XCTAssertEqual(Set(seen).count, StatsVerbosity.allCases.count, "a tier is unreachable")
        XCTAssertEqual(tier, .off, "the cycle does not return to where it started")
    }
}
