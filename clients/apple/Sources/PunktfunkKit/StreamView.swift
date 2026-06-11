// SwiftUI presentation: AVSampleBufferDisplayLayer fed straight from the punktfunk/1 connection.
//
// Stage-1 presenter (see README): the layer accepts *compressed* HEVC sample buffers and
// does hardware decode + display itself — fastest path to pixels, IOSurface-backed
// zero-copy on Apple silicon. Stage 2 (explicit VTDecompressionSession + CAMetalLayer)
// replaces this when we start tuning frame pacing / measuring glass-to-glass.
//
// The view also owns the input-capture state machine (Moonlight-style): capture is a
// deliberate, reversible state — engaged when the stream starts and when the user clicks
// into the video, released by ⌘⎋ or focus loss, and NEVER engaged by mere app
// activation (the click that activates the window may be a title-bar drag or a resize —
// warping the cursor there is exactly the intrusiveness this design removes). While
// released, nothing is forwarded to the host and the local cursor is free.
//
// macOS-first (NSViewRepresentable); the iOS variant is the same layer under
// UIViewRepresentable.

#if os(macOS)
import AppKit
import AVFoundation
import SwiftUI

/// Hides the LOCAL cursor while captured. The host renders its own cursor, and the local
/// one both diverges from it (the host applies acceleration/clamping to our raw deltas)
/// and can wander out of the window — a click there would focus another app. So while
/// captured we do what Moonlight does: warp the cursor into the view, freeze it
/// (`CGAssociateMouseAndMouseCursorPosition(false)` — GCMouse still delivers raw HID
/// deltas), and hide it. hide/unhide and associate are balanced via `captured`.
private final class CursorCapture {
    private var captured = false

    func capture(in view: NSView) {
        guard !captured, let window = view.window, view.bounds.width > 0 else { return }
        // Park the cursor mid-view so a click can't land in (and activate) another app.
        let rectOnScreen = window.convertToScreen(view.convert(view.bounds, to: nil))
        let primaryHeight = NSScreen.screens.first?.frame.height ?? 0
        CGWarpMouseCursorPosition(
            CGPoint(x: rectOnScreen.midX, y: primaryHeight - rectOnScreen.midY))
        CGAssociateMouseAndMouseCursorPosition(0)
        NSCursor.hide()
        captured = true
    }

    func release() {
        guard captured else { return }
        CGAssociateMouseAndMouseCursorPosition(1)
        NSCursor.unhide()
        captured = false
    }
}

public struct StreamView: NSViewRepresentable {
    private let connection: PunktfunkConnection
    private let captureEnabled: Bool
    private let onCaptureChange: ((Bool) -> Void)?
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?

    /// `onFrame`/`onSessionEnd` fire on the pump thread — hop to the main actor for UI.
    /// `captureEnabled: false` disables input capture entirely while UI (e.g. a trust
    /// prompt) is layered over the stream; flipping it to true auto-engages capture
    /// once. `onCaptureChange` (main thread) reports engage/release — drive the HUD's
    /// "click to capture" / "⌘⎋ releases" hint with it.
    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil
    ) {
        self.connection = connection
        self.captureEnabled = captureEnabled
        self.onCaptureChange = onCaptureChange
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
    }

    public func makeNSView(context: Context) -> StreamLayerView {
        let view = StreamLayerView()
        view.onCaptureChange = onCaptureChange
        view.captureEnabled = captureEnabled
        view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return view
    }

    public func updateNSView(_ view: StreamLayerView, context: Context) {
        view.onCaptureChange = onCaptureChange
        view.captureEnabled = captureEnabled
        // SwiftUI reuses the NSView across state changes — repoint the pump only when the
        // connection identity actually changed.
        if view.connection !== connection {
            view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        }
    }

    public static func dismantleNSView(_ view: StreamLayerView, coordinator: ()) {
        view.stop()
    }
}

public final class StreamLayerView: NSView {
    private let displayLayer = AVSampleBufferDisplayLayer()
    private var pump: StreamPump?
    public private(set) var connection: PunktfunkConnection?
    private let cursorCapture = CursorCapture()
    private var inputCapture: InputCapture?
    private var appObservers: [NSObjectProtocol] = []
    private var windowObservers: [NSObjectProtocol] = []

    /// Whether input capture is currently engaged (cursor hidden+frozen, mouse/keyboard
    /// forwarded). Main-thread only.
    public private(set) var captured = false
    /// One-shot auto-engage request (stream start, trust confirmed) — attempted as soon
    /// as the view is in a window with real bounds, then dropped, so it can never fire
    /// surprisingly later (e.g. on a resize).
    private var pendingAutoCapture = false

    /// Reports engage/release on the main thread.
    public var onCaptureChange: ((Bool) -> Void)?

    /// Main-thread only. False = input capture disabled outright (UI layered over the
    /// stream); flipping to true auto-engages once.
    public var captureEnabled = true {
        didSet {
            guard captureEnabled != oldValue else { return }
            if captureEnabled {
                requestAutoCapture()
            } else {
                releaseCapture()
            }
        }
    }

    public override init(frame: NSRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        layer = displayLayer // layer-hosting: assign before wantsLayer
        wantsLayer = true
        // Focus loss releases capture. Becoming active does NOT re-engage: the click
        // that activates the window may be on the title bar (a drag) or a resize edge —
        // the user clicks into the video (or hits ⌘⎋) when they want capture back.
        appObservers.append(NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.releaseCapture()
        })
    }

    public required init?(coder: NSCoder) { fatalError("not used") }

    public override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        windowObservers.forEach(NotificationCenter.default.removeObserver(_:))
        windowObservers.removeAll()
        guard let window else {
            releaseCapture()
            return
        }
        // ⌘-key-equivalents stay live while captured, so Settings (⌘,), a new window
        // (⌘N), or Minimize (⌘M) can take key status without the APP resigning active —
        // capture must release then too, or the new window inherits a hidden, frozen
        // cursor and its local typing is double-delivered to the host.
        for name in [NSWindow.didResignKeyNotification, NSWindow.didMiniaturizeNotification] {
            windowObservers.append(NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { [weak self] _ in
                self?.releaseCapture()
            })
        }
        attemptPendingCapture()
    }

    public override func layout() {
        super.layout()
        attemptPendingCapture() // bounds become real here on first presentation
    }

    // MARK: - Capture state machine

    /// Clicking into the video engages capture; that click is local (engagement), so
    /// InputCapture suppresses its press/release toward the host. Clicks while captured
    /// are the host's (GC forwards them) — nothing to do here.
    public override func mouseDown(with event: NSEvent) {
        if captureEnabled, !captured {
            engageCapture(fromClick: true)
            return
        }
        super.mouseDown(with: event)
    }

    /// A click from another app counts (one click into the video captures, not two).
    public override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    /// The engage click is complete — drop its suppression latch (see InputCapture;
    /// guards against GC delivering both halves of the click before our mouseDown).
    public override func mouseUp(with event: NSEvent) {
        inputCapture?.endClickSuppression()
        super.mouseUp(with: event)
    }

    /// Scroll is forwarded from here, not from GCMouse: trackpad/Magic Mouse gestures
    /// never reach GameController's scroll dpad. While captured the cursor is parked
    /// mid-view, so this view receives every scroll event. Precise (gesture) deltas are
    /// pixels — ~0.1 wheel notch per pixel (SDL's factor) → ×12 for WHEEL_DELTA(120);
    /// classic wheels report lines, one notch = ±1 → ×120. Signs pass through as-is,
    /// preserving the user's local (natural-)scrolling preference.
    public override func scrollWheel(with event: NSEvent) {
        guard captured, let inputCapture else {
            super.scrollWheel(with: event)
            return
        }
        let scale: Float = event.hasPreciseScrollingDeltas ? 12 : 120
        inputCapture.sendScroll(
            dx: Float(event.scrollingDeltaX) * scale,
            dy: Float(event.scrollingDeltaY) * scale)
    }

    // While captured, the view is first responder and consumes key events — GC delivers
    // them to the host independently, and consuming here stops the responder chain's
    // "unhandled keyDown" beep without touching the event stream GC may rely on.
    // ⌘-combos arrive via performKeyEquivalent instead and stay fully functional (⌘D).
    public override var acceptsFirstResponder: Bool { true }
    public override func keyDown(with event: NSEvent) {
        if captured { return }
        super.keyDown(with: event)
    }
    public override func keyUp(with event: NSEvent) {
        if captured { return }
        super.keyUp(with: event)
    }

    private func requestAutoCapture() {
        pendingAutoCapture = true
        attemptPendingCapture()
    }

    private func attemptPendingCapture() {
        guard pendingAutoCapture, window != nil, bounds.width > 0 else { return }
        pendingAutoCapture = false // one shot, even if the engage below is refused
        engageCapture(fromClick: false)
    }

    private func engageCapture(fromClick: Bool) {
        // A click is explicit intent AND may arrive mid-activation (acceptsFirstMouse:
        // NSApp.isActive / isKeyWindow are still false for the click coming in from
        // another app) — only the auto-engage paths require already-held key status.
        guard captureEnabled, !captured, pump != nil, window != nil,
              fromClick || (NSApp.isActive && window?.isKeyWindow == true)
        else { return }
        cursorCapture.capture(in: self)
        inputCapture?.setForwarding(true, suppressClick: fromClick)
        captured = true
        window?.makeFirstResponder(self)
        notifyCaptureChange(true)
    }

    private func releaseCapture() {
        guard captured else { return }
        cursorCapture.release()
        inputCapture?.setForwarding(false)
        captured = false
        notifyCaptureChange(false)
    }

    /// Engage/release can run inside a SwiftUI update pass (captureEnabled flips in
    /// updateNSView; release in dismantleNSView) — publishing model state synchronously
    /// there is undefined behavior, so the callback is deferred a runloop turn.
    private func notifyCaptureChange(_ captured: Bool) {
        guard let onCaptureChange else { return }
        DispatchQueue.main.async { onCaptureChange(captured) }
    }

    // MARK: - Pump

    /// Pump thread: pull AUs from the connection, wrap, enqueue. The first IDR yields the
    /// format description; non-IDR AUs before it are dropped (the host opens with an IDR).
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil
    ) {
        stop()
        self.connection = connection

        // The view owns the session's input capture: handlers attach now, but nothing is
        // forwarded until capture engages (captureEnabled + auto-engage or a click).
        let capture = InputCapture(connection: connection)
        capture.onToggleCapture = { [weak self] in
            // The ⌘⎋ monitor is app-wide — only the key window's stream owns the toggle
            // (two stream windows would otherwise flip each other's capture).
            guard let self, self.window?.isKeyWindow == true else { return }
            if self.captured {
                self.releaseCapture()
            } else {
                self.engageCapture(fromClick: false)
            }
        }
        capture.onPreempted = { [weak self] in
            // A newer session took the GC handler slots — staying "captured" here would
            // be a cursor trap with dead input.
            self?.releaseCapture()
        }
        capture.start()
        inputCapture = capture

        let pump = StreamPump()
        pump.start(
            connection: connection, layer: displayLayer,
            onFrame: onFrame, onSessionEnd: onSessionEnd)
        self.pump = pump
        requestAutoCapture() // entering a session is the deliberate "capture me" moment
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection — that stays with
    /// whoever owns it (PunktfunkConnection.close() is safe alongside a draining pump).
    public func stop() {
        releaseCapture()
        inputCapture?.stop()
        inputCapture = nil
        pump?.stop()
        pump = nil
        connection = nil
    }

    deinit {
        appObservers.forEach(NotificationCenter.default.removeObserver(_:))
        windowObservers.forEach(NotificationCenter.default.removeObserver(_:))
        pump?.stop()
    }
}
#endif
