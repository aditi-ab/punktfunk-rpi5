// Stage-2 presenter, present half: draw a decoded NV12 CVPixelBuffer into a CAMetalLayer
// drawable with a BT.709 YUV→RGB shader. The display link (owned by the hosting view) drives
// `render` once per vsync with the target present time, so a present can finally be stamped and
// the present tail hand-paced. See docs apple-stage2-presenter.md.
//
// Main-thread only: created during view setup, `render` called from the view's CADisplayLink
// (which fires on the main runloop). The Metal objects + texture cache are touched only here.

#if canImport(Metal) && canImport(QuartzCore)
import CoreGraphics
import CoreVideo
import Metal
import QuartzCore

/// Runtime-compiled (no metallib build step needed in SwiftPM): a fullscreen triangle and a
/// BT.709 limited-range NV12→RGB fragment shader. uv.y is flipped (1 - p.y) so the top-left-
/// origin texture presents upright (NDC y is up), not upside down. (Colorspace is BT.709 SDR
/// for now — matches the host; 10-bit/HDR + other matrices are a later tie-in.)
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

fragment float4 pf_frag(VOut in [[stage_in]],
                        texture2d<float> lumaTex [[texture(0)]],
                        texture2d<float> chromaTex [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y = lumaTex.sample(s, in.uv).r;
    float2 c = chromaTex.sample(s, in.uv).rg;
    // BT.709, 8-bit limited (video) range → full-range RGB.
    y = (y - 16.0/255.0) * (255.0/219.0);
    float u = (c.x - 128.0/255.0) * (255.0/224.0);
    float v = (c.y - 128.0/255.0) * (255.0/224.0);
    float r = y + 1.5748 * v;
    float g = y - 0.1873 * u - 0.4681 * v;
    float b = y + 1.8556 * u;
    return float4(saturate(float3(r, g, b)), 1.0);
}

// HDR: 10-bit P010 (BT.2020, limited range), Y'CbCr that is PQ-encoded. We apply the BT.2020
// matrix to get PQ-encoded R'G'B' and output it as-is — the CAMetalLayer's itur_2100_PQ colour
// space + EDR tells the compositor the samples are PQ, so it does the PQ→display mapping. No EOTF
// here (matching the host, which emitted BT.2020 PQ). P010 stores the 10-bit code in the high bits
// of each 16-bit sample, so an .r16Unorm sample reads ~code/1023 (the /1024 vs /1023 error is < 0.1%).
fragment float4 pf_frag_hdr(VOut in [[stage_in]],
                            texture2d<float> lumaTex [[texture(0)]],
                            texture2d<float> chromaTex [[texture(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y = lumaTex.sample(s, in.uv).r;
    float2 c = chromaTex.sample(s, in.uv).rg;
    // BT.2020 10-bit limited (video) range → full-range PQ R'G'B'.
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
    /// SDR (BT.709 8-bit NV12 → bgra8) and HDR (BT.2020 PQ 10-bit P010 → rgba16Float) pipelines.
    /// Selected per frame by `render`; the layer is reconfigured when the mode flips (HDR toggle).
    private let pipelineSDR: MTLRenderPipelineState
    private let pipelineHDR: MTLRenderPipelineState
    private var textureCache: CVMetalTextureCache?
    /// Current layer configuration — switched lazily in `configure(hdr:)` when a frame's mode differs.
    private var hdrActive = false

    /// nil if Metal is unavailable (no GPU / a headless CI) — the caller falls back to stage-1.
    public init?() {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue()
        else { return nil }
        self.device = device
        self.queue = queue
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
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &textureCache)
        guard textureCache != nil else { return nil }

        let layer = CAMetalLayer()
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.isOpaque = true
        // Triple-buffer: more in-flight drawables before `nextDrawable()` (called on the
        // display-link / MAIN thread) has to block waiting for one to free.
        layer.maximumDrawableCount = 3
        #if os(macOS)
        // The display link already paces exactly one present per vsync. Leaving the layer's
        // own vsync wait on means `commandBuffer.present` ALSO blocks for the hardware vsync,
        // so `nextDrawable()` stalls the MAIN thread until a drawable frees — windowed, the
        // WindowServer's looser compositing hides it; FULLSCREEN's tighter, more-direct path
        // serializes the main thread to the display and the stall surfaces as bad judder.
        // Disabling the layer-level sync lets present return promptly (the display link is the
        // pacing source), which is what fixes the fullscreen stutter. macOS-only property.
        layer.displaySyncEnabled = false
        #endif
        self.layer = layer
    }

    /// Track the stream mode (the host can Reconfigure mid-stream). Size is in pixels.
    public func setDrawableSize(_ size: CGSize) {
        guard size.width > 0, size.height > 0 else { return }
        if layer.drawableSize != size { layer.drawableSize = size }
    }

    /// Reconfigure the layer for SDR or HDR when the stream mode flips (HDR toggle). HDR uses an
    /// rgba16Float drawable + a BT.2020 PQ colour space + EDR, so the compositor PQ-maps to the
    /// display; SDR uses the plain 8-bit sRGB path. Main-thread only (called from `render`).
    private func configure(hdr: Bool) {
        guard hdr != hdrActive else { return }
        hdrActive = hdr
        if hdr {
            layer.pixelFormat = .rgba16Float
            layer.colorspace = CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            #if os(macOS)
            layer.wantsExtendedDynamicRangeContent = true
            #endif
        } else {
            layer.pixelFormat = .bgra8Unorm
            layer.colorspace = nil
            #if os(macOS)
            layer.wantsExtendedDynamicRangeContent = false
            #endif
        }
    }

    /// Draw one decoded frame to the next drawable and present it. `isHDR` selects the 10-bit
    /// BT.2020 PQ path (P010 input) vs the 8-bit BT.709 path (NV12 input). Returns true on success;
    /// false when there's no drawable yet, a texture couldn't be made, or Metal errored — the
    /// caller then doesn't stamp a present for this frame.
    @discardableResult
    public func render(_ pixelBuffer: CVPixelBuffer, isHDR: Bool = false) -> Bool {
        configure(hdr: isHDR)
        // P010 stores 10-bit luma/chroma in 16-bit samples → R16/RG16; NV12 is 8-bit → R8/RG8.
        let lumaFmt: MTLPixelFormat = isHDR ? .r16Unorm : .r8Unorm
        let chromaFmt: MTLPixelFormat = isHDR ? .rg16Unorm : .rg8Unorm
        guard let textureCache,
              let luma = makeTexture(pixelBuffer, plane: 0, format: lumaFmt, cache: textureCache),
              let chroma = makeTexture(pixelBuffer, plane: 1, format: chromaFmt, cache: textureCache)
        else { return false }

        // The hosting view owns drawableSize (aspect-fit to its bounds); skip until it's laid
        // out. The fullscreen triangle scales the decoded texture to fill the drawable.
        guard layer.drawableSize.width > 0, layer.drawableSize.height > 0,
              let drawable = layer.nextDrawable(),
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
        encoder.setRenderPipelineState(isHDR ? pipelineHDR : pipelineSDR)
        encoder.setFragmentTexture(CVMetalTextureGetTexture(luma), index: 0)
        encoder.setFragmentTexture(CVMetalTextureGetTexture(chroma), index: 1)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        commandBuffer.present(drawable) // present at the next vsync — lowest latency
        // Hold the CVMetalTextures + the source pixel buffer (its IOSurface) alive until the GPU
        // finishes sampling — releasing them at scope exit could free the backing mid-read.
        commandBuffer.addCompletedHandler { _ in _ = (luma, chroma, pixelBuffer) }
        commandBuffer.commit()
        return true
    }

    /// Returns the CVMetalTexture (not just its MTLTexture) so the caller can keep it alive past
    /// the draw — the MTLTexture is only valid while its CVMetalTexture is retained.
    private func makeTexture(
        _ pixelBuffer: CVPixelBuffer, plane: Int, format: MTLPixelFormat,
        cache: CVMetalTextureCache
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
}
#endif
