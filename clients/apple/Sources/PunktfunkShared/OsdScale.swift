// Overlay UI scale — how much larger than this screen's normal UI the streaming chrome (the stats
// HUD, the quick-action ring) draws. `RenderScale` sizes the video itself; this sizes the chrome
// over it.
//
// Derived from the platform, never stored: pt normalises pixel density, not viewing distance, and
// the only device whose distance breaks pt's arm's-length assumption is a TV. Physical screen size
// is deliberately not an input — UIKit exposes no diagonal, and the EDID numbers other platforms
// report are wrong often enough to mis-size the very living-room case this exists for.
//
// The Android twin is `OsdScaleUi.kt`; `tv` must match its `TV_OSD_SCALE`.

import CoreGraphics

public enum OsdScale {
    /// A living-room set sits roughly 3x further away than a phone, but the chrome need not grow
    /// 3x: it is read in glances, and the ring is a stick target rather than dense text. 1.75
    /// clears the 10-foot legibility floor without walling off the game.
    public static let tv: CGFloat = 1.75

    /// The multiplier for this device. 1 anywhere held or sat in front of, where pt already fits.
    public static var current: CGFloat {
        #if os(tvOS)
        return tv
        #else
        return 1
        #endif
    }
}
