import XCTest

#if canImport(Metal)
import Metal
@testable import PunktfunkKit

final class MetalPresenterTests: XCTestCase {
    /// `MetalVideoPresenter.init?()` compiles the runtime Metal shaders (the BT.709/BT.2020 YUV→RGB
    /// fragment shaders plus the Catmull-Rom luma sampler). A `nil` result on a GPU-equipped host
    /// means a shader failed to compile — this catches a malformed shader before it silently
    /// degrades stage-2 to a stage-1 fallback on device.
    func testPresenterInitCompilesShaders() throws {
        guard MTLCreateSystemDefaultDevice() != nil else {
            throw XCTSkip("no Metal device available in this environment")
        }
        XCTAssertNotNil(
            MetalVideoPresenter(),
            "stage-2 Metal shaders failed to compile (presenter init returned nil)")
    }
}
#endif
