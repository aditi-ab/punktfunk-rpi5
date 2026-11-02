// This device's display, as `EffectiveSettings.streamMode(native:)` reads a Native setting.

#if os(macOS)
import AppKit
#else
import UIKit
#endif

@MainActor
public enum NativeDisplay {
    #if os(iOS) || os(tvOS)
    /// The screen the app's window is on. A foldable has two built-in screens and `UIScreen.main`
    /// stays the cover one, so the key window decides. An attached monitor's scene never counts.
    public static var screen: UIScreen {
        let scenes = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .filter { $0.session.role == .windowApplication }
        let key = scenes.first { $0.windows.contains(where: \.isKeyWindow) }
        return (key ?? scenes.first)?.screen ?? UIScreen.main
    }

    /// `screen` in landscape pixels, at its top refresh.
    public static func mode(on screen: UIScreen) -> (width: Int, height: Int, hz: Int) {
        let bounds = screen.nativeBounds // portrait-oriented pixels (tvOS: the TV mode)
        return (
            Int(max(bounds.width, bounds.height)), Int(min(bounds.width, bounds.height)),
            screen.maximumFramesPerSecond)
    }
    #endif

    /// This app's display in landscape pixels, at its top refresh. On a Mac that is the panel
    /// below the camera housing (`NSScreen.notchSafePixelSize`), never the framebuffer a scaled
    /// mode renders into: native fullscreen stops below the housing, and only that fullscreen
    /// scans out Direct. The whole panel stays a mode of its own, picked by name.
    public static var mode: (width: Int, height: Int, hz: Int) {
        #if os(macOS)
        guard let screen = NSScreen.main else { return (1920, 1080, 60) }
        let safe = screen.notchSafePixelSize
        return (safe.width, safe.height, screen.maximumFramesPerSecond)
        #elseif os(visionOS)
        // No screen to read: a window can grow past 4K's detail, and every Vision Pro runs 90 Hz.
        // Fixed until visionOS reports a refresh ceiling; M5 panels reach 120.
        return (3840, 2160, 90)
        #else
        return mode(on: screen)
        #endif
    }
}
