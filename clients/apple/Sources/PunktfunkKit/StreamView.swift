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
import os

/// Same diagnostic switch as InputCapture: PUNKTFUNK_INPUT_DEBUG=1 logs when the macOS
/// NSEvent mouse monitor (relative motion + buttons) is installed/removed, so the user can
/// confirm the new motion path is actually live for a session.
private let streamInputLog = Logger(subsystem: "io.unom.punktfunk", category: "input")
private let streamInputDebug =
    ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

/// Hides the LOCAL cursor while captured. The host renders its own cursor, and the local
/// one both diverges from it (the host applies acceleration/clamping to our deltas) and
/// can wander out of the window — a click there would focus another app. So while captured
/// we do what Moonlight does: warp the cursor into the view, freeze it
/// (`CGAssociateMouseAndMouseCursorPosition(false)` — under which NSEvent mouseMoved/
/// dragged deltas become the relative motion StreamLayerView forwards), and hide it.
/// hide/unhide and associate are balanced via `captured`.
///
/// In CLIENT-SIDE-CURSOR mode (gamescope, whose capture carries no host cursor) this is a
/// no-op: the local cursor stays visible and free, and StreamLayerView forwards ABSOLUTE
/// positions instead — the visible system cursor IS the on-screen cursor. `disassociate`
/// selects between the two; `release()` only undoes what `capture` actually did.
private final class CursorCapture {
    private var captured = false
    /// Whether the engaged capture actually disassociated+hid (false in cursor-visible mode),
    /// so `release()` only restores when it must.
    private var disassociated = false

    /// Returns whether capture actually engaged. It can fail mid app-activation — the click
    /// that reactivates the app delivers `mouseDown` before the app is frontmost, and
    /// `CGAssociateMouseAndMouseCursorPosition` is refused then — so the caller must stay
    /// released and let the NEXT click retry, never latching a half-captured state. With
    /// `disassociate: false` (cursor-visible mode) it always engages — there is no grab to
    /// be refused, the cursor stays free and visible.
    func capture(in view: NSView, disassociate: Bool) -> Bool {
        guard !captured, let window = view.window, view.bounds.width > 0 else { return false }
        if disassociate {
            // Park the cursor mid-view so a click can't land in (and activate) another app.
            let rectOnScreen = window.convertToScreen(view.convert(view.bounds, to: nil))
            let primaryHeight = NSScreen.screens.first?.frame.height ?? 0
            CGWarpMouseCursorPosition(
                CGPoint(x: rectOnScreen.midX, y: primaryHeight - rectOnScreen.midY))
            guard CGAssociateMouseAndMouseCursorPosition(0) == .success else { return false }
            NSCursor.hide()
        }
        captured = true
        disassociated = disassociate
        return true
    }

    func release() {
        guard captured else { return }
        if disassociated {
            CGAssociateMouseAndMouseCursorPosition(1)
            NSCursor.unhide()
        }
        captured = false
        disassociated = false
    }
}

public struct StreamView: NSViewRepresentable {
    private let connection: PunktfunkConnection
    private let captureEnabled: Bool
    private let onCaptureChange: ((Bool) -> Void)?
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?
    private let presentMeter: LatencyMeter?

    /// `onFrame`/`onSessionEnd` fire on the pump thread — hop to the main actor for UI.
    /// `captureEnabled: false` disables input capture entirely while UI (e.g. a trust
    /// prompt) is layered over the stream; flipping it to true auto-engages capture
    /// once. `onCaptureChange` (main thread) reports engage/release — drive the HUD's
    /// "click to capture" / "⌘⎋ releases" hint with it. `presentMeter` records capture→present
    /// when the stage-2 presenter is active (`punktfunk.presenter == "stage2"`).
    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil,
        presentMeter: LatencyMeter? = nil
    ) {
        self.connection = connection
        self.captureEnabled = captureEnabled
        self.onCaptureChange = onCaptureChange
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
        self.presentMeter = presentMeter
    }

    public func makeNSView(context: Context) -> StreamLayerView {
        let view = StreamLayerView()
        view.onCaptureChange = onCaptureChange
        view.captureEnabled = captureEnabled
        view.presentMeter = presentMeter
        view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return view
    }

    public func updateNSView(_ view: StreamLayerView, context: Context) {
        view.onCaptureChange = onCaptureChange
        view.captureEnabled = captureEnabled
        view.presentMeter = presentMeter
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
    /// Stage-2 presenter (opt-in via `punktfunk.presenter`): a CAMetalLayer sublayer driven by a
    /// display link instead of the StreamPump → displayLayer path. nil = stage-1 (default).
    var presentMeter: LatencyMeter?
    private var stage2: Stage2Pipeline?
    private var stage2Link: CADisplayLink?
    private var metalLayer: CAMetalLayer?
    public private(set) var connection: PunktfunkConnection?
    private let cursorCapture = CursorCapture()
    private var inputCapture: InputCapture?
    private var appObservers: [NSObjectProtocol] = []
    private var windowObservers: [NSObjectProtocol] = []
    /// Local NSEvent monitor carrying relative mouse MOTION + BUTTONS to the host while
    /// captured (GCMouse's own delivery proved unreliable on macOS — see InputCapture).
    /// Installed on engage, removed on release; nil while not captured.
    private var mouseEventMonitor: Any?
    /// The window's `acceptsMouseMovedEvents` value before client-side-cursor capture raised
    /// it (nil = not raised by us); restored on release so we leave the window as we found it.
    private var savedAcceptsMouseMoved: Bool?

    /// Whether input capture is currently engaged (cursor hidden+frozen, mouse/keyboard
    /// forwarded). Main-thread only.
    public private(set) var captured = false

    /// Client-side-cursor mode: when true the local system cursor stays VISIBLE over the
    /// stream and the mouse monitor forwards ABSOLUTE positions (the visible cursor is the
    /// on-screen cursor — gamescope draws none, so no double cursor); when false the existing
    /// captured/disassociated relative path runs unchanged. Initialized at session start from
    /// the `cursorMode` setting + the host's resolved compositor, toggled live by ⌘⇧C. A live
    /// flip re-engages capture in the new mode so disassociation + the abs/rel choice swap
    /// atomically. Main-thread only.
    private var cursorVisible = false
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
        // Becoming key RETRIES a still-pending session-start auto-capture — the case where a
        // session began (reconnect) while this window wasn't key yet, so engageCapture(fromClick:
        // false) was refused by its key-window guard and, with no retry, capture stayed off and
        // input dead. This is a no-op once capture engaged (pendingAutoCapture is cleared) and
        // after a manual ⌘⎋/focus-loss release (the flag is already false), so it does NOT
        // resurrect the deliberately-rejected "auto-grab on every activation" behavior.
        windowObservers.append(NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification, object: window, queue: .main
        ) { [weak self] _ in
            self?.attemptPendingCapture()
        })
        attemptPendingCapture()
    }

    public override func layout() {
        super.layout()
        attemptPendingCapture() // bounds become real here on first presentation
        layoutMetalLayer() // keep the stage-2 sublayer aspect-fit to the view
    }

    public override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        // `layout()` isn't guaranteed on a manual-frame (no-Auto-Layout) live resize, so the
        // stage-2 metal sublayer's drawableSize could stay at the old size while the view grows —
        // the compositor then upscales a too-small drawable and the video turns blocky. Resize the
        // drawable here too so it always tracks the window's pixel size (no stale upscale).
        layoutMetalLayer()
    }

    // MARK: - Capture state machine

    /// Clicking into the video engages capture; that click is local (engagement), so
    /// InputCapture suppresses its press/release toward the host. Clicks while captured
    /// are the host's (GC forwards them) — nothing to do here.
    public override func mouseDown(with event: NSEvent) {
        if streamInputDebug {
            streamInputLog.debug(
                "mouseDown: captureEnabled=\(self.captureEnabled, privacy: .public) captured=\(self.captured, privacy: .public)")
        }
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

    // While captured, the view is first responder and SENDS key events to the host straight
    // from NSEvent — GCKeyboard delivery proved unreliable on macOS (the same GameController
    // quirk that killed GCMouse motion, fixed in e414ec0), so the macOS GCKeyboard send path
    // is disabled and NSEvent is the single source. We map NSEvent.keyCode (a Carbon virtual
    // keycode) → Windows VK and forward via InputCapture.sendKey, then CONSUME (return without
    // super) to stop the responder chain's "unhandled keyDown" beep. Keys with no VK mapping
    // are still consumed while captured so they don't beep either. The ⌘⎋ toggle's Esc is
    // swallowed upstream by InputCapture's keyDown monitor (suppressedVK), so it never gets
    // here as a send; ⌘-combos still arrive via performKeyEquivalent and stay functional (⌘D).
    // Modifier keys never fire keyDown/keyUp — they come through flagsChanged below.
    public override var acceptsFirstResponder: Bool { true }
    // A click after the app was inactive (Cmd-Tab away and back) must reach mouseDown so the
    // user can re-capture — the deliberate design is that becoming active does NOT auto-grab;
    // you click into the video. Default NSViews aren't key-view candidates, which can drop
    // that first click; opting in keeps the view a valid click/responder target.
    public override var canBecomeKeyView: Bool { true }
    public override func keyDown(with event: NSEvent) {
        if captured {
            if let ic = inputCapture, let vk = InputCapture.keyCodeToVK[event.keyCode] {
                ic.sendKey(vk, down: true) // autorepeat (event.isARepeat) passes through — fine for VK
            }
            return // consume even unmapped keys while captured (no beep)
        }
        super.keyDown(with: event)
    }
    public override func keyUp(with event: NSEvent) {
        if captured {
            if let ic = inputCapture, let vk = InputCapture.keyCodeToVK[event.keyCode] {
                ic.sendKey(vk, down: false)
            }
            return
        }
        super.keyUp(with: event)
    }
    /// Modifier keys (shift/control/option/command) arrive ONLY as flagsChanged on macOS,
    /// never keyDown/keyUp — InputCapture diffs the raw flags to recover each L/R down/up.
    public override func flagsChanged(with event: NSEvent) {
        if captured, let inputCapture {
            inputCapture.handleFlagsChanged(UInt(event.modifierFlags.rawValue))
            return
        }
        super.flagsChanged(with: event)
    }

    private func requestAutoCapture() {
        pendingAutoCapture = true
        attemptPendingCapture()
    }

    private func attemptPendingCapture() {
        guard pendingAutoCapture, window != nil, bounds.width > 0 else { return }
        engageCapture(fromClick: false)
        // Clear the one-shot only once it ACTUALLY engaged. If the engage was refused — the
        // app/window isn't key yet (common right after a reconnect), or the cursor grab raced
        // app activation — leave it armed so didBecomeKey (or the next layout pass) retries.
        // This stays scoped to session start: a later manual release (⌘⎋, focus loss) doesn't
        // re-arm it, so it never resurrects auto-grab-on-activation.
        if captured { pendingAutoCapture = false }
    }

    private func engageCapture(fromClick: Bool) {
        // A click is explicit intent AND may arrive mid-activation (acceptsFirstMouse:
        // NSApp.isActive / isKeyWindow are still false for the click coming in from
        // another app) — only the auto-engage paths require already-held key status.
        // `connection != nil` (not `pump`) is the session-active gate — the stage-2 presenter
        // runs without a StreamPump, and capture must still engage there.
        guard captureEnabled, !captured, connection != nil, window != nil,
              fromClick || (NSApp.isActive && window?.isKeyWindow == true)
        else { return }
        // If the cursor grab is refused (e.g. the reactivating click arrives before the app is
        // frontmost), stay released so the NEXT click retries — never latch captured=true over
        // a free cursor, which would make mouseDown's `!captured` guard reject every later click.
        // In client-side-cursor mode there is no grab (the cursor stays visible) — capture
        // always engages and the monitor forwards absolute positions instead.
        guard cursorCapture.capture(in: self, disassociate: !cursorVisible) else { return }
        inputCapture?.setForwarding(true, suppressClick: fromClick)
        // Install AFTER the warp + setForwarding: the engage warp generates no forwarded
        // delta (the monitor isn't up yet), and the engage click's suppression latch is
        // already armed, so the monitor only ever sees genuine post-engage input.
        installMouseMonitor()
        captured = true
        window?.makeFirstResponder(self)
        notifyCaptureChange(true)
    }

    private func releaseCapture() {
        guard captured else { return }
        removeMouseMonitor()
        cursorCapture.release()
        inputCapture?.setForwarding(false)
        captured = false
        notifyCaptureChange(false)
    }

    /// A single local monitor for motion + buttons, installed only while captured. A local
    /// monitor is more robust than view overrides for relative motion: it sidesteps the
    /// `window.acceptsMouseMovedEvents`/tracking-area/responder-chain requirements, and
    /// since the cursor is frozen mid-view while captured every such event belongs here.
    /// ALL four motion types are covered so motion keeps flowing during a button-held drag,
    /// not just `.mouseMoved`. NSEvent deltas under disassociation are OS-pointer-
    /// acceleration-applied (not raw HID) — what Moonlight's macOS client ships; if the
    /// host re-accelerates there's mild double-acceleration, acceptable and fixable later
    /// via IOHID. Events are returned (not swallowed): the cursor is frozen, so they're
    /// inert locally.
    ///
    /// In client-side-cursor mode the cursor is NOT frozen, so bare `.mouseMoved` events are
    /// only generated while `window.acceptsMouseMovedEvents` is true — we enable it here and
    /// restore it on removal so absolute hover-motion keeps flowing without a click held.
    private func installMouseMonitor() {
        guard mouseEventMonitor == nil else { return }
        if cursorVisible {
            savedAcceptsMouseMoved = window?.acceptsMouseMovedEvents
            window?.acceptsMouseMovedEvents = true
        }
        mouseEventMonitor = NSEvent.addLocalMonitorForEvents(matching: [
            .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged,
            .leftMouseDown, .leftMouseUp, .rightMouseDown, .rightMouseUp,
            .otherMouseDown, .otherMouseUp,
        ]) { [weak self] event in
            guard let self, self.captured, let ic = self.inputCapture else { return event }
            switch event.type {
            case .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged:
                if self.cursorVisible {
                    // Client-side cursor: forward the ABSOLUTE position (mapped through the
                    // aspect-fit letterbox into host pixels), the same path the iPad pointer
                    // fallback uses. Events in the letterbox bars are dropped (nil host point).
                    if let p = self.hostPoint(from: event) {
                        ic.sendMouseAbs(x: p.x, y: p.y, surfaceWidth: p.w, surfaceHeight: p.h)
                    }
                } else {
                    ic.sendMotion(dx: Float(event.deltaX), dy: Float(event.deltaY)) // no y-negation
                }
            case .leftMouseDown: ic.sendMouseButton(1, pressed: true)
            case .leftMouseUp: ic.sendMouseButton(1, pressed: false)
            case .rightMouseDown: ic.sendMouseButton(3, pressed: true)
            case .rightMouseUp: ic.sendMouseButton(3, pressed: false)
            case .otherMouseDown: ic.sendMouseButton(self.wireButton(for: event), pressed: true)
            case .otherMouseUp: ic.sendMouseButton(self.wireButton(for: event), pressed: false)
            default: break
            }
            return event
        }
        if streamInputDebug { streamInputLog.debug("mouse NSEvent monitor installed (capture engaged)") }
    }

    private func removeMouseMonitor() {
        if let monitor = mouseEventMonitor {
            NSEvent.removeMonitor(monitor)
            mouseEventMonitor = nil
            if streamInputDebug { streamInputLog.debug("mouse NSEvent monitor removed (capture released)") }
        }
        // Restore the window's prior mouse-moved-events setting if we raised it (cursor mode).
        if let saved = savedAcceptsMouseMoved {
            window?.acceptsMouseMovedEvents = saved
            savedAcceptsMouseMoved = nil
        }
    }

    /// One host-pixel point on the negotiated output, with the surface dimensions the host
    /// rescales against (surface == host mode, so the host applies no extra scaling).
    private struct HostPoint { let x: Int32; let y: Int32; let w: UInt32; let h: UInt32 }

    /// Map an NSEvent's cursor location into host-mode pixels for the client-side-cursor
    /// (absolute) path. NSEvent.locationInWindow is window space, origin BOTTOM-left (+y up);
    /// we convert to this view's space, FLIP y to the host's top-left (+y down) convention,
    /// then aspect-fit-letterbox into the host mode exactly like the iOS touch/pointer path.
    /// Returns nil for events in the letterbox bars (outside the video rect) so the host's
    /// cursor isn't dragged onto a black edge, and until a mode is negotiated.
    private func hostPoint(from event: NSEvent) -> HostPoint? {
        guard let connection else { return nil }
        let mode = connection.currentMode()
        guard mode.width > 0, mode.height > 0 else { return nil }
        // Window → view coords (non-flipped: origin bottom-left), then flip y into view-top-left.
        let inView = convert(event.locationInWindow, from: nil)
        let p = CGPoint(x: inView.x, y: bounds.height - inView.y)
        // The video occupies the aspect-fit rect inside the (non-flipped) bounds; AVMakeRect's
        // origin is bottom-left, so flip its minY too to match p's top-left space.
        let fit = AVMakeRect(
            aspectRatio: CGSize(width: Int(mode.width), height: Int(mode.height)),
            insideRect: bounds)
        guard fit.width > 0, fit.height > 0 else { return nil }
        let videoMinYTop = bounds.height - fit.maxY
        let u = (p.x - fit.minX) / fit.width
        let v = (p.y - videoMinYTop) / fit.height
        guard u >= 0, u <= 1, v >= 0, v <= 1 else { return nil } // letterbox bars
        let hx = Int32((u * CGFloat(mode.width)).rounded().clamped(0, CGFloat(mode.width - 1)))
        let hy = Int32((v * CGFloat(mode.height)).rounded().clamped(0, CGFloat(mode.height - 1)))
        return HostPoint(x: hx, y: hy, w: mode.width, h: mode.height)
    }

    /// NSEvent `buttonNumber` → GameStream wire id for the "other" buttons: 2 = middle,
    /// 3 = first side (X1), 4 = second side (X2). Unknown extras fall back to middle.
    private func wireButton(for event: NSEvent) -> UInt32 {
        switch event.buttonNumber {
        case 2: return 2 // middle
        case 3: return 4 // X1
        case 4: return 5 // X2
        default: return 2
        }
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
        // ⌘⇧C flips the client-side cursor live. Only the key window's stream owns it (same
        // guard as the ⌘⎋ capture toggle). Re-engage capture in the new mode so disassociation
        // and the absolute/relative forwarding choice swap atomically — releaseCapture restores
        // the old mode's grab (if any), engageCapture installs the new one.
        // ⌘⇧C would flip the client-side cursor live — NEUTERED while the feature is disabled
        // (see the cursorVisible resolution below): toggling it on under gamescope's relative-only
        // input traps the pointer. Restore this body when absolute/synthetic-cursor support lands.
        capture.onToggleCursor = {}
        capture.start()
        inputCapture = capture

        // Client-side cursor is TEMPORARILY DISABLED. It positions the host cursor with ABSOLUTE
        // events, but gamescope's input socket (EIS) grants only a relative pointer, so those are
        // silently dropped — the pointer never moves and clicks/scroll land on the stuck position
        // (looks like "all input dead"). gamescope is exactly the compositor Auto enabled it for.
        // Forced off until per-compositor gating (KWin/GNOME/Sway have absolute) or a synthetic-
        // cursor-over-relative path lands; the resolution logic below is kept for that. See the
        // ⌘⇧C handler (also neutered) and the cursorMode setting (hidden).
        cursorVisible = false
        _ = connection.resolvedCompositor // (was: Auto → gamescope; kept to document intent)

        // Presenter choice — stage-2 is the DEFAULT (explicit VTDecompressionSession decode + a
        // CAMetalLayer/display-link present): it can detect + recover a wedged decoder where
        // stage-1's AVSampleBufferDisplayLayer freezes hard on a lost HEVC reference. Stage-1 is
        // reachable only via the DEBUG presenter toggle; release always takes stage-2 (the stage-1
        // pump below stays the automatic fallback if Metal is missing).
        #if DEBUG
        let forceStage1 = UserDefaults.standard.string(forKey: DefaultsKey.presenter) == "stage1"
        #else
        let forceStage1 = false
        #endif
        if !forceStage1,
           let meter = presentMeter,
           let pipeline = Stage2Pipeline(presentMeter: meter) {
            startStage2(pipeline, connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        } else {
            let pump = StreamPump()
            pump.start(
                connection: connection, layer: displayLayer,
                onFrame: onFrame, onSessionEnd: onSessionEnd)
            self.pump = pump
        }
        requestAutoCapture() // entering a session is the deliberate "capture me" moment
    }

    // MARK: - Stage-2 presenter (VTDecompressionSession → CAMetalLayer + display link)

    private func startStage2(
        _ pipeline: Stage2Pipeline, connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?, onSessionEnd: (@Sendable () -> Void)?
    ) {
        let metal = pipeline.layer
        // The opaque metal layer composites OVER the AVSampleBufferDisplayLayer base, which sits
        // idle (un-enqueued) in stage-2. contentsScale + frame are set in layoutMetalLayer().
        displayLayer.addSublayer(metal)
        metalLayer = metal
        stage2 = pipeline
        layoutMetalLayer()
        // Weak-proxy target so the link doesn't form a retain cycle with the view (see
        // DisplayLinkProxy) — the link retains the proxy; the proxy holds the view weakly.
        let proxy = DisplayLinkProxy { [weak self] link in self?.stage2Tick(link) }
        let link = displayLink(target: proxy, selector: #selector(DisplayLinkProxy.tick(_:)))
        link.add(to: .main, forMode: .common)
        stage2Link = link
        pipeline.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
    }

    private func stage2Tick(_ link: CADisplayLink) {
        stage2?.renderTick(
            targetPresentNs: Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: link.targetTimestamp))
    }

    /// Position the metal sublayer aspect-fit in the view (the host streams at the client's native
    /// mode, so this is usually the full bounds; it letterboxes a resized window). Only the layer
    /// FRAME is set here — the presenter sizes the drawable to the decoded frame and the layer's
    /// contentsGravity (.resizeAspect) scales it to this frame via the system compositor, so a
    /// resized window rescales through the system's filter (matching stage-1) instead of the shader.
    private func layoutMetalLayer() {
        guard let metalLayer, let connection else { return }
        let mode = connection.currentMode()
        let fit: NSRect = (mode.width > 0 && mode.height > 0)
            ? AVMakeRect(
                aspectRatio: CGSize(width: Int(mode.width), height: Int(mode.height)),
                insideRect: bounds)
            : bounds
        // No implicit resize animation; refresh contentsScale on a retina↔non-retina move.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        metalLayer.contentsScale = window?.backingScaleFactor ?? 1
        metalLayer.frame = fit
        CATransaction.commit()
    }

    public override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        layoutMetalLayer() // backing scale changed (e.g. moved to a non-retina display)
    }

    private func teardownStage2() {
        stage2Link?.invalidate()
        stage2Link = nil
        stage2?.stop()
        stage2 = nil
        metalLayer?.removeFromSuperlayer()
        metalLayer = nil
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection — that stays with
    /// whoever owns it (PunktfunkConnection.close() is safe alongside a draining pump).
    public func stop() {
        releaseCapture()
        removeMouseMonitor() // belt-and-suspenders: releaseCapture no-ops if not captured
        inputCapture?.stop()
        inputCapture = nil
        pump?.stop()
        pump = nil
        teardownStage2()
        connection = nil
    }

    deinit {
        removeMouseMonitor()
        appObservers.forEach(NotificationCenter.default.removeObserver(_:))
        windowObservers.forEach(NotificationCenter.default.removeObserver(_:))
        pump?.stop()
        teardownStage2() // invalidate the display link + stop the pipeline if stop() was missed
    }
}

extension CGFloat {
    /// Clamp into a [lo, hi] range — keeps the absolute-cursor mapping inside the host's
    /// pixel bounds even if a stray event reports a point a hair past the video rect.
    fileprivate func clamped(_ lo: CGFloat, _ hi: CGFloat) -> CGFloat {
        Swift.min(Swift.max(self, lo), hi)
    }
}
#endif
