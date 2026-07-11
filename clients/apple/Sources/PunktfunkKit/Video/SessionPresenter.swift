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
#if os(tvOS)
import UIKit
#endif

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

/// Which presenter a session runs. Stage-2/stage-3 are the same Metal pipeline with arrival vs
/// glass-gated present pacing (`PresentPacing` — see Stage2Pipeline for the tradeoff, and why
/// stage-3 exists: stage-2's present-on-arrival saturates the layer's FIFO image queue on panels
/// running near the stream rate). Stage-1 (compressed video straight to the system layer) is a
/// DEBUG-only diagnostic. Internal (not private) for unit tests.
enum PresenterChoice: Equatable {
    case stage1
    case stage2
    case stage3

    /// Resolve from the `PUNKTFUNK_PRESENTER` env override (A/B without touching settings) first,
    /// then the persisted `DefaultsKey.presenter` setting; anything unknown (or an empty env var)
    /// falls back to the platform default. `allowStage1` is false in release builds, where a
    /// leftover DEBUG "stage1" value silently maps to the default rather than reviving the
    /// freeze-prone fallback.
    static func resolve(setting: String?, env: String?, allowStage1: Bool) -> PresenterChoice {
        let raw = env.flatMap { $0.isEmpty ? nil : $0 } ?? setting
        switch raw {
        case "stage1": return allowStage1 ? .stage1 : platformDefault
        case "stage2": return .stage2
        case "stage3": return .stage3
        default: return platformDefault
        }
    }

    /// tvOS defaults to GLASS pacing: an Apple TV is the sticky-FIFO worst case by construction —
    /// a fixed 60 Hz panel fed a 60 fps stream, where arrival pacing pins the layer's image queue
    /// at ~3 drawables and every frame rides ~50 ms of queue (the measured display stage there).
    /// The Settings picker can still force stage-2 for an A/B. Everything else keeps stage-2 (the
    /// proven default; ProMotion/desktop panels out-tick the stream often enough to drain).
    static var platformDefault: PresenterChoice {
        #if os(tvOS)
        .stage3
        #else
        .stage2
        #endif
    }
}

final class SessionPresenter {
    private var pump: StreamPump?
    private var stage2: Stage2Pipeline?
    private var stage2Link: CADisplayLink?
    private var metalLayer: CAMetalLayer?
    private var connection: PunktfunkConnection?
    /// The decoded frame's REAL pixel dimensions (ground truth, pushed by the view from the pump's
    /// `onDecodedSize` new-mode-IDR callback). Used for the aspect-fit in `layout` in preference to
    /// `connection.currentMode()`, which (a) lags a mid-stream resize — it only updates on the
    /// `Reconfigured` ack, and a resize-END produces no bounds change to re-run `layout` afterward —
    /// and (b) can disagree with what the host actually DELIVERED (Windows corrective-ack falls back
    /// to an advertised mode). The pixels we're drawing are the only correct aspect source; a wrong
    /// one here is the "black bars + stretched" resize artifact. nil until the first frame → `layout`
    /// falls back to `currentMode()`. Main-thread only.
    private var contentSize: CGSize?

    /// Start the presenter for `connection`. `baseLayer` is the view's AVSampleBufferDisplayLayer:
    /// stage-1 enqueues into it; stage-2 leaves it idle and composites an opaque CAMetalLayer
    /// sublayer over it. `makeDisplayLink` supplies the platform link (macOS `NSView.displayLink`
    /// tracks the view's display; iOS/tvOS uses the plain `CADisplayLink` init) — only called when
    /// stage-2 engages. Call `layout(in:contentsScale:)` right after so the sublayer has a frame
    /// before the first tick.
    func start(
        connection: PunktfunkConnection,
        baseLayer: AVSampleBufferDisplayLayer,
        endToEndMeter: LatencyMeter?,
        decodeMeter: LatencyMeter? = nil,
        displayMeter: LatencyMeter? = nil,
        makeDisplayLink: (AnyObject, Selector) -> CADisplayLink,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil
    ) {
        stop()
        self.connection = connection

        // Presenter choice — stage-2 is the DEFAULT (explicit VTDecompressionSession decode + a
        // CAMetalLayer/display-link present): it can detect + recover a wedged decoder where
        // stage-1's AVSampleBufferDisplayLayer freezes hard on a lost HEVC reference. Stage-3 is
        // the same pipeline with glass-gated present pacing (the settings picker's live A/B — see
        // PresentPacing). Stage-1 is reachable only via the DEBUG presenter value; release maps it
        // back to stage-2 (the stage-1 pump below stays the automatic fallback if Metal is missing).
        #if DEBUG
        let allowStage1 = true
        #else
        let allowStage1 = false
        #endif
        let choice = PresenterChoice.resolve(
            setting: UserDefaults.standard.string(forKey: DefaultsKey.presenter),
            env: ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENTER"],
            allowStage1: allowStage1)
        if choice != .stage1,
           let pipeline = Stage2Pipeline(
               endToEndMeter: endToEndMeter, decodeMeter: decodeMeter,
               displayMeter: displayMeter,
               pacing: choice == .stage3 ? .glass : .arrival) {
            let metal = pipeline.layer
            // The opaque metal layer composites OVER the AVSampleBufferDisplayLayer base, which
            // sits idle (un-enqueued) in stage-2. contentsScale + frame are set in layout().
            baseLayer.addSublayer(metal)
            metalLayer = metal
            stage2 = pipeline
            // The link is the vsync CLOCK + putBack-retry nudge, not the presentation trigger
            // (frame arrival is — see Stage2Pipeline's header). timestamp→targetTimestamp is the
            // link's own report of the current refresh period (tracks VRR rate changes).
            let proxy = DisplayLinkProxy { [weak self] link in
                self?.stage2?.renderTick(
                    targetMediaTime: link.targetTimestamp,
                    period: link.targetTimestamp - link.timestamp)
            }
            let link = makeDisplayLink(proxy, #selector(DisplayLinkProxy.tick(_:)))
            link.add(to: .main, forMode: .common)
            stage2Link = link
            syncFrameRate(hz: connection.currentMode().refreshHz)
            pipeline.start(
                connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd,
                onDecodedSize: onDecodedSize)
        } else {
            let pump = StreamPump()
            pump.start(
                connection: connection, layer: baseLayer,
                onFrame: onFrame, onSessionEnd: onSessionEnd, onDecodedSize: onDecodedSize)
            self.pump = pump
        }
    }

    /// Hint the display link with the stream's cadence. On iOS/tvOS a range is always required:
    /// without one, ProMotion devices cap CADisplayLink at 60 Hz (iPhones additionally need
    /// `CADisableMinimumFrameDurationOnPhone` in Info.plist), so a 120 fps stream would present
    /// at half rate with the ring silently dropping every other frame. `maximum` allows up to 120
    /// so the system MAY tick faster than a sub-120 stream (each extra tick is a near-free empty
    /// `renderTick`, and presenting on a denser grid shortens the decode→glass wait).
    ///
    /// The `allowVRR` setting (default on) widens that hint into a true variable-refresh request:
    /// `preferred` = the stream rate with a low floor, so a ProMotion / adaptive-sync display can
    /// drop its physical refresh to match the content. With VRR off we fall back to the proven
    /// behavior — iOS keeps a 30 Hz floor; macOS leaves the NSView link at its display's native
    /// rate (it already tracks the display and must NOT be capped to the stream rate).
    /// Re-applied from `layout` so a mid-session `Reconfigure` picks up a new refresh.
    private func syncFrameRate(hz: UInt32) {
        guard hz > 0, let link = stage2Link else { return }
        let hzF = Float(hz)
        let allowVRR = UserDefaults.standard.object(forKey: DefaultsKey.allowVRR) as? Bool ?? true
        #if os(macOS)
        // Off: `.default` = the link free-runs at the display's native rate (pre-VRR behavior).
        // On: request the content rate with a 24 Hz floor — capped at the display, never at the
        // stream rate, so an adaptive-sync panel can track the stream.
        let range: CAFrameRateRange = allowVRR
            ? CAFrameRateRange(minimum: min(hzF, 24), maximum: max(hzF, 120), preferred: hzF)
            : .default
        #else
        // A range is mandatory here (see above); VRR only lowers the floor (24 vs 30) so the
        // panel can drop deeper to match content on a sub-rate or momentarily stalling stream.
        let floor = allowVRR ? min(hzF, 24) : min(hzF, 30)
        let range = CAFrameRateRange(minimum: floor, maximum: max(hzF, 120), preferred: hzF)
        #endif
        if link.preferredFrameRateRange != range { link.preferredFrameRateRange = range }
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
        // Aspect source: the ACTUAL decoded dims when known (survives a lagging `currentMode()` and a
        // host that delivered a different size than requested), else the negotiated mode. The shader
        // stretches the frame across the WHOLE drawable, so this rect's aspect is the only thing that
        // keeps the picture undistorted — a stale aspect here is the post-resize black-bars+stretch.
        let aspect: CGSize? = {
            if let c = contentSize, c.width > 0, c.height > 0 { return c }
            if mode.width > 0, mode.height > 0 {
                return CGSize(width: Int(mode.width), height: Int(mode.height))
            }
            return nil
        }()
        let fit: CGRect = aspect.map { AVMakeRect(aspectRatio: $0, insideRect: bounds) } ?? bounds
        // No implicit resize animation; contentsScale tracks the view's backing/display scale.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        metalLayer.contentsScale = contentsScale
        metalLayer.frame = fit
        CATransaction.commit()
        // Hand the resulting pixel size to the render thread (it must not read layer geometry
        // cross-thread) — this is what the presenter sizes its drawable to.
        stage2?.setDrawableTarget(CGSize(
            width: (fit.width * contentsScale).rounded(),
            height: (fit.height * contentsScale).rounded()))
        #if os(tvOS)
        // Push the display's live EDR headroom alongside: > 1 means the TV is composited in an
        // HDR mode (the session's AVDisplayManager request landed — see StreamViewIOS), and HDR
        // frames flip to PQ passthrough. The stream view also re-layouts on mode-switch/screen-
        // mode notifications, so a mid-session switch reaches here without a bounds change.
        stage2?.setDisplayHeadroom(UIScreen.main.currentEDRHeadroom)
        #endif
    }

    /// Record the decoded frame's real dimensions (the view hops the pump's `onDecodedSize` to main
    /// and calls this) so `layout` aspect-fits to what's actually on screen instead of the possibly-
    /// stale `currentMode()`. Only stores — the caller re-runs `layout` right after, because a
    /// resize-END produces no bounds change to trigger one. No-op on a zero/unchanged size.
    func setContentSize(_ size: CGSize) {
        guard size.width > 0, size.height > 0, size != contentSize else { return }
        contentSize = size
    }

    /// Stop the active pump/pipeline (≤ one poll timeout; stage-2 joins its pump) and detach the
    /// stage-2 layer + link. Does not close the connection — that stays with whoever owns it.
    /// Idempotent.
    func stop() {
        contentSize = nil // a new session re-derives it from its first frame
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
