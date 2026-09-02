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
// positions are differenced into relative mouse deltas. Contact and lift come from the
// surface's own touch report (`buttonA.isTouched`) where the remote has one, else from the
// exact-zero snap and a quiet gap. Handlers (not a poll) — the same in-session delivery
// GamepadCapture relies on.
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
    /// When the finger landed; nil while lifted. Set by the touch report where the remote has
    /// one, else by the first sample after a lift or a quiet gap.
    private var contactAt: Date?
    private var lastSampleAt = Date.distantPast
    /// `buttonA.touchedChangedHandler` has fired: lifts are explicit, and a sample with no
    /// contact is the release ramp — ignored.
    private var reportsTouch = false
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
    /// Motion inside this of contact is dropped. The framework ramps the reported position
    /// from the centre to the finger over the first callbacks of a touch — read as motion,
    /// every touch was a jump toward wherever the surface was touched.
    private static let contactSettle: TimeInterval = 0.06
    /// No sample for this long is a lift the remote never snapped to (0, 0) for.
    private static let quietGap: TimeInterval = 0.12

    /// Fired ON MAIN after Back/Menu was held ≥ `disconnectHold` and released.
    public var onDisconnectRequest: (() -> Void)?
    /// Fired ON MAIN after a SHORT Back/Menu press (released under `disconnectHold`): the
    /// quick-action ring's opener on tvOS (design/touch-client-overlay.md §2.5). A long hold
    /// still exits.
    public var onShortBack: (() -> Void)?
    /// A remote gesture while the ring owns the remote (`ringOpen`): a swipe steps the
    /// highlight, a click fires it, Play/Pause recentres, a short Back closes.
    public var onRingNav: ((RingNav) -> Void)?
    /// The ring is up: gestures drive it (`onRingNav`) and nothing reaches the host — a click
    /// already sent down is lifted now, GamepadCapture's own `ringOpen` rule.
    public var ringOpen = false {
        didSet {
            guard ringOpen != oldValue else { return }
            swipeAnchor = nil
            if ringOpen {
                cancelPlayPause()
                releaseHeld()
            }
        }
    }
    /// The finger position the next ring step is measured from; nil = lifted.
    private var swipeAnchor: (x: Float, y: Float)?
    /// Finger travel that steps the ring once (axes span ±1): about a quarter of the surface,
    /// so a held swipe walks several slots.
    private static let swipeStep: Float = 0.45

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
            old.buttonA.touchedChangedHandler = nil
            old.buttonX.pressedChangedHandler = nil
            old.buttonMenu.pressedChangedHandler = nil
        }
        // Timers first, then the lift: a tap whose release is still owed is held state, so
        // `releaseHeld` below is what sends its button-up.
        cancelPlayPause()
        releaseHeld()
        lastTouch = nil
        contactAt = nil
        reportsTouch = false
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
            MainActor.assumeIsolated { self?.clickChanged(pressed: pressed) }
        }
        micro.buttonA.touchedChangedHandler = { [weak self] _, _, _, touched in
            MainActor.assumeIsolated { self?.touchChanged(touched) }
        }
        micro.buttonX.pressedChangedHandler = { [weak self] _, _, pressed in
            MainActor.assumeIsolated { self?.playPauseChanged(pressed: pressed) }
        }
        micro.buttonMenu.pressedChangedHandler = { [weak self] _, _, pressed in
            MainActor.assumeIsolated { self?.menuChanged(pressed: pressed) }
        }
    }

    /// The surface's touch report: contact starts the settle, a lift ends the gesture at once
    /// — nothing after it counts, the release ramp included.
    private func touchChanged(_ touched: Bool) {
        reportsTouch = true
        lastTouch = nil
        swipeAnchor = nil
        contactAt = touched ? Date() : nil
    }

    private func touchMoved(x: Float, y: Float) {
        let now = Date()
        let quiet = now.timeIntervalSince(lastSampleAt) > Self.quietGap
        lastSampleAt = now
        // Exact (0, 0) is the lift snap; with a touch report, a sample after the lift is the
        // release ramp. Either way drop the anchor so the next touch starts fresh.
        guard x != 0 || y != 0, contactAt != nil || !reportsTouch else {
            lastTouch = nil
            swipeAnchor = nil
            if !reportsTouch { contactAt = nil }
            return
        }
        defer { lastTouch = (x, y) }
        // First contact — or the first sample after a quiet gap, a lift the remote never
        // snapped for — anchors and moves nothing.
        guard let last = lastTouch, !quiet else {
            if contactAt == nil || quiet { contactAt = now }
            swipeAnchor = (x, y)
            return
        }
        // Inside the settle the anchor follows the ramp; the diff starts where it ends.
        if let contactAt, now.timeIntervalSince(contactAt) < Self.contactSettle {
            swipeAnchor = (x, y)
            return
        }
        let stepX = x - last.x
        let stepY = y - last.y
        // The release tail (and any tracking glitch) shows up as a single impossible jump —
        // see `maxStep`. Skip the emission; the deferred anchor update above still follows the
        // reported position, so the gesture cleanly re-anchors instead of retracing.
        guard abs(stepX) < Self.maxStep, abs(stepY) < Self.maxStep else {
            swipeAnchor = (x, y)
            return
        }
        if ringOpen { return ringSwipe(x: x, y: y) }
        let dx = stepX * Self.pointerScale / 2 // axes span ±1 → full swipe = 2.0
        let dy = -stepY * Self.pointerScale / 2 // GC +y is up; mouse +y is down
        let ix = Int32(dx.rounded())
        let iy = Int32(dy.rounded())
        guard ix != 0 || iy != 0 else { return }
        connection.send(.mouseMove(dx: ix, dy: iy))
    }

    /// A ring step per `swipeStep` of travel from the anchor, along the dominant axis; the
    /// anchor moves with each step so a long swipe keeps walking.
    private func ringSwipe(x: Float, y: Float) {
        guard let a = swipeAnchor else {
            swipeAnchor = (x, y)
            return
        }
        let dx = x - a.x
        let dy = y - a.y
        guard abs(dx) >= Self.swipeStep || abs(dy) >= Self.swipeStep else { return }
        swipeAnchor = (x, y)
        onRingNav?(abs(dx) >= abs(dy) ? (dx > 0 ? .right : .left) : (dy > 0 ? .up : .down))
    }

    /// Surface click: the left button — or, with the ring up, its confirm. A release after the
    /// ring closed lifts a button the host never saw down, which is harmless.
    private func clickChanged(pressed: Bool) {
        if ringOpen {
            if pressed { onRingNav?(.confirm) }
            return
        }
        setButton(1, down: pressed)
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
        // With the ring up the spare button is the pad's Y: back to the centre.
        if ringOpen {
            if pressed { onRingNav?(.centre) }
            return
        }
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
        } else if ringOpen {
            // Back inside the ring is the pad's B: out of the sheet, else close.
            onRingNav?(.back)
        } else {
            // A short press opens the quick-action ring. It is never forwarded as a host key —
            // that would make trackpad fumbles type — and the accompanying UIKit menu press is
            // swallowed in ContentView.
            onShortBack?()
        }
    }

    private func releaseHeld() {
        for button in heldButtons {
            connection.send(.mouseButton(button, down: false))
        }
        heldButtons.removeAll()
    }
}
#endif
