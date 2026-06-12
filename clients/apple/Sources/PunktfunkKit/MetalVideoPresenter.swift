// Stage-2 presenter, present half: draw a decoded NV12 CVPixelBuffer into a CAMetalLayer
// drawable with a BT.709 YUV→RGB shader. The display link (owned by the hosting view) drives
// `render` once per vsync with the target present time, so a present can finally be stamped and
// the present tail hand-paced. See docs apple-stage2-presenter.md.
//
// Main-thread only: created during view setup, `render` called from the view's CADisplayLink
// (which fires on the main runloop). The Metal objects + texture cache are touched only here.

#if canImport(Metal) && canImport(QuartzCore)
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
"""

public final class MetalVideoPresenter {
    /// The layer the hosting view installs (as a sublayer) and sizes to its bounds.
    public let layer: CAMetalLayer

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private var textureCache: CVMetalTextureCache?

    /// nil if Metal is unavailable (no GPU / a headless CI) — the caller falls back to stage-1.
    public init?() {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue()
        else { return nil }
        self.device = device
        self.queue = queue
        do {
            let library = try device.makeLibrary(source: shaderSource, options: nil)
            let desc = MTLRenderPipelineDescriptor()
            desc.vertexFunction = library.makeFunction(name: "pf_vtx")
            desc.fragmentFunction = library.makeFunction(name: "pf_frag")
            desc.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipeline = try device.makeRenderPipelineState(descriptor: desc)
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
        self.layer = layer
    }

    /// Track the stream mode (the host can Reconfigure mid-stream). Size is in pixels.
    public func setDrawableSize(_ size: CGSize) {
        guard size.width > 0, size.height > 0 else { return }
        if layer.drawableSize != size { layer.drawableSize = size }
    }

    /// Draw one decoded frame to the next drawable and present it. Returns true on success;
    /// false when there's no drawable yet, a texture couldn't be made, or Metal errored — the
    /// caller then doesn't stamp a present for this frame.
    @discardableResult
    public func render(_ pixelBuffer: CVPixelBuffer) -> Bool {
        guard let textureCache,
              let luma = makeTexture(pixelBuffer, plane: 0, format: .r8Unorm, cache: textureCache),
              let chroma = makeTexture(pixelBuffer, plane: 1, format: .rg8Unorm, cache: textureCache)
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
        encoder.setRenderPipelineState(pipeline)
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
