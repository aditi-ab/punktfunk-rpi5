// Per-session presenter stack shared by the macOS and iOS/tvOS stream views: stage-2 (explicit
// VTDecompressionSession decode → CAMetalLayer, driven by the hosting view's CADisplayLink) is the
// default; stage-1 (StreamPump → AVSampleBufferDisplayLayer) is the Metal-unavailable / DEBUG
// fallback. The views own the platform bits — capture, window/scale tracking, and constructing the
// display link — and delegate the shared presenter lifecycle here.
//
// Main-thread only: start/layout/stop and the display-link tick all run on the main runloop.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import QuartzCore

/// Weak-target wrapper for CADisplayLink. The link retains its target, so targeting a view or
/// presenter directly makes a `owner → link → owner` cycle that only `invalidate()` breaks — if a
/// teardown is ever missed the owner leaks and keeps ticking. The proxy is what the link retains;
/// the handler closure captures the owner `[weak]`, so the owner can deallocate and its `deinit`
/// invalidate the link.
public final class DisplayLinkProxy: NSObject {
    private let onTick: (CADisplayLink) -> Void
    public init(_ onTick: @escaping (CADisplayLink) -> Void) { self.onTick = onTick }
    @objc public func tick(_ link: CADisplayLink) { onTick(link) }
}

final class SessionPresenter {
    private var pump: StreamPump?
    private var stage2: Stage2Pipeline?
    private var stage2Link: CADisplayLink?
    private var metalLayer: CAMetalLayer?
    private var connection: PunktfunkConnection?

    /// Start the presenter for `connection`. `baseLayer` is the view's AVSampleBufferDisplayLayer:
    /// stage-1 enqueues into it; stage-2 leaves it idle and composites an opaque CAMetalLayer
    /// sublayer over it. `makeDisplayLink` supplies the platform link (macOS `NSView.displayLink`
    /// tracks the view's display; iOS/tvOS uses the plain `CADisplayLink` init) — only called when
    /// stage-2 engages. Call `layout(in:contentsScale:)` right after so the sublayer has a frame
    /// before the first tick.
    func start(
        connection: PunktfunkConnection,
        baseLayer: AVSampleBufferDisplayLayer,
        presentMeter: LatencyMeter?,
        presentTailMeter: LatencyMeter? = nil,
        makeDisplayLink: (AnyObject, Selector) -> CADisplayLink,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        stop()
        self.connection = connection

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
           let pipeline = Stage2Pipeline(
               presentMeter: presentMeter, presentTailMeter: presentTailMeter) {
            let metal = pipeline.layer
            // The opaque metal layer composites OVER the AVSampleBufferDisplayLayer base, which
            // sits idle (un-enqueued) in stage-2. contentsScale + frame are set in layout().
            baseLayer.addSublayer(metal)
            metalLayer = metal
            stage2 = pipeline
            let proxy = DisplayLinkProxy { [weak self] link in
                self?.stage2?.renderTick(
                    targetPresentNs: Stage2Pipeline.realtimeNs(
                        forDisplayLinkTimestamp: link.targetTimestamp))
            }
            let link = makeDisplayLink(proxy, #selector(DisplayLinkProxy.tick(_:)))
            link.add(to: .main, forMode: .common)
            stage2Link = link
            syncFrameRate(hz: connection.currentMode().refreshHz)
            pipeline.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        } else {
            let pump = StreamPump()
            pump.start(
                connection: connection, layer: baseLayer,
                onFrame: onFrame, onSessionEnd: onSessionEnd)
            self.pump = pump
        }
    }

    /// Ask the display link for the stream's own cadence. iOS/tvOS-only: without an explicit
    /// range, ProMotion devices cap CADisplayLink at 60 Hz (iPhones additionally need
    /// `CADisableMinimumFrameDurationOnPhone` in Info.plist), so a 120 fps stream would present
    /// at half rate with the ring silently dropping every other frame. `maximum` allows up to
    /// 120 so the system MAY tick faster than a sub-120 stream (each extra tick is a near-free
    /// empty `renderTick`, and presenting on a denser grid shortens the decode→glass wait); the
    /// macOS NSView link already tracks its display and must NOT be capped to the stream rate.
    /// Re-applied from `layout` so a mid-session `Reconfigure` picks up a new refresh.
    private func syncFrameRate(hz: UInt32) {
        #if !os(macOS)
        guard hz > 0, let link = stage2Link else { return }
        let hzF = Float(hz)
        if link.preferredFrameRateRange.preferred != hzF {
            link.preferredFrameRateRange = CAFrameRateRange(
                minimum: min(30, hzF), maximum: max(hzF, 120), preferred: hzF)
        }
        #endif
    }

    /// Position the stage-2 metal sublayer aspect-fit in the hosting view (the host streams at the
    /// client's native mode, so this is usually the full bounds; it letterboxes a resized window).
    /// The layer FRAME + contentsScale set here are what the presenter sizes its drawable from
    /// (frame × scale) — the shader then performs the decoded→on-screen scale (bicubic luma), so a
    /// native-mode session stays pixel-exact 1:1 and a mismatched window beats the compositor's
    /// bilinear. No-op for stage-1 or before start.
    func layout(in bounds: CGRect, contentsScale: CGFloat) {
        guard let metalLayer, let connection else { return }
        let mode = connection.currentMode()
        syncFrameRate(hz: mode.refreshHz) // track a mid-session Reconfigure's new refresh
        let fit: CGRect = (mode.width > 0 && mode.height > 0)
            ? AVMakeRect(
                aspectRatio: CGSize(width: Int(mode.width), height: Int(mode.height)),
                insideRect: bounds)
            : bounds
        // No implicit resize animation; contentsScale tracks the view's backing/display scale.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        metalLayer.contentsScale = contentsScale
        metalLayer.frame = fit
        CATransaction.commit()
    }

    /// Stop the active pump/pipeline (≤ one poll timeout; stage-2 joins its pump) and detach the
    /// stage-2 layer + link. Does not close the connection — that stays with whoever owns it.
    /// Idempotent.
    func stop() {
        pump?.stop()
        pump = nil
        stage2Link?.invalidate()
        stage2Link = nil
        stage2?.stop() // stops the pump (synchronous join) + drops the decode session
        stage2 = nil
        metalLayer?.removeFromSuperlayer()
        metalLayer = nil
        connection = nil
    }

    deinit {
        // The owning view's stop() normally ran already; this covers a missed teardown so the
        // display link can't keep ticking a deallocated pipeline.
        stage2Link?.invalidate()
        stage2?.stop()
        pump?.stop()
    }
}
#endif
