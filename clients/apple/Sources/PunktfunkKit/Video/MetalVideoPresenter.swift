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
#if os(macOS)
import IOSurface
#endif
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

/// PUNKTFUNK_SDR_COLORSPACE=srgb — A/B hatch for the SDR layer's colour tag. Today the SDR layer
/// ships with `colorspace = nil`, which on macOS means NO colour matching: the BT.709/sRGB-encoded
/// stream is displayed with the panel's native primaries — mild oversaturation on every P3 Mac.
/// `srgb` tags the layer so CoreAnimation colour-matches it into the panel's gamut (the strictly
/// correct rendering). Kept OFF by default until the on-glass A/B confirms it (the nil path is the
/// long-proven look, and some users may prefer the vivid rendition); flip the default once verified.
private let sdrColorspaceOverride: CGColorSpace? = {
    guard ProcessInfo.processInfo.environment["PUNKTFUNK_SDR_COLORSPACE"] == "srgb" else {
        return nil
    }
    return CGColorSpace(name: CGColorSpace.sRGB)
}()

/// Runtime-compiled (no metallib build step needed in SwiftPM): a fullscreen triangle and Y′CbCr→RGB
/// fragment shaders whose conversion arrives as three constant rows computed per frame on the CPU
/// (`CscRows` — the Swift port of pf-client-core's `csc_rows`, from the decoded buffer's actual
/// signaling). One set of coefficients honors BT.601/709/2020 × full/limited × 8/10-bit instead of
/// the old hardcoded BT.709/BT.2020 matrices — a BT.601-signaled stream (a Linux host's RGB-input
/// NVENC) used to render with BT.709 coefficients, a constant hue error. uv.y is flipped (1 - p.y)
/// so the top-left-origin texture presents upright (NDC y is up). The HDR shader outputs PQ-encoded
/// R′G′B′ as-is — the CAMetalLayer's `itur_2100_PQ` colour space + `edrMetadata` tell the system
/// compositor the samples are PQ and how to tone-map them (no EOTF here, matching the host's
/// BT.2020 PQ emission).
private let shaderSource = """
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; float2 uv; };

// The CPU-computed CSC rows (CscRows.swift, layout-matched): rgb[i] = dot(ri.xyz, yuv) + ri.w.
// Range expansion, the matrix, and the 10-bit MSB-packing factor are all folded in.
struct CscUniform { float4 r0; float4 r1; float4 r2; };

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

// The shared sample + row-multiply: Y′CbCr (bicubic luma, siting-corrected bilinear chroma) →
// RGB via the per-frame rows. A full-size 4:4:4 chroma plane needs no change vs 4:2:0 (the siting
// offset self-disables). What the result MEANS depends on the stream: an SDR frame's rows yield
// gamma-encoded RGB, an HDR frame's rows yield PQ-encoded R′G′B′ — the fragment variants below
// differ only in what they do next.
float3 sampleRgb(texture2d<float> lumaTex, texture2d<float> chromaTex, float2 uv,
                 constant CscUniform& csc) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
#ifdef PF_BILINEAR_LUMA
    // DEBUG (PUNKTFUNK_BILINEAR_LUMA=1): plain bilinear luma — Catmull-Rom OFF. A/B lever to see if
    // the bicubic overshoot contributes to edge fringing. NOTE: at a true 1:1 present both paths
    // reduce to the identity texel, so if this toggle VISIBLY changes the picture, the present is
    // NOT 1:1 (there's a resample); if it looks identical, the fringing is upstream (codec/source/OS).
    float lumaY = lumaTex.sample(s, uv).r;
#else
    float lumaY = catmullRomLuma(lumaTex, s, uv);
#endif
    float3 yuv = float3(lumaY,
                        chromaTex.sample(s, chromaUV(lumaTex, chromaTex, uv)).rg);
    return saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                           dot(csc.r1.xyz, yuv) + csc.r1.w,
                           dot(csc.r2.xyz, yuv) + csc.r2.w));
}

// SDR: 8-bit NV12 / 4:4:4 → full-range RGB, transfer left baked (shown as-is, the proven SDR
// layer config).
fragment float4 pf_frag(VOut in [[stage_in]],
                        texture2d<float> lumaTex [[texture(0)]],
                        texture2d<float> chromaTex [[texture(1)]],
                        constant CscUniform& csc [[buffer(0)]]) {
    return float4(sampleRgb(lumaTex, chromaTex, in.uv, csc), 1.0);
}

// PyroWave planar SDR: three separate R8 planes (Y full-res, Cb/Cr half-res 4:2:0) from the
// Metal wavelet decoder — the Metal twin of pf-presenter's planar_csc.frag. Same bicubic luma
// and left-cosited chroma correction as the biplanar path (chromaUV self-disables at 4:4:4).
fragment float4 pf_frag_planar(VOut in [[stage_in]],
                               texture2d<float> lumaTex [[texture(0)]],
                               texture2d<float> cbTex [[texture(1)]],
                               texture2d<float> crTex [[texture(2)]],
                               constant CscUniform& csc [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
#ifdef PF_BILINEAR_LUMA
    float lumaY = lumaTex.sample(s, in.uv).r;
#else
    float lumaY = catmullRomLuma(lumaTex, s, in.uv);
#endif
    float2 cuv = chromaUV(lumaTex, cbTex, in.uv);
    float3 yuv = float3(lumaY, cbTex.sample(s, cuv).r, crTex.sample(s, cuv).r);
    float3 rgb = saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                                 dot(csc.r1.xyz, yuv) + csc.r1.w,
                                 dot(csc.r2.xyz, yuv) + csc.r2.w));
    return float4(rgb, 1.0);
}

// HDR: 10-bit P010 / 4:4:4 (BT.2020, PQ-encoded Y′CbCr) → full-range PQ R′G′B′, output as-is —
// the CAMetalLayer's itur_2100_PQ colour space + edrMetadata tell the compositor the samples are
// PQ, so it does the PQ→display tone-map. No EOTF here. The rows fold in the exact 10-bit
// MSB-packing factor (the old hardcoded shader carried a documented ~0.1% /1024-vs-/1023 error).
fragment float4 pf_frag_hdr(VOut in [[stage_in]],
                            texture2d<float> lumaTex [[texture(0)]],
                            texture2d<float> chromaTex [[texture(1)]],
                            constant CscUniform& csc [[buffer(0)]]) {
    return float4(sampleRgb(lumaTex, chromaTex, in.uv, csc), 1.0);
}

// HDR on tvOS when the display is composited WITHOUT HDR headroom (SDR output mode, or the user
// disabled Match Dynamic Range): no Metal EDR API exists there (CAEDRMetadata /
// wantsExtendedDynamicRangeContent are API_UNAVAILABLE(tvos)), and a bare PQ colour-space tag
// composites UNtone-mapped — the CAMetalLayer header says so outright — which showed as a badly
// overblown picture on Apple TV. So this variant finishes the job in-shader: PQ EOTF → linear
// light, 203-nit reference white (BT.2408) anchored at display white, extended-Reinhard highlight
// rolloff with a 1000-nit knee, BT.2020→BT.709 primaries, BT.709 OETF — into the proven SDR layer
// config. The 10-bit BT.2020 stream keeps its full decode depth; only the final presentation is
// display-referred SDR. (When the display IS in an HDR mode — requested per session via
// AVDisplayManager, see StreamViewIOS — tvOS presents pf_frag_hdr's PQ passthrough instead:
// in a genuine HDR10 output, PQ passthrough is the correct emission and the TV tone-maps.)
fragment float4 pf_frag_hdr_tv(VOut in [[stage_in]],
                               texture2d<float> lumaTex [[texture(0)]],
                               texture2d<float> chromaTex [[texture(1)]],
                               constant CscUniform& csc [[buffer(0)]]) {
    // Y′CbCr → full-range PQ R′G′B′ via the per-frame rows (as pf_frag_hdr).
    float3 pq = sampleRgb(lumaTex, chromaTex, in.uv, csc);
    // ST 2084 EOTF: PQ code value → linear light, 1.0 = 10,000 nits.
    const float m1 = 2610.0/16384.0;
    const float m2 = 78.84375;
    const float c1 = 3424.0/4096.0;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 p = pow(pq, 1.0/m2);
    float3 lin = pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0/m1);
    // Scene-referred with diffuse white at 1.0 (the same 203-nit anchor the EDR path uses).
    float3 t = lin * (10000.0/203.0);
    // BT.2020 → BT.709 primaries while still linear; negatives are out-of-gamut, floor them.
    float3 t709 = float3(
        dot(t, float3( 1.6605, -0.5876, -0.0728)),
        dot(t, float3(-0.1246,  1.1329, -0.0083)),
        dot(t, float3(-0.0182, -0.1006,  1.1187)));
    t709 = max(t709, 0.0);
    // Extended Reinhard: 1.0 stays put, the 1000-nit knee lands at display white, above rolls off.
    const float w = 1000.0/203.0;
    float3 mapped = saturate(t709 * (1.0 + t709 / (w * w)) / (1.0 + t709));
    // BT.709 OETF — the same encoding the SDR stream arrives in, so both paths present alike.
    float3 e = select(1.099 * pow(mapped, 0.45) - 0.099, 4.5 * mapped, mapped < 0.018);
    return float4(e, 1.0);
}
"""

public final class MetalVideoPresenter {
    /// The layer the hosting view installs (as a sublayer) and sizes to its bounds.
    public let layer: CAMetalLayer

    #if os(macOS)
    /// The WINDOWED-mode PyroWave present target: a plain CALayer sized like `layer` (installed
    /// as a sibling ABOVE it), fed IOSurfaces via `contents` inside ordinary CATransactions.
    ///
    /// Why this exists — the macOS DCP KERNEL PANIC ("mismatched swapID's" @UnifiedPipeline.cpp,
    /// WindowServer dies, machine reboots): out-of-band CAMetalLayer image-queue swaps into a
    /// COMPOSITED (windowed) session race WindowServer's own swap submissions on high-refresh
    /// displays, and the race survives glass pacing — a fully serialized one-in-flight present
    /// stream still panicked a 240 Hz Mac Studio (2026-07-18, twice). So in windowed mode we stop
    /// using the image queue entirely and present the way video players do: render the planar CSC
    /// into an IOSurface pool and swap `contents` on main — WindowServer treats it as ordinary
    /// damage on its own composite cadence, coalescing faster-than-refresh updates instead of
    /// latching queue swaps mid-cycle. Fullscreen keeps the CAMetalLayer path (direct-scanout
    /// promotion, no compositing, no panic reports). Contents updates are transparent to the
    /// layer below when nil, so flipping modes just covers/uncovers the metal layer.
    public let surfaceLayer: CALayer = {
        let l = CALayer()
        l.contentsGravity = .resize // frame is already aspect-fit + pixel-snapped by layout
        l.isOpaque = true
        l.actions = ["contents": NSNull(), "bounds": NSNull(), "position": NSNull()]
        return l
    }()

    /// One IOSurface-backed render target of the windowed present pool. All pool state is
    /// RENDER-THREAD confined; only the immutable surface refs cross to main (contents swap).
    private struct SurfaceSlot {
        let surface: IOSurfaceRef
        let texture: MTLTexture
        /// Monotonic use stamp — the reuse picker takes the least-recently-rendered free slot.
        var seq: UInt64 = 0
    }

    private var surfacePool: [SurfaceSlot] = []
    private var surfacePoolSize: CGSize = .zero
    private var surfaceSeq: UInt64 = 0
    /// Index of the slot most recently handed to the layer — never rewritten next, even if its
    /// use count already dropped (the compositor may still be scanning out the previous frame).
    private var lastHandedOff: Int?
    /// Staged (under `stagingLock`, like every cross-thread input): the hosting view's windowed
    /// vs fullscreen state, pushed from main via `setSurfacePresents`. Drained in `renderPlanar`.
    private var surfacePresentsStaged = false
    /// Render-thread copy, so pool teardown happens exactly once on a mode flip.
    private var surfacePresentsActive = false
    #endif

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    /// SDR (BT.709 8-bit → bgra8) and HDR (BT.2020 PQ 10-bit → rgba16Float) pipelines. Selected per
    /// frame in `render`; the layer is reconfigured to match when the session flips (HDR toggle).
    private let pipelineSDR: MTLRenderPipelineState
    private let pipelineHDR: MTLRenderPipelineState
    /// tvOS only: the in-shader PQ→SDR tone-map fallback (pf_frag_hdr_tv → bgra8), used whenever
    /// the display is composited without HDR headroom — see `setDisplayHeadroom`. nil elsewhere.
    private let pipelineHDRToneMap: MTLRenderPipelineState?
    /// PyroWave's 3-plane SDR path (pf_frag_planar → bgra8) — see `renderPlanar`.
    private let pipelinePlanar: MTLRenderPipelineState
    private var textureCache: CVMetalTextureCache?

    /// The PyroWave Metal decoder records on the presenter's device + queue: one device means
    /// decode, CSC and present share textures with zero interop, and one queue means Metal's
    /// hazard tracking orders a ring-slot rewrite after the render still sampling it.
    var metalDevice: MTLDevice { device }
    var metalQueue: MTLCommandQueue { queue }

    /// Current layer configuration — switched in `configure(hdr:)` when a frame's HDR-ness differs.
    /// Render-thread confined once the pipeline runs (Stage2Pipeline.start's one pre-thread
    /// `configure` call is ordered before the thread starts, so it doesn't race).
    private var hdrActive = false
    /// tvOS only: whether HDR frames currently present as PQ PASSTHROUGH (display has HDR headroom
    /// — its own tone-map applies) vs the in-shader tone-map fallback. Render-thread confined;
    /// derived from the staged display headroom at the top of every `render`.
    private var hdrPassthroughActive = false
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
    /// tvOS: the display's current EDR headroom (UIScreen.currentEDRHeadroom), pushed from the
    /// main thread (SessionPresenter.layout + the mode-switch observers). > 1 ⇒ the display is
    /// composited with HDR headroom, so HDR frames present as PQ passthrough; otherwise the
    /// in-shader tone-map keeps the picture from blowing out. 1 (the default) is the safe start.
    private var stagedDisplayHeadroom: CGFloat = 1.0

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
        let pipelineHDRToneMap: MTLRenderPipelineState?
        let pipelinePlanar: MTLRenderPipelineState
        do {
            // DEBUG A/B lever: PUNKTFUNK_BILINEAR_LUMA=1 compiles the shader with Catmull-Rom OFF
            // (plain bilinear luma) by prepending a #define ahead of the source. Default (unset) is
            // the normal bicubic path. Read at presenter creation — set it in the environment and
            // relaunch to flip; the log line confirms which path built.
            let bilinearLuma = ProcessInfo.processInfo.environment["PUNKTFUNK_BILINEAR_LUMA"] == "1"
            let source = (bilinearLuma ? "#define PF_BILINEAR_LUMA 1\n" : "") + shaderSource
            if bilinearLuma {
                presenterLog.info("stage2: PUNKTFUNK_BILINEAR_LUMA=1 — Catmull-Rom luma DISABLED (bilinear)")
            }
            let library = try device.makeLibrary(source: source, options: nil)
            let vtx = library.makeFunction(name: "pf_vtx")
            let sdr = MTLRenderPipelineDescriptor()
            sdr.vertexFunction = vtx
            sdr.fragmentFunction = library.makeFunction(name: "pf_frag")
            sdr.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelineSDR = try device.makeRenderPipelineState(descriptor: sdr)
            let hdr = MTLRenderPipelineDescriptor()
            hdr.vertexFunction = vtx
            hdr.fragmentFunction = library.makeFunction(name: "pf_frag_hdr")
            hdr.colorAttachments[0].pixelFormat = .rgba16Float // PQ passthrough
            pipelineHDR = try device.makeRenderPipelineState(descriptor: hdr)
            #if os(tvOS)
            // tvOS carries BOTH HDR pipelines: PQ passthrough when the display is composited
            // with HDR headroom, the in-shader tone-map (→ the 8-bit SDR config) when it isn't.
            // See setDisplayHeadroom / configureColor.
            let tm = MTLRenderPipelineDescriptor()
            tm.vertexFunction = vtx
            tm.fragmentFunction = library.makeFunction(name: "pf_frag_hdr_tv")
            tm.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelineHDRToneMap = try device.makeRenderPipelineState(descriptor: tm)
            #else
            pipelineHDRToneMap = nil
            #endif
            let planar = MTLRenderPipelineDescriptor()
            planar.vertexFunction = vtx
            planar.fragmentFunction = library.makeFunction(name: "pf_frag_planar")
            planar.colorAttachments[0].pixelFormat = .bgra8Unorm // PyroWave is 8-bit SDR
            pipelinePlanar = try device.makeRenderPipelineState(descriptor: planar)
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
            pipelineHDRToneMap: pipelineHDRToneMap, pipelinePlanar: pipelinePlanar,
            textureCache: textureCache, layer: layer)
    }

    private init(
        device: MTLDevice, queue: MTLCommandQueue, pipelineSDR: MTLRenderPipelineState,
        pipelineHDR: MTLRenderPipelineState, pipelineHDRToneMap: MTLRenderPipelineState?,
        pipelinePlanar: MTLRenderPipelineState,
        textureCache: CVMetalTextureCache, layer: CAMetalLayer
    ) {
        self.device = device
        self.queue = queue
        self.pipelineSDR = pipelineSDR
        self.pipelineHDR = pipelineHDR
        self.pipelineHDRToneMap = pipelineHDRToneMap
        self.pipelinePlanar = pipelinePlanar
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
        #if os(tvOS)
        // Reconfigure on an HDR flip AND on a passthrough↔tone-map flip: the display's headroom
        // changes when the AVDisplayManager mode switch (requested at session start) completes —
        // typically a second or two into the session.
        stagingLock.lock()
        let passthrough = stagedDisplayHeadroom > 1.0
        stagingLock.unlock()
        guard hdr != hdrActive || (hdr && passthrough != hdrPassthroughActive) else { return }
        hdrActive = hdr
        hdrPassthroughActive = passthrough
        #else
        guard hdr != hdrActive else { return }
        hdrActive = hdr
        #endif
        configureColor(hdr: hdr)
    }

    /// tvOS: park the display's current EDR headroom (a MAIN-thread `UIScreen` read — pushed by
    /// SessionPresenter.layout and the stream view's mode-switch observers). > 1 flips HDR frames
    /// to PQ passthrough (the display's own tone-map applies); ≤ 1 keeps the in-shader tone-map.
    /// Applied by the render thread on the next frame, like every other staged value here.
    public func setDisplayHeadroom(_ headroom: CGFloat) {
        stagingLock.lock()
        stagedDisplayHeadroom = headroom
        stagingLock.unlock()
    }

    /// Set the layer's pixel format + colour config for SDR or HDR. MAIN THREAD ONLY. EDR is requested
    /// on macOS + iOS (the old `#if os(macOS)` guard left iOS EDR half-engaged). tvOS has NO EDR API
    /// (`wantsExtendedDynamicRangeContent`/`edrMetadata`/`CAEDRMetadata` are all unavailable there) —
    /// and a bare PQ colour-space tag composites UNtone-mapped (the "overblown HDR" Apple TV report),
    /// so tvOS instead tone-maps PQ→SDR in the shader (pf_frag_hdr_tv) and keeps the SDR layer config.
    private func configureColor(hdr: Bool) {
        if hdr {
            #if os(tvOS)
            if hdrPassthroughActive {
                // Display composited WITH HDR headroom (the session's AVDisplayManager request
                // landed): emit PQ passthrough — in a real HDR10 output that's the correct
                // emission, and the TV applies its own tone-map.
                layer.pixelFormat = .rgba16Float
                layer.colorspace = CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            } else {
                // SDR-composited display: PQ would render untone-mapped (blown out) — the
                // pf_frag_hdr_tv shader tone-maps to SDR instead.
                layer.pixelFormat = .bgra8Unorm
                layer.colorspace = nil
            }
            #else
            layer.pixelFormat = .rgba16Float
            layer.colorspace = CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            layer.wantsExtendedDynamicRangeContent = true
            // Anchor reference white. Re-apply the real grade if one already arrived (0xCE before the
            // flip); otherwise the bare 203-nit anchor. Without this anchor the PQ signal is too bright.
            layer.edrMetadata = makeEDR(lastHdrMeta)
            #endif
        } else {
            // SDR: gamma-encoded BT.709 [0,1] in an 8-bit drawable. Default: nil colorspace = NO
            // colour matching on macOS (the panel's native primaries — the long-proven look,
            // slightly oversaturated on P3 panels); PUNKTFUNK_SDR_COLORSPACE=srgb tags the layer
            // for correct colour matching instead (A/B pending — see sdrColorspaceOverride).
            layer.pixelFormat = .bgra8Unorm
            layer.colorspace = sdrColorspaceOverride
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

    #if os(macOS)
    /// Park the windowed-vs-fullscreen present routing (MAIN thread — the hosting view pushes its
    /// window state on every layout). true = PyroWave frames present via `surfaceLayer` contents
    /// (the DCP swapID-panic mitigation — see `surfaceLayer`); false = the CAMetalLayer path.
    /// Applied by the render thread on the next frame, like every other staged value here.
    public func setSurfacePresents(_ on: Bool) {
        stagingLock.lock()
        surfacePresentsStaged = on
        stagingLock.unlock()
    }
    #endif

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
        // The frame's Y′CbCr→RGB rows, from its ACTUAL signaling (buffer attachments + pixel
        // format) — a BT.601-signaled stream gets 601 coefficients, full-range gets full-range
        // expansion; recomputed per frame because the host can flip colour in-band (SDR↔HDR).
        var csc = CscRows.rows(
            CscRows.signal(of: pixelBuffer), depth: tenBit ? 10 : 8, msbPacked: tenBit)
        guard let textureCache,
              let luma = makeTexture(
                pixelBuffer, plane: 0, format: tenBit ? .r16Unorm : .r8Unorm, cache: textureCache),
              let chroma = makeTexture(
                pixelBuffer, plane: 1, format: tenBit ? .rg16Unorm : .rg8Unorm, cache: textureCache)
        else { return false }

        #if os(tvOS)
        // HDR splits by the display's headroom (kept in step with the layer by `configure` above):
        // PQ passthrough into an HDR-composited display, the tone-map shader otherwise.
        let hdrPipeline = hdrPassthroughActive ? pipelineHDR : (pipelineHDRToneMap ?? pipelineHDR)
        let pipeline = hdrActive ? hdrPipeline : pipelineSDR
        #else
        let pipeline = hdrActive ? pipelineHDR : pipelineSDR
        #endif
        let decodedSize = CGSize(
            width: CVPixelBufferGetWidth(pixelBuffer), height: CVPixelBufferGetHeight(pixelBuffer))
        return encodePresent(
            decodedSize: decodedSize, targetFromLayout: targetFromLayout, pipeline: pipeline,
            presentAtMediaTime: presentAtMediaTime, onPresented: onPresented,
            // Hold the CVMetalTextures + source pixel buffer (its IOSurface) alive until the GPU
            // finishes sampling — releasing them at scope exit could free the backing mid-read.
            keepAlive: [luma, chroma, pixelBuffer]
        ) { encoder in
            encoder.setFragmentTexture(CVMetalTextureGetTexture(luma), index: 0)
            encoder.setFragmentTexture(CVMetalTextureGetTexture(chroma), index: 1)
            encoder.setFragmentBytes(&csc, length: MemoryLayout<CscUniform>.stride, index: 0)
        }
    }

    /// Draw one PyroWave planar frame (three R8 planes off the Metal wavelet decoder) and
    /// present it. RENDER THREAD, same contract as `render` — PyroWave is 8-bit SDR, so the
    /// layer always takes the plain SDR config, and the CSC rows arrive precomputed from the
    /// stream's own sequence-header signaling (no CVPixelBuffer to inspect).
    @discardableResult
    func renderPlanar(
        _ planes: WaveletPlanes,
        presentAtMediaTime: CFTimeInterval? = nil,
        onPresented: ((Int64?) -> Void)? = nil
    ) -> Bool {
        stagingLock.lock()
        let targetFromLayout = drawableTarget
        #if os(macOS)
        let surfaceMode = surfacePresentsStaged
        #endif
        stagingLock.unlock()
        configure(hdr: false)
        var csc = planes.csc
        #if os(macOS)
        if surfaceMode != surfacePresentsActive {
            surfacePresentsActive = surfaceMode
            presenterLog.info(
                "stage2: windowed surface presents \(surfaceMode ? "ON" : "OFF", privacy: .public) (PyroWave DCP-panic mitigation)")
            if !surfaceMode {
                // Back to the metal path (fullscreen): drop the pool — at 5K it holds >100 MB,
                // and re-entering windowed mode rebuilds it in one frame.
                surfacePool.removeAll()
                surfacePoolSize = .zero
                lastHandedOff = nil
            }
        }
        if surfaceMode {
            return renderPlanarToSurface(
                planes, targetFromLayout: targetFromLayout, csc: &csc, onPresented: onPresented)
        }
        #endif
        return encodePresent(
            decodedSize: CGSize(width: planes.width, height: planes.height),
            targetFromLayout: targetFromLayout, pipeline: pipelinePlanar,
            presentAtMediaTime: presentAtMediaTime, onPresented: onPresented,
            // The ring textures stay valid by ring depth; retaining them here also pins the
            // slot's set until the sample completes (mirrors the biplanar keep-alive).
            keepAlive: [planes.y, planes.cb, planes.cr]
        ) { encoder in
            encoder.setFragmentTexture(planes.y, index: 0)
            encoder.setFragmentTexture(planes.cb, index: 1)
            encoder.setFragmentTexture(planes.cr, index: 2)
            encoder.setFragmentBytes(&csc, length: MemoryLayout<CscUniform>.stride, index: 0)
        }
    }

    #if os(macOS)
    /// The windowed-mode present tail (see `surfaceLayer` for why this path exists): render the
    /// planar CSC into a pooled IOSurface and hand it to `surfaceLayer.contents` on MAIN inside a
    /// plain CATransaction — an ordinary damaged-layer update on WindowServer's own composite
    /// cadence, no CAMetalLayer image-queue swap anywhere. `presentAtMediaTime` doesn't apply
    /// (the compositor paces); `onPresented` fires after the contents swap is committed, stamped
    /// with CLOCK_REALTIME then — the closest observable analogue of "reached glass" here (the
    /// composite follows within a refresh, so the meters' display stage reads slightly optimistic).
    private func renderPlanarToSurface(
        _ planes: WaveletPlanes, targetFromLayout: CGSize, csc: inout CscUniform,
        onPresented: ((Int64?) -> Void)?
    ) -> Bool {
        let decodedSize = CGSize(width: planes.width, height: planes.height)
        let targetSize = (targetFromLayout.width > 0 && targetFromLayout.height > 0)
            ? targetFromLayout : decodedSize
        ensureSurfacePool(size: targetSize)
        guard let slotIndex = takeSurfaceSlot(),
              let commandBuffer = queue.makeCommandBuffer()
        else { return false }
        let slot = surfacePool[slotIndex]

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = slot.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else {
            return false
        }
        encoder.setRenderPipelineState(pipelinePlanar)
        encoder.setFragmentTexture(planes.y, index: 0)
        encoder.setFragmentTexture(planes.cb, index: 1)
        encoder.setFragmentTexture(planes.cr, index: 2)
        encoder.setFragmentBytes(&csc, length: MemoryLayout<CscUniform>.stride, index: 0)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        let surface = slot.surface
        let surfaceLayer = surfaceLayer // captured directly — the handler must not retain self
        let keepAlive: [Any] = [planes.y, planes.cb, planes.cr]
        commandBuffer.addCompletedHandler { _ in
            _ = keepAlive // ring textures pinned until the GPU finished sampling
            DispatchQueue.main.async {
                CATransaction.begin()
                CATransaction.setDisableActions(true)
                surfaceLayer.contents = surface
                CATransaction.commit()
                onPresented?(
                    Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime()))
            }
        }
        commandBuffer.commit()
        lastHandedOff = slotIndex
        return true
    }

    /// (Re)build the pool at `size` — 4 BGRA8 IOSurface render targets (one on glass, one queued
    /// in CA, one rendering, one spare). RENDER THREAD. A failed allocation leaves the pool empty;
    /// the caller returns false and the ring's putBack + display-link retry take over.
    private func ensureSurfacePool(size: CGSize) {
        guard size != surfacePoolSize else { return }
        surfacePool.removeAll()
        surfacePoolSize = size
        lastHandedOff = nil
        let w = Int(size.width)
        let h = Int(size.height)
        guard w > 0, h > 0 else { return }
        // 256-byte row alignment satisfies both IOSurface and Metal linear-texture rules.
        let bytesPerRow = ((w * 4) + 255) & ~255
        let props: [String: Any] = [
            kIOSurfaceWidth as String: w,
            kIOSurfaceHeight as String: h,
            kIOSurfaceBytesPerElement as String: 4,
            kIOSurfaceBytesPerRow as String: bytesPerRow,
            kIOSurfacePixelFormat as String: kCVPixelFormatType_32BGRA,
        ]
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: .bgra8Unorm, width: w, height: h, mipmapped: false)
        desc.usage = [.renderTarget]
        desc.storageMode = .shared
        for _ in 0..<4 {
            guard let surface = IOSurfaceCreate(props as CFDictionary),
                  let texture = device.makeTexture(descriptor: desc, iosurface: surface, plane: 0)
            else {
                surfacePool.removeAll()
                return
            }
            surfacePool.append(SurfaceSlot(surface: surface, texture: texture))
        }
    }

    /// Pick the slot to render into: never the one just handed to the layer (the compositor may
    /// still scan it), prefer surfaces the window server isn't holding (`IOSurfaceIsInUse`), and
    /// among those the least recently rendered. Falls back to the LRU busy slot rather than
    /// stalling — a visible glitch at worst, never a queue-up. RENDER THREAD.
    private func takeSurfaceSlot() -> Int? {
        guard !surfacePool.isEmpty else { return nil }
        var free: Int?
        var busy: Int?
        for i in surfacePool.indices where i != lastHandedOff {
            if !IOSurfaceIsInUse(surfacePool[i].surface) {
                if free == nil || surfacePool[i].seq < surfacePool[free!].seq { free = i }
            } else {
                if busy == nil || surfacePool[i].seq < surfacePool[busy!].seq { busy = i }
            }
        }
        guard let pick = free ?? busy else { return nil }
        surfaceSeq += 1
        surfacePool[pick].seq = surfaceSeq
        return pick
    }
    #endif

    /// The shared present tail of `render`/`renderPlanar`: size the drawable, encode one
    /// fullscreen triangle with `pipeline` (`bind` supplies the fragment resources), schedule
    /// the present and the on-glass callback.
    private func encodePresent(
        decodedSize: CGSize, targetFromLayout: CGSize, pipeline: MTLRenderPipelineState,
        presentAtMediaTime: CFTimeInterval?, onPresented: ((Int64?) -> Void)?,
        keepAlive: [Any], bind: (MTLRenderCommandEncoder) -> Void
    ) -> Bool {
        // Size the drawable to the LAYER's pixels (its laid-out frame × contentsScale, pushed here by
        // SessionPresenter.layout via `setDrawableTarget` — not read off the layer, whose geometry the
        // main thread owns) so the Catmull-Rom shader performs the decoded→on-screen scale in one pass:
        // a native-mode session stays exactly 1:1 (the kernel reduces to the identity texel), and a
        // window bigger than the host's mode gets bicubic luma instead of the compositor's bilinear.
        // Before the first layout (zero target) fall back to the decoded size. drawableSize does NOT
        // track bounds (defaults to 0), so set it BEFORE nextDrawable; re-set only on a change
        // (layout / Reconfigure / HDR flip — and every frame of a live resize, which is fine).
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
        encoder.setRenderPipelineState(pipeline)
        bind(encoder)
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
        // Keep the bound sources alive until the GPU finishes sampling (see the callers).
        commandBuffer.addCompletedHandler { _ in _ = keepAlive }
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
            // Explicit verdict: is the shader presenting 1:1 (decoded == drawable) or resampling? The
            // scale ratio makes a residual match-window mismatch obvious. If this says 1:1 but the
            // picture is still soft, the resample is downstream of us (macOS compositor — a scaled
            // display mode, or a fractional-pixel window position), not the shader.
            let sx = decoded.width > 0 ? drawable.width / decoded.width : 0
            let sy = decoded.height > 0 ? drawable.height / decoded.height : 0
            let verdict = decoded == drawable
                ? "1:1 (no resample)"
                : String(format: "RESAMPLE scale=%.4fx%.4f", sx, sy)
            let msg =
                "stage2: decoded \(Int(decoded.width))x\(Int(decoded.height)) → drawable \(Int(drawable.width))x\(Int(drawable.height)) [\(verdict)] hdr=\(hdrActive)"
            presenterLog.info("\(msg, privacy: .public)")
        }
    }
    #endif
}
#endif
