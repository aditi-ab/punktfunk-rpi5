import XCTest

#if canImport(Metal)
import CoreVideo
import Metal
import QuartzCore
@testable import PunktfunkKit

final class MetalPresenterTests: XCTestCase {
    /// `MetalVideoPresenter.make()` compiles the runtime Metal shaders (the BT.709/BT.2020 YUV→RGB
    /// fragment shaders plus the Catmull-Rom luma sampler). A `nil` result on a GPU-equipped host
    /// means a shader failed to compile — this catches a malformed shader before it silently
    /// degrades stage-2 to a stage-1 fallback on device.
    func testPresenterInitCompilesShaders() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device available in this environment")
        }
        XCTAssertNotNil(
            MetalVideoPresenter.make(),
            "stage-2 Metal shaders failed to compile (presenter init returned nil)")
    }

    /// The HDR fix: `configure(hdr:)` must put the layer into the BT.2020-PQ EDR configuration with a
    /// reference-white anchor (`edrMetadata`) — the missing anchor was what made HDR render "too
    /// bright". SDR must use the plain 8-bit path with EDR off and no metadata. A mid-session flip is a
    /// per-mode reconfigure, so the round trip back to SDR must fully restore the SDR config.
    func testConfigureHDRSetsEDRAnchor() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: true)
        XCTAssertEqual(presenter.layer.pixelFormat, .rgba16Float, "HDR uses an EDR-capable drawable")
        XCTAssertNotNil(presenter.layer.colorspace, "HDR layer must be tagged (itur_2100_PQ)")
        XCTAssertTrue(
            presenter.layer.wantsExtendedDynamicRangeContent, "EDR must be requested on all platforms")
        XCTAssertNotNil(
            presenter.layer.edrMetadata,
            "HDR must anchor reference white via edrMetadata (the fix for 'too bright')")

        // Mid-session HDR→SDR flip: the 8-bit path, EDR off, no metadata.
        presenter.configure(hdr: false)
        XCTAssertEqual(presenter.layer.pixelFormat, .bgra8Unorm, "SDR uses the plain 8-bit drawable")
        XCTAssertFalse(presenter.layer.wantsExtendedDynamicRangeContent)
        XCTAssertNil(presenter.layer.edrMetadata)
    }

    /// A 10-bit SDR session presents through the 10-bit SDR drawable: sRGB-tagged like 8-bit SDR,
    /// no EDR, no metadata — and the layer must actually vend that format, which is the one thing
    /// the SDK's stale "two supported values" comment cannot tell us. HDR outranks the depth flag,
    /// and an 8-bit session keeps the 8-bit drawable.
    func testConfigureTenBitSDRWidensTheDrawable() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false, tenBitSDR: true)
        XCTAssertEqual(
            presenter.layer.pixelFormat, .bgr10a2Unorm, "10-bit SDR uses the 10-bit unorm drawable")
        XCTAssertNotNil(presenter.layer.colorspace, "10-bit SDR stays sRGB-tagged like 8-bit SDR")
        XCTAssertFalse(presenter.layer.wantsExtendedDynamicRangeContent, "10-bit SDR is not EDR")
        XCTAssertNil(presenter.layer.edrMetadata)
        presenter.layer.drawableSize = CGSize(width: 64, height: 64)
        if let drawable = presenter.layer.nextDrawable() {
            XCTAssertEqual(
                drawable.texture.pixelFormat, .bgr10a2Unorm,
                "the layer must vend the 10-bit format rather than fall back")
        }
        presenter.configure(hdr: true, tenBitSDR: true)
        XCTAssertEqual(presenter.layer.pixelFormat, .rgba16Float, "HDR outranks the depth flag")
        presenter.configure(hdr: false, tenBitSDR: false)
        XCTAssertEqual(
            presenter.layer.pixelFormat, .bgra8Unorm, "an 8-bit session keeps the 8-bit drawable")
    }

    /// `render` of a 10-bit SDR buffer reconciles the layer to the 10-bit drawable by itself — the
    /// per-frame path, not only the session-start configure — and presents without trapping.
    func testRenderTenBitSDRFrameWidensTheDrawable() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false)
        XCTAssertEqual(presenter.layer.pixelFormat, .bgra8Unorm)
        var pb: CVPixelBuffer?
        let attrs: [CFString: Any] = [kCVPixelBufferMetalCompatibilityKey: true]
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, 256, 256, kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            attrs as CFDictionary, &pb)
        guard status == kCVReturnSuccess, let pixelBuffer = pb else {
            throw XCTSkip("couldn't allocate a P010 test pixel buffer")
        }
        XCTAssertTrue(MetalVideoPresenter.tenBitBuffer(pixelBuffer))
        _ = presenter.render(pixelBuffer, isHDR: false)
        XCTAssertEqual(
            presenter.layer.pixelFormat, .bgr10a2Unorm,
            "a 10-bit SDR frame reconciles the layer to the 10-bit drawable")
    }

    /// `render` with a freshly-allocated NV12 buffer must present without crashing or hanging — the
    /// main-thread present path is the highest-risk part of the stage-2 rewrite. (A headless CI with no
    /// display can still allocate a drawable from a CAMetalLayer; if it can't, render returns false,
    /// which is also a valid non-crashing outcome.)
    func testRenderDoesNotCrashOnNV12Frame() throws {
        guard let presenter = MetalVideoPresenter.make() else {
            throw XCTSkip("no Metal device available in this environment")
        }
        presenter.configure(hdr: false)
        var pb: CVPixelBuffer?
        let attrs: [CFString: Any] = [kCVPixelBufferMetalCompatibilityKey: true]
        let status = CVPixelBufferCreate(
            kCFAllocatorDefault, 256, 256, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            attrs as CFDictionary, &pb)
        guard status == kCVReturnSuccess, let pixelBuffer = pb else {
            throw XCTSkip("could not allocate a test pixel buffer")
        }
        // Just asserting it returns (true or false) without trapping — the layer may have no drawable
        // source headless, so a false return is acceptable.
        _ = presenter.render(pixelBuffer, isHDR: false)
    }
}
#endif
