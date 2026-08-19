#if os(macOS)
import AppKit
import XCTest

@testable import PunktfunkKit

/// Pins the macOS ⌘-chord passthrough — the rule deciding which keyDowns `InputCapture`'s local
/// monitor takes off AppKit and forwards to the host instead of letting a menu key equivalent
/// claim them. Two things are worth a test rather than a comment:
///
///  * ⌘Q reaching the host at all. That is the whole point — it is the compositor chord on
///    Hyprland/KDE/GNOME, and it used to quit the client.
///  * ⌘⎋ and ⌃⌘F NOT reaching it, under every combination. They are the way out of a captured
///    stream; forward either and the user is locked in.
final class CommandChordTests: XCTestCase {
    // kVK_ANSI_* — physical positions, layout-independent (the same constants the monitor uses).
    private let q: UInt16 = 12, w: UInt16 = 13, h: UInt16 = 4, m: UInt16 = 46
    private let f: UInt16 = 3, esc: UInt16 = 53, leftArrow: UInt16 = 123

    /// Captured, setting on, capture mouse model — the shipping default.
    private func forwards(
        _ keyCode: UInt16, _ flags: NSEvent.ModifierFlags,
        forwarding: Bool = true, inhibit: Bool = true, desktop: Bool = false
    ) -> Bool {
        InputCapture.forwardsCommandChord(
            keyCode: keyCode, flags: flags, forwarding: forwarding,
            inhibitShortcuts: inhibit, desktopMouse: desktop)
    }

    func testCommandChordsGoToTheHostWhileCaptured() {
        XCTAssertTrue(forwards(q, .command)) // ⌘Q — the reported break
        XCTAssertTrue(forwards(w, .command))
        XCTAssertTrue(forwards(h, .command))
        XCTAssertTrue(forwards(m, .command))
        XCTAssertTrue(forwards(q, [.command, .shift])) // ⇧⌘Q
        XCTAssertTrue(forwards(m, [.command, .control, .option, .shift]))
    }

    func testTheEscapeHatchesAreNeverForwarded() {
        // ⌘⎋ releases capture, ⌃⌘F leaves fullscreen. Neither may ever reach the host.
        XCTAssertFalse(forwards(esc, .command))
        XCTAssertFalse(forwards(f, [.control, .command]))
        XCTAssertTrue(InputCapture.isClientReservedChord(keyCode: esc, flags: .command))
        XCTAssertTrue(
            InputCapture.isClientReservedChord(keyCode: f, flags: [.control, .command]))
    }

    /// The reservation is exact: it is ⌘⎋ and ⌃⌘F specifically, not "anything with Esc or F in
    /// it". ⇧⌘⎋ and ⌘F are the host's like any other chord.
    func testNeighbouringChordsAreNotReserved() {
        XCTAssertTrue(forwards(esc, [.command, .shift]))
        XCTAssertTrue(forwards(f, .command))
        XCTAssertFalse(InputCapture.isClientReservedChord(keyCode: f, flags: .command))
    }

    func testNothingWithoutCommandIsClaimedHere() {
        // The ⌃⌥⇧ family and bare keys reach the monitor's earlier blocks / the responder chain.
        XCTAssertFalse(forwards(q, [.control, .option, .shift]))
        XCTAssertFalse(forwards(q, []))
        XCTAssertFalse(forwards(esc, []))
    }

    func testReleasedCaptureLeavesTheMenuAlone() {
        // Not forwarding = the user is in the local UI: ⌘Q must quit the app, ⌘W close the window.
        XCTAssertFalse(forwards(q, .command, forwarding: false))
        XCTAssertFalse(forwards(w, .command, forwarding: false))
    }

    func testTheCrossClientSettingTurnsItOff() {
        XCTAssertFalse(forwards(q, .command, inhibit: false))
    }

    func testTheDesktopMouseModelKeepsChordsLocal() {
        // Matches the SDL clients' keyboard grab: a remote desktop is something you ⌘Tab away from.
        XCTAssertFalse(forwards(q, .command, desktop: true))
        XCTAssertFalse(forwards(q, .command, inhibit: true, desktop: true))
    }

    /// `deviceIndependentFlagsMask` also carries Caps Lock and the `.function`/`.numericPad` bits
    /// every arrow key sets, so comparing it for equality made chords stop being recognized in
    /// exactly the states a user does not connect to their keyboard: Caps Lock on, or the chord
    /// spelled with an arrow. `chordFlags` isolates the four real modifiers.
    func testCapsLockAndArrowBitsDoNotChangeAChord() throws {
        let capsQ = try XCTUnwrap(keyEvent(q, [.command, .capsLock]))
        XCTAssertEqual(InputCapture.chordFlags(capsQ), .command)
        XCTAssertTrue(forwards(q, InputCapture.chordFlags(capsQ)))

        // ⌘⎋ with Caps Lock on is still the escape hatch, not a chord for the host.
        let capsEsc = try XCTUnwrap(keyEvent(esc, [.command, .capsLock]))
        XCTAssertEqual(InputCapture.chordFlags(capsEsc), .command)
        XCTAssertFalse(forwards(esc, InputCapture.chordFlags(capsEsc)))

        // ⌘← — arrows set .function|.numericPad, which say nothing about the chord.
        let cmdLeft = try XCTUnwrap(keyEvent(leftArrow, [.command, .function, .numericPad]))
        XCTAssertEqual(InputCapture.chordFlags(cmdLeft), .command)
        XCTAssertTrue(forwards(leftArrow, InputCapture.chordFlags(cmdLeft)))
    }

    /// A forwarded chord is only useful if the key has a host VK — the monitor swallows either
    /// way, so an unmapped one would silently do nothing. Spot-check the common ⌘ letters.
    func testTheCommonChordKeysMapToHostVKs() {
        XCTAssertEqual(InputCapture.keyCodeToVK[q], 0x51) // VK 'Q'
        XCTAssertEqual(InputCapture.keyCodeToVK[w], 0x57) // VK 'W'
        XCTAssertEqual(InputCapture.keyCodeToVK[h], 0x48) // VK 'H'
        XCTAssertEqual(InputCapture.keyCodeToVK[m], 0x4D) // VK 'M'
        XCTAssertEqual(InputCapture.keyCodeToVK[leftArrow], 0x25) // VK_LEFT
    }

    /// The system-shortcut tap (⌘Space, ⌘Tab — the keys macOS claims before the app sees them)
    /// takes keys off the system ONLY while captured, under the capture mouse model, with the app
    /// frontmost. Any other state must pass through: a tap that eats keys for the whole Mac is the
    /// failure to pin here.
    func testTheSystemShortcutTapOnlyClaimsWhileCapturedAndFrontmost() {
        XCTAssertTrue(InputCapture.tapClaims(forwarding: true, desktopMouse: false, appActive: true))
        XCTAssertFalse(InputCapture.tapClaims(forwarding: false, desktopMouse: false, appActive: true))
        XCTAssertFalse(InputCapture.tapClaims(forwarding: true, desktopMouse: true, appActive: true))
        XCTAssertFalse(InputCapture.tapClaims(forwarding: true, desktopMouse: false, appActive: false))
    }

    /// The keys the tap exists for must have host VKs — it reposts them into the ordinary key path,
    /// which drops unmapped keyCodes on the floor.
    func testTheSystemShortcutKeysMapToHostVKs() {
        XCTAssertEqual(InputCapture.keyCodeToVK[49], 0x20) // Space (⌘Space)
        XCTAssertEqual(InputCapture.keyCodeToVK[48], 0x09) // Tab (⌘Tab)
        XCTAssertEqual(InputCapture.keyCodeToVK[126], 0x26) // Up arrow (⌃↑ Mission Control)
    }

    private func keyEvent(_ keyCode: UInt16, _ flags: NSEvent.ModifierFlags) -> NSEvent? {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 0,
            windowNumber: 0, context: nil, characters: "", charactersIgnoringModifiers: "",
            isARepeat: false, keyCode: keyCode)
    }
}
#endif
