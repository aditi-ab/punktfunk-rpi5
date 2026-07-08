// Stage-2 presenter, present half: draw a decoded NV12 / P010 / 4:4:4 CVPixelBuffer into a CAMetalLayer
// drawable with a Y′CbCr→RGB shader. The hosting view's CADisplayLink still paces the pipeline once per
// vsync, but the actual `render` runs on Stage2Pipeline's dedicated RENDER THREAD (the link tick just
// signals it), so `nextDrawable()`'s blocking never lands on the main thread. See docs
// apple-stage2-presenter.md.
//
// Threading: created during view setup (main); `render`/`configure` run on the render thread — the
// layer's drawable/format/colour mutations all happen there (CAMetalLayer is designed for dedicated
// render threads; only the layer's GEOMETRY — frame/contentsScale — is touched from main, in
// SessionPresenter.layout, which also pushes the resulting pixel size here via `setDrawableTarget`
// so the render thread never reads layer geometry cross-thread). `setHdrMeta` (pump thread) and
// `setDrawableTarget` (main) only write lock-guarded staging state the render thread drains.

#if canImport(Metal) && canImport(QuartzCore)
import CoreGraphics
import CoreVideo
import Metal
import QuartzCore
import os

private let presenterLog = Logger(subsystem: "io.unom.punktfunk", category: "presenter")

/// HDR reference white (BT.2408 "HDR Reference White"): the absolute luminance, in nits, that the
/// PQ signal's diffuse white sits at. Passed to `CAEDRMetadata.hdr10(opticalOutputScale:)`, it anchors
/// 203-nit diffuse white at EDR 1.0 (the display's SDR-white level) and lets the system tone-map the
/// brighter highlights into the panel's headroom. This is the missing anchor that made the old HDR path
/// render "way too bright" (no `edrMetadata` → no reference-white anchoring); a LARGER value renders
/// dimmer. Matches the host's standard PQ reference white.
private let hdrReferenceWhiteNits: Float = 203.0

/// Runtime-compiled (no metallib build step needed in SwiftPM): a fullscreen triangle and BT.709 SDR
/// and BT.2020-PQ HDR Y′CbCr→RGB fragment shaders. uv.y is flipped (1 - p.y) so the top-left-origin
/// texture presents upright (NDC y is up). The HDR shader outputs PQ-encoded R′G′B′ as-is — the
/// CAMetalLayer's `itur_2100_PQ` colour space + `edrMetadata` tell the system compositor the samples
/// are PQ and how to tone-map them (no EOTF here, matching the host's BT.2020 PQ emission).
private let shaderSource = """
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; float2 uv; };

vertex VOut pf_vtx(uint vid [[vertex_id]]) {
    float2 p = float2(float((vid << 1) & 2), float(vid & 2));
    VOut o;
    o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = float2(p.x, 1.0 - p.y);
    return o;
}

// Bicubic (Catmull-Rom) sampling of the single-channel luma plane. The drawable is sized to the
// LAYER's pixels (see `render`), so this kernel performs the decoded→on-screen scale: when the
// window/view is bigger than the host's fixed mode a bilinear upscale looks soft; Catmull-Rom
// keeps edges crisp — matching AVSampleBufferDisplayLayer's (stage-1) scaler — and reduces to the
// exact texel at 1:1, so a native-resolution present stays pixel-exact.
// Nine bilinear taps (TheRealMJP's optimisation of the 16-tap kernel); `s` MUST be a linear
// sampler. Luma carries the perceived detail, so only it gets bicubic; chroma stays bilinear.
float catmullRomLuma(texture2d<float> tex, sampler s, float2 uv) {
    float2 texSize = float2(tex.get_width(), tex.get_height());
    float2 samplePos = uv * texSize;
    float2 tc1 = floor(samplePos - 0.5) + 0.5;
    float2 f = samplePos - tc1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 off12 = w2 / w12;
    float2 tc0 = (tc1 - 1.0) / texSize;
    float2 tc3 = (tc1 + 2.0) / texSize;
    float2 tc12 = (tc1 + off12) / texSize;
    float r = 0.0;
    r += tex.sample(s, float2(tc0.x,  tc0.y)).r  * (w0.x  * w0.y);
    r += tex.sample(s, float2(tc12.x, tc0.y)).r  * (w12.x * w0.y);
    r += tex.sample(s, float2(tc3.x,  tc0.y)).r  * (w3.x  * w0.y);
    r += tex.sample(s, float2(tc0.x,  tc12.y)).r * (w0.x  * w12.y);
    r += tex.sample(s, float2(tc12.x, tc12.y)).r * (w12.x * w12.y);
    r += tex.sample(s, float2(tc3.x,  tc12.y)).r * (w3.x  * w12.y);
    r += tex.sample(s, float2(tc0.x,  tc3.y)).r  * (w0.x  * w3.y);
    r += tex.sample(s, float2(tc12.x, tc3.y)).r  * (w12.x * w3.y);
    r += tex.sample(s, float2(tc3.x,  tc3.y)).r  * (w3.x  * w3.y);
    return r;
}

// 4:2:0 chroma is left-cosited horizontally (H.273 chroma_loc type 0 — the MPEG convention the
// host encodes and VideoToolbox decodes as-is), but sampling the half-res plane at the luma UV
// assumes CENTER siting — a ~0.5-luma-px rightward chroma shift on hard colored edges. Offset the
// sample by +0.25 chroma texels to re-align (libplacebo/mpv's correction). Vertical siting for
// type 0 is centered, which plain sampling already matches. A full-size 4:4:4 plane has no
// subsampling to correct — the offset self-disables when the plane widths match.
float2 chromaUV(texture2d<float> lumaTex, texture2d<float> chromaTex, float2 uv) {
    if (chromaTex.get_width() < lumaTex.get_width()) {
        uv.x += 0.25 / float(chromaTex.get_width());
    }
    return uv;
}

// SDR: 8-bit NV12 / 4:4:4 (BT.709, limited/video range) → full-range RGB. Chroma is sampled at the
// (siting-corrected) luma UV, so a full-size 4:4:4 chroma plane needs no shader change vs 4:2:0.
fragment float4 pf_frag(VOut in [[stage_in]],
                        texture2d<float> lumaTex [[texture(0)]],
                        texture2d<float> chromaTex [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y = catmullRomLuma(lumaTex, s, in.uv);
    float2 c = chromaTex.sample(s, chromaUV(lumaTex, chromaTex, in.uv)).rg;
    // BT.709, 8-bit limited (video) range → full-range RGB.
    y = (y - 16.0/255.0) * (255.0/219.0);
    float u = (c.x - 128.0/255.0) * (255.0/224.0);
    float v = (c.y - 128.0/255.0) * (255.0/224.0);
    float r = y + 1.5748 * v;
    float g = y - 0.1873 * u - 0.4681 * v;
    float b = y + 1.8556 * u;
    return float4(saturate(float3(r, g, b)), 1.0);
}

// HDR: 10-bit P010 / 4:4:4 (BT.2020, limited range), Y′CbCr that is PQ-encoded. We apply the BT.2020
// matrix to get PQ-encoded R′G′B′ and output it as-is — the CAMetalLayer's itur_2100_PQ colour space
// + edrMetadata tell the compositor the samples are PQ, so it does the PQ→display tone-map. No EOTF
// here. P010/x444 store the 10-bit code in the high bits of each 16-bit sample, so an .r16Unorm sample
// reads ~code/1023 (the /1024 vs /1023 error is < 0.1%).
fragment float4 pf_frag_hdr(VOut in [[stage_in]],
                            texture2d<float> lumaTex [[texture(0)]],
                            texture2d<float> chromaTex [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y = catmullRomLuma(lumaTex, s, in.uv);
    float2 c = chromaTex.sample(s, chromaUV(lumaTex, chromaTex, in.uv)).rg;
    // BT.2020 10-bit limited (video) range → full-range PQ R′G′B′.
    y = (y - 64.0/1023.0) * (1023.0/876.0);
    float u = (c.x - 512.0/1023.0) * (1023.0/896.0);
    float v = (c.y - 512.0/1023.0) * (1023.0/896.0);
    float r = y + 1.4746 * v;
    float g = y - 0.16455 * u - 0.57135 * v;
    float b = y + 1.8814 * u;
    return float4(saturate(float3(r, g, b)), 1.0);
}
"""

public final class MetalVideoPresenter {
    /// The layer the hosting view installs (as a sublayer) and sizes to its bounds.
    public let layer: CAMetalLayer

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    /// SDR (BT.709 8-bit → bgra8) and HDR (BT.2020 PQ 10-bit → rgba16Float) pipelines. Selected per
    /// frame in `render`; the layer is reconfigured to match when the session flips (HDR toggle).
    private let pipelineSDR: MTLRenderPipelineState
    private let pipelineHDR: MTLRenderPipelineState
    private var textureCache: CVMetalTextureCache?

    /// Current layer configuration — switched in `configure(hdr:)` when a frame's HDR-ness differs.
    /// Render-thread confined once the pipeline runs (Stage2Pipeline.start's one pre-thread
    /// `configure` call is ordered before the thread starts, so it doesn't race).
    private var hdrActive = false
    /// Last HDR mastering grade received via `setHdrMeta` (the host's 0xCE). Cached so a mid-session
    /// SDR→HDR flip's `configureColor` re-applies the real grade instead of clobbering it back to the
    /// bare reference-white anchor (an out-of-order race otherwise: `setHdrMeta` and the flip both write
    /// `edrMetadata`). Render-thread confined (drained from `pendingHdrMeta` at the top of `render`).
    private var lastHdrMeta: PunktfunkConnection.HdrMeta?

    /// Cross-thread staging, all under `stagingLock`: the pump thread parks a fresh 0xCE grade in
    /// `pendingHdrMeta` and the main thread parks the layout-derived drawable pixel size in
    /// `drawableTarget`; the render thread drains both at the top of `render`, so every layer
    /// format/colour mutation stays on the one thread that also calls `nextDrawable()`.
    private let stagingLock = NSLock()
    private var pendingHdrMeta: PunktfunkConnection.HdrMeta?
    private var drawableTarget: CGSize = .zero

    #if DEBUG
    /// Last logged "decoded→drawable" signature, so the diagnostic logs only on a size/HDR change.
    private var lastSizeSig = ""
    #endif

    /// nil if Metal is unavailable (no GPU / a headless CI) or a shader fails to compile — the caller
    /// falls back to stage-1.
    public static func make() -> MetalVideoPresenter? {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue()
        else { return nil }
        let pipelineSDR: MTLRenderPipelineState
        let pipelineHDR: MTLRenderPipelineState
        do {
            let library = try device.makeLibrary(source: shaderSource, options: nil)
            let vtx = library.makeFunction(name: "pf_vtx")
            let sdr = MTLRenderPipelineDescriptor()
            sdr.vertexFunction = vtx
            sdr.fragmentFunction = library.makeFunction(name: "pf_frag")
            sdr.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelineSDR = try device.makeRenderPipelineState(descriptor: sdr)
            let hdr = MTLRenderPipelineDescriptor()
            hdr.vertexFunction = vtx
            hdr.fragmentFunction = library.makeFunction(name: "pf_frag_hdr")
            hdr.colorAttachments[0].pixelFormat = .rgba16Float // EDR-capable
            pipelineHDR = try device.makeRenderPipelineState(descriptor: hdr)
        } catch {
            return nil
        }
        var cache: CVMetalTextureCache?
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        guard let textureCache = cache else { return nil }

        let layer = CAMetalLayer()
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.isOpaque = true
        #if os(macOS)
        // displaySyncEnabled MUST stay false on macOS. It has flip-flopped, so the full history:
        // sync ON was tried twice and starves the drawable pool both times — on macOS 26 a synced
        // present only reaches glass when the WindowServer composites the window, and its FramePacing
        // path does not treat our out-of-band image-queue presents as damage, so with a static scene
        // the ONLY recurring damage is the 1 Hz stats HUD update: presents queue, all drawables stay
        // held, `nextDrawable()` sleeps (sampled: ~70% of the render thread inside
        // CAMetalLayerPrivateNextDrawableLocked → usleep), and the stream turns into a ~1 fps
        // slideshow with normal-LOOKING stats (each rare frame is fresh, newest-wins ring). The
        // 94fb7d1b fullscreen judder was the same starvation biting the then-main-thread render.
        // With sync OFF the flip is immediate; the vsync alignment that sync was supposed to give
        // (the HUD-off direct-scanout pacing fix) comes from scheduling the present at the display
        // link's target time instead (`present(at:)` — see `render`).
        layer.displaySyncEnabled = false
        #endif
        // The drawable is rendered at the LAYER's pixel size (set per-frame in `render`), so the
        // shader — not the compositor — performs the decoded→on-screen scale (bicubic luma; the
        // compositor's contentsGravity path is plain bilinear). The gravity stays aspect-fit as a
        // transient fallback: during a live resize the compositor may composite a drawable from
        // the previous layout before the next render catches up.
        layer.contentsGravity = .resizeAspect
        // Triple-buffer: more in-flight drawables before `nextDrawable()` (called on the display-link /
        // MAIN thread) has to block waiting for one to free.
        layer.maximumDrawableCount = 3

        return MetalVideoPresenter(
            device: device, queue: queue, pipelineSDR: pipelineSDR, pipelineHDR: pipelineHDR,
            textureCache: textureCache, layer: layer)
    }

    private init(
        device: MTLDevice, queue: MTLCommandQueue, pipelineSDR: MTLRenderPipelineState,
        pipelineHDR: MTLRenderPipelineState, textureCache: CVMetalTextureCache, layer: CAMetalLayer
    ) {
        self.device = device
        self.queue = queue
        self.pipelineSDR = pipelineSDR
        self.pipelineHDR = pipelineHDR
        self.textureCache = textureCache
        self.layer = layer
    }

    /// Configure the layer + active pipeline for an SDR or HDR session. Called once at session start
    /// (main, before the render thread exists) and again per-frame from `render` on the RENDER THREAD
    /// (idempotent — the guard makes a same-state call a no-op), so a mid-session HDR toggle (the host
    /// re-inits its encoder; the decoded `frame.isHDR` flips) reconfigures here automatically. HDR uses
    /// an rgba16Float drawable + BT.2020 PQ colour space + EDR with a 203-nit reference-white anchor;
    /// SDR uses the plain 8-bit sRGB path.
    public func configure(hdr: Bool) {
        guard hdr != hdrActive else { return }
        hdrActive = hdr
        configureColor(hdr: hdr)
    }

    /// Set the layer's pixel format + colour config for SDR or HDR. MAIN THREAD ONLY. EDR is requested
    /// on macOS + iOS (the old `#if os(macOS)` guard left iOS EDR half-engaged). tvOS has NO EDR API
    /// (`wantsExtendedDynamicRangeContent`/`edrMetadata`/`CAEDRMetadata` are all unavailable there), so
    /// it gets the PQ pixel format + colour space only — the tvOS compositor tone-maps from those.
    private func configureColor(hdr: Bool) {
        if hdr {
            layer.pixelFormat = .rgba16Float
            layer.colorspace = CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            #if !os(tvOS)
            layer.wantsExtendedDynamicRangeContent = true
            // Anchor reference white. Re-apply the real grade if one already arrived (0xCE before the
            // flip); otherwise the bare 203-nit anchor. Without this anchor the PQ signal is too bright.
            layer.edrMetadata = makeEDR(lastHdrMeta)
            #endif
        } else {
            // SDR: gamma-encoded BT.709 [0,1] in an 8-bit drawable; a nil colorspace tags it device/sRGB
            // (the proven SDR path — never showed the "too bright" issue, which was HDR-only).
            layer.pixelFormat = .bgra8Unorm
            layer.colorspace = nil
            #if !os(tvOS)
            layer.wantsExtendedDynamicRangeContent = false
            layer.edrMetadata = nil
            #endif
        }
    }

    #if !os(tvOS)
    private func makeEDR(_ meta: PunktfunkConnection.HdrMeta?) -> CAEDRMetadata {
        CAEDRMetadata.hdr10(
            displayInfo: meta?.masteringDisplayColorVolume(),
            contentInfo: meta?.contentLightLevelInfo(),
            opticalOutputScale: hdrReferenceWhiteNits)
    }
    #endif

    /// Update the HDR mastering metadata (drained from the host's 0xCE datagram) to refine the system
    /// tone-map from the real grade. Called from the PUMP thread — the grade is only PARKED here (lock-
    /// guarded); the render thread applies it at the top of the next `render`, keeping every layer
    /// colour mutation on the one thread that also vends drawables.
    public func setHdrMeta(_ meta: PunktfunkConnection.HdrMeta) {
        stagingLock.lock()
        pendingHdrMeta = meta
        stagingLock.unlock()
    }

    /// Park the drawable pixel size the shader should render at: the metal layer's laid-out frame ×
    /// contentsScale, both owned by the MAIN thread (SessionPresenter.layout pushes it on every layout/
    /// backing change). The render thread reads this instead of the layer's geometry so it never
    /// touches main-owned CALayer state. Zero until the first layout → `render` falls back to the
    /// decoded frame size.
    public func setDrawableTarget(_ size: CGSize) {
        stagingLock.lock()
        drawableTarget = size
        stagingLock.unlock()
    }

    /// Draw one decoded frame to the next drawable and present it. RENDER THREAD (Stage2Pipeline's;
    /// `nextDrawable()` may block up to a frame — that wait belongs here, never on main). `isHDR`
    /// selects the 10-bit BT.2020 PQ path vs the 8-bit BT.709 path and is reconciled with the
    /// layer config via `configure`. Returns true on success; false when there's no drawable yet, a
    /// texture couldn't be made, or Metal errored — the caller then doesn't stamp a present (and can
    /// requeue the frame). `onPresented` fires once the drawable actually reached glass, with the
    /// `CLOCK_REALTIME` instant from the drawable's `presentedTime` — or nil when the system reports
    /// none (a dropped drawable). It runs on a Metal callback thread; keep the handler thread-safe.
    ///
    /// `presentAtMediaTime` (a `CACurrentMediaTime`-basis host time — the display link's
    /// `targetTimestamp`) schedules the flip ON the vsync instead of "as soon as the GPU finishes":
    /// with the layer's own sync disabled (mandatory on macOS — see init) an immediate present hits
    /// glass mid-refresh whenever the layer is direct-scanout promoted (fullscreen, no HUD), which
    /// is the "frametimes are off with the stats HUD closed" report. nil presents immediately
    /// (`PUNKTFUNK_PRESENT_MODE=immediate` — the pre-fix behavior, kept as a diagnostic A/B).
    @discardableResult
    public func render(
        _ pixelBuffer: CVPixelBuffer, isHDR: Bool = false,
        presentAtMediaTime: CFTimeInterval? = nil,
        onPresented: ((Int64?) -> Void)? = nil
    ) -> Bool {
        // Drain the cross-thread staging (see `stagingLock`): the layout-derived drawable size and
        // any freshly-arrived HDR grade, both applied from this thread.
        stagingLock.lock()
        let targetFromLayout = drawableTarget
        let newHdrMeta = pendingHdrMeta
        pendingHdrMeta = nil
        stagingLock.unlock()

        // Reconcile the layer with the decoded frame's HDR-ness (handles a mid-session SDR↔HDR flip).
        configure(hdr: isHDR)
        if let newHdrMeta {
            self.lastHdrMeta = newHdrMeta
            // tvOS has no edrMetadata — the cached grade is still kept (a later HDR flip's
            // configureColor is where it matters there). macOS/iOS refine the live tone-map now.
            #if !os(tvOS)
            if hdrActive { layer.edrMetadata = makeEDR(newHdrMeta) }
            #endif
        }

        // P010/x444 store 10-bit luma/chroma in 16-bit samples → R16/RG16; NV12/444v is 8-bit → R8/RG8.
        // Derived from the actual decoded buffer so a 4:4:4 (full chroma plane) frame just works.
        let pf = CVPixelBufferGetPixelFormatType(pixelBuffer)
        let tenBit =
            pf == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            || pf == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
            || pf == kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange
            || pf == kCVPixelFormatType_444YpCbCr10BiPlanarFullRange
        guard let textureCache,
              let luma = makeTexture(
                pixelBuffer, plane: 0, format: tenBit ? .r16Unorm : .r8Unorm, cache: textureCache),
              let chroma = makeTexture(
                pixelBuffer, plane: 1, format: tenBit ? .rg16Unorm : .rg8Unorm, cache: textureCache)
        else { return false }

        // Size the drawable to the LAYER's pixels (its laid-out frame × contentsScale, pushed here by
        // SessionPresenter.layout via `setDrawableTarget` — not read off the layer, whose geometry the
        // main thread owns) so the Catmull-Rom shader performs the decoded→on-screen scale in one pass:
        // a native-mode session stays exactly 1:1 (the kernel reduces to the identity texel), and a
        // window bigger than the host's mode gets bicubic luma instead of the compositor's bilinear.
        // Before the first layout (zero target) fall back to the decoded size. drawableSize does NOT
        // track bounds (defaults to 0), so set it BEFORE nextDrawable; re-set only on a change
        // (layout / Reconfigure / HDR flip — and every frame of a live resize, which is fine).
        let decodedSize = CGSize(
            width: CVPixelBufferGetWidth(pixelBuffer), height: CVPixelBufferGetHeight(pixelBuffer))
        let targetSize = (targetFromLayout.width > 0 && targetFromLayout.height > 0)
            ? targetFromLayout : decodedSize
        if layer.drawableSize != targetSize { layer.drawableSize = targetSize }
        #if DEBUG
        logSizeIfChanged(decoded: decodedSize, drawable: targetSize)
        #endif
        guard let drawable = layer.nextDrawable(),
              let commandBuffer = queue.makeCommandBuffer()
        else { return false }

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else {
            return false
        }
        encoder.setRenderPipelineState(hdrActive ? pipelineHDR : pipelineSDR)
        encoder.setFragmentTexture(CVMetalTextureGetTexture(luma), index: 0)
        encoder.setFragmentTexture(CVMetalTextureGetTexture(chroma), index: 1)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        if let onPresented {
            #if targetEnvironment(simulator)
            // The simulator SDK exposes neither addPresentedHandler nor presentedTime — report
            // nil so the caller stamps with its display-link estimate (the pre-presentedTime
            // behavior; simulator numbers are indicative only anyway).
            onPresented(nil)
            #else
            // Registered BEFORE present. presentedTime is CACurrentMediaTime-based; 0 means the
            // system never put this drawable on glass (dropped) — report nil, the caller falls
            // back to its display-link estimate.
            drawable.addPresentedHandler { d in
                onPresented(
                    d.presentedTime > 0
                        ? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: d.presentedTime)
                        : nil)
            }
            #endif
        }
        // Scheduled on the vsync when the pipeline gave us the link's target (see the doc comment);
        // immediate otherwise. A target already in the past presents immediately — same thing.
        if let presentAtMediaTime {
            commandBuffer.present(drawable, atTime: presentAtMediaTime)
        } else {
            commandBuffer.present(drawable)
        }
        // Hold the CVMetalTextures + source pixel buffer (its IOSurface) alive until the GPU finishes
        // sampling — releasing them at scope exit could free the backing mid-read.
        commandBuffer.addCompletedHandler { _ in _ = (luma, chroma, pixelBuffer) }
        commandBuffer.commit()
        return true
    }

    /// Returns the CVMetalTexture (not just its MTLTexture) so the caller can keep it alive past the
    /// draw — the MTLTexture is only valid while its CVMetalTexture is retained.
    private func makeTexture(
        _ pixelBuffer: CVPixelBuffer, plane: Int, format: MTLPixelFormat, cache: CVMetalTextureCache
    ) -> CVMetalTexture? {
        let w = CVPixelBufferGetWidthOfPlane(pixelBuffer, plane)
        let h = CVPixelBufferGetHeightOfPlane(pixelBuffer, plane)
        var cvTexture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault, cache, pixelBuffer, nil, format, w, h, plane, &cvTexture)
        guard status == kCVReturnSuccess, let cvTexture,
              CVMetalTextureGetTexture(cvTexture) != nil
        else { return nil }
        return cvTexture
    }

    #if DEBUG
    private func logSizeIfChanged(decoded: CGSize, drawable: CGSize) {
        let sig = "\(Int(decoded.width))x\(Int(decoded.height))→\(Int(drawable.width))x\(Int(drawable.height))|hdr\(hdrActive ? 1 : 0)"
        if sig != lastSizeSig {
            lastSizeSig = sig
            let msg =
                "stage2: decoded \(Int(decoded.width))x\(Int(decoded.height)) → drawable \(Int(drawable.width))x\(Int(drawable.height)) hdr=\(hdrActive)"
            presenterLog.info("\(msg, privacy: .public)")
        }
    }
    #endif
}
#endif
