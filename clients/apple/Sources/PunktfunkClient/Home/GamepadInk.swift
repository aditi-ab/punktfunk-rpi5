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
    /// The glass base at `alpha` — what a surface's material is washed with so it carries the
    /// palette's hue (the console fills its panels with exactly this colour).
    func glass(_ alpha: Double) -> Color { glass.opacity(alpha) }
    /// A drop shadow: always black — a white shadow is not a shadow — but softened on a pale
    /// field, where full-strength black under every card reads as a smear rather than depth.
    func shadow(_ alpha: Double) -> Color { .black.opacity(alpha * (isLight ? 0.4 : 1)) }

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

    /// The ink for a stored `ui_palette` id, resolved WITHOUT the environment.
    ///
    /// For the screens that publish their own ink with `gamepadPaletteInk()`. A view's
    /// `@Environment` resolves against its PARENT — the modifier a screen applies to its own body
    /// covers its descendants, never the body's own `ink.…` references — so such a screen reads
    /// whatever was published ABOVE it. Nested inside another gamepad screen (the iOS shell's
    /// layers) that happens to be right; presented as a cover or a sheet (tvOS, macOS) there is
    /// nothing above it and it gets the bare dark default. That is precisely how a pale palette
    /// came out with a WHITE title, white row labels and a violet focus wash on an Apple TV, while
    /// the child views in the same screen — the hint bar, the host tiles, the glass — were
    /// correctly dark-on-pale.
    ///
    /// Declare it beside an `@AppStorage(DefaultsKey.uiPalette)`, which is what re-renders the
    /// screen when the setting changes (`GamepadInkModifier` reads the same key).
    static func stored(_ paletteID: String) -> GamepadInk {
        .of(GamepadPalette.named(paletteID))
    }

    /// The online pip — deliberately NOT palette-derived: a status colour must not change
    /// meaning with the wallpaper (the console's rule; this is its `ONLINE_GREEN` verbatim).
    static let onlineGreen = Color(red: 0.20, green: 0.84, blue: 0.29)
    /// An armed destructive action (the host menu's Remove). Palette-independent for exactly the
    /// same reason as the pip above, and the more strongly so: the one colour on this UI that
    /// means "this does not come back" cannot be allowed to drift toward the wallpaper on a warm
    /// palette, or read as a highlight on a red one.
    static let warningRed = Color(red: 0.94, green: 0.28, blue: 0.26)
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
    /// Resolve the stored `ui_palette` and publish its ink — AND the matching colour scheme — to
    /// everything below. Applied by the gamepad screens' common root so no individual view has to
    /// read the setting.
    ///
    /// `active` exists for the one surface that is the same view in both worlds: `LibraryView`
    /// renders the coverflow under the gamepad UI and a plain grid without it. Passing `false`
    /// publishes nothing, because the touch/desktop layouts sit on the SYSTEM background, where a
    /// palette's scheme would invert their own system colours instead of matching them.
    func gamepadPaletteInk(_ active: Bool = true) -> some View {
        modifier(GamepadInkModifier(active: active))
    }
}

private struct GamepadInkModifier: ViewModifier {
    var active = true
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    /// The ambient scheme from ABOVE this modifier — what gets republished unchanged when the
    /// gamepad UI isn't the one drawing, so `active: false` is a true no-op rather than a branch
    /// that would change this view's identity.
    @Environment(\.colorScheme) private var systemScheme

    func body(content: Content) -> some View {
        let palette = GamepadPalette.named(paletteID)
        return content
            .environment(\.gamepadInk, active ? GamepadInk.of(palette) : .dark)
            // The ink alone was never enough. Every SYSTEM-derived colour that lands on these
            // screens — `.secondary` in a placeholder, a `.bordered` button's chrome, a
            // NavigationStack's title, a material's frost — resolves against the DEVICE's
            // appearance, which no part of this app had ever set. On iPhone and Mac that is often
            // Light, so the pale palettes looked correct by accident; an Apple TV is Dark
            // essentially always, so on tvOS every one of them came out WHITE on a pale field and
            // the interface was unreadable. Publishing the scheme here — once, beside the ink it
            // has to agree with — is what makes a pale palette mean "light" to UIKit too.
            .environment(\.colorScheme, active ? (palette.light ? .light : .dark) : systemScheme)
    }
}

#endif
