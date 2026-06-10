// SwiftUI presentation: AVSampleBufferDisplayLayer fed straight from the punktfunk/1 connection.
//
// Stage-1 presenter (see README): the layer accepts *compressed* HEVC sample buffers and
// does hardware decode + display itself — fastest path to pixels, IOSurface-backed
// zero-copy on Apple silicon. Stage 2 (explicit VTDecompressionSession + CAMetalLayer)
// replaces this when we start tuning frame pacing / measuring glass-to-glass.
//
// macOS-first (NSViewRepresentable); the iOS variant is the same layer under
// UIViewRepresentable.

#if os(macOS)
import AppKit
import AVFoundation
import SwiftUI

/// Hides the LOCAL cursor while streaming. The host renders its own cursor, and the local
/// one both diverges from it (the host applies acceleration/clamping to our raw deltas)
/// and can wander out of the window — a click there would focus another app. So while the
/// stream has focus we do what Moonlight does: warp the cursor into the view, freeze it
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
    private let capturesCursor: Bool
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?

    /// `onFrame`/`onSessionEnd` fire on the pump thread — hop to the main actor for UI.
    /// `capturesCursor: false` keeps the local cursor usable while UI (e.g. a trust
    /// prompt) is layered over the stream; flip it to true to enter capture.
    public init(
        connection: PunktfunkConnection,
        capturesCursor: Bool = true,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil
    ) {
        self.connection = connection
        self.capturesCursor = capturesCursor
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
    }

    public func makeNSView(context: Context) -> StreamLayerView {
        let view = StreamLayerView()
        view.capturesCursor = capturesCursor
        view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return view
    }

    public func updateNSView(_ view: StreamLayerView, context: Context) {
        view.capturesCursor = capturesCursor
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
    /// Cancellation handle owned by exactly one pump thread — a restart hands the old pump
    /// its own token, so it can never be revived by a newer start().
    private final class PumpToken: @unchecked Sendable {
        private let lock = NSLock()
        private var live = true
        var isLive: Bool {
            lock.lock()
            defer { lock.unlock() }
            return live
        }
        func cancel() {
            lock.lock()
            live = false
            lock.unlock()
        }
    }

    private let displayLayer = AVSampleBufferDisplayLayer()
    private var token: PumpToken?
    public private(set) var connection: PunktfunkConnection?
    private let cursorCapture = CursorCapture()
    private var appObservers: [NSObjectProtocol] = []

    /// Main-thread only. False = leave the local cursor alone (UI layered over the
    /// stream); switching back to true re-enters capture immediately.
    public var capturesCursor = true {
        didSet {
            if capturesCursor {
                captureCursorIfStreaming()
            } else {
                cursorCapture.release()
            }
        }
    }

    public override init(frame: NSRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        layer = displayLayer // layer-hosting: assign before wantsLayer
        wantsLayer = true
        // The cursor comes back whenever the app loses focus (Cmd+Tab is the escape
        // hatch) and is re-captured when the stream regains it.
        appObservers.append(NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.cursorCapture.release()
        })
        appObservers.append(NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.captureCursorIfStreaming()
        })
    }

    public required init?(coder: NSCoder) { fatalError("not used") }

    public override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window == nil {
            cursorCapture.release()
        } else {
            captureCursorIfStreaming()
        }
    }

    private func captureCursorIfStreaming() {
        guard capturesCursor, token != nil, NSApp.isActive else { return }
        cursorCapture.capture(in: self)
    }

    /// Pump thread: pull AUs from the connection, wrap, enqueue. The first IDR yields the
    /// format description; non-IDR AUs before it are dropped (the host opens with an IDR).
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil
    ) {
        stop()
        let token = PumpToken()
        self.token = token
        self.connection = connection
        let layer = displayLayer
        layer.flush() // drop any frames a previous connection left queued

        let thread = Thread {
            var format: CMVideoFormatDescription?
            while token.isLive {
                do {
                    guard let au = try connection.nextAU(timeoutMs: 100) else { continue }
                    onFrame?(au)
                    if let f = AnnexB.formatDescription(fromIDR: au.data) {
                        format = f // refreshed on every IDR (mode changes included)
                    }
                    if layer.status == .failed {
                        // Decode wedged: flush and re-gate on the next in-band parameter
                        // sets — resuming with a delta frame can't recover. (A
                        // request-IDR channel on punktfunk/1 is a host-side TODO; with the
                        // host's infinite GOP this may otherwise stay black until the
                        // next recovery keyframe.)
                        layer.flush()
                        format = AnnexB.formatDescription(fromIDR: au.data)
                    }
                    guard let f = format,
                          let sample = AnnexB.sampleBuffer(au: au, format: f),
                          token.isLive // don't enqueue a stale frame after a restart
                    else { continue }
                    layer.enqueue(sample)
                } catch {
                    if token.isLive {
                        onSessionEnd?()
                    }
                    break // session closed
                }
            }
        }
        thread.name = "punktfunk-pump"
        thread.qualityOfService = .userInteractive
        thread.start()
        captureCursorIfStreaming()
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection — that stays with
    /// whoever owns it (PunktfunkConnection.close() is safe alongside a draining pump).
    public func stop() {
        cursorCapture.release()
        token?.cancel()
        token = nil
        connection = nil
    }

    deinit {
        appObservers.forEach(NotificationCenter.default.removeObserver(_:))
        token?.cancel()
    }
}
#endif
