// GlassStyle.swift — the app's single, availability-gated entry point to Apple's "Liquid
// Glass" (iOS / macOS / tvOS 26). Every Liquid Glass symbol (glassEffect, Glass, the
// .glassProminent button style …) is HARD-gated to OS 26: referencing one with our
// deployment targets (macOS 14 / iOS 17 / tvOS 17) is a COMPILE error, not a silent no-op,
// unless it sits behind `if #available`. So all glass in the app routes through the two
// helpers below, each of which falls back to the EXACT look the app shipped before
// (.regularMaterial / .borderedProminent) — nothing regresses on older OSes, and the gating
// lives in exactly one file.

import SwiftUI

// MARK: - Glass background

/// Liquid Glass behind a floating / overlay surface, with the pre-26 `.regularMaterial`
/// look as the fallback. Use ONLY on the floating control / overlay layer (the streaming
/// HUD, the trust card, the touch exit chip) — never on content tiles or dense forms (HIG).
///
/// `glassEffect()`'s own default shape is a Capsule, so panels MUST pass an explicit shape
/// (a RoundedRectangle / Circle) or they render as a pill. `interactive` makes the glass
/// react to press — only meaningful when the glass itself is the tap target.
private struct GlassBackground<S: Shape>: ViewModifier {
    let shape: S
    var interactive = false

    func body(content: Content) -> some View {
        if #available(iOS 26, macOS 26, tvOS 26, *) {
            content.glassEffect(interactive ? .regular.interactive() : .regular, in: shape)
        } else {
            content.background(.regularMaterial, in: shape)
        }
    }
}

extension View {
    /// Liquid Glass (26+) or the existing `.regularMaterial` (pre-26) behind a floating
    /// surface. Pass the surface's shape explicitly — glass defaults to a Capsule otherwise.
    func glassBackground<S: Shape>(_ shape: S, interactive: Bool = false) -> some View {
        modifier(GlassBackground(shape: shape, interactive: interactive))
    }
}

// MARK: - Glass primary button

/// The single prominent action on a floating / overlay or sheet surface: the Liquid-Glass
/// prominent button style on 26+, falling back to `.borderedProminent` (the app's current
/// primary style) below. Apply directly to a `Button`; role / keyboardShortcut / disabled
/// chain after it as usual. tvOS stays `.borderedProminent` always — glass chrome fights the
/// focus engine, and keeping it preserves today's tvOS look exactly.
private struct GlassProminentButton: ViewModifier {
    func body(content: Content) -> some View {
        #if os(tvOS)
        content.buttonStyle(.borderedProminent)
        #else
        if #available(iOS 26, macOS 26, *) {
            content.buttonStyle(.glassProminent)
        } else {
            content.buttonStyle(.borderedProminent)
        }
        #endif
    }
}

extension View {
    /// Liquid-Glass prominent style (26+, non-tvOS) or `.borderedProminent`. Drop-in for the
    /// `.buttonStyle(.borderedProminent)` on a surface's primary action.
    func glassProminentButtonStyle() -> some View {
        modifier(GlassProminentButton())
    }
}

// MARK: - Console glass (gamepad host tiles + settings rows)

/// Liquid Glass tuned for the gamepad UI's "console" surfaces — the host-carousel tiles and
/// the settings rows. Unlike `glassBackground` (floating-overlay only, per HIG), this deliberately
/// clads content tiles / dense rows: a chosen part of the 10-foot console look. `tint` washes the
/// glass toward a color (the palette accent on the focused / primary surface); `interactive` makes
/// it flex on press.
///
/// Every tier is WASHED with the palette's `ink.glass` — the same surface colour the console
/// fills its panels with — so switching the background palette recolours the surfaces, not just
/// the text on them. The wash alphas are tune-on-device values with one fixed direction: the
/// pale palettes' white frost needs MORE body than the dark glass (the console's 0.66-vs-0.62
/// pair), because a thin white wash over a colourful field reads as haze, not as a surface.
private struct ConsoleGlass<S: Shape>: ViewModifier {
    let shape: S
    var tint: Color?
    var interactive = false
    /// The console surface follows the background palette: a PALE field needs the material to
    /// frost light and the glass to read as white, or the dark ink on top of it disappears.
    /// Defaults to the dark ink, so every non-gamepad caller is unchanged.
    @Environment(\.gamepadInk) private var ink

    private var scheme: ColorScheme { ink.isLight ? .light : .dark }
    /// The palette wash over the material tiers (the material itself supplies the blur body).
    private var materialWash: Color { ink.glass(ink.isLight ? 0.55 : 0.40) }

    func body(content: Content) -> some View {
        // The scheme goes on the WHOLE modified view, not just the fill inside `.background {}`.
        // Scoped to the fill it frosts the material correctly and stops there, so a system colour
        // in the row's own content (a `.secondary` label, a `.bordered` button) still resolved
        // against the device appearance — which is how the pale palettes came out light-on-light
        // on tvOS, whose appearance is always Dark. The 26 branch had it right all along; the
        // tvOS and pre-26 branches were the odd ones out.
        #if os(tvOS)
        // ALWAYS the material fallback on tvOS: the gamepad settings list is 15+ of these
        // surfaces, and live Liquid Glass per row made the whole screen visibly laggy on the
        // Apple TV's GPU (same class of call GlassProminentButton already makes — glass fights
        // the 10-foot platform). The wash and tint ride overlays — two flat fills, no GPU cost.
        content
            .background {
                shape.fill(.ultraThinMaterial)
                    .environment(\.colorScheme, scheme)
                    .overlay { shape.fill(materialWash) }
                    .overlay {
                        if let tint { shape.fill(tint) }
                    }
            }
            .environment(\.colorScheme, scheme)
        #else
        if #available(iOS 26, macOS 26, *) {
            content.glassEffect(glass, in: shape).environment(\.colorScheme, scheme)
        } else {
            content
                .background {
                    shape.fill(.ultraThinMaterial)
                        .environment(\.colorScheme, scheme)
                        .overlay { shape.fill(materialWash) }
                        .overlay {
                            if let tint { shape.fill(tint) }
                        }
                }
                .environment(\.colorScheme, scheme)
        }
        #endif
    }

    #if !os(tvOS)
    @available(iOS 26, macOS 26, *)
    private var glass: Glass {
        // Liquid Glass has ONE tint channel, so the palette wash and the caller's tint share
        // it: mixed 60 % toward the caller's (the focused row must still read accented on
        // every palette) over the palette base. If device QA finds the mixed focus wash too
        // weak, the escape hatch is `tint ?? wash` — today's focused look, bit for bit.
        let wash = ink.glass(ink.isLight ? 0.60 : 0.45)
        var g: Glass = .regular.tint(
            tint.map { wash.mix(with: $0, by: 0.6) } ?? wash)
        if interactive { g = g.interactive() }
        return g
    }
    #endif
}

extension View {
    /// Liquid Glass for a console surface (a host tile / settings row), or `.ultraThinMaterial`
    /// pre-26 — both washed with the palette's own glass colour, both frosting to the palette's
    /// scheme. Pass the surface's shape explicitly — glass defaults to a Capsule.
    func consoleGlass<S: Shape>(_ shape: S, tint: Color? = nil, interactive: Bool = false) -> some View {
        modifier(ConsoleGlass(shape: shape, tint: tint, interactive: interactive))
    }
}

// MARK: - Console floating glass (the gamepad screens' close buttons)

/// `glassBackground` for a floating control INSIDE the gamepad UI (the close ✕): same shape
/// contract, but washed with the palette's ink and frosted to the palette's scheme — plain
/// `glassBackground` follows the SYSTEM appearance, which leaves the frost dark under dark ink
/// when a pale palette is up. The non-gamepad floating surfaces (the HUD, the trust card, the
/// touch connect modal) keep plain `glassBackground`: they sit over video or the touch UI,
/// where the palette means nothing.
private struct ConsoleGlassBackground<S: Shape>: ViewModifier {
    let shape: S
    var interactive = false
    @Environment(\.gamepadInk) private var ink

    private var scheme: ColorScheme { ink.isLight ? .light : .dark }

    func body(content: Content) -> some View {
        if #available(iOS 26, macOS 26, tvOS 26, *) {
            content
                .glassEffect(
                    (interactive ? Glass.regular.interactive() : .regular)
                        .tint(ink.glass(ink.isLight ? 0.60 : 0.45)),
                    in: shape)
                .environment(\.colorScheme, scheme)
        } else {
            // Same hoist as ConsoleGlass: the content needs the scheme too, not only the frost.
            content
                .background {
                    shape.fill(.regularMaterial)
                        .environment(\.colorScheme, scheme)
                        .overlay { shape.fill(ink.glass(ink.isLight ? 0.55 : 0.40)) }
                }
                .environment(\.colorScheme, scheme)
        }
    }
}

extension View {
    /// Palette-washed floating glass for the gamepad screens' own controls. Same fallback story
    /// as `glassBackground` (`.regularMaterial` pre-26), plus the ink wash and scheme flip.
    func consoleGlassBackground<S: Shape>(_ shape: S, interactive: Bool = false) -> some View {
        modifier(ConsoleGlassBackground(shape: shape, interactive: interactive))
    }
}
