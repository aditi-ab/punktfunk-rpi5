// iOS/iPadOS presenter: the same AVSampleBufferDisplayLayer + StreamPump as macOS,
// hosted in a UIViewController so the scene can pointer-lock (the iPadOS equivalent of
// the Mac's cursor capture — with a hardware mouse/trackpad the system cursor is hidden
// and GCMouse's raw deltas drive the host cursor alone; the system only honors the lock
// fullscreen-and-frontmost, so in Stage Manager it degrades to Mac-style "both cursors
// visible" forwarding).
//
// FINGER touch and INDIRECT POINTER (mouse/trackpad) are routed apart by UITouch.type.
// Direct fingers (and Pencil) always forward as wire touches — every finger maps to a touch
// id, coordinates mapped through the aspect-fit letterbox into host-mode pixels (surface ==
// host mode, so the host's rescale is the identity).
//
// A hardware mouse/trackpad is a pointer, not a finger. When the scene is pointer-LOCKED
// (full-screen + frontmost iPad) GCMouse delivers raw relative deltas and the system hides
// the cursor — the gaming-grade path. When it CAN'T lock (Stage Manager, not frontmost,
// iPhone) the system shows its own cursor and routes the mouse through UIKit's pointer path:
// hover + indirect-pointer touches, which we forward as ABSOLUTE cursor position (+ buttons)
// so the host cursor tracks the visible local one. We never forward an indirect pointer as a
// touch — doing so hid the cursor and made the host see taps instead of a moving mouse.
// GCMouse is gated off whenever the lock isn't held so the two paths can't double-send.
// Hardware keyboard forwarding shares InputCapture with macOS — auto-engaged when streaming
// starts, ⌘⎋ toggles (detected from the HID stream; there is no NSEvent monitor here).
//
// The public type is named StreamView like its macOS twin (each is platform-gated), so
// the SwiftUI app layer is identical on both platforms.

#if os(iOS) || os(tvOS)
import AVFoundation
import GameController
import PunktfunkCore
import SwiftUI
import UIKit
import os

/// Same diagnostic switch as InputCapture (PUNKTFUNK_INPUT_DEBUG=1): on iOS we log the
/// resolved pointer-lock state each time capture engages, so the user can see whether the
/// scene actually locked (GCMouse only delivers deltas while it did) or whether we're on
/// the touch fallback.
private let iosInputLog = Logger(subsystem: "io.unom.punktfunk", category: "input")
private let iosInputDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

public struct StreamView: UIViewControllerRepresentable {
    private let connection: PunktfunkConnection
    private let captureEnabled: Bool
    private let onCaptureChange: ((Bool) -> Void)?
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?

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

    public func makeUIViewController(context: Context) -> StreamViewController {
        let controller = StreamViewController()
        controller.onCaptureChange = onCaptureChange
        controller.captureEnabled = captureEnabled
        controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return controller
    }

    public func updateUIViewController(_ controller: StreamViewController, context: Context) {
        controller.onCaptureChange = onCaptureChange
        controller.captureEnabled = captureEnabled
        if controller.connection !== connection {
            controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        }
    }

    public static func dismantleUIViewController(
        _ controller: StreamViewController, coordinator: ()
    ) {
        controller.stop()
    }
}

public final class StreamViewController: UIViewController {
    public private(set) var connection: PunktfunkConnection?
    private var pump: StreamPump?
    private var observers: [NSObjectProtocol] = []
    #if os(iOS)
    private var inputCapture: InputCapture?
    fileprivate var captured = false
    private var pointerInteraction: UIPointerInteraction?
    /// Capture state at the last resign, restored on the next foreground — otherwise the
    /// mouse/keyboard stay released after navigating out and nothing re-grabs them.
    private var wasCapturedOnResign = false
    #endif

    /// Reads whether the scene's pointer is actually locked right now; nil = state
    /// unavailable (no scene yet, or pre-availability). Only while this is true does GCMouse
    /// deliver relative deltas — otherwise the touch path carries input.
    private func pointerLockEngaged() -> Bool? {
        #if os(iOS)
        return view.window?.windowScene?.pointerLockState?.isLocked
        #else
        return nil
        #endif
    }

    var onCaptureChange: ((Bool) -> Void)?

    var captureEnabled = true {
        didSet {
            guard captureEnabled != oldValue else { return }
            #if os(iOS)
            setCaptured(captureEnabled)
            #endif
        }
    }

    private var streamView: StreamLayerUIView {
        // swiftlint:disable:next force_cast
        view as! StreamLayerUIView
    }

    public override func loadView() {
        view = StreamLayerUIView()
        #if os(iOS)
        // Hide the iPadOS cursor while it hovers the video: the host renders its own
        // cursor from our deltas, so the local one only diverges from it. This hides the
        // pointer; true pointer LOCK (below) is what makes GCMouse deliver relative deltas
        // — and the system only grants it on a full-screen, frontmost iPad scene.
        let interaction = UIPointerInteraction(delegate: self)
        view.addInteraction(interaction)
        pointerInteraction = interaction
        #endif
    }

    #if os(iOS)
    // Pointer lock is only meaningful on iPad (iPhone has no hardware-pointer lock) and
    // only when capture is engaged. The system additionally requires full-screen + frontmost
    // and may drop it (Slide Over/Stage Manager/backgrounding) — verified in setCaptured().
    public override var prefersPointerLocked: Bool {
        captured && UIDevice.current.userInterfaceIdiom == .pad
    }
    public override var prefersHomeIndicatorAutoHidden: Bool { true }

    // If SwiftUI's UIHostingController reparents us, a plain container parent that forwards
    // its pointer-lock decision to its children will then reach this VC. (UIHostingController
    // itself does not consult children, which is why GCMouse deltas can never arrive there —
    // the touch path, always forwarded, is the unconditional fallback.)
    public override var childViewControllerForPointerLock: UIViewController? { self }
    #endif

    func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        stop()
        self.connection = connection
        loadViewIfNeeded()
        #if os(iOS)
        // Read the LIVE mode per touch batch — an accepted requestMode() mid-stream
        // changes the letterbox, and touches must follow it.
        streamView.currentHostMode = { [weak connection] in
            guard let connection else { return .zero }
            let mode = connection.currentMode()
            return CGSize(width: Double(mode.width), height: Double(mode.height))
        }
        streamView.onTouchEvent = { [weak self, weak connection] event in
            // Touch IS the intent during a trusted session, but must not leak to the host
            // while a trust prompt is up (captureEnabled == false) — gate it on that. The
            // ⌘⎋ mouse/keyboard toggle (captured) deliberately does NOT gate touch.
            guard self?.captureEnabled == true else { return }
            connection?.send(event)
        }
        // Indirect pointer (mouse/trackpad with no lock) → absolute cursor + buttons, routed
        // through InputCapture so the forwarding gate and release-on-blur apply uniformly.
        streamView.onPointerMoveAbs = { [weak self] p in
            self?.inputCapture?.sendMouseAbs(
                x: p.x, y: p.y, surfaceWidth: p.w, surfaceHeight: p.h)
        }
        streamView.onPointerButton = { [weak self] button, down in
            self?.inputCapture?.sendMouseButton(button, pressed: down)
        }
        // Trackpad two-finger / wheel scroll → host scroll. The pan recognizer is the
        // UNLOCKED regime; while locked, GCMouse's scroll handler owns it — mirror the
        // sendMouseAbs !gcMouseForwarding gate so the two can't double-send.
        streamView.onScroll = { [weak self] dx, dy in
            guard let self, self.inputCapture?.gcMouseForwarding == false else { return }
            self.inputCapture?.sendScroll(dx: dx, dy: dy)
        }

        let capture = InputCapture(connection: connection)
        capture.onToggleCapture = { [weak self] in
            guard let self else { return }
            self.setCaptured(!self.captured)
        }
        capture.onPreempted = { [weak self] in
            self?.setCaptured(false)
        }
        capture.start()
        inputCapture = capture
        #endif

        let pump = StreamPump()
        pump.start(
            connection: connection, layer: streamView.displayLayer,
            onFrame: onFrame, onSessionEnd: onSessionEnd)
        self.pump = pump

        #if os(iOS)
        // GC only delivers while active; everything held is flushed by InputCapture's
        // own resign observer — here we just mirror the capture state for the HUD and
        // the pointer lock.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            self.wasCapturedOnResign = self.captured
            self.setCaptured(false)
        })
        // Returning to the foreground restores the capture the user had before leaving —
        // without this the mouse/keyboard stay released and nothing re-grabs them (touch
        // always plays regardless). The macOS twin re-engages on a click into the video.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self, self.wasCapturedOnResign, self.captureEnabled, self.pump != nil
            else { return }
            self.setCaptured(true)
        })
        // The system can grant or drop the lock without us asking (Slide Over, Stage Manager,
        // entering/leaving foregroundActive). Re-resolve the mouse routing on every change:
        // GCMouse (locked) vs the absolute UIKit pointer path (unlocked), and the
        // hidden-vs-visible local cursor.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIPointerLockState.didChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.syncPointerLock()
        })

        if captureEnabled {
            setCaptured(true) // entering a session is the deliberate "capture me" moment
        }
        #endif
    }

    func stop() {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        #if os(iOS)
        setCaptured(false)
        inputCapture?.stop()
        inputCapture = nil
        streamView.onTouchEvent = nil
        streamView.onPointerMoveAbs = nil
        streamView.onPointerButton = nil
        streamView.onScroll = nil
        streamView.currentHostMode = nil
        #endif
        pump?.stop()
        pump = nil
        connection = nil
    }

    #if os(iOS)
    private func setCaptured(_ on: Bool) {
        if on {
            guard captureEnabled, !captured, pump != nil else { return }
            inputCapture?.setForwarding(true)
            captured = true
        } else {
            guard captured else { return }
            inputCapture?.setForwarding(false)
            captured = false
        }
        setNeedsUpdateOfPrefersPointerLocked()
        syncPointerLock() // resolve cursor + GCMouse/absolute routing for the current state
        let onCaptureChange = onCaptureChange
        let captured = captured
        DispatchQueue.main.async { [weak self] in
            onCaptureChange?(captured)
            // The lock request is async — the resolved state can land a runloop later, and the
            // initial grant may precede our didChange observer, so re-resolve the routing here.
            self?.syncPointerLock()
        }
    }

    /// Resolve the mouse routing for the scene's CURRENT pointer-lock state: GCMouse (relative
    /// deltas + buttons) while locked, the absolute UIKit pointer path while not, and the
    /// hidden-vs-visible local cursor to match. Idempotent — safe to call on every lock-state
    /// change and capture toggle. Main queue.
    private func syncPointerLock() {
        let locked = pointerLockEngaged() == true
        let useGCMouse = captured && locked
        // Lock dropped (or capture ended) while the GCMouse path held a button down: once
        // gcMouseForwarding flips false its release handler is gated off, so flush any held
        // mouse button here before the switch — otherwise it sticks down on the host.
        if inputCapture?.gcMouseForwarding == true, !useGCMouse {
            inputCapture?.releaseMouseButtons()
        }
        inputCapture?.gcMouseForwarding = useGCMouse
        pointerInteraction?.invalidate() // re-resolve the hidden/visible cursor for the state
        if iosInputDebug {
            iosInputLog.debug(
                "pointer lock isLocked=\(locked, privacy: .public) captured=\(self.captured, privacy: .public)")
        }
    }
    #endif

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        pump?.stop()
    }
}

#if os(iOS)
extension StreamViewController: UIPointerInteractionDelegate {
    public func pointerInteraction(
        _ interaction: UIPointerInteraction, styleFor region: UIPointerRegion
    ) -> UIPointerStyle? {
        // Hide the local cursor only when the scene is actually pointer-LOCKED — then the
        // host renders its own cursor from GCMouse deltas and a visible local one would just
        // diverge. When the lock isn't held the cursor stays VISIBLE so the user can aim; the
        // pointer is forwarded as an absolute position, both cursors tracking together.
        captured && pointerLockEngaged() == true ? .hidden() : nil
    }
}
#endif

/// The layer-backed video surface + touch source. Touches are mapped through the
/// aspect-fit letterbox into host-mode pixels (surface == host mode, so the host-side
/// rescale is the identity); touches outside the video area are clamped onto its edge.
final class StreamLayerUIView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    var displayLayer: AVSampleBufferDisplayLayer {
        // swiftlint:disable:next force_cast
        layer as! AVSampleBufferDisplayLayer
    }

    #if os(iOS)
    /// A position already mapped into host-mode pixels, with the surface dims the host
    /// rescales against (== host mode, so its rescale is the identity).
    struct HostPoint { let x: Int32; let y: Int32; let w: UInt32; let h: UInt32 }

    /// Reads the LIVE negotiated mode in pixels (the touch/pointer coordinate space).
    var currentHostMode: (() -> CGSize)?
    /// Direct fingers / Pencil → wire touch events.
    var onTouchEvent: ((PunktfunkInputEvent) -> Void)?
    /// Indirect pointer (mouse/trackpad with no lock) → absolute cursor moves.
    var onPointerMoveAbs: ((HostPoint) -> Void)?
    /// Indirect-pointer buttons (GameStream ids: 1=left 3=right); `down` = press.
    var onPointerButton: ((_ button: UInt32, _ down: Bool) -> Void)?
    /// Trackpad two-finger / wheel scroll (no lock) → host scroll deltas, WHEEL(120)-scaled.
    var onScroll: ((_ dx: Float, _ dy: Float) -> Void)?

    /// Wire touch ids per active direct UITouch; ids are reused after the touch ends.
    private var touchIDs: [ObjectIdentifier: UInt32] = [:]
    /// GameStream button held per active indirect-pointer touch (one click/drag session);
    /// released when that touch ends.
    private var pointerButtons: [ObjectIdentifier: UInt32] = [:]
    #endif

    override init(frame: CGRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        #if os(iOS)
        isMultipleTouchEnabled = true
        // Button-less mouse/trackpad movement (no lock) arrives as hover, not touches —
        // forward it as absolute cursor moves so the host cursor tracks without a click held.
        addGestureRecognizer(
            UIHoverGestureRecognizer(target: self, action: #selector(handleHover)))
        // Trackpad two-finger / wheel scroll → a scroll-ONLY pan: allowedTouchTypes = []
        // rejects finger drags (those stay host touches), allowedScrollTypesMask accepts the
        // indirect scroll devices. Forwarded as host scroll deltas.
        let scrollPan = UIPanGestureRecognizer(target: self, action: #selector(handleScroll))
        scrollPan.allowedScrollTypesMask = .all
        scrollPan.allowedTouchTypes = []
        addGestureRecognizer(scrollPan)
        #endif
        backgroundColor = .black
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    #if os(iOS)
    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .down)
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .move)
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .up)
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .up)
    }

    private enum TouchKind { case down, move, up }

    /// Split a touch batch by kind: an INDIRECT POINTER (mouse/trackpad with no lock) drives
    /// the host cursor as an absolute mouse; everything else (direct finger, Pencil) is a host
    /// touch. Mixed batches are possible, so partition rather than branch on the first touch.
    private func route(_ touches: Set<UITouch>, event: UIEvent?, kind: TouchKind) {
        var fingers: Set<UITouch> = []
        for touch in touches {
            if touch.type == .indirectPointer {
                handleIndirectPointer(touch, event: event, kind: kind)
            } else {
                fingers.insert(touch)
            }
        }
        if !fingers.isEmpty { forwardTouches(fingers, kind: kind) }
    }

    /// An indirect-pointer touch is a button-held click/drag session: forward its position as
    /// an absolute cursor move and its button as a mouse button (down on begin, up on end).
    private func handleIndirectPointer(_ touch: UITouch, event: UIEvent?, kind: TouchKind) {
        let key = ObjectIdentifier(touch)
        let host = hostPoint(from: touch.location(in: self))
        switch kind {
        case .down:
            let button = Self.gsButton(for: event?.buttonMask ?? .primary)
            pointerButtons[key] = button
            if let host { onPointerMoveAbs?(host) } // place the cursor, then press
            onPointerButton?(button, true)
        case .move:
            if let host { onPointerMoveAbs?(host) }
        case .up:
            if let host { onPointerMoveAbs?(host) }
            if let button = pointerButtons.removeValue(forKey: key) {
                onPointerButton?(button, false)
            }
        }
    }

    private func forwardTouches(_ touches: Set<UITouch>, kind: TouchKind) {
        guard onTouchEvent != nil else { return }
        for touch in touches {
            let key = ObjectIdentifier(touch)
            let id: UInt32
            switch kind {
            case .down:
                id = nextFreeID()
                touchIDs[key] = id
            case .move, .up:
                guard let known = touchIDs[key] else { continue }
                id = known
            }
            if kind == .up {
                touchIDs.removeValue(forKey: key)
                onTouchEvent?(.touchUp(id: id))
                continue
            }
            guard let h = hostPoint(from: touch.location(in: self)) else { continue }
            onTouchEvent?(
                kind == .down
                    ? .touchDown(id: id, x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h)
                    : .touchMove(id: id, x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h))
        }
    }

    /// Button-less mouse/trackpad movement (no lock) → absolute cursor move.
    @objc private func handleHover(_ recognizer: UIHoverGestureRecognizer) {
        switch recognizer.state {
        case .began, .changed:
            if let h = hostPoint(from: recognizer.location(in: self)) { onPointerMoveAbs?(h) }
        default:
            break
        }
    }

    /// Trackpad / wheel scroll (no lock) → host scroll deltas. The translation is consumed
    /// each callback so the next is a fresh delta. Sign/scale are tunable (≈ one notch per
    /// ~10 pt): finger up scrolls up (host +y), x passes through — the host WHEEL convention.
    @objc private func handleScroll(_ g: UIPanGestureRecognizer) {
        guard g.state == .began || g.state == .changed else { return }
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        onScroll?(Float(t.x) * 12, Float(-t.y) * 12)
    }

    /// Map a view-space point through the aspect-fit letterbox into host-mode pixels; points
    /// outside the video area clamp onto its edge. nil until a mode is negotiated.
    private func hostPoint(from p: CGPoint) -> HostPoint? {
        guard let hostMode = currentHostMode?(), hostMode.width > 0, hostMode.height > 0
        else { return nil }
        let video = AVMakeRect(aspectRatio: hostMode, insideRect: bounds)
        guard video.width > 0, video.height > 0 else { return nil }
        let x = Int32(((p.x - video.minX) / video.width * hostMode.width)
            .rounded().clamped(to: 0...(hostMode.width - 1)))
        let y = Int32(((p.y - video.minY) / video.height * hostMode.height)
            .rounded().clamped(to: 0...(hostMode.height - 1)))
        return HostPoint(x: x, y: y, w: UInt32(hostMode.width), h: UInt32(hostMode.height))
    }

    /// `.secondary` (right button / two-finger click) → GameStream right (3); else left (1).
    private static func gsButton(for mask: UIEvent.ButtonMask) -> UInt32 {
        mask.contains(.secondary) ? 3 : 1
    }

    private func nextFreeID() -> UInt32 {
        var id: UInt32 = 0
        while touchIDs.values.contains(id) { id += 1 }
        return id
    }
    #endif
}

#if os(iOS)
extension CGFloat {
    fileprivate func clamped(to range: ClosedRange<CGFloat>) -> CGFloat {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}
#endif
#endif
