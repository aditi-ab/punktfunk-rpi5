// The gamepad UI's background colour families.
//
// A palette is NOT a second hand-tuned colour field: it is a hue rotation + saturation scale
// applied to the ONE field GamepadScreenBackground already draws, so every palette inherits its
// structure (dark corners, bright interior pools, warm-left/cool-right) and the brand default is
// exactly the shipped look — `violet` is the identity transform.
//
// The table and the `tint` math are mirrored in `pf-console-ui`'s `library.rs` (Rust) and the
// Android client's `GamepadPalette.kt` (Kotlin) under the same ids, so the shared `ui_palette`
// setting names the same colour family on every client. Keep the three copies in step: a palette
// added here without the others is a value the other clients will silently render as Violet.
//
// It lives in PunktfunkShared rather than next to the views because that is the target the tests
// can reach — the arithmetic below is the part that has to agree across three languages.

import Foundation
import simd

public struct GamepadPalette: Identifiable, Equatable, Sendable {
    /// The stored `ui_palette` value (`DefaultsKey.uiPalette`).
    public let id: String
    /// What the settings row shows.
    public let name: String
    /// Hue rotation about the grey axis, degrees — positive runs red → green → blue.
    public let hueDegrees: Double
    /// Saturation scale about luminance; 1 keeps the source saturation.
    public let saturation: Double

    /// The six shipped palettes, in cycling order: the brand violet, then cool → warm, then the
    /// neutral.
    public static let all: [GamepadPalette] = [
        GamepadPalette(id: "violet", name: "Violet", hueDegrees: 0, saturation: 1.0),
        GamepadPalette(id: "tide", name: "Tide", hueDegrees: -70, saturation: 1.0),
        GamepadPalette(id: "forest", name: "Forest", hueDegrees: -130, saturation: 0.9),
        GamepadPalette(id: "ember", name: "Ember", hueDegrees: 105, saturation: 1.0),
        GamepadPalette(id: "rose", name: "Rose", hueDegrees: 60, saturation: 0.95),
        GamepadPalette(id: "graphite", name: "Graphite", hueDegrees: 0, saturation: 0.12),
    ]

    /// The palette stored under `id`, falling back to the brand default — an unknown name is a
    /// palette a newer client shipped, not a reason to draw nothing.
    public static func named(_ id: String) -> GamepadPalette {
        all.first { $0.id == id } ?? all[0]
    }

    /// `true` for the identity transform, so the default path can skip the per-colour work.
    public var isIdentity: Bool { hueDegrees == 0 && saturation == 1 }

    /// Apply this palette to one RGB triple.
    public func tint(_ c: SIMD3<Double>) -> SIMD3<Double> {
        guard !isIdentity else { return c }
        return GamepadPalette.tint(c, hueDegrees: hueDegrees, saturation: saturation)
    }

    /// Rotate `c` about the grey axis by `hueDegrees` (Rodrigues — the same rotation the field's
    /// own ±8° warm/cool sway uses, in the same orientation) and scale its saturation about
    /// luminance. Clamped, because a large rotation can push a channel out of gamut.
    ///
    /// Deliberately computed here rather than left to SwiftUI's `.hueRotation`: that modifier's
    /// exact behaviour is the framework's, and the Rust and Kotlin clients have no equivalent —
    /// doing the arithmetic on the COLOURS keeps the three implementations identical.
    public static func tint(
        _ c: SIMD3<Double>, hueDegrees: Double, saturation: Double
    ) -> SIMD3<Double> {
        let a = hueDegrees * .pi / 180
        let cs = cos(a)
        let sn = sin(a)
        let invSqrt3 = 1 / 3.0.squareRoot()
        let grey = (c.x + c.y + c.z) / 3 * (1 - cs)
        // The `sn` term is cross(k, c) with k = (1,1,1)/√3.
        let rot = SIMD3(
            c.x * cs + (c.z - c.y) * invSqrt3 * sn + grey,
            c.y * cs + (c.x - c.z) * invSqrt3 * sn + grey,
            c.z * cs + (c.y - c.x) * invSqrt3 * sn + grey)
        let luma = 0.2126 * rot.x + 0.7152 * rot.y + 0.0722 * rot.z
        func mix(_ v: Double) -> Double { min(max(luma + (v - luma) * saturation, 0), 1) }
        return SIMD3(mix(rot.x), mix(rot.y), mix(rot.z))
    }
}
