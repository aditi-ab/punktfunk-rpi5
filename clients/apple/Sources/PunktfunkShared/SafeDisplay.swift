// Safe-area stream sizing — the pure geometry behind the "safe area" resolution row.
//
// An iPhone clips the picture in HARDWARE: the sensor housing (notch / Dynamic Island) and the four
// rounded corners eat whatever the stream draws underneath them. The session view is deliberately
// edge-to-edge (ContentView's `.ignoresSafeArea()` on iOS) and the presenter aspect-FITS the host
// mode into it, so which pixels survive is decided entirely by the mode's aspect ratio:
//
//   * A 16:9 mode on a 19.5:9 phone pillarboxes, and those black bars land exactly on the unsafe
//     regions. That is why 1080p has always "just worked" and never needed a setting.
//   * The device's NATIVE mode has the screen's own aspect ratio, so it fills every pixel —
//     including the ones behind the housing and under the corner radii. That is the mode that
//     loses its corners, and the reason this file exists.
//
// So the fix needs no layout change and no input change: ask the host for a mode that is narrower
// by the safe-area insets, and the existing aspect-fit centres it inside the safe region. Pointer
// input keeps mapping correctly for free, because `hostPoint(from:)` derives the video rect from
// the live host mode (`AVMakeRect(aspectRatio:insideRect:)`) instead of assuming full-bleed.
//
// The formula is Moonlight's (its settings' resolution table carries the same row): full native
// height, width reduced by the left+right safe-area insets. Width-only is not a simplification —
// under aspect-fit only one axis can bind, and on a landscape phone that axis is always the
// horizontal one. Insetting the height too would shrink the picture without uncovering anything.

import Foundation

public enum SafeDisplay {
    /// The host rejects odd dimensions and anything under 320×200 (`validate_dimensions` in
    /// `pf-encode`), so the computed mode is even-floored and clamped exactly like `RenderScale`.
    public static let minWidth = 320
    public static let minHeight = 200

    /// A portrait top inset at or above this many points means a sensor housing rather than a
    /// status bar. Notched and Dynamic Island iPhones report 44–59 pt; a plain status bar (older
    /// iPhones, every iPad) reports 20–24 pt. Used only by [`sideInsetPoints`] and only when the
    /// horizontal insets are unavailable — see there for why that case exists at all.
    public static let housingTopInsetThreshold: Double = 40

    /// The per-side inset, in points, that the **landscape** stream will be subject to — which is
    /// not necessarily the inset the caller can read right now.
    ///
    /// The stream is always landscape, but the settings screen the resolution row is rendered in may
    /// be portrait, and `safeAreaInsets` only ever describes the CURRENT orientation. In portrait a
    /// notched iPhone reports its housing on `top` and reports `left`/`right` as zero, so reading
    /// the horizontal insets there would compute "no inset needed" for exactly the devices that
    /// need one.
    ///
    /// - In landscape, `max(left, right)` is the answer directly. (iOS symmetrizes the two so
    ///   content stays centred, so they normally agree; `max` is simply the safe reduction.)
    /// - In portrait, the housing's portrait TOP inset equals its landscape SIDE inset on every
    ///   notched/Dynamic Island iPhone — the same physical intrusion, measured on the axis that
    ///   happens to be vertical at the time — so `top` is the correct stand-in. It is accepted only
    ///   on phones and only past [`housingTopInsetThreshold`], so an iPad's status bar (or an older
    ///   iPhone's) never fabricates an inset for a device with nothing to avoid.
    ///
    /// Returns 0 when there is no housing to route around, which makes the safe mode identical to
    /// the native one — and the caller's dedup then drops the duplicate row on its own.
    public static func sideInsetPoints(
        left: Double, right: Double, top: Double, isPhone: Bool
    ) -> Double {
        let horizontal = max(left, right)
        if horizontal > 0 { return horizontal }
        if isPhone, top >= housingTopInsetThreshold { return top }
        return 0
    }

    /// The landscape safe-area mode in PIXELS: full native height, width reduced by
    /// `sideInsetPoints` on each side.
    ///
    /// `nativeWidth`/`nativeHeight` are the device's native landscape pixels (the long edge first —
    /// `UIScreen.main.nativeBounds` is portrait-oriented, so the caller swaps). `scale` converts the
    /// point-valued insets into those same pixels and must therefore be `nativeScale`, not `scale`:
    /// with Display Zoom on, the two differ and only the former matches `nativeBounds`.
    ///
    /// Even-floored and clamped so the result is directly host-valid — an odd width is rejected
    /// outright by the encoder, and an inset subtraction lands odd about half the time.
    public static func mode(
        nativeWidth: Int, nativeHeight: Int, sideInsetPoints: Double, scale: Double
    ) -> (width: Int, height: Int) {
        let insetPixels = max(0, sideInsetPoints) * max(scale, 1) * 2 // both sides
        let width = Double(nativeWidth) - insetPixels
        let evenFloor: (Double, Int) -> Int = { value, minimum in
            max(Int(value.rounded(.down)), minimum) / 2 * 2
        }
        return (evenFloor(width, minWidth), evenFloor(Double(nativeHeight), minHeight))
    }
}
