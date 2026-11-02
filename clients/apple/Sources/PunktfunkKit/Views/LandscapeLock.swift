// Phones hold landscape under the stream and the console: both are landscape layouts.

#if os(iOS)
import UIKit

/// Holds a phone's screen in landscape while the console shows or the stream's picture does.
/// Tablets and other large screens stay free, and a monitor that takes the picture frees the
/// phone. A landscape-only answer also overrides the system portrait lock. The app delegate asks
/// `mask(for:)` on every rotation, so the screen size is read live: a fold or unfold re-decides.
@MainActor
public enum LandscapeLock {
    /// Under this shorter screen side, in points, a screen is a phone's. The smallest iPad is 744.
    static let phoneSide: CGFloat = 600

    /// The console is on screen.
    public static var console = false {
        didSet { if console != oldValue { refresh() } }
    }

    /// A stream is up.
    public static var streaming = false {
        didSet { if streaming != oldValue { refresh() } }
    }

    /// The orientations `window` may take now.
    public static func mask(for window: UIWindow?) -> UIInterfaceOrientationMask {
        guard let scene = window?.windowScene, holds(scene) else { return .all }
        return .landscape
    }

    private static func holds(_ scene: UIWindowScene) -> Bool {
        let size = scene.screen.bounds.size
        guard scene.session.role == .windowApplication, min(size.width, size.height) < phoneSide
        else { return false }
        return console || (streaming && ExternalDisplay.shared.screen == nil)
    }

    /// Ask every app scene again. One that now holds turns to landscape at once.
    static func refresh() {
        for case let scene as UIWindowScene in UIApplication.shared.connectedScenes
        where scene.session.role == .windowApplication {
            for window in scene.windows {
                window.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
            }
            if holds(scene) {
                scene.requestGeometryUpdate(.iOS(interfaceOrientations: .landscape))
            }
        }
    }
}
#endif
