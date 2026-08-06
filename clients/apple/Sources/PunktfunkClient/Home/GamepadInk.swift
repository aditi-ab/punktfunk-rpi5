// The ink the gamepad UI draws with under the chosen background palette.
//
// The console screens were white-on-dark throughout with the brand violet hardcoded as the
// accent. Both had to become palette-derived at once: a pale field needs dark text or it is
// unreadable, and a violet focus wash on a copper field is exactly the clash this exists to fix.
//
// Handed down the view tree as an environment value rather than passed to each screen, so a
// leaf (a row, a hint pill, a card) can ask for the right colour without every caller in between
// knowing about palettes. `pf-console-ui` does the same thing with a thread-local `Ink`.

import PunktfunkShared
import SwiftUI

#if os(iOS) || os(macOS) || os(tvOS)

struct GamepadInk: Equatable, Sendable {
    /// Primary text/glyph colour.
    let fg: Color
    /// Focus wash, selected tab pill, switch track, caret — the palette's own accent.
    let accent: Color
    /// What reads ON the accent (a filled pill's label).
    let onAccent: Color
    /// The base fill every glass surface starts from.
    let glass: Color
    /// What a wash laid UNDER text tends toward: black on a dark field, white on a pale one.
    let shade: Color
    /// How hard those washes go. A pale field needs far less — mixing toward white at the dark
    /// field's strength bleaches the chroma straight out of the gradient.
    let shadeScale: Double
    /// True when the field is pale, for the few places that need to branch rather than blend
    /// (a material's `colorScheme`, a shadow's presence).
    let isLight: Bool

    /// The foreground at `alpha`.
    func fg(_ alpha: Double) -> Color { fg.opacity(alpha) }
    /// The accent at `alpha`.
    func accent(_ alpha: Double) -> Color { accent.opacity(alpha) }
    /// A wash under text: `alpha` is the dark-field strength, scaled for a pale one.
    func shade(_ alpha: Double) -> Color { shade.opacity(alpha * shadeScale) }

    static func of(_ p: GamepadPalette) -> GamepadInk {
        let accent = Color(red: p.accent.x, green: p.accent.y, blue: p.accent.z)
        let accentLuma = 0.2126 * p.accent.x + 0.7152 * p.accent.y + 0.0722 * p.accent.z
        // Chosen by luminance, not by `light`: an accent is picked for contrast against the
        // GLASS, not against the field.
        let onAccent: Color = accentLuma > 0.55 ? .black : .white
        guard p.light else {
            return GamepadInk(
                fg: .white, accent: accent, onAccent: onAccent,
                glass: Color(red: 0.086, green: 0.086, blue: 0.125),
                shade: .black, shadeScale: 1, isLight: false)
        }
        return GamepadInk(
            // Tinted toward the palette's own ground so it doesn't read as a foreign grey.
            fg: Color(red: p.ground.x * 0.16, green: p.ground.y * 0.14, blue: p.ground.z * 0.20),
            accent: accent, onAccent: onAccent,
            glass: .white,
            shade: .white, shadeScale: 0.45, isLight: true)
    }

    /// The shipped dark look — what a preview or a test composition gets.
    static let dark = GamepadInk.of(GamepadPalette.named("violet"))
}

private struct GamepadInkKey: EnvironmentKey {
    static let defaultValue = GamepadInk.dark
}

extension EnvironmentValues {
    /// The ink of the palette currently drawing. Set once, high up (see `GamepadInkModifier`).
    var gamepadInk: GamepadInk {
        get { self[GamepadInkKey.self] }
        set { self[GamepadInkKey.self] = newValue }
    }
}

extension View {
    /// Resolve the stored `ui_palette` and publish its ink to everything below. Applied by the
    /// gamepad screens' common root so no individual view has to read the setting.
    func gamepadPaletteInk() -> some View { modifier(GamepadInkModifier()) }
}

private struct GamepadInkModifier: ViewModifier {
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"

    func body(content: Content) -> some View {
        content.environment(\.gamepadInk, GamepadInk.of(GamepadPalette.named(paletteID)))
    }
}

#endif
