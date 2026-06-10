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
import AVFoundation
import SwiftUI

public struct StreamView: NSViewRepresentable {
    private let connection: PunktfunkConnection
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?

    /// `onFrame`/`onSessionEnd` fire on the pump thread — hop to the main actor for UI.
    public init(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil
    ) {
        self.connection = connection
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
    }

    public func makeNSView(context: Context) -> StreamLayerView {
        let view = StreamLayerView()
        view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return view
    }

    public func updateNSView(_ view: StreamLayerView, context: Context) {
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

    public override init(frame: NSRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        layer = displayLayer // layer-hosting: assign before wantsLayer
        wantsLayer = true
    }

    public required init?(coder: NSCoder) { fatalError("not used") }

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
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection — that stays with
    /// whoever owns it (PunktfunkConnection.close() is safe alongside a draining pump).
    public func stop() {
        token?.cancel()
        token = nil
        connection = nil
    }

    deinit {
        token?.cancel()
    }
}
#endif
