// This device's display, as `EffectiveSettings.streamMode(native:)` reads a Native setting.

#if os(macOS)
import AppKit
#else
import UIKit
#endif

@MainActor
public enum NativeDisplay {
    /// The main display in landscape pixels, at its top refresh. On a Mac that is the panel, not
    /// the framebuffer a scaled mode renders into (`NSScreen.panelPixelSize`).
    public static var mode: (width: Int, height: Int, hz: Int) {
        #if os(macOS)
        guard let screen = NSScreen.main else { return (1920, 1080, 60) }
        let panel = screen.panelPixelSize
        return (panel.width, panel.height, screen.maximumFramesPerSecond)
        #else
        let bounds = UIScreen.main.nativeBounds // portrait-oriented pixels (tvOS: the TV mode)
        return (
            Int(max(bounds.width, bounds.height)), Int(min(bounds.width, bounds.height)),
            UIScreen.main.maximumFramesPerSecond)
        #endif
    }
}
