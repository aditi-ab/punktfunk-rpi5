// The gamepad UI's background colour families, and the ink each one calls for.
//
// A palette is a short ordered ramp of DISTINCT hues, not one hue at several brightnesses. The
// 4×4 mesh samples that ramp diagonally with a per-cell offset (`cellRamp`), so neighbouring
// cells land on different parts of it and the colours pool and swirl the way a real gradient
// poster does; the mesh's existing control-point drift then moves those pools around. An earlier
// version rotated ONE field's hue per palette, which is why every non-default palette read flat.
//
// A palette also owns the UI sitting on it: `accent` is the focus wash / selected pill / switch
// colour, and `light` flips the ink so a pale field gets dark text instead of white.
//
// The table, `ramp` and `cellRamp` are mirrored in `pf-console-ui`'s `library.rs` (Rust) and the
// Android client's `GamepadPalette.kt` (Kotlin) under the same ids, so one `ui_palette` value is
// one look on every client. Keep the three copies in step: a palette added here without the
// others is a value the other clients silently render as Violet.
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
    /// The colour ramp, dark end first. Empty = use `violetMesh` verbatim (the brand default,
    /// kept bit-identical to what every install already sees).
    public let stops: [SIMD3<Double>]
    /// The field's ground — what the corners settle onto and what the calm mix lifts toward.
    public let ground: SIMD3<Double>
    /// The UI accent: focus wash, selected tab pill, switch track, caret.
    public let accent: SIMD3<Double>
    /// A pale field: the UI flips to dark ink and the legibility scrims go white.
    public let light: Bool

    /// Where each of the 16 mesh cells samples the ramp. The base is the diagonal
    /// `0.5·(x + y)` — top-left is the ramp's dark end, bottom-right its bright one — and the
    /// per-cell nudges break the banding a pure diagonal would give, so hues pool instead of
    /// striping.
    static let cellRamp: [Double] = [
         0.10, -0.06,  0.04, -0.12,
        -0.08,  0.14, -0.10,  0.06,
         0.06, -0.12,  0.16, -0.04,
        -0.10,  0.08, -0.06,  0.12,
    ]

    /// The brand default's 16 mesh colours, row-major 4×4: dark-violet corners sink the frame,
    /// the edges carry mid-tone violets, and the interior holds the bright brand family.
    public static let violetMesh: [SIMD3<Double>] = {
        let corner = SIMD3(0.075, 0.060, 0.160)
        return [
            corner, SIMD3(0.34, 0.27, 0.72), SIMD3(0.30, 0.26, 0.74), corner,
            SIMD3(0.42, 0.20, 0.54), SIMD3(0.49, 0.39, 0.95), SIMD3(0.28, 0.31, 0.84), SIMD3(0.16, 0.26, 0.64),
            SIMD3(0.45, 0.23, 0.60), SIMD3(0.53, 0.31, 0.75), SIMD3(0.35, 0.35, 0.91), SIMD3(0.19, 0.28, 0.70),
            corner, SIMD3(0.22, 0.18, 0.54), SIMD3(0.24, 0.20, 0.58), corner,
        ]
    }()

    /// The brand default's blob ramp — the four colours the pre-18/15 legacy field used, kept so
    /// `violet` is unchanged on older OSes too.
    static let violetBlobs: [SIMD3<Double>] = [
        SIMD3(0.53, 0.47, 0.96), SIMD3(0.24, 0.20, 0.72), SIMD3(0.62, 0.30, 0.80),
        SIMD3(0.22, 0.38, 0.86), SIMD3(0.53, 0.47, 0.96),
    ]

    /// The thirteen shipped palettes: the brand default, six more dark fields, then six pale
    /// ones. Cycling order runs dark → light, so stepping the row walks the whole range one way.
    public static let all: [GamepadPalette] = [
        // --- dark fields (white ink) ---
        GamepadPalette(
            id: "violet", name: "Violet", stops: [],
            ground: SIMD3(0.075, 0.060, 0.160), accent: SIMD3(0.525, 0.471, 0.961), light: false),
        GamepadPalette(
            // For OLED and AMOLED panels, where a black pixel is a pixel switched off — no glow,
            // no power. The first two stops are literally (0,0,0), so the shaded half of the
            // field is genuinely off rather than "very dark grey", and the ground is pure black
            // too: the calm mix on the form screens lifts toward nothing. What is left is a
            // faint indigo→violet ember in the bright corner. The accent stays the brand violet
            // — focus has to be findable on black.
            //
            // Named for the look, not the panel technology: black with a thin violet corona is an
            // eclipse, and it belongs beside Nebula and Abyss rather than reading as a spec sheet.
            // ⚠ The ID stays "oled" — it is the stored `ui_palette` value AND the cross-client key
            // (pf-console-ui's library.rs, the Android GamepadPalette.kt), so renaming it would
            // orphan every saved choice and desync the three clients. Only the label moved.
            id: "oled", name: "Eclipse",
            stops: [SIMD3(0.000, 0.000, 0.000), SIMD3(0.000, 0.000, 0.000),
                    SIMD3(0.010, 0.020, 0.100), SIMD3(0.045, 0.016, 0.115),
                    SIMD3(0.120, 0.024, 0.130)],
            ground: SIMD3(0, 0, 0), accent: SIMD3(0.525, 0.471, 0.961), light: false),
        GamepadPalette(
            // Deep indigo climbing through violet into a hot magenta.
            id: "nebula", name: "Nebula",
            stops: [SIMD3(0.07, 0.05, 0.20), SIMD3(0.26, 0.14, 0.54), SIMD3(0.52, 0.20, 0.72),
                    SIMD3(0.82, 0.26, 0.62), SIMD3(0.98, 0.46, 0.68)],
            ground: SIMD3(0.055, 0.040, 0.135), accent: SIMD3(0.95, 0.42, 0.72), light: false),
        GamepadPalette(
            // Ink-blue water: teal → cerulean → a violet undertow.
            id: "abyss", name: "Abyss",
            stops: [SIMD3(0.02, 0.10, 0.17), SIMD3(0.04, 0.28, 0.42), SIMD3(0.07, 0.46, 0.63),
                    SIMD3(0.16, 0.38, 0.78), SIMD3(0.26, 0.22, 0.58)],
            ground: SIMD3(0.018, 0.070, 0.130), accent: SIMD3(0.26, 0.76, 0.92), light: false),
        GamepadPalette(
            // Banked coals: plum embers → crimson → burnt orange → gold.
            id: "ember", name: "Ember",
            stops: [SIMD3(0.16, 0.03, 0.10), SIMD3(0.45, 0.06, 0.12), SIMD3(0.72, 0.18, 0.06),
                    SIMD3(0.90, 0.42, 0.08), SIMD3(0.95, 0.68, 0.18)],
            ground: SIMD3(0.090, 0.035, 0.040), accent: SIMD3(0.98, 0.62, 0.26), light: false),
        GamepadPalette(
            // Forest floor into moss and a lime break.
            id: "moss", name: "Moss",
            stops: [SIMD3(0.03, 0.11, 0.09), SIMD3(0.06, 0.27, 0.20), SIMD3(0.09, 0.45, 0.31),
                    SIMD3(0.28, 0.61, 0.28), SIMD3(0.58, 0.77, 0.31)],
            ground: SIMD3(0.025, 0.085, 0.070), accent: SIMD3(0.48, 0.86, 0.46), light: false),
        GamepadPalette(
            // Neutral, but never flat: barely-there saturation that still travels from a cool
            // charcoal to a warm stone.
            id: "graphite", name: "Graphite",
            stops: [SIMD3(0.06, 0.07, 0.11), SIMD3(0.15, 0.18, 0.25), SIMD3(0.30, 0.31, 0.35),
                    SIMD3(0.45, 0.42, 0.38), SIMD3(0.60, 0.56, 0.49)],
            ground: SIMD3(0.055, 0.055, 0.070), accent: SIMD3(0.78, 0.80, 0.86), light: false),
        // --- pale fields (dark ink) ---
        GamepadPalette(
            // The holographic foil: rose → lilac → periwinkle → aqua, with a white bloom.
            id: "holo", name: "Holo",
            stops: [SIMD3(0.99, 0.72, 0.90), SIMD3(0.80, 0.60, 0.98), SIMD3(0.58, 0.62, 0.99),
                    SIMD3(0.55, 0.86, 0.98), SIMD3(0.94, 0.98, 1.00)],
            ground: SIMD3(0.96, 0.92, 0.99), accent: SIMD3(0.42, 0.28, 0.86), light: true),
        GamepadPalette(
            // The poster sunset: periwinkle → magenta → scarlet → tangerine → gold.
            id: "sunset", name: "Sunset",
            stops: [SIMD3(0.55, 0.45, 0.92), SIMD3(0.86, 0.31, 0.66), SIMD3(0.97, 0.26, 0.34),
                    SIMD3(0.99, 0.51, 0.18), SIMD3(1.00, 0.80, 0.22)],
            ground: SIMD3(0.98, 0.74, 0.34), accent: SIMD3(0.64, 0.13, 0.44), light: true),
        GamepadPalette(
            // Peach into blush and lilac — the softest of the set.
            id: "bloom", name: "Bloom",
            stops: [SIMD3(1.00, 0.86, 0.72), SIMD3(0.99, 0.73, 0.79), SIMD3(0.95, 0.65, 0.89),
                    SIMD3(0.82, 0.68, 0.96), SIMD3(0.73, 0.79, 0.99)],
            ground: SIMD3(0.99, 0.90, 0.89), accent: SIMD3(0.72, 0.24, 0.55), light: true),
        GamepadPalette(
            // First light: pale gold → coral → lilac.
            id: "dawn", name: "Dawn",
            stops: [SIMD3(1.00, 0.92, 0.70), SIMD3(1.00, 0.80, 0.62), SIMD3(0.99, 0.66, 0.62),
                    SIMD3(0.90, 0.62, 0.78), SIMD3(0.77, 0.69, 0.95)],
            ground: SIMD3(1.00, 0.93, 0.82), accent: SIMD3(0.82, 0.33, 0.28), light: true),
        GamepadPalette(
            // Sea glass: mint → aqua → a pale sky.
            id: "mint", name: "Mint",
            stops: [SIMD3(0.82, 0.98, 0.90), SIMD3(0.62, 0.94, 0.88), SIMD3(0.55, 0.88, 0.95),
                    SIMD3(0.63, 0.82, 0.99), SIMD3(0.82, 0.87, 1.00)],
            ground: SIMD3(0.90, 0.98, 0.96), accent: SIMD3(0.04, 0.42, 0.40), light: true),
        GamepadPalette(
            // Near-white, but iridescent rather than flat — rose, sky, mint and cream in turn.
            id: "opal", name: "Opal",
            stops: [SIMD3(0.98, 0.92, 0.96), SIMD3(0.87, 0.93, 0.99), SIMD3(0.91, 0.99, 0.95),
                    SIMD3(0.99, 0.96, 0.88), SIMD3(0.94, 0.90, 0.99)],
            ground: SIMD3(0.97, 0.96, 0.99), accent: SIMD3(0.36, 0.32, 0.44), light: true),
    ]

    /// The palette stored under `id`, falling back to the brand default — an unknown name is a
    /// palette a newer client shipped, not a reason to draw nothing.
    public static func named(_ id: String) -> GamepadPalette {
        all.first { $0.id == id } ?? all[0]
    }

    /// Sample an ordered colour ramp at `t` ∈ [0, 1] (linear between neighbouring stops).
    public static func ramp(_ stops: [SIMD3<Double>], _ t: Double) -> SIMD3<Double> {
        guard let first = stops.first else { return SIMD3(0, 0, 0) }
        guard stops.count > 1 else { return first }
        let x = min(max(t, 0), 1) * Double(stops.count - 1)
        let i = min(Int(x.rounded(.down)), stops.count - 2)
        let f = x - Double(i)
        return stops[i] + (stops[i + 1] - stops[i]) * f
    }

    /// The 16 mesh colours for this palette: the ramp sampled per cell, or `violetMesh` verbatim
    /// for the brand default.
    public var meshColors: [SIMD3<Double>] {
        guard !stops.isEmpty else { return Self.violetMesh }
        return (0..<16).map { i in
            let (x, y) = (Double(i % 4) / 3.0, Double(i / 4) / 3.0)
            return Self.ramp(stops, 0.5 * (x + y) + Self.cellRamp[i])
        }
    }

    /// Four drifting blob colours for the pre-18/15 legacy field. Spread across the ramp so it
    /// still shows several hues at once.
    public var blobColors: [SIMD3<Double>] {
        let s = stops.isEmpty ? Self.violetBlobs : stops
        return (0..<4).map { Self.ramp(s, 0.15 + 0.25 * Double($0)) }
    }
}
