// iOS/iPadOS presenter: the same AVSampleBufferDisplayLayer + StreamPump as macOS,
// hosted in a UIViewController so the scene can pointer-lock (the iPadOS equivalent of
// the Mac's cursor capture — with a hardware mouse/trackpad the system cursor is hidden
// and GCMouse's raw deltas drive the host cursor alone; the system only honors the lock
// fullscreen-and-frontmost, so in Stage Manager it degrades to Mac-style "both cursors
// visible" forwarding).
//
// Touch is the primary input and is always forwarded (touching the video IS explicit
// intent): every finger maps to a wire touch id, coordinates are mapped through the
// aspect-fit letterbox into host-mode pixels, so surface == host mode and the host's
// rescale is the identity. Hardware keyboard/mouse forwarding shares InputCapture with
// macOS — auto-engaged when streaming starts, ⌘⎋ toggles (detected from the HID stream;
// there is no NSEvent monitor here).
//
// The public type is named StreamView like its macOS twin (each is platform-gated), so
// the SwiftUI app layer is identical on both platforms.

#if os(iOS)
import AVFoundation
import GameController
import PunktfunkCore
import SwiftUI
import UIKit

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
    private var inputCapture: InputCapture?
    private var captured = false
    private var observers: [NSObjectProtocol] = []

    var onCaptureChange: ((Bool) -> Void)?

    var captureEnabled = true {
        didSet {
            guard captureEnabled != oldValue else { return }
            setCaptured(captureEnabled)
        }
    }

    private var streamView: StreamLayerUIView {
        // swiftlint:disable:next force_cast
        view as! StreamLayerUIView
    }

    public override func loadView() {
        view = StreamLayerUIView()
    }

    public override var prefersPointerLocked: Bool { captured }
    public override var prefersHomeIndicatorAutoHidden: Bool { true }

    func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        stop()
        self.connection = connection
        loadViewIfNeeded()
        // Read the LIVE mode per touch batch — an accepted requestMode() mid-stream
        // changes the letterbox, and touches must follow it.
        streamView.currentHostMode = { [weak connection] in
            guard let connection else { return .zero }
            let mode = connection.currentMode()
            return CGSize(width: Double(mode.width), height: Double(mode.height))
        }
        streamView.onTouchEvent = { [weak connection] event in
            connection?.send(event)
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

        let pump = StreamPump()
        pump.start(
            connection: connection, layer: streamView.displayLayer,
            onFrame: onFrame, onSessionEnd: onSessionEnd)
        self.pump = pump

        // GC only delivers while active; everything held is flushed by InputCapture's
        // own resign observer — here we just mirror the capture state for the HUD and
        // the pointer lock.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.setCaptured(false)
        })

        if captureEnabled {
            setCaptured(true) // entering a session is the deliberate "capture me" moment
        }
    }

    func stop() {
        setCaptured(false)
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        inputCapture?.stop()
        inputCapture = nil
        pump?.stop()
        pump = nil
        connection = nil
        streamView.onTouchEvent = nil
        streamView.currentHostMode = nil
    }

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
        let onCaptureChange = onCaptureChange
        let captured = captured
        DispatchQueue.main.async { onCaptureChange?(captured) }
    }

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        pump?.stop()
    }
}

/// The layer-backed video surface + touch source. Touches are mapped through the
/// aspect-fit letterbox into host-mode pixels (surface == host mode, so the host-side
/// rescale is the identity); touches outside the video area are clamped onto its edge.
final class StreamLayerUIView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    var displayLayer: AVSampleBufferDisplayLayer {
        // swiftlint:disable:next force_cast
        layer as! AVSampleBufferDisplayLayer
    }

    /// Reads the LIVE negotiated mode in pixels (the touch coordinate space).
    var currentHostMode: (() -> CGSize)?
    var onTouchEvent: ((PunktfunkInputEvent) -> Void)?

    /// Wire touch ids per active UITouch; ids are reused after the touch ends.
    private var touchIDs: [ObjectIdentifier: UInt32] = [:]

    override init(frame: CGRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        isMultipleTouchEnabled = true
        backgroundColor = .black
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        forward(touches, kind: .down)
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        forward(touches, kind: .move)
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        forward(touches, kind: .up)
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        forward(touches, kind: .up)
    }

    private enum TouchKind { case down, move, up }

    private func forward(_ touches: Set<UITouch>, kind: TouchKind) {
        guard let hostMode = currentHostMode?(),
              hostMode.width > 0, hostMode.height > 0, onTouchEvent != nil
        else { return }
        let video = AVMakeRect(aspectRatio: hostMode, insideRect: bounds)
        guard video.width > 0, video.height > 0 else { return }
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
            let p = touch.location(in: self)
            let x = Int32(((p.x - video.minX) / video.width * hostMode.width)
                .rounded().clamped(to: 0...(hostMode.width - 1)))
            let y = Int32(((p.y - video.minY) / video.height * hostMode.height)
                .rounded().clamped(to: 0...(hostMode.height - 1)))
            let w = UInt32(hostMode.width)
            let h = UInt32(hostMode.height)
            onTouchEvent?(
                kind == .down
                    ? .touchDown(id: id, x: x, y: y, surfaceWidth: w, surfaceHeight: h)
                    : .touchMove(id: id, x: x, y: y, surfaceWidth: w, surfaceHeight: h))
        }
    }

    private func nextFreeID() -> UInt32 {
        var id: UInt32 = 0
        while touchIDs.values.contains(id) { id += 1 }
        return id
    }
}

extension CGFloat {
    fileprivate func clamped(to range: ClosedRange<CGFloat>) -> CGFloat {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}
#endif
