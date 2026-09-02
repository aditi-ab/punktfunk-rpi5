// Where the overlay-scale preference meets SwiftUI: this device's `OsdScale.DeviceClass`, and the
// modifier that draws the stats HUD at the resolved size.
//
// The HUD is scaled with `scaleEffect` from the corner it occupies, so it grows inward and cannot
// push itself off screen — the same anchor its enter transition already uses. The quick-action
// ring is NOT scaled this way: it centres on wherever the twist happened, and a `scaleEffect`
// about a fixed anchor would walk an off-centre ring across the screen, so `RingOverlay` takes the
// multiplier as a parameter and applies it to its own metrics.

import PunktfunkShared
import SwiftUI

#if canImport(UIKit)
import UIKit
#endif

extension OsdScale {
    /// This device's class. Fixed per platform except on iOS, where the idiom separates a phone
    /// from an iPad. Physical screen size plays no part — UIKit does not report it.
    public static var deviceClass: DeviceClass {
        #if os(tvOS)
        return .tv
        #elseif os(macOS)
        return .desktop
        #else
        return UIDevice.current.userInterfaceIdiom == .pad ? .tablet : .handheld
        #endif
    }

    /// The multiplier to draw with for a stored preference on this device.
    public static func resolved(_ pref: Double) -> Double {
        resolve(pref, for: deviceClass)
    }
}

extension View {
    /// Draw the stats HUD at the overlay scale, growing out of `anchor` — the corner it is pinned
    /// to. A no-op at 1.0, which is every near-field device's Automatic.
    func osdScaled(_ pref: Double, anchor: UnitPoint) -> some View {
        scaleEffect(OsdScale.resolved(pref), anchor: anchor)
    }
}
