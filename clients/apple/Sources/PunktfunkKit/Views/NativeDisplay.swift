// This device's display, as `EffectiveSettings.streamMode(native:)` reads a Native setting.

#if os(macOS)
import AppKit
#else
import UIKit
#endif

@MainActor
public enum NativeDisplay {
    /// The main display in landscape pixels, at its top refresh.
    public static var mode: (width: Int, height: Int, hz: Int) {
        #if os(macOS)
        guard let screen = NSScreen.main else { return (1920, 1080, 60) }
        let scale = screen.backingScaleFactor
        return (
            Int(screen.frame.width * scale), Int(screen.frame.height * scale),
            screen.maximumFramesPerSecond)
        #elseif os(visionOS)
        // No screen to read: a window can grow past 4K's detail, and every Vision Pro runs 90 Hz.
        // Fixed until visionOS reports a refresh ceiling; M5 panels reach 120.
        return (3840, 2160, 90)
        #else
        let bounds = UIScreen.main.nativeBounds // portrait-oriented pixels (tvOS: the TV mode)
        return (
            Int(max(bounds.width, bounds.height)), Int(min(bounds.width, bounds.height)),
            UIScreen.main.maximumFramesPerSecond)
        #endif
    }
}
