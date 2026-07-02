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
// (full-screen + frontmost iPad, and the user hasn't disabled pointer capture in Settings —
// see PointerLockChain, which steers the lock request through SwiftUI's hosting controllers)
// GCMouse delivers raw relative deltas and the system hides the cursor — the gaming-grade path.
// InputCapture handles EVERY connected mouse (GCMouse.mice), not just the current one, so a
// trackpad + a second pointer (e.g. a Universal Control mouse) both drive. When the scene CAN'T
// lock (Stage Manager, not frontmost, iPhone, capture disabled) the system shows its own cursor
// and routes the mouse through UIKit's pointer path: hover + indirect-pointer touches, which we
// forward as ABSOLUTE cursor position (+ buttons) so the host cursor tracks the visible local one.
// We never forward an indirect pointer as a touch — doing so hid the cursor and made the host see
// taps instead of a moving mouse. The two paths are mutually exclusive on `gcMouseForwarding`
// (== locked): GCMouse forwards only WHILE locked, the UIKit indirect path (motion, buttons AND
// scroll) only while NOT locked — so a pointer that emits both channels under lock can't double-send.
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
    private let presentMeter: LatencyMeter?
    private let presentTailMeter: LatencyMeter?

    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil,
        presentMeter: LatencyMeter? = nil,
        presentTailMeter: LatencyMeter? = nil
    ) {
        self.connection = connection
        self.captureEnabled = captureEnabled
        self.onCaptureChange = onCaptureChange
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
        self.presentMeter = presentMeter
        self.presentTailMeter = presentTailMeter
    }

    public func makeUIViewController(context: Context) -> StreamViewController {
        let controller = StreamViewController()
        controller.onCaptureChange = onCaptureChange
        controller.captureEnabled = captureEnabled
        controller.presentMeter = presentMeter
        controller.presentTailMeter = presentTailMeter
        controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return controller
    }

    public func updateUIViewController(_ controller: StreamViewController, context: Context) {
        controller.onCaptureChange = onCaptureChange
        controller.captureEnabled = captureEnabled
        controller.presentMeter = presentMeter
        controller.presentTailMeter = presentTailMeter
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
    private var observers: [NSObjectProtocol] = []
    /// Record capture→present / decode→present when the stage-2 presenter is active.
    /// Consulted at start().
    var presentMeter: LatencyMeter?
    var presentTailMeter: LatencyMeter?
    /// The shared presenter stack: stage-2 (CAMetalLayer sublayer + display link) with the
    /// stage-1 StreamPump → displayLayer path as the Metal-unavailable / DEBUG fallback.
    private let presenter = SessionPresenter()
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
        // Re-size the stage-2 drawable if the display scale changes without a bounds change (e.g.
        // moving to an external display at a different scale) — the iOS analogue of macOS's
        // viewDidChangeBackingProperties relayout. The handler takes the VC as its argument, so it
        // doesn't capture self (no retain cycle with the registration).
        registerForTraitChanges([UITraitDisplayScale.self]) { (vc: StreamViewController, _) in
            vc.layoutMetalLayer()
        }
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
    /// Whether the user wants the mouse/trackpad pointer CAPTURED (pointer lock → relative
    /// movement, the gaming default) rather than forwarded as an absolute position (desktop
    /// use). Read live from UserDefaults so it tracks the Settings toggle; defaults to on when
    /// unset. iPad-only — gated again in `prefersPointerLocked`.
    private var pointerCaptureEnabled: Bool {
        UserDefaults.standard.object(forKey: DefaultsKey.pointerCapture) as? Bool ?? true
    }

    /// Whether the pointer should be CAPTURED right now: iPad, capture engaged, and the user
    /// hasn't opted into the absolute (desktop) pointer. The system additionally requires
    /// full-screen + frontmost and may drop the lock (Slide Over/Stage Manager/backgrounding) —
    /// syncPointerLock() handles the actual grant/drop and falls back to absolute when unlocked.
    private var wantsPointerLock: Bool {
        captured && pointerCaptureEnabled && UIDevice.current.userInterfaceIdiom == .pad
    }

    public override var prefersPointerLocked: Bool { wantsPointerLock }
    public override var prefersHomeIndicatorAutoHidden: Bool { true }

    // NOTE: we deliberately do NOT override `childViewControllerForPointerLock`. The default
    // returns nil, which tells the system to use THIS controller's own `prefersPointerLocked` —
    // exactly what we want, since `PointerLockChain` forces our SwiftUI ancestors to forward the
    // downward walk to us and we are the terminal anchor. Returning `self` here would make the
    // system ask the same controller forever (it keeps delegating to the returned child) →
    // unbounded recursion → stack overflow once the chain actually reaches us.

    /// (Re)build or tear down the forced pointer-lock forwarding chain from this controller to the
    /// window root so the system actually resolves our `prefersPointerLocked`. Safe to call
    /// repeatedly — it no-ops until the view is in a window with a parent chain, and re-runs from
    /// the appearance/parent callbacks once SwiftUI has placed us.
    private func updatePointerLockChain() {
        // Engaging needs a live parent chain to the window root; disengaging is always safe and
        // must run even after the view has left the window (session teardown) so the stamped
        // SwiftUI ancestors are cleared.
        if wantsPointerLock, view.window != nil {
            PointerLockChain.engage(self)
        } else {
            PointerLockChain.disengage(self)
        }
    }

    public override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        // SwiftUI places us in the hierarchy AFTER start()'s setCaptured(true), and may reparent us
        // later — re-anchor the chain here so a lock requested before we had a parent still lands.
        updatePointerLockChain()
    }

    public override func didMove(toParent parent: UIViewController?) {
        super.didMove(toParent: parent)
        updatePointerLockChain() // chain shape changed — re-anchor (or no-op if not yet in a window)
    }
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
        // Fresh session: drop any resign/foreground capture-restore state left over from a
        // prior session (stop() doesn't clear it). Otherwise a stale `true` could later
        // re-engage capture on a foreground that the new session never asked for.
        wasCapturedOnResign = false
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
        // Indirect pointer (mouse/trackpad) WITHOUT a lock → absolute cursor + buttons + scroll.
        // While the scene is pointer-LOCKED the GCMouse path owns motion AND buttons AND scroll, so
        // the whole UIKit indirect path is gated off here (`gcMouseForwarding`). The trackpad and a
        // mouse BOTH report through GCMouse under lock and ALSO emit UIKit indirect-pointer events
        // (pinned at the locked position) — without this gate a click double-sends (GCMouse + UIKit)
        // and a second pointer (e.g. a Universal Control mouse) competes with the trackpad. The gate
        // is the exact mirror of the GCMouse handlers, which fire only while locked.
        streamView.onPointerMoveAbs = { [weak self] p in
            guard let self, self.inputCapture?.gcMouseForwarding == false else { return }
            self.inputCapture?.sendMouseAbs(
                x: p.x, y: p.y, surfaceWidth: p.w, surfaceHeight: p.h)
        }
        streamView.onPointerButton = { [weak self] button, down in
            guard let self, self.inputCapture?.gcMouseForwarding == false else { return }
            self.inputCapture?.sendMouseButton(button, pressed: down)
        }
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

        // Presenter choice + lifecycle live in SessionPresenter (shared with macOS): stage-2
        // (explicit VTDecompressionSession decode + a CAMetalLayer/display-link present) by
        // default, the stage-1 pump as the Metal-missing / DEBUG fallback.
        presenter.start(
            connection: connection,
            baseLayer: streamView.displayLayer,
            presentMeter: presentMeter,
            presentTailMeter: presentTailMeter,
            makeDisplayLink: { CADisplayLink(target: $0, selector: $1) },
            onFrame: onFrame,
            onSessionEnd: onSessionEnd)
        layoutMetalLayer()

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
            // inputCapture != nil: don't try to restore before this session's capture is wired
            // up — setForwarding would silently no-op on the nil handlers and leave input dead.
            guard let self, self.wasCapturedOnResign, self.captureEnabled,
                  self.connection != nil, self.inputCapture != nil
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
        presenter.stop()
        connection = nil
    }

    public override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        layoutMetalLayer()
    }

    /// The display scale to render the metal drawable at. `traitCollection.displayScale` is the
    /// canonical render scale and is reliable once the controller is in the hierarchy;
    /// `view.contentScaleFactor` can read 1.0 before the view attaches to a window/screen, which
    /// would size the drawable at point resolution → a pixelated, upscaled mess. Falls back to the
    /// main screen scale if the trait is still unspecified.
    private var renderScale: CGFloat {
        let s = traitCollection.displayScale
        return s > 0 ? s : UIScreen.main.scale
    }

    /// Aspect-fit the stage-2 metal sublayer to the view at the canonical render scale
    /// (see SessionPresenter.layout).
    private func layoutMetalLayer() {
        presenter.layout(in: streamView.bounds, contentsScale: renderScale)
    }

    #if os(iOS)
    private func setCaptured(_ on: Bool) {
        if on {
            // `connection != nil` is the session-active gate (presenter internals are opaque here).
            guard captureEnabled, !captured, connection != nil else { return }
            inputCapture?.setForwarding(true)
            captured = true
        } else {
            guard captured else { return }
            inputCapture?.setForwarding(false)
            captured = false
        }
        setNeedsUpdateOfPrefersPointerLocked()
        updatePointerLockChain() // (re)anchor the SwiftUI ancestors so the lock actually resolves
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
        presenter.stop() // invalidate the display link + stop the pipeline if stop() was missed
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
#endif
