// The trust card's controls legend: the pad's own button glyphs beside what each does.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
import GameController

/// The glyph a button wears in a legend: the ACTIVE controller's own (Xbox "A", DualSense ✕, …)
/// via `sfSymbolsName` while one is attached, else the glyph of the last pad this device ever saw
/// (`GamepadManager.lastKnownKind` → `GamepadGlyphs`), else the caller's generic fallback.
///
/// The middle rung keeps a DualSense user's ✕/◯ legends when the pad dozes off and `active`
/// goes nil. On tvOS the middle rung is the Siri Remote instead: a role the remote has no
/// button for yields an empty glyph, and the hint bar drops it.
@MainActor
func buttonGlyph(
    _ button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>, fallback: String
) -> String {
    let manager = GamepadManager.shared
    if let live = manager.active?.controller.extendedGamepad?[keyPath: button].sfSymbolsName {
        return live
    }
    guard let role = GamepadButtonRole(keyPath: button) else { return fallback }
    #if os(tvOS)
    return GamepadGlyphs.remoteSymbol(role) ?? ""
    #else
    return GamepadGlyphs.symbol(role, for: manager.lastKnownKind)
    #endif
}

/// One glyph + label cell in a hint bar.
struct GamepadHint: Identifiable {
    let glyph: String
    let text: String
    /// What tapping/clicking this cell does — the same thing its button does.
    var action: (() -> Void)? = nil
    var id: String { glyph + text }
}

/// A row of glyph + label cells on a glass pill.
struct GamepadHintBar: View {
    let hints: [GamepadHint]

    #if os(tvOS)
    private let glyphFont: CGFloat = 27
    private let textFont: CGFloat = 20
    private let pad: CGFloat = 18
    #else
    private let glyphFont: CGFloat = 19
    private let textFont: CGFloat = 14
    private let pad: CGFloat = 13
    #endif

    var body: some View {
        HStack(spacing: 18) {
            // An empty glyph names a button the input in hand doesn't have (see `buttonGlyph`).
            ForEach(hints.filter { !$0.glyph.isEmpty }) { hint in
                cell(hint)
            }
        }
        .font(.geist(textFont, .semibold, relativeTo: .subheadline))
        .foregroundStyle(.white.opacity(0.85))
        .padding(pad)
        .glassBackground(Capsule())
    }

    /// A button where it has somewhere to go; tvOS cells are labels, the focus engine owns input.
    @ViewBuilder private func cell(_ hint: GamepadHint) -> some View {
        #if os(tvOS)
        label(hint)
        #else
        if let action = hint.action {
            Button(action: action) { label(hint) }
                .buttonStyle(.plain)
                .accessibilityLabel(hint.text)
        } else {
            label(hint)
        }
        #endif
    }

    private func label(_ hint: GamepadHint) -> some View {
        HStack(spacing: 7) {
            Image(systemName: hint.glyph)
                .font(.system(size: glyphFont))
                .foregroundStyle(.white)
            Text(hint.text)
        }
        .fixedSize()
        .contentShape(Rectangle())
    }
}
#endif
