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
// starts, ⌘⎋ toggles and ⌃⌥⇧Q releases (both detected from the HID stream; there is no NSEvent
// monitor here). ⌃⌥⇧Q is the cross-client Ctrl+Alt+Shift+Q — it un-captures so the Magic Keyboard
// trackpad drives the local iPad UI again.
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
#if os(tvOS)
import AVKit // AVDisplayManager — the per-session display-mode (HDR10/refresh) request
#endif

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
    private let onResizeTarget: ((UInt32, UInt32) -> Void)?
    private let onDecodedSize: (@Sendable (Int, Int) -> Void)?
    private let endToEndMeter: LatencyMeter?
    private let decodeMeter: LatencyMeter?
    private let displayMeter: LatencyMeter?

    /// `onDisconnectRequest` exists for call-site parity with the macOS StreamView (the
    /// captured-state ⌃⌥⇧D combo is detected by the macOS NSEvent monitor only); on iOS a
    /// hardware keyboard reaches Disconnect through the Stream menu's key equivalent instead,
    /// so the parameter is accepted and unused here.
    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onDisconnectRequest: (() -> Void)? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil,
        onResizeTarget: ((UInt32, UInt32) -> Void)? = nil,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        endToEndMeter: LatencyMeter? = nil,
        decodeMeter: LatencyMeter? = nil,
        displayMeter: LatencyMeter? = nil
    ) {
        self.connection = connection
        self.captureEnabled = captureEnabled
        self.onCaptureChange = onCaptureChange
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
        self.onResizeTarget = onResizeTarget
        self.onDecodedSize = onDecodedSize
        self.endToEndMeter = endToEndMeter
        self.decodeMeter = decodeMeter
        self.displayMeter = displayMeter
    }

    public func makeUIViewController(context: Context) -> StreamViewController {
        let controller = StreamViewController()
        controller.onCaptureChange = onCaptureChange
        controller.captureEnabled = captureEnabled
        controller.endToEndMeter = endToEndMeter
        controller.decodeMeter = decodeMeter
        controller.displayMeter = displayMeter
        controller.onResizeTarget = onResizeTarget
        controller.onDecodedSize = onDecodedSize
        controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return controller
    }

    public func updateUIViewController(_ controller: StreamViewController, context: Context) {
        controller.onCaptureChange = onCaptureChange
        controller.captureEnabled = captureEnabled
        controller.endToEndMeter = endToEndMeter
        controller.decodeMeter = decodeMeter
        controller.displayMeter = displayMeter
        controller.onResizeTarget = onResizeTarget
        controller.onDecodedSize = onDecodedSize
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

#if os(tvOS)
/// tvOS: a GCEventViewController with `controllerUserInteractionEnabled = false` routes game-
/// controller (and Siri Remote) input EXCLUSIVELY to the GameController framework while the
/// stream is up. Without it a pad's B/Menu press doubles as a UIKit menu press — which ended
/// the session (or suspended the whole app) from ordinary gameplay; a SwiftUI
/// `.onExitCommand {}` swallow proved unreliable with nothing focusable on screen. Every
/// in-session exit is GC-level by design: the pad's escape chord (GamepadCapture) and the
/// remote's hold-Back (SiriRemotePointer).
public typealias StreamViewControllerBase = GCEventViewController
#else
public typealias StreamViewControllerBase = UIViewController
#endif

public final class StreamViewController: StreamViewControllerBase {
    public private(set) var connection: PunktfunkConnection?
    private var observers: [NSObjectProtocol] = []
    /// Record the unified latency stages (end-to-end / decode / display) when the stage-2
    /// presenter is active. Consulted at start().
    var endToEndMeter: LatencyMeter?
    var decodeMeter: LatencyMeter?
    var displayMeter: LatencyMeter?
    /// The shared presenter stack: stage-2 (CAMetalLayer sublayer + display link) with the
    /// stage-1 StreamPump → displayLayer path as the Metal-unavailable / DEBUG fallback.
    private let presenter = SessionPresenter()
    #if os(tvOS)
    /// The window's display manager the session's mode request was set on — held weakly so
    /// stop() can clear the request even after the view has left the window.
    private weak var sessionDisplayManager: AVDisplayManager?
    #endif
    #if os(iOS)
    private var inputCapture: InputCapture?
    fileprivate var captured = false
    private var pointerInteraction: UIPointerInteraction?
    /// Capture state at the last resign, restored on the next foreground — otherwise the
    /// mouse/keyboard stay released after navigating out and nothing re-grabs them.
    private var wasCapturedOnResign = false
    /// Match-window resize follower (C3) — non-nil while a session is active AND the `matchWindow`
    /// setting is on (DEFAULT on, for pixel-exact scene streaming); fed the view's physical-pixel
    /// size from `viewDidLayoutSubviews` so an iPad Stage Manager / Split View scene resize
    /// renegotiates the host mode (1:1, no presenter resample). iOS only (iPhone naturally no-ops
    /// its fixed full-screen scene; tvOS drives display modes via AVDisplayManager instead).
    private var matchFollower: MatchWindowFollower?
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
    /// Resize-overlay START: forwarded to the Match-window follower so a scene resize drives the
    /// blur+spinner the instant the window differs from the live mode (iOS only — tvOS has no
    /// follower). See `MatchWindowFollower.onResizeTarget`.
    var onResizeTarget: ((UInt32, UInt32) -> Void)?
    /// Resize-overlay END: the presenter reports the coded dims of each new-mode IDR here, so the
    /// overlay clears when a frame at the requested size actually decodes.
    var onDecodedSize: (@Sendable (Int, Int) -> Void)?
    /// Last decoded size fed into the presenter's aspect-fit. A new-mode IDR (an iPad scene resize,
    /// or a tvOS AVDisplayManager mode switch) re-fits the metal sublayer to the REAL content aspect
    /// here — `viewDidLayoutSubviews` only re-runs on a bounds change, which a resize-END lacks, so
    /// without this the layer keeps its pre-resize aspect and stretches the new frame into it. Main.
    private var lastDecodedContentSize: CGSize?

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
        #if os(tvOS)
        // Kill the pad/remote → UIKit press path at the source for the whole session (see the
        // GCEventViewController typealias above). GC delivery is untouched: GamepadCapture
        // forwards the pad, SiriRemotePointer drives the pointer and owns the remote exit.
        controllerUserInteractionEnabled = false
        #endif
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

    #if os(tvOS)
    // The GCEventViewController's interaction flag applies to the deepest such controller
    // CONTAINING THE FIRST RESPONDER — inside SwiftUI's hosting-controller sandwich that is not
    // guaranteed to be us unless we anchor the responder chain here explicitly.
    public override var canBecomeFirstResponder: Bool { true }

    public override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        becomeFirstResponder()
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
            guard let self else { return }
            // Released → a trackpad/mouse click into the video RE-ENGAGES capture (the iPad
            // analogue of macOS's `mouseDown → engageCapture(fromClick:)`, and the click-mirror of
            // the ⌘⎋ / ⌃⌥⇧Q keyboard toggles). Only the button-DOWN engages; that click is the local
            // engage gesture, so it's suppressed toward the host (`fromClick`) and never forwarded —
            // its release is swallowed by InputCapture's suppress latch, whichever path delivers it.
            // (Finger taps are untouched: touch always plays directly, so only the indirect pointer
            // re-captures.) Captured already → the absolute path forwards the button as before.
            if !self.captured {
                if down, self.captureEnabled { self.setCaptured(true, fromClick: true) }
                return
            }
            guard self.inputCapture?.gcMouseForwarding == false else { return }
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
        // ⌃⌥⇧Q (cross-client parity with macOS/Windows/Linux) releases the captured pointer +
        // keyboard so the Magic Keyboard trackpad returns to driving the local iPad UI. Detected
        // from the HID stream in InputCapture (no NSEvent monitor on iOS); unlike the ⌘⎋ toggle it
        // only ever RELEASES — re-pressing it while already released is a no-op (setCaptured guards).
        capture.onReleaseCapture = { [weak self] in
            self?.setCaptured(false)
        }
        capture.onPreempted = { [weak self] in
            self?.setCaptured(false)
        }
        capture.start()
        inputCapture = capture
        // Match-window (C3): when ON, follow the scene's pixel size so a resizable iPad scene
        // streams 1:1 (pixel-exact) instead of the presenter resampling a fixed-mode frame into it.
        // `viewDidLayoutSubviews` feeds it — covers Stage Manager / Split View resizes and rotation.
        // iPhone is a fixed full-screen scene, so this naturally no-ops (reports the device mode).
        // OPT-IN — `?? false` matches the Settings toggle (which also defaults off); an unset
        // default keeps the explicit mode.
        let follower = MatchWindowFollower(
            connection: connection,
            enabled: UserDefaults.standard.object(forKey: DefaultsKey.matchWindow) as? Bool ?? false)
        follower.onResizeTarget = onResizeTarget
        matchFollower = follower
        #endif

        // Presenter choice + lifecycle live in SessionPresenter (shared with macOS): stage-2
        // (explicit VTDecompressionSession decode + a CAMetalLayer/display-link present) by
        // default, the stage-1 pump as the Metal-missing / DEBUG fallback.
        // Intercept the pump's coded-dims callback: re-fit the metal sublayer to the real content
        // aspect (main thread) BEFORE forwarding to the owner's overlay END-signal. Fires only on a
        // size CHANGE (first frame + each resolved resize), so this is rare, not per-frame.
        let overlayDecodedSize = onDecodedSize
        presenter.start(
            connection: connection,
            baseLayer: streamView.displayLayer,
            endToEndMeter: endToEndMeter,
            decodeMeter: decodeMeter,
            displayMeter: displayMeter,
            makeDisplayLink: { CADisplayLink(target: $0, selector: $1) },
            onFrame: onFrame,
            onSessionEnd: onSessionEnd,
            onDecodedSize: { [weak self] w, h in
                DispatchQueue.main.async { self?.noteDecodedContentSize(width: w, height: h) }
                overlayDecodedSize?(w, h)
            })
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
        // The Stream menu's "Release Mouse" (⌃⌥⇧Q) posts this — the discoverable menu surface for
        // the RELEASED state. While CAPTURED the combo is recognized from the HID stream in
        // InputCapture (onReleaseCapture) before the menu sees it, so in practice this fires as a
        // not-captured no-op (setCaptured guards it); wired for honesty + a non-GC fallback. Only the
        // foreground-active scene's stream acts — the iPad analogue of macOS's key-window guard, so a
        // second Stage Manager scene isn't released out from under the user.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkReleaseCapture, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self,
                  self.view.window?.windowScene?.activationState == .foregroundActive else { return }
            self.setCaptured(false)
        })

        if captureEnabled {
            setCaptured(true) // entering a session is the deliberate "capture me" moment
        }
        #endif

        #if os(tvOS)
        // The TV's mode switch (requested in applyDisplayCriteriaIfNeeded) completes
        // asynchronously, and a dynamic-range-only switch doesn't re-layout by itself —
        // re-layout on the switch/mode notifications so the presenter sees the new EDR
        // headroom immediately (layout pushes UIScreen.currentEDRHeadroom down).
        observers.append(NotificationCenter.default.addObserver(
            forName: .AVDisplayManagerModeSwitchEnd, object: nil, queue: .main
        ) { [weak self] _ in self?.layoutMetalLayer() })
        observers.append(NotificationCenter.default.addObserver(
            forName: UIScreen.modeDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.layoutMetalLayer() })
        #endif
    }

    func stop() {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        #if os(iOS)
        setCaptured(false)
        inputCapture?.stop()
        inputCapture = nil
        // Release anything the touch-driven mouse still holds (a mid-drag session end) while
        // onTouchEvent can still deliver the button-up.
        streamView.resetTouchInput()
        streamView.onTouchEvent = nil
        streamView.onPointerMoveAbs = nil
        streamView.onPointerButton = nil
        streamView.onScroll = nil
        streamView.currentHostMode = nil
        matchFollower = nil
        #endif
        #if os(tvOS)
        // Return the TV to the user's preferred mode — the home screen must not stay in the
        // session's HDR10/refresh mode.
        sessionDisplayManager?.preferredDisplayCriteria = nil
        sessionDisplayManager = nil
        #endif
        presenter.stop()
        lastDecodedContentSize = nil // the next session re-derives it from its first frame
        connection = nil
    }

    public override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        layoutMetalLayer()
        #if os(iOS)
        // Match-window (C3): feed the follower the view's physical-pixel size (points × scale).
        let b = streamView.bounds
        if b.width > 0, b.height > 0 {
            let scale = renderScale
            matchFollower?.noteSize(
                widthPx: Int((b.width * scale).rounded()),
                heightPx: Int((b.height * scale).rounded()))
        }
        #endif
        #if os(tvOS)
        applyDisplayCriteriaIfNeeded()
        #endif
    }

    #if os(tvOS)
    /// Ask the TV for a display mode matching the session — HDR10 at the stream's refresh rate —
    /// via AVDisplayManager, the tvOS mechanism custom renderers use for HDR output (AVFoundation
    /// playback layers do this implicitly). Honored only when the user allows matching (tvOS
    /// Settings → Video and Audio → Match Content); the presenter reads the RESULT off UIScreen's
    /// EDR headroom (pushed in SessionPresenter.layout) and keeps the in-shader tone-map whenever
    /// the switch never lands, so an SDR-composited display can't show blown-out PQ either way.
    /// Applied once per session, as soon as the window and the negotiated mode both exist; the
    /// stop() teardown clears it.
    private func applyDisplayCriteriaIfNeeded() {
        guard let manager = view.window?.avDisplayManager, let connection,
              manager.preferredDisplayCriteria == nil,
              UserDefaults.standard.object(forKey: DefaultsKey.hdrEnabled) as? Bool ?? true
        else { return }
        let mode = connection.currentMode()
        guard mode.width > 0, mode.height > 0, mode.refreshHz > 0 else { return }
        // A synthetic HDR10-HEVC format description carrying the negotiated mode — what the
        // stream decodes to. AVDisplayCriteria(refreshRate:formatDescription:) matches the
        // display to it (tvOS 17+, our deployment floor).
        let ext: [CFString: Any] = [
            kCMFormatDescriptionExtension_ColorPrimaries:
                kCMFormatDescriptionColorPrimaries_ITU_R_2020,
            kCMFormatDescriptionExtension_TransferFunction:
                kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ,
            kCMFormatDescriptionExtension_YCbCrMatrix:
                kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
        ]
        var desc: CMFormatDescription?
        CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, codecType: kCMVideoCodecType_HEVC,
            width: Int32(mode.width), height: Int32(mode.height),
            extensions: ext as CFDictionary, formatDescriptionOut: &desc)
        guard let desc else { return }
        manager.preferredDisplayCriteria = AVDisplayCriteria(
            refreshRate: Float(mode.refreshHz), formatDescription: desc)
        sessionDisplayManager = manager
    }
    #endif

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

    /// A new decoded size landed (a scene/mode resize's new IDR, or the first frame): push it to the
    /// presenter's aspect-fit and re-layout NOW. A resize-END triggers no `viewDidLayoutSubviews`, so
    /// this is what makes the metal sublayer track the new content aspect instead of stretching the
    /// new frame into the pre-resize box. Deduped so a same-size repeat is a no-op. Main thread.
    private func noteDecodedContentSize(width: Int, height: Int) {
        let size = CGSize(width: width, height: height)
        guard size.width > 0, size.height > 0, size != lastDecodedContentSize else { return }
        lastDecodedContentSize = size
        presenter.setContentSize(size)
        layoutMetalLayer()
    }

    #if os(iOS)
    /// `fromClick` marks a click-driven engage (the released-state pointer click that re-captures):
    /// that click's press/release are suppressed toward the host — it's the local engage gesture,
    /// not a host click — exactly as macOS's `engageCapture(fromClick:)` does. Keyboard-driven
    /// engages (⌘⎋) pass false so a normal click still reaches the host.
    private func setCaptured(_ on: Bool, fromClick: Bool = false) {
        if on {
            // `connection != nil` is the session-active gate (presenter internals are opaque here).
            guard captureEnabled, !captured, connection != nil else { return }
            inputCapture?.setForwarding(true, suppressClick: fromClick)
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
    /// Direct fingers / Pencil → wire events: real touches in passthrough mode, or the
    /// touch-driven mouse events (`TouchMouse`) in the trackpad/pointer modes.
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
    /// Touch-driven mouse for the trackpad/pointer `TouchInputMode`s (see TouchMouse.swift).
    private lazy var touchMouse: TouchMouse = {
        let mouse = TouchMouse()
        mouse.send = { [weak self] event in self?.onTouchEvent?(event) }
        mouse.hostPoint = { [weak self] point in self?.hostPoint(from: point) }
        return mouse
    }()
    /// The finger route latched at gesture start — a Settings change mid-gesture applies to
    /// the NEXT touch, so one gesture never splits across input models.
    private var fingerRoute: TouchInputMode?

    /// Release anything the touch-driven mouse holds and forget gesture state — session stop.
    func resetTouchInput() {
        touchMouse.reset()
        fingerRoute = nil
    }
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
        route(touches, event: event, kind: .cancel)
    }

    private enum TouchKind { case down, move, up, cancel }

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
        if !fingers.isEmpty { forwardFingers(fingers, kind: kind) }
    }

    /// Route direct fingers by the touch-input model, latched for the whole gesture:
    /// passthrough → real wire touches; trackpad/pointer → the TouchMouse gesture engine.
    private func forwardFingers(_ touches: Set<UITouch>, kind: TouchKind) {
        let mode = fingerRoute ?? TouchInputMode.current
        fingerRoute = mode
        switch mode {
        case .touch:
            // A cancellation lifts the wire touch like a normal up — the host just sees the
            // contact end.
            forwardTouches(touches, kind: kind == .cancel ? .up : kind)
        case .trackpad, .pointer:
            switch kind {
            case .down: touchMouse.began(touches, in: self, trackpad: mode == .trackpad)
            case .move: touchMouse.moved(touches, in: self)
            case .up: touchMouse.ended(touches, in: self)
            case .cancel: touchMouse.cancelled(touches)
            }
        }
        if touchIDs.isEmpty, touchMouse.isIdle { fingerRoute = nil }
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
        case .up, .cancel:
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
            case .move, .up, .cancel:
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
