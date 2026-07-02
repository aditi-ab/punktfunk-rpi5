// Chrome shared by the gamepad-driven screens (GamepadHomeView, GamepadSettingsView,
// GamepadAddHostView, LibraryCoverflowView): the full-bleed console backdrop, the
// controller-glyph hint bar, and the connected-controller status chip. One look across every
// screen is what makes the gamepad UI read as a coherent mode rather than a set of themed pages.
// iOS/iPadOS and macOS (the couch Mac-mini case); tvOS keeps its native focus engine instead.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)
import GameController

/// The active controller's real glyph for a button (Xbox "A", DualSense ✕, …) via
/// `sfSymbolsName`; a generic fallback before a controller profile resolves.
/// @MainActor: GamepadManager is main-actor-bound (inside a View body this was implicit).
@MainActor
func buttonGlyph(
    _ button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>, fallback: String
) -> String {
    GamepadManager.shared.active?.controller.extendedGamepad?[keyPath: button].sfSymbolsName
        ?? fallback
}

/// Top padding for a gamepad screen's pinned title. macOS gets extra clearance — the launcher
/// title sits right under the window titlebar and the settings/add-host sheets have no titlebar
/// at all, so the iOS value hugs the top edge there.
func gamepadTitleTopPadding(compact: Bool) -> CGFloat {
    #if os(macOS)
    26
    #else
    compact ? 4 : 10
    #endif
}

/// One glyph + label cell in a hint bar.
struct GamepadHint: Identifiable {
    let glyph: String
    let text: String
    var id: String { glyph + text }
}

/// The pinned controls legend every gamepad screen shows bottom-leading (via `.safeAreaInset`).
/// Same font/spacing everywhere so the legend reads as system chrome, not per-screen decoration.
struct GamepadHintBar: View {
    let hints: [GamepadHint]

    var body: some View {
        HStack(spacing: 18) {
            ForEach(hints) { hint in
                HStack(spacing: 7) {
                    Image(systemName: hint.glyph)
                        .font(.system(size: 19))
                        .foregroundStyle(.white)
                    Text(hint.text)
                }
                .fixedSize() // keep glyph + label together; never truncate a hint mid-word
            }
        }
        .font(.geist(14, .semibold, relativeTo: .subheadline))
        .foregroundStyle(.white.opacity(0.85))
    }
}

/// The console backdrop: a living "aurora" field in the brand's violet family — soft color blobs
/// drifting on slow Lissajous paths over black, in the direction of Apple Music's animated player
/// background but calmer (long 30–90 s periods, muted opacities, a legibility scrim on top, so it
/// reads as ambience behind the cards, never as content). Deliberately pure SwiftUI rather than a
/// .metal shader: these sources are built both by SwiftPM (`swift run`/tests) and by the Xcode
/// project's synchronized folders, and a compiled metallib is only reliably bundled in one of the
/// two — radial gradients driven by a TimelineView give the same look with none of that risk.
///
/// Applied via `.background { }` — NOT as a ZStack sibling — so the `.ignoresSafeArea()` here
/// can't inflate the caller's layout past the safe area (see the layout discipline note in
/// GamepadHomeView's header). Honors Reduce Motion by freezing the field at a fixed phase.
struct GamepadScreenBackground: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    /// One drifting color blob: a base position + drift ellipse (unit coordinates), angular
    /// speeds (rad/s — periods of 30–90 s), and a radius that slowly breathes.
    private struct Blob {
        let color: Color
        let center: CGPoint
        let drift: CGSize
        let speed: (x: Double, y: Double)
        let phase: (x: Double, y: Double)
        /// Radius as a fraction of the view's larger dimension (+ breathing amplitude/speed).
        let radius: CGFloat
        let breathe: (amount: CGFloat, speed: Double)
        let opacity: Double
    }

    /// The brand violet, a deeper indigo, a warmer plum, and a cool blue — related hues so the
    /// field shifts within one temperature instead of strobing through the rainbow.
    private static let blobs: [Blob] = [
        Blob(color: Color(red: 0.53, green: 0.47, blue: 0.96), // brand violet
             center: CGPoint(x: 0.30, y: 0.24), drift: CGSize(width: 0.16, height: 0.10),
             speed: (0.111, 0.083), phase: (0.0, 1.9),
             radius: 0.52, breathe: (0.07, 0.061), opacity: 0.52),
        Blob(color: Color(red: 0.24, green: 0.20, blue: 0.72), // deep indigo
             center: CGPoint(x: 0.78, y: 0.66), drift: CGSize(width: 0.13, height: 0.14),
             speed: (0.071, 0.096), phase: (2.4, 0.7),
             radius: 0.58, breathe: (0.08, 0.049), opacity: 0.55),
        Blob(color: Color(red: 0.62, green: 0.30, blue: 0.80), // plum
             center: CGPoint(x: 0.16, y: 0.82), drift: CGSize(width: 0.12, height: 0.09),
             speed: (0.089, 0.067), phase: (4.1, 3.2),
             radius: 0.44, breathe: (0.09, 0.078), opacity: 0.42),
        Blob(color: Color(red: 0.22, green: 0.38, blue: 0.86), // cool blue
             center: CGPoint(x: 0.70, y: 0.12), drift: CGSize(width: 0.10, height: 0.08),
             speed: (0.059, 0.104), phase: (1.2, 5.0),
             radius: 0.40, breathe: (0.06, 0.055), opacity: 0.38),
    ]

    var body: some View {
        Group {
            if reduceMotion {
                field(at: 0)
            } else {
                // 30 Hz is plenty for centimeters-per-minute drift, and halves the redraw cost
                // of a battery-fed couch device vs. the default display rate.
                TimelineView(.animation(minimumInterval: 1.0 / 30.0)) { context in
                    field(at: context.date.timeIntervalSinceReferenceDate)
                }
            }
        }
        .ignoresSafeArea()
    }

    private func field(at t: TimeInterval) -> some View {
        GeometryReader { geo in
            let side = max(geo.size.width, geo.size.height)
            ZStack {
                Color.black
                ZStack {
                    ForEach(Self.blobs.indices, id: \.self) { i in
                        blobView(Self.blobs[i], at: t, in: geo.size, side: side)
                    }
                }
                // ±10° over ~5 min — the whole field very slowly warms and cools.
                .hueRotation(.degrees(sin(t * 0.021) * 10))
                // Composite the additive blobs offscreen once instead of per-layer.
                .drawingGroup()
                // Legibility scrim: the title (top) and detail/hints (bottom) always sit on
                // near-black, whatever the blobs are doing behind them.
                LinearGradient(
                    stops: [
                        .init(color: .black.opacity(0.55), location: 0),
                        .init(color: .black.opacity(0.15), location: 0.35),
                        .init(color: .black.opacity(0.20), location: 0.65),
                        .init(color: .black.opacity(0.60), location: 1),
                    ],
                    startPoint: .top, endPoint: .bottom)
            }
        }
    }

    private func blobView(_ blob: Blob, at t: TimeInterval, in size: CGSize, side: CGFloat) -> some View {
        let x = blob.center.x + blob.drift.width * CGFloat(sin(t * blob.speed.x + blob.phase.x))
        let y = blob.center.y + blob.drift.height * CGFloat(cos(t * blob.speed.y + blob.phase.y))
        let r = side * blob.radius
            * (1 + blob.breathe.amount * CGFloat(sin(t * blob.breathe.speed + blob.phase.x)))
        return Circle()
            .fill(RadialGradient(
                colors: [blob.color, blob.color.opacity(0)],
                center: .center, startRadius: 0, endRadius: r / 2))
            .frame(width: r, height: r)
            .position(x: x * size.width, y: y * size.height)
            .opacity(blob.opacity)
            .blendMode(.plusLighter)
    }
}

/// A darkening scrim behind a pinned tray (a screen title, the hints/detail bar, the keyboard
/// tray): scrollable rows pass beneath those insets, so without this the tray text and the row
/// underneath render interleaved. Fades toward the content so it reads as depth, not a bar.
struct GamepadTrayScrim: View {
    let edge: VerticalEdge

    var body: some View {
        LinearGradient(
            stops: [
                .init(color: .black.opacity(0.92), location: 0),
                .init(color: .black.opacity(0.85), location: 0.55),
                .init(color: .black.opacity(0), location: 1),
            ],
            startPoint: edge == .top ? .top : .bottom,
            endPoint: edge == .top ? .bottom : .top)
            // Grow past the tray so the fade-to-clear happens OUTSIDE its bounds — the tray's own
            // text always sits on the near-opaque part, rows dim before they reach it.
            .padding(edge == .top ? .bottom : .top, -32)
            .ignoresSafeArea()
    }
}

/// "Which pad is driving this UI" — the active controller's name and battery, worn as a quiet
/// chip in the launcher's top bar. Callers observe GamepadManager already, so this re-renders
/// when the pad or its battery state changes.
struct ControllerStatusChip: View {
    let controller: GamepadManager.DiscoveredController

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: controller.hasTouchpadAndMotion
                ? "playstation.logo" : "gamecontroller.fill")
                .font(.system(size: 12))
            Text(controller.name)
                .lineLimit(1)
            if let level = controller.batteryLevel {
                Image(systemName: batterySymbol(level))
                    .font(.system(size: 12))
                    .foregroundStyle(level <= 0.2 && !controller.isCharging
                        ? AnyShapeStyle(.red) : AnyShapeStyle(.white.opacity(0.7)))
            }
        }
        .font(.geist(12, .medium, relativeTo: .caption))
        .foregroundStyle(.white.opacity(0.7))
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(Capsule().fill(.white.opacity(0.08)))
        .overlay(Capsule().strokeBorder(.white.opacity(0.12), lineWidth: 1))
    }

    private func batterySymbol(_ level: Float) -> String {
        if controller.isCharging { return "battery.100.bolt" }
        switch level {
        case ..<0.125: return "battery.0"
        case ..<0.375: return "battery.25"
        case ..<0.625: return "battery.50"
        case ..<0.875: return "battery.75"
        default: return "battery.100"
        }
    }
}
#endif
