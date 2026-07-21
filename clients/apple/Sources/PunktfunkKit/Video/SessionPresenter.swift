// Per-session presenter stack shared by the macOS and iOS/tvOS stream views: the Metal pipeline
// (explicit VTDecompressionSession decode → CAMetalLayer) is the default — deadline-paced
// stage-4 on iOS/tvOS, arrival-paced stage-2 on macOS (see PresenterChoice.platformDefault);
// the user-facing choice is the INTENT (PresentPriority: latency vs smoothness+buffer — the
// 2026-07 rebuild, design/apple-presentation-rebuild.md), the stage ladder is env-only debug.
// Stage-1 (StreamPump → AVSampleBufferDisplayLayer) is the Metal-unavailable / DEBUG fallback.
// The views own the platform bits — capture, window/scale tracking, and constructing the
// display link (arrival/glass pacing only; deadline pacing runs its own CAMetalDisplayLink) —
// and delegate the shared presenter lifecycle here.
//
// Main-thread only: start/layout/stop and the display-link tick all run on the main runloop.

#if canImport(Metal) && canImport(QuartzCore)
import AVFoundation
import Foundation
import PunktfunkShared
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

/// Which presenter a session runs. Stage-2/3/4 are the same Metal pipeline with different present
/// pacing (`PresentPacing` — see Stage2Pipeline for the full tradeoff): stage-2 presents on frame
/// arrival, stage-3 gates presents on the on-glass callback, stage-4 presents into
/// CAMetalDisplayLink-vended drawables (deadline pacing — iOS/tvOS only; see `PresentPacing`'s
/// doc for why the vsync-latching platforms need it). Stage-1 (compressed video straight to the
/// system layer) is a DEBUG-only diagnostic. Internal (not private) for unit tests.
enum PresenterChoice: Equatable {
    case stage1
    case stage2
    case stage3
    case stage4

    /// Resolve from the `PUNKTFUNK_PRESENTER` env override (A/B without touching settings) first,
    /// then the persisted `DefaultsKey.presenter` setting; anything unknown (or an empty env var)
    /// falls back to the platform default. `allowStage1` is false in release builds, where a
    /// leftover DEBUG "stage1" value silently maps to the default rather than reviving the
    /// freeze-prone fallback.
    static func resolve(setting: String?, env: String?, allowStage1: Bool) -> PresenterChoice {
        explicit(setting: setting, env: env, allowStage1: allowStage1) ?? platformDefault
    }

    /// The user's EXPLICIT stage selection, nil when they haven't made one (unset/unknown values,
    /// and a release build's gated "stage1"). Split from `resolve` so a codec-conditional default
    /// (see `SessionPresenter.pacing`) can apply only when the user hasn't picked a stage — an
    /// explicit "stage2" must stay a faithful A/B of arrival pacing. "stage4" resolves only on
    /// iOS/tvOS: macOS's present path is entangled with the sync-off/DCP-panic saga (see
    /// MetalVideoPresenter's init) and stays on its proven pacings until deadline presents are
    /// deliberately validated there — a synced "stage4" value maps back to the platform default.
    static func explicit(setting: String?, env: String?, allowStage1: Bool) -> PresenterChoice? {
        let raw = env.flatMap { $0.isEmpty ? nil : $0 } ?? setting
        switch raw {
        case "stage1": return allowStage1 ? .stage1 : nil
        case "stage2": return .stage2
        case "stage3": return .stage3
        case "stage4":
            #if os(macOS)
            return nil
            #else
            return .stage4
            #endif
        default: return nil
        }
    }

    /// iOS/iPadOS/tvOS default to DEADLINE pacing (stage-4), macOS to arrival (stage-2).
    ///
    /// The iOS/tvOS layers ALWAYS vsync-latch presents into a FIFO image queue
    /// (`displaySyncEnabled` is macOS-only API), and at stream rate ≈ panel rate — an Apple TV's
    /// fixed 60 Hz by construction; an iPhone/iPad with VRR (default on, preferred = stream rate)
    /// steering the panel to the stream — that queue's depth is STICKY: one burst fills it and,
    /// with arrivals and latches then running at the same rate, it NEVER drains. Every queued
    /// present costs a full refresh, forever: the 2026-07 iPad Pro (2752×2064@120) field ladder
    /// read ~30 ms display on arrival (~3 refreshes of queue), 22–28 ms glass-gated at depth 2
    /// (a standing queue of 2 — the depth-2 experiment's post-mortem), 14 ms at depth 1. Glass
    /// pacing (stage-3) bounds the queue but presents still serialize on the on-glass callback;
    /// deadline pacing (stage-4) is the fix for the remainder: one CAMetalDisplayLink-vended
    /// drawable per refresh, presented the moment a frame decodes — the queue cannot exist and
    /// nothing waits on callbacks (see `PresentPacing.deadline`).
    ///
    /// tvOS joined iOS on the deadline engine in the 2026-07 presentation rebuild
    /// (design/apple-presentation-rebuild.md — the engine is field-proven on iOS and strictly
    /// simpler than the glass gate it replaces; `PUNKTFUNK_PRESENTER=stage3` remains the
    /// fallback lever if a TV-specific issue surfaces). macOS keeps stage-2: with the layer's
    /// sync off, presents are out-of-band flips that don't queue, so arrival is genuinely
    /// lowest-latency there.
    static var platformDefault: PresenterChoice {
        #if os(iOS) || os(tvOS)
        .stage4
        #else
        .stage2
        #endif
    }
}

/// The user's presentation INTENT — what replaced the visible stage picker in the 2026-07
/// rebuild (design/apple-presentation-rebuild.md). Two intents, one engine per platform:
///
/// - `.latency` (the default): every frame shows as soon as the display can take it — the
///   newest-wins zero-queue store; network/decode jitter appears as the occasional repeat or
///   drop. This is the configuration the whole 2026-07 pacing saga optimized.
/// - `.smooth(buffer:)`: a small deliberate jitter buffer (`FrameStore.fifo`) evens the present
///   cadence at the cost of `buffer` refresh intervals of added display latency — which the HUD
///   SHOWS (only the OS floor is shaved, never the user's chosen buffer). `buffer` ∈ 1…3;
///   the "Automatic" setting (stored 0) currently maps to 2.
///
/// Mechanism stays internal: intents map onto `PresentPacing`/`FrameStore.Policy` per platform
/// in `SessionPresenter.start`; the stage ladder survives only as the PUNKTFUNK_PRESENTER debug
/// env lever. Internal (not private) for unit tests.
enum PresentPriority: Equatable {
    case latency
    case smooth(buffer: Int)

    /// Resolve from the persisted settings: `DefaultsKey.presentPriority` ("latency" default;
    /// anything but "smooth" — unset, garbage, a synced unknown future value — falls back to
    /// latency) and `DefaultsKey.smoothBuffer` (0/out-of-range = Automatic = 2).
    static func resolve(setting: String?, bufferSetting: Int?) -> PresentPriority {
        guard setting == "smooth" else { return .latency }
        let raw = bufferSetting ?? 0
        return .smooth(buffer: (1...3).contains(raw) ? raw : 2)
    }

    /// The frame hand-off policy this intent runs (see `FrameStore`).
    var storePolicy: FrameStorePolicy {
        switch self {
        case .latency: return .newestWins
        case .smooth(let buffer): return .fifo(capacity: buffer)
        }
    }
}

final class SessionPresenter {
    /// Present pacing for this session. Stage-3 always means glass gating; under the stage-2
    /// default, macOS PyroWave sessions ALSO get glass gating — for SMOOTHNESS, not as the panic
    /// fix (that is the windowed transactional present — see `setComposited`). PyroWave's wavelet
    /// decode is near-instant Metal compute, so a network clump presents within the same
    /// millisecond, and it is the codec that sustains stream rates above the panel's refresh; the
    /// glass gate admits one presented-but-undisplayed swap at a time (serialized on the on-glass
    /// callback, 100 ms stale backstop) so those bursts coalesce in the newest-wins ring instead
    /// of flooding the queue. (Glass pacing was ALSO the original DCP-panic mitigation attempt —
    /// disproven: a fully serialized stream still panicked, which is why the real fix moved to the
    /// present mechanism.) An explicit stage-2 pick (setting/env) still forces arrival pacing —
    /// that A/B lever must stay honest. VideoToolbox codecs keep arrival pacing: decode latency
    /// spaces their presents.
    static func pacing(
        for choice: PresenterChoice, explicit: PresenterChoice?, codec: VideoCodec
    ) -> PresentPacing {
        if choice == .stage4 { return .deadline }
        if choice == .stage3 { return .glass }
        #if os(macOS)
        if explicit == nil, codec == .pyrowave { return .glass }
        #endif
        return .arrival
    }

    /// The glass gate's in-flight present budget (`PresentGate` capacity): 1 everywhere.
    ///
    /// Depth 1 is the only depth that works. The 2026-07 depth-2 experiment (one flip scanning
    /// out + one queued, predicted ~5–8 ms at 120 Hz) REGRESSED the iPad Pro's display stage to
    /// 22–28 ms vs depth 1's 14: any second gate slot becomes a STANDING queue — a burst fills
    /// it, and with presents and latches then running at the same rate the occupancy never
    /// returns to zero, so every frame permanently rides one extra refresh per slot. A bounded
    /// FIFO can cap the queue but nothing ever drains it; the prediction assumed an idle queue
    /// that doesn't exist after the first Wi-Fi clump. Sub-refresh display latency needs pacing
    /// that can't queue at all — that's stage-4 (`PresentPacing.deadline`), not a deeper gate.
    ///
    /// `PUNKTFUNK_GATE_DEPTH` (1…3) still overrides on iOS/tvOS so the standing-queue ladder
    /// stays reproducible on-device; macOS is pinned to 1, env ignored — a deeper gate only builds
    /// a standing queue (see above), and macOS glass pacing exists for PyroWave smoothness
    /// (see `pacing`), where depth 1 is the point. Internal (not private) for unit tests.
    static func gateDepth(env: String?) -> Int {
        #if os(macOS)
        return 1
        #else
        if let env, let depth = Int(env), (1...3).contains(depth) { return depth }
        return 1
        #endif
    }

    private var pump: StreamPump?
    private var stage2: Stage2Pipeline?
    private var stage2Link: CADisplayLink?
    private var metalLayer: CAMetalLayer?
    #if os(macOS)
    /// The last windowed-vs-fullscreen present routing pushed to the pipeline — see
    /// `setComposited` (the DCP swapID-panic mitigation). Main-thread only, like all of this.
    private var transactionalPresentActive = false
    #endif
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
        presentFloorMeter: LatencyMeter? = nil,
        makeDisplayLink: (AnyObject, Selector) -> CADisplayLink,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil
    ) {
        stop()
        self.connection = connection

        // Presentation resolution (design/apple-presentation-rebuild.md). The Metal pipeline is
        // the DEFAULT (explicit VTDecompressionSession decode + a CAMetalLayer present): it can
        // detect + recover a wedged decoder where stage-1's AVSampleBufferDisplayLayer freezes
        // hard on a lost HEVC reference. The MECHANISM (pacing) is per-platform via
        // PresenterChoice.platformDefault — deadline on iOS/tvOS, arrival on macOS — overridable
        // only by the hidden PUNKTFUNK_PRESENTER debug env (the legacy persisted stage picker
        // value is deliberately ignored). The user-facing choice is the INTENT
        // (PresentPriority): latency (newest-wins zero-queue store) vs smoothness (a FIFO jitter
        // buffer; on macOS it additionally paces presents onto the vsync grid so the buffer
        // drains on display cadence). Stage-1 is reachable only via env in DEBUG; release maps
        // it back to the default (the stage-1 pump below stays the automatic Metal-missing
        // fallback).
        #if DEBUG
        let allowStage1 = true
        #else
        let allowStage1 = false
        #endif
        let explicit = PresenterChoice.explicit(
            setting: nil, // the legacy DefaultsKey.presenter picker value is no longer read
            env: ProcessInfo.processInfo.environment["PUNKTFUNK_PRESENTER"],
            allowStage1: allowStage1)
        let choice = explicit ?? PresenterChoice.platformDefault
        let pacing = Self.pacing(for: choice, explicit: explicit, codec: connection.videoCodec)
        let priority = PresentPriority.resolve(
            setting: UserDefaults.standard.string(forKey: DefaultsKey.presentPriority),
            bufferSetting: UserDefaults.standard.object(forKey: DefaultsKey.smoothBuffer) as? Int)
        // macOS smoothness rides arrival pacing + forced vsync scheduling; under a glass-paced
        // macOS session (the PyroWave DCP mitigation) the gate already serializes on the
        // display, so the FIFO alone provides the buffering.
        #if os(macOS)
        let vsyncPaced = priority != .latency && pacing == .arrival
        #else
        let vsyncPaced = false
        #endif
        if choice != .stage1,
           let pipeline = Stage2Pipeline(
               endToEndMeter: endToEndMeter, decodeMeter: decodeMeter,
               displayMeter: displayMeter,
               presentFloorMeter: presentFloorMeter,
               pacing: pacing,
               gateDepth: Self.gateDepth(
                   env: ProcessInfo.processInfo.environment["PUNKTFUNK_GATE_DEPTH"]),
               storePolicy: priority.storePolicy,
               vsyncPaced: vsyncPaced) {
            let metal = pipeline.layer
            // The opaque metal layer composites OVER the AVSampleBufferDisplayLayer base, which
            // sits idle (un-enqueued) in stage-2. contentsScale + frame are set in layout().
            baseLayer.addSublayer(metal)
            metalLayer = metal
            #if os(macOS)
            transactionalPresentActive = false
            #endif
            stage2 = pipeline
            // The link is the vsync CLOCK + putBack-retry nudge, not the presentation trigger
            // (frame arrival is — see Stage2Pipeline's header). timestamp→targetTimestamp is the
            // link's own report of the current refresh period (tracks VRR rate changes).
            // DEADLINE pacing needs neither: its CAMetalDisplayLink (pipeline-owned) is the vsync
            // clock, and every one of its updates re-checks the ring, which IS the retry tick —
            // a second link would only fight it over the frame-rate hint.
            if pacing != .deadline {
                let proxy = DisplayLinkProxy { [weak self] link in
                    self?.stage2?.renderTick(
                        targetMediaTime: link.targetTimestamp,
                        period: link.targetTimestamp - link.timestamp)
                }
                let link = makeDisplayLink(proxy, #selector(DisplayLinkProxy.tick(_:)))
                link.add(to: .main, forMode: .common)
                stage2Link = link
            }
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
        guard hz > 0 else { return }
        // Deadline pacing: the hint goes to the pipeline's CAMetalDisplayLink instead (staged;
        // applied from the link's own thread — see Stage2Pipeline.setFrameRateHint). A no-op
        // under arrival/glass pacing, where the CADisplayLink below is the one hinted link.
        stage2?.setFrameRateHint(hz: Float(hz))
        guard let link = stage2Link else { return }
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
        // Snap the sublayer frame to the BACKING PIXEL GRID. AVMakeRect centers the aspect-fit rect,
        // so its origin/size are usually fractional points; a metal sublayer whose frame doesn't land
        // on whole device pixels is RESAMPLED by the macOS/UIKit compositor during composite — a
        // uniform "everything looks soft" blur — even when the drawable itself is pixel-exact 1:1
        // (verified via the stage2 "[1:1 (no resample)]" log while the picture was still soft). Round
        // origin AND size to device pixels so the composite is a true 1:1 blit. Idempotent when the
        // frame is already aligned (e.g. fullscreen fit == integer bounds), so it's a no-op there.
        let scale = contentsScale > 0 ? contentsScale : 1
        let snapped = CGRect(
            x: (fit.origin.x * scale).rounded() / scale,
            y: (fit.origin.y * scale).rounded() / scale,
            width: (fit.width * scale).rounded() / scale,
            height: (fit.height * scale).rounded() / scale)
        // No implicit resize animation; contentsScale tracks the view's backing/display scale.
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        metalLayer.contentsScale = contentsScale
        metalLayer.frame = snapped
        CATransaction.commit()
        // Hand the resulting pixel size to the render thread (it must not read layer geometry
        // cross-thread) — this is what the presenter sizes its drawable to. Uses the SNAPPED size so
        // the drawable's texel count equals the on-screen device-pixel count exactly (1 texel ↔ 1
        // device pixel); with the frame snapped, this equals the pre-snap rounded value, so the
        // decoded↔drawable 1:1 the log confirmed is preserved.
        stage2?.setDrawableTarget(CGSize(
            width: (snapped.width * scale).rounded(),
            height: (snapped.height * scale).rounded()))
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

    #if os(macOS)
    /// Route presents for the window's composited state (MAIN thread — the view pushes it on
    /// every layout, which fullscreen transitions always trigger). A COMPOSITED (windowed) session
    /// presents the drawable through a Core Animation transaction
    /// (`CAMetalLayer.presentsWithTransaction`) instead of the async image queue — the DCP
    /// "mismatched swapID's" kernel-panic mitigation (see `MetalVideoPresenter`; the async-swap
    /// race survives glass pacing, so pacing alone was not enough). ALL codecs: PyroWave hit it
    /// 2026-07-18 and windowed HEVC hit the same 240 Hz Mac Studio 2026-07-21 — it is the async
    /// image queue itself, not any codec or present rate. Fullscreen keeps the async path (direct
    /// scanout, lowest latency, no panic there). The full HDR/EDR render path is preserved in both.
    func setComposited(_ composited: Bool) {
        guard let stage2 else { return }
        guard composited != transactionalPresentActive else { return }
        transactionalPresentActive = composited
        stage2.setTransactionalPresent(composited)
    }
    #endif

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
        #if os(macOS)
        transactionalPresentActive = false
        #endif
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
