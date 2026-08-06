// Chrome shared by the gamepad-driven screens (GamepadHomeView, GamepadSettingsView,
// GamepadAddHostView, LibraryCoverflowView): the full-bleed console backdrop, the
// controller-glyph hint bar, and the connected-controller status chip. One look across every
// screen is what makes the gamepad UI read as a coherent mode rather than a set of themed pages.
// iOS/iPadOS, macOS (the couch Mac-mini case), and tvOS — where the same screens are driven by
// the native focus engine instead of the controller poll (see GamepadCarousel/GamepadMenuList).

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
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

/// Point size for a gamepad screen's pinned title: TV-large on tvOS (read from the couch), the
/// in-hand compact-aware sizes elsewhere.
func gamepadTitleSize(compact: Bool) -> CGFloat {
    #if os(tvOS)
    44
    #else
    compact ? 20 : 30
    #endif
}

/// Metrics shared by the gamepad form screens' glass rows (GamepadSettingsView,
/// GamepadAddHostView) — one set of numbers so the two screens read as the same surface,
/// sized for the couch on tvOS and for the hand elsewhere.
enum GamepadFormMetrics {
    #if os(tvOS)
    static let headerFont: CGFloat = 17
    static let labelFont: CGFloat = 23
    static let valueFont: CGFloat = 21
    static let iconFont: CGFloat = 24
    static let iconWidth: CGFloat = 40
    static let chevronFont: CGFloat = 16
    static let rowHPad: CGFloat = 24
    static let rowVPad: CGFloat = 19
    static let rowCorner: CGFloat = 18
    static let rowMaxWidth: CGFloat = 920
    static let detailFont: CGFloat = 19
    static let closeFont: CGFloat = 20
    static let closeSide: CGFloat = 48
    #else
    static let headerFont: CGFloat = 12
    static let labelFont: CGFloat = 16
    static let valueFont: CGFloat = 15
    static let iconFont: CGFloat = 17
    static let iconWidth: CGFloat = 28
    static let chevronFont: CGFloat = 12
    static let rowHPad: CGFloat = 16
    static let rowVPad: CGFloat = 13
    static let rowCorner: CGFloat = 14
    static let rowMaxWidth: CGFloat = 620
    static let detailFont: CGFloat = 13
    static let closeFont: CGFloat = 14
    static let closeSide: CGFloat = 34
    #endif
}

/// One glyph + label cell in a hint bar.
struct GamepadHint: Identifiable {
    let glyph: String
    let text: String
    var id: String { glyph + text }
}

/// The pinned controls legend every gamepad screen shows bottom-leading (via `.safeAreaInset`).
/// Same font/spacing everywhere so the legend reads as system chrome, not per-screen decoration —
/// worn as a self-contained Liquid Glass pill (like the top-bar controller chip) so it floats over
/// the backdrop instead of dissolving into it.
struct GamepadHintBar: View {
    @Environment(\.gamepadInk) private var ink
    let hints: [GamepadHint]

    // 10-foot legend on tvOS, in-hand sizes elsewhere.
    #if os(tvOS)
    private static let glyphFont: CGFloat = 27
    private static let textFont: CGFloat = 20
    private static let pad: CGFloat = 18
    #else
    private static let glyphFont: CGFloat = 19
    private static let textFont: CGFloat = 14
    private static let pad: CGFloat = 13
    #endif

    var body: some View {
        HStack(spacing: 18) {
            ForEach(hints) { hint in
                HStack(spacing: 7) {
                    Image(systemName: hint.glyph)
                        .font(.system(size: Self.glyphFont))
                        .foregroundStyle(ink.fg)
                    Text(hint.text)
                }
                .fixedSize() // keep glyph + label together; never truncate a hint mid-word
            }
        }
        .font(.geist(Self.textFont, .semibold, relativeTo: .subheadline))
        .foregroundStyle(ink.fg(0.85))
        .padding(Self.pad)
        .consoleGlass(Capsule())
        .overlay(Capsule().strokeBorder(ink.fg(0.12), lineWidth: 1))
    }
}

/// The console backdrop: a living aurora drifting slowly over black so it reads as ambience behind
/// the cards, never as content. On iOS 18 / macOS 15+ it's an animated `MeshGradient` — a continuous
/// silk of colour whose control points wander on slow, out-of-phase sinusoids — finished with an
/// elliptical vignette (pools light in the centre, sinks the corners) and a top/bottom legibility
/// scrim. Older OSes fall back to the original drifting radial-blob field, unchanged, so nothing
/// regresses.
///
/// `calm` is what the FORM screens (settings, add-host) wear: the same living field with its pools
/// dimmed onto its own corner colour, so those screens keep real colour under their Liquid Glass
/// rows without the launcher's contrast. They used to sit on a still gradient; nothing in the
/// gamepad UI is backed by a static image now. Motion is identical in both modes on purpose — only
/// the contrast differs, so a screen change can't make the field jump.
///
/// `GamepadPalette` recolours the whole thing (the shared `ui_palette` setting) by transforming the
/// COLOURS, not by stacking a filter — see GamepadPalette.swift for why.
///
/// Deliberately pure SwiftUI, no `.metal`: these sources build under both SwiftPM (`swift run`/
/// tests) and the Xcode project's synchronized folders, and a compiled metallib is only reliably
/// bundled in one of the two. MeshGradient + TimelineView give the silky look with none of that
/// risk. Applied via `.background { }` — NOT a ZStack sibling — so the `.ignoresSafeArea()` here
/// can't inflate the caller's layout past the safe area (see the layout note in GamepadHomeView's
/// header). Honors Reduce Motion by freezing the field at a fixed phase.
struct GamepadScreenBackground: View {
    @Environment(\.gamepadInk) private var ink
    /// Quiet the field for a form screen (see the type comment).
    var calm = false

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"

    var body: some View {
        let palette = GamepadPalette.named(paletteID)
        Group {
            if reduceMotion {
                composite(at: 0, palette: palette)
            } else {
                // 30 Hz is plenty for a field that drifts centimetres per minute, and halves the
                // redraw cost of a battery-fed couch device vs. the display's native rate.
                TimelineView(.animation(minimumInterval: 1.0 / 30.0)) { context in
                    composite(at: context.date.timeIntervalSinceReferenceDate, palette: palette)
                }
            }
        }
        .ignoresSafeArea()
    }

    /// The colour field under a very slow warm/cool hue sway, the calm flattening, an elliptical
    /// vignette, and the title/hints legibility scrim — in that order, matching the console
    /// shader's `composite` so the two platforms' backdrops stay the same picture.
    private func composite(at t: TimeInterval, palette: GamepadPalette) -> some View {
        // Where the scrims tend, and how hard. Mixing a PALE field toward white at the dark
        // field's strength bleaches the chroma straight out of the gradient, so a pale palette
        // gets under half — the same `u_scrim.a` the console shader carries.
        let scrim: Color = palette.light ? ink.fg : .black
        let strength = palette.light ? 0.45 : 1.0
        return ZStack {
            Self.color(palette.ground)
            colorField(at: t, palette: palette)
                // ±8° over ~5 min — the whole field very slowly warms and cools.
                .hueRotation(.degrees(sin(t * 0.021) * 8))
                // Calm = col·0.6 + ground·0.4: over the ground, `.opacity` IS the multiply…
                .opacity(calm ? 0.6 : 1)
            if calm {
                // …and a plusLighter wash of the palette's own ground IS the add. Chosen so the
                // ground lands exactly where it was and the bright pools come down to meet it.
                Self.color(palette.ground)
                    .opacity(0.4)
                    .blendMode(.plusLighter)
            }
            // Cinematic vignette: the edges settle toward the scrim so the cards sit in the
            // pooled light. Soft (extends past the frame) so the corners deepen rather than
            // crush. Halved under calm: a launcher's cards sit in the pooled centre, but a form
            // screen's rows run out toward the edges, where crushing them just eats the list.
            EllipticalGradient(
                colors: [.clear, scrim.opacity((calm ? 0.21 : 0.42) * strength)],
                center: .center, startRadiusFraction: 0.25, endRadiusFraction: 1.15)
            // Legibility grounding for the pinned title (top) and hint pill (bottom). This one
            // works on the field itself (it's the backdrop's bottom layer — nothing behind it to
            // blur), so it stays a gradient, just a light one.
            LinearGradient(
                stops: [
                    .init(color: scrim.opacity(0.38 * strength), location: 0),
                    .init(color: scrim.opacity(0.06 * strength), location: 0.32),
                    .init(color: scrim.opacity(0.08 * strength), location: 0.68),
                    .init(color: scrim.opacity(0.40 * strength), location: 1),
                ],
                startPoint: .top, endPoint: .bottom)
        }
    }

    @ViewBuilder private func colorField(at t: TimeInterval, palette: GamepadPalette) -> some View {
        if #available(iOS 18, macOS 15, tvOS 18, *) {
            MeshGradient(
                width: 4, height: 4,
                points: Self.meshPoints(at: t),
                colors: palette.meshColors.map(Self.color),
                smoothsColors: true)
        } else {
            LegacyBlobField(t: t, palette: palette)
        }
    }

    // MARK: - MeshGradient aurora (iOS 18 / macOS 15+)

    static func color(_ c: SIMD3<Double>) -> Color {
        Color(red: c.x, green: c.y, blue: c.z)
    }

    /// The 4×4 control points at time `t`: every boundary point is PINNED to the frame (so the mesh
    /// always fills edge-to-edge — a drifting edge point would shrink the mesh and expose the black
    /// behind it), while only the four interior points wander on slow, out-of-phase sinusoids
    /// (periods ~90–130 s) so the bright colour pools breathe without ever looking like they loop.
    private static func meshPoints(at t: TimeInterval) -> [SIMD2<Float>] {
        func wob(_ bx: Float, _ by: Float, _ a: Float,
                 _ sx: Double, _ sy: Double, _ ph: Double) -> SIMD2<Float> {
            SIMD2(bx + a * Float(sin(t * sx + ph)), by + a * Float(cos(t * sy + ph * 1.3)))
        }
        return [
            SIMD2(0, 0), SIMD2(0.333, 0), SIMD2(0.667, 0), SIMD2(1, 0),
            SIMD2(0, 0.333),
                wob(0.333, 0.333, 0.11, 0.049, 0.063, 0.4),
                wob(0.667, 0.333, 0.10, 0.055, 0.052, 2.1),
            SIMD2(1, 0.333),
            SIMD2(0, 0.667),
                wob(0.333, 0.667, 0.10, 0.058, 0.049, 3.6),
                wob(0.667, 0.667, 0.12, 0.047, 0.061, 5.0),
            SIMD2(1, 0.667),
            SIMD2(0, 1), SIMD2(0.333, 1), SIMD2(0.667, 1), SIMD2(1, 1),
        ]
    }
}

/// Pre-18/15 fallback for `GamepadScreenBackground`: the original drifting radial-blob field — four
/// soft colour blobs on slow Lissajous paths, additively blended. Geometry and motion are verbatim
/// so older OSes see exactly the aurora they shipped with (the mesh path is the upgrade for OS
/// 18/15+); only the blob COLOURS now pass through the palette, so an older device honours the
/// setting too instead of being stuck on violet.
private struct LegacyBlobField: View {
    let t: TimeInterval
    let palette: GamepadPalette

    /// One drifting color blob: a base position + drift ellipse (unit coordinates), angular speeds
    /// (rad/s — periods of 30–90 s), and a radius that slowly breathes. The COLOUR comes from the
    /// palette's ramp at draw time (see `blobColors`), so an older OS honours the setting too.
    private struct Blob {
        let center: CGPoint
        let drift: CGSize
        let speed: (x: Double, y: Double)
        let phase: (x: Double, y: Double)
        let radius: CGFloat
        let breathe: (amount: CGFloat, speed: Double)
        let opacity: Double
    }

    private static let blobs: [Blob] = [
        Blob(center: CGPoint(x: 0.30, y: 0.24), drift: CGSize(width: 0.16, height: 0.10),
             speed: (0.111, 0.083), phase: (0.0, 1.9),
             radius: 0.52, breathe: (0.07, 0.061), opacity: 0.52),
        Blob(center: CGPoint(x: 0.78, y: 0.66), drift: CGSize(width: 0.13, height: 0.14),
             speed: (0.071, 0.096), phase: (2.4, 0.7),
             radius: 0.58, breathe: (0.08, 0.049), opacity: 0.55),
        Blob(center: CGPoint(x: 0.16, y: 0.82), drift: CGSize(width: 0.12, height: 0.09),
             speed: (0.089, 0.067), phase: (4.1, 3.2),
             radius: 0.44, breathe: (0.09, 0.078), opacity: 0.42),
        Blob(center: CGPoint(x: 0.70, y: 0.12), drift: CGSize(width: 0.10, height: 0.08),
             speed: (0.059, 0.104), phase: (1.2, 5.0),
             radius: 0.40, breathe: (0.06, 0.055), opacity: 0.38),
    ]

    var body: some View {
        GeometryReader { geo in
            let side = max(geo.size.width, geo.size.height)
            ZStack {
                ForEach(Self.blobs.indices, id: \.self) { i in
                    blobView(Self.blobs[i], tone: palette.blobColors[i], in: geo.size, side: side)
                }
            }
            .drawingGroup()
        }
    }

    private func blobView(
        _ blob: Blob, tone: SIMD3<Double>, in size: CGSize, side: CGFloat
    ) -> some View {
        let x = blob.center.x + blob.drift.width * CGFloat(sin(t * blob.speed.x + blob.phase.x))
        let y = blob.center.y + blob.drift.height * CGFloat(cos(t * blob.speed.y + blob.phase.y))
        let r = side * blob.radius
            * (1 + blob.breathe.amount * CGFloat(sin(t * blob.breathe.speed + blob.phase.x)))
        let color = GamepadScreenBackground.color(tone)
        return Circle()
            .fill(RadialGradient(
                colors: [color, color.opacity(0)],
                center: .center, startRadius: 0, endRadius: r / 2))
            .frame(width: r, height: r)
            .position(x: x * size.width, y: y * size.height)
            .opacity(blob.opacity)
            // Additive only works over a DARK ground; over a pale one every blob saturates to
            // white and the field turns grey. Pale palettes tint instead.
            .blendMode(palette.light ? .normal : .plusLighter)
    }
}

/// A blur gradient behind a pinned tray (a screen title, the hints/detail bar, the keyboard tray):
/// scrollable rows pass beneath those insets, so without this the tray text and the row underneath
/// render interleaved. Pure blur — a dark material faded out by a gradient mask, no dark tint — so
/// the tray's text sits on a softly blurred backdrop that dissolves into the rows.
struct GamepadTrayScrim: View {
    let edge: VerticalEdge
    @Environment(\.gamepadInk) private var ink

    var body: some View {
        let fromEdge: UnitPoint = edge == .top ? .top : .bottom
        let toContent: UnitPoint = edge == .top ? .bottom : .top
        Rectangle()
            .fill(.ultraThinMaterial)
            // Force the frost to match the PALETTE, not the system appearance: the tray exists
            // to keep the pinned title legible, so it has to frost dark under white ink and
            // light under dark ink.
            .environment(\.colorScheme, ink.isLight ? .light : .dark)
            // Fade the whole blur out toward the content so it dissolves rather than ending on a line.
            .mask {
                LinearGradient(
                    stops: [
                        .init(color: .black, location: 0),
                        .init(color: .black.opacity(0.9), location: 0.5),
                        .init(color: .clear, location: 1),
                    ],
                    startPoint: fromEdge, endPoint: toContent)
            }
            // Grow past the tray so the fade-to-clear happens OUTSIDE its bounds — the tray's own
            // text always sits on the strong part, rows blur out before they reach it.
            .padding(edge == .top ? .bottom : .top, -32)
            .ignoresSafeArea()
    }
}

/// The backdrop for the gamepad UI's form screens (settings, add-host). It used to be a STILL pair
/// of glows over a deep indigo base — deliberately not near-black, because Liquid Glass refracts
/// whatever sits behind it and over black the rows turn invisible. It is now the launcher's own
/// living field at `calm`, which keeps that luminance under the glass, keeps the palette setting
/// honoured on every screen rather than only the launcher, and leaves nothing in the gamepad UI
/// backed by a static image. Kept as its own type because that is what the form screens ask for by
/// name; the console (`pf-console-ui`) made the same substitution behind its `Bg::Form`.
struct GamepadFormBackground: View {
    var body: some View {
        GamepadScreenBackground(calm: true)
    }
}

#if os(tvOS)
/// Bare chrome for the focusable console Buttons (carousel cards, menu-list rows) on tvOS: the
/// tile/row draws its own look and the screen's own focus treatment marks the focused element
/// (the carousel's `.scrollTransition` center pop, the list row's `focused` styling), so the
/// system's lift/halo would double up on it. Press feedback is a small dip, matching the
/// interactive-glass feel elsewhere.
struct ConsoleBareButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.97 : 1)
            .animation(.smooth(duration: 0.15), value: configuration.isPressed)
    }
}
#endif

/// "Which pad is driving this UI" — the active controller's name and battery, worn as a quiet
/// chip in the launcher's top bar. Callers observe GamepadManager already, so this re-renders
/// when the pad or its battery state changes.
struct ControllerStatusChip: View {
    @Environment(\.gamepadInk) private var ink
    let controller: GamepadManager.DiscoveredController

    // Legible from the couch on tvOS, quiet in hand elsewhere.
    #if os(tvOS)
    private static let font: CGFloat = 17
    private static let hPad: CGFloat = 16
    private static let vPad: CGFloat = 10
    #else
    private static let font: CGFloat = 12
    private static let hPad: CGFloat = 12
    private static let vPad: CGFloat = 7
    #endif

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: controller.hasTouchpadAndMotion
                ? "playstation.logo" : "gamecontroller.fill")
                .font(.system(size: Self.font))
            Text(controller.name)
                .lineLimit(1)
            if let level = controller.batteryLevel {
                Image(systemName: batterySymbol(level))
                    .font(.system(size: Self.font))
                    .foregroundStyle(level <= 0.2 && !controller.isCharging
                        ? AnyShapeStyle(.red) : AnyShapeStyle(ink.fg(0.7)))
            }
        }
        .font(.geist(Self.font, .medium, relativeTo: .caption))
        .foregroundStyle(ink.fg(0.7))
        .padding(.horizontal, Self.hPad)
        .padding(.vertical, Self.vPad)
        .background(Capsule().fill(ink.fg(0.08)))
        .overlay(Capsule().strokeBorder(ink.fg(0.12), lineWidth: 1))
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
