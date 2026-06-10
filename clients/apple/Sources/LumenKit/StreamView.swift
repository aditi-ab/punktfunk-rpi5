// SwiftUI presentation: AVSampleBufferDisplayLayer fed straight from the lumen/1 connection.
//
// Stage-1 presenter (see README): the layer accepts *compressed* HEVC sample buffers and
// does hardware decode + display itself — fastest path to pixels, IOSurface-backed
// zero-copy on Apple silicon. Stage 2 (explicit VTDecompressionSession + CAMetalLayer)
// replaces this when we start tuning frame pacing / measuring glass-to-glass.
//
// SCAFFOLD: written on the Linux host, not yet compiled against Xcode. macOS-first
// (NSViewRepresentable); the iOS variant is the same layer under UIViewRepresentable.

#if os(macOS)
import AVFoundation
import SwiftUI

public struct StreamView: NSViewRepresentable {
    private let connection: LumenConnection

    public init(connection: LumenConnection) {
        self.connection = connection
    }

    public func makeNSView(context: Context) -> StreamLayerView {
        let view = StreamLayerView()
        view.start(connection: connection)
        return view
    }

    public func updateNSView(_ view: StreamLayerView, context: Context) {}
}

public final class StreamLayerView: NSView {
    private let displayLayer = AVSampleBufferDisplayLayer()
    private var pump: Thread?
    private var running = false

    public override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        displayLayer.videoGravity = .resizeAspect
        layer = displayLayer
    }

    public required init?(coder: NSCoder) { fatalError("not used") }

    /// Pump thread: pull AUs from the connection, wrap, enqueue. The first IDR yields the
    /// format description; non-IDR AUs before it are dropped (the host opens with an IDR).
    public func start(connection: LumenConnection) {
        guard !running else { return }
        running = true
        let layer = displayLayer
        let thread = Thread { [weak self] in
            var format: CMVideoFormatDescription?
            while self?.running == true {
                do {
                    guard let au = try connection.nextAU(timeoutMs: 100) else { continue }
                    if let f = AnnexB.formatDescription(fromIDR: au.data) {
                        format = f // refreshed on every IDR (mode changes included)
                    }
                    guard let f = format,
                          let sample = AnnexB.sampleBuffer(au: au, format: f)
                    else { continue }
                    if layer.status == .failed {
                        layer.flush()
                    }
                    layer.enqueue(sample)
                } catch {
                    break // session closed
                }
            }
        }
        thread.name = "lumen-pump"
        thread.qualityOfService = .userInteractive
        pump = thread
        thread.start()
    }

    public func stop() {
        running = false
    }

    deinit { running = false }
}
#endif
