// A Vision Pro session's picture: a RealityKit texture the Metal presenter always draws into.

#if os(visionOS)
import CoreGraphics
import Metal
import Observation
import RealityKit
import UIKit

/// A Vision Pro session's picture, for the whole session. The Metal presenter draws each frame
/// into this queue's next drawable in place of its layer's, and RealityKit shows the newest at
/// the panel's rate: flat in the session's window, or bent in the curve's volume. The stop only
/// moves the texture, so the presenter never changes path. The queue follows the frame's size
/// and range: a change rebuilds it on the main actor, and frames wait.
///
/// SDR drawables are sRGB textures, drawn through a plain view: the shader writes encoded
/// BT.709, and RealityKit decodes it on sampling. HDR drawables are half-float under an HDR
/// colour texture: the shader writes linear values past 1.0 and nothing tone-maps them.
@Observable
public final class CurveTarget: @unchecked Sendable {
    /// Main actor: what the screen's material shows; nil until the first frame's size lands.
    public private(set) var texture: TextureResource?
    /// Main actor: the picture's width over its height, once a frame has landed.
    public private(set) var aspect: Double?
    /// Main actor: the curve's volume shows the picture, not the session's window.
    public var curved = false

    /// Main actor: the session's stream view; a pinch on the curve goes there.
    @ObservationIgnored public weak var pointer: (any CurvePointerSink)?
    /// Main actor: the volume's view. Keyed, its window keeps this session's keyboard and mouse.
    @ObservationIgnored public weak var anchor: CurveAnchor?

    @ObservationIgnored private let lock = NSLock()
    @ObservationIgnored private var queue: TextureResource.DrawableQueue?
    @ObservationIgnored private var building: Shape?

    public init() {}

    /// A queue's size and range.
    private struct Shape: Equatable {
        let width: Int
        let height: Int
        let hdr: Bool

        var format: MTLPixelFormat { hdr ? .rgba16Float : .bgra8Unorm_srgb }
    }

    struct Frame {
        let drawable: TextureResource.Drawable
        /// The drawable's texture as the presenter's pipelines write it.
        let view: MTLTexture
    }

    /// Render thread: the next drawable at `size` and range, or nil while the queue is rebuilt.
    func next(size: CGSize, hdr: Bool) -> Frame? {
        let shape = Shape(width: Int(size.width), height: Int(size.height), hdr: hdr)
        guard shape.width > 0, shape.height > 0 else { return nil }
        lock.lock()
        let queue = self.queue
        let fits = queue?.width == shape.width && queue?.height == shape.height
            && queue?.pixelFormat == shape.format
        let build = !fits && building != shape
        if build { building = shape }
        lock.unlock()
        if build { Task { @MainActor in self.build(shape) } }
        guard fits, let queue, let drawable = try? queue.nextDrawable(),
              let view = drawable.texture.makeTextureView(
                  pixelFormat: hdr ? .rgba16Float : .bgra8Unorm)
        else { return nil }
        return Frame(drawable: drawable, view: view)
    }

    /// A queue that fails to build is asked for again from a frame half a second later.
    @MainActor private func build(_ shape: Shape) {
        let descriptor = TextureResource.DrawableQueue.Descriptor(
            pixelFormat: shape.format, width: shape.width, height: shape.height,
            usage: [.renderTarget, .shaderRead, .pixelFormatView], mipmapsMode: .none)
        guard let queue = try? TextureResource.DrawableQueue(descriptor) else {
            Task { @MainActor in
                try? await Task.sleep(for: .milliseconds(500))
                self.lock.withLock { if self.building == shape { self.building = nil } }
            }
            return
        }
        queue.allowsNextDrawableTimeout = true
        // A range change takes a texture of the other semantic: the screen rebuilds its material.
        if texture == nil || shape.hdr != (self.queue?.pixelFormat == .rgba16Float) {
            texture = Self.blank(semantic: shape.hdr ? .hdrColor : .color)
        }
        texture?.replace(withDrawables: queue)
        lock.lock()
        self.queue = queue
        building = nil
        lock.unlock()
        aspect = Double(shape.width) / Double(shape.height)
    }

    /// One black pixel: a texture to hang the first queue on.
    @MainActor private static func blank(semantic: TextureResource.Semantic) -> TextureResource? {
        guard let context = CGContext(
            data: nil, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue),
            let image = context.makeImage()
        else { return nil }
        return try? TextureResource(image: image, options: .init(semantic: semantic))
    }
}

/// A UIKit view in the curve's volume. The soft keyboard opens only for the key window's first
/// responder, so while the curve shows the picture this one takes it for the stream view.
public final class CurveAnchor: UIView, UIKeyInput {
    /// Main actor: the stream view the keyboard types into, set as it opens.
    weak var keys: StreamLayerUIView?

    public override var canBecomeFirstResponder: Bool { true }

    // Literal keys, as the stream view takes them.
    public var autocorrectionType: UITextAutocorrectionType { get { .no } set {} }
    public var autocapitalizationType: UITextAutocapitalizationType { get { .none } set {} }
    public var spellCheckingType: UITextSpellCheckingType { get { .no } set {} }
    public var smartQuotesType: UITextSmartQuotesType { get { .no } set {} }
    public var smartDashesType: UITextSmartDashesType { get { .no } set {} }
    public var smartInsertDeleteType: UITextSmartInsertDeleteType { get { .no } set {} }
    public var keyboardType: UIKeyboardType { get { .asciiCapable } set {} }

    public var hasText: Bool { false }
    public func insertText(_ text: String) { keys?.insertText(text) }
    public func deleteBackward() { keys?.deleteBackward() }
}

/// What a pinch on the curved screen is doing.
public enum CurvePointerPhase: Sendable {
    case began, moved, ended
}

/// The session's stream view: it takes the curve's pinches as its own, and Match window follows
/// the curve's size while the curve shows the picture.
@MainActor
public protocol CurvePointerSink: AnyObject {
    /// A pinch at picture coordinates `u`, `v`: 0…1 from the top left.
    func curvePointer(u: Double, v: Double, phase: CurvePointerPhase)
    /// The curved screen's size in points: its arc's length by its height.
    func curveSize(width: Double, height: Double)
    /// The picture moved onto the curve, or back into the window.
    func curveShown(_ shown: Bool)
}
#endif
