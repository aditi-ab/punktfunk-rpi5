// The Siri Remote as a pointing device during a tvOS streaming session — the remote's touch
// surface drives the HOST cursor (relative deltas, like a laptop trackpad), a surface press
// clicks (left button), and Play/Pause right-clicks. It also owns the remote's DELIBERATE
// session exit: hold Back/Menu ≥ `disconnectHold`. A short Back press does nothing — the
// UIKit menu press it also generates is swallowed by ContentView's session branch, so neither
// a trackpad fumble nor a game-controller B press can end the session (the pad's exit is the
// L1+R1+Start+Select chord in GamepadCapture).
//
// The remote is read through GameController as a GCMicroGamepad with
// `reportsAbsoluteDpadValues = true`: the dpad axes then report the finger's ABSOLUTE position
// on the surface (±1, +y up) while touched, and snap to exactly (0, 0) on lift. Successive
// positions are differenced into relative mouse deltas; the exact-zero snap is treated as a
// lift (a real touch at the mathematical centre is measure-zero, and one dropped delta there
// is imperceptible). Handlers (not a poll) — the same in-session delivery GamepadCapture
// relies on.
//
// Lifecycle mirrors GamepadCapture: started by SessionModel when streaming begins (never
// during the trust prompt), stopped on disconnect; held buttons are released on stop so the
// host never keeps a stuck click.

#if os(tvOS)
import Foundation
import GameController
import UIKit

@MainActor
public final class SiriRemotePointer {
    private let connection: PunktfunkConnection
    private var observers: [NSObjectProtocol] = []
    private var bound: GCController?
    /// Finger position (±1 axes) at the last dpad callback while touched; nil = lifted.
    private var lastTouch: (x: Float, y: Float)?
    /// Wire buttons currently held (1 = left, 3 = right) — released on stop/unbind.
    private var heldButtons: Set<UInt32> = []
    /// When Back/Menu went down; a release after `disconnectHold` fires the exit.
    private var menuDownAt: Date?
    /// Counts a held Play/Pause down to `statsHold`; nil when the button is up or already
    /// resolved. See `playPauseChanged`.
    private var playPauseTimer: Timer?
    /// The held Play/Pause has already been spent on a stats cycle, so its release must not also
    /// right-click.
    private var statsHoldFired = false
    /// Trails a delivered right-click tap by `tapPress` to release it — see `deliverRightClick`.
    private var rightReleaseTimer: Timer?

    /// Hold Back/Menu at least this long (then release) to end the session. Shorter than the
    /// controller chord's 1.5 s — the remote has no way to trip this during gameplay.
    private static let disconnectHold: TimeInterval = 1.0
    /// Hold Play/Pause this long to cycle the stats overlay instead of right-clicking. It is the
    /// remote's only spare button, and on an Apple TV with no controller in the room this is the
    /// ONLY route to the numbers (⌃⌥⇧S wants a keyboard, the three-finger tap a touchscreen).
    /// Shorter than `disconnectHold`: nothing destructive rides on it.
    private static let statsHold: TimeInterval = 0.5
    /// pf-client-core's `TAP_PRESS`, borrowed for the deferred right-click: its release trails
    /// the press by this much, so the two transitions can't fold into nothing downstream.
    private static let tapPress: TimeInterval = 0.05
    /// A full edge-to-edge swipe moves the host cursor about this many pixels. The surface is
    /// small; two comfortable swipes should cross a 1080p desktop.
    private static let pointerScale: Float = 1100
    /// Largest single-callback finger travel accepted as real motion (surface units; the axes
    /// span ±1, so 0.4 ≈ a fifth of the pad). On RELEASE the hardware slides the reported
    /// position back to (0, 0) through intermediate callbacks — naive differencing turns that
    /// tail into reverse deltas that RETRACE the whole swipe, so the cursor springs back to its
    /// anchor and the pointer feels absolute. Real finger motion arrives as many small steps
    /// (even a fast flick stays well under this per callback); the release tail arrives as one
    /// or two huge jumps — discard those (the anchor still follows, so nothing accumulates).
    private static let maxStep: Float = 0.4

    /// Fired ON MAIN after Back/Menu was held ≥ `disconnectHold` and released.
    public var onDisconnectRequest: (() -> Void)?

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    public func start() {
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCControllerDidConnect, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.rebind() }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCControllerDidDisconnect, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.rebind() }
        })
        rebind()
    }

    public func stop() {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        bind(nil)
    }

    /// The Siri Remote is the non-extended controller carrying a microGamepad — a full gamepad
    /// (which also EXPOSES a microGamepad view of itself) must never be captured here, its
    /// buttons belong to GamepadCapture.
    private func rebind() {
        let remote = GCController.controllers().first {
            $0.extendedGamepad == nil && $0.microGamepad != nil
        }
        bind(remote)
    }

    private func bind(_ controller: GCController?) {
        guard controller !== bound else { return }
        if let old = bound?.microGamepad {
            old.dpad.valueChangedHandler = nil
            old.buttonA.pressedChangedHandler = nil
            old.buttonX.pressedChangedHandler = nil
            old.buttonMenu.pressedChangedHandler = nil
        }
        // Timers first, then the lift: a tap whose release is still owed is held state, so
        // `releaseHeld` below is what sends its button-up.
        cancelPlayPause()
        releaseHeld()
        lastTouch = nil
        menuDownAt = nil
        bound = controller
        guard let micro = controller?.microGamepad else { return }

        // Absolute finger position instead of the emulated dpad — the raw surface is what a
        // trackpad needs. Rotation stays off: the remote's natural grip is the coordinate frame.
        micro.reportsAbsoluteDpadValues = true
        micro.allowsRotation = false

        micro.dpad.valueChangedHandler = { [weak self] _, x, y in
            MainActor.assumeIsolated { self?.touchMoved(x: x, y: y) }
        }
        // Surface click = left button; Play/Pause = right (the remote's only spare face button),
        // or — held — the stats-overlay cycle. See `playPauseChanged`.
        micro.buttonA.pressedChangedHandler = { [weak self] _, _, pressed in
            MainActor.assumeIsolated { self?.setButton(1, down: pressed) }
        }
        micro.buttonX.pressedChangedHandler = { [weak self] _, _, pressed in
            MainActor.assumeIsolated { self?.playPauseChanged(pressed: pressed) }
        }
        micro.buttonMenu.pressedChangedHandler = { [weak self] _, _, pressed in
            MainActor.assumeIsolated { self?.menuChanged(pressed: pressed) }
        }
    }

    private func touchMoved(x: Float, y: Float) {
        // Exact (0, 0) is the lift snap — drop the anchor so the next touch starts a fresh
        // gesture instead of a jump-delta from the old position.
        guard x != 0 || y != 0 else {
            lastTouch = nil
            return
        }
        defer { lastTouch = (x, y) }
        guard let last = lastTouch else { return } // first contact anchors, moves nothing
        let stepX = x - last.x
        let stepY = y - last.y
        // The release tail (and any tracking glitch) shows up as a single impossible jump —
        // see `maxStep`. Skip the emission; the deferred anchor update above still follows the
        // reported position, so the gesture cleanly re-anchors instead of retracing.
        guard abs(stepX) < Self.maxStep, abs(stepY) < Self.maxStep else { return }
        let dx = stepX * Self.pointerScale / 2 // axes span ±1 → full swipe = 2.0
        let dy = -stepY * Self.pointerScale / 2 // GC +y is up; mouse +y is down
        let ix = Int32(dx.rounded())
        let iy = Int32(dy.rounded())
        guard ix != 0 || iy != 0 else { return }
        connection.send(.mouseMove(dx: ix, dy: iy))
    }

    private func setButton(_ button: UInt32, down: Bool) {
        if down { heldButtons.insert(button) } else { heldButtons.remove(button) }
        connection.send(.mouseButton(button, down: down))
    }

    /// Play/Pause: a TAP right-clicks, a HOLD (`statsHold`) cycles the stats overlay instead.
    ///
    /// The right button is therefore DEFERRED until the press resolves, rather than going down on
    /// contact: once the host has seen a button-down there is no taking it back, and a right
    /// button held for half a second is a context menu on every desktop this streams. The shape
    /// is the hold-Select gesture's (`GamepadCapture.gestureFiltered`) — suppress, then deliver a
    /// tap on release or the gesture past the threshold — so the two behave alike.
    private func playPauseChanged(pressed: Bool) {
        if pressed {
            statsHoldFired = false
            let timer = Timer(timeInterval: Self.statsHold, repeats: false) { [weak self] _ in
                Task { @MainActor in self?.statsHoldElapsed() }
            }
            RunLoop.main.add(timer, forMode: .common)
            playPauseTimer?.invalidate()
            playPauseTimer = timer
            return
        }
        playPauseTimer?.invalidate()
        playPauseTimer = nil
        // The hold already spent this press on a cycle — its release clicks nothing.
        guard !statsHoldFired else {
            statsHoldFired = false
            return
        }
        deliverRightClick()
    }

    /// The threshold passed with Play/Pause still down → cycle the overlay and consume the press.
    /// Writes the shared `statsVerbosity` default every reader observes through @AppStorage — the
    /// same cycle as ⌃⌥⇧S, the three-finger tap and the controller's Select + X.
    private func statsHoldElapsed() {
        playPauseTimer = nil
        statsHoldFired = true
        StatsVerbosity.cycle()
    }

    /// A Play/Pause tap, delivered now that it resolved as one: the right button down, its
    /// release `tapPress` behind so the pair can't collapse into nothing downstream.
    private func deliverRightClick() {
        // A previous tap's owed release goes out FIRST — two taps inside `tapPress` would
        // otherwise send the host two downs in a row (the rule GamepadCapture's held-back Select
        // tap follows for the same reason).
        finishRightClick()
        setButton(3, down: true)
        let timer = Timer(timeInterval: Self.tapPress, repeats: false) { [weak self] _ in
            Task { @MainActor in self?.finishRightClick() }
        }
        RunLoop.main.add(timer, forMode: .common)
        rightReleaseTimer = timer
    }

    /// Release a tap's right button if one is still owed; nothing otherwise.
    private func finishRightClick() {
        guard rightReleaseTimer != nil else { return }
        rightReleaseTimer?.invalidate()
        rightReleaseTimer = nil
        setButton(3, down: false)
    }

    /// Drop any in-flight Play/Pause state (unbind / stop). Timers only — a right button already
    /// sent down is held state, and `releaseHeld` is what lifts it.
    private func cancelPlayPause() {
        playPauseTimer?.invalidate()
        playPauseTimer = nil
        rightReleaseTimer?.invalidate()
        rightReleaseTimer = nil
        statsHoldFired = false
    }

    private func menuChanged(pressed: Bool) {
        if pressed {
            menuDownAt = Date()
            return
        }
        let heldFor = menuDownAt.map { Date().timeIntervalSince($0) } ?? 0
        menuDownAt = nil
        if heldFor >= Self.disconnectHold {
            onDisconnectRequest?()
        }
        // A short press is deliberately nothing: the accompanying UIKit menu press is swallowed
        // in ContentView, and forwarding it as a host key would make trackpad fumbles type.
    }

    private func releaseHeld() {
        for button in heldButtons {
            connection.send(.mouseButton(button, down: false))
        }
        heldButtons.removeAll()
    }
}
#endif
