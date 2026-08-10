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

/// The glyph a button wears in a legend: the ACTIVE controller's own (Xbox "A", DualSense ✕, …)
/// via `sfSymbolsName` while one is attached, else the glyph of the last pad this device ever saw
/// (`GamepadManager.lastKnownKind` → `GamepadGlyphs`), else the caller's generic fallback.
///
/// The middle rung is the whole point. `active` is nil whenever the pad sleeps, disconnects or
/// runs flat — and permanently under `gamepadUIMode == "always"`, which puts the console UI up
/// with no pad by design — and the fallbacks are letter glyphs, so a DualSense user's ✕/◯ legends
/// used to turn into A/B the moment the controller dozed off. The remembered kind keeps the
/// legends speaking the pad the user actually owns. The `fallback` still covers the genuinely
/// unknown case: a fresh install that has never seen a controller, and any button outside the six
/// `GamepadButtonRole` names.
///
/// @MainActor: GamepadManager is main-actor-bound (inside a View body this was implicit).
@MainActor
func buttonGlyph(
    _ button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>, fallback: String
) -> String {
    let manager = GamepadManager.shared
    if let live = manager.active?.controller.extendedGamepad?[keyPath: button].sfSymbolsName {
        return live
    }
    guard let role = GamepadButtonRole(keyPath: button) else { return fallback }
    return GamepadGlyphs.symbol(role, for: manager.lastKnownKind)
}

/// Top padding for a gamepad screen's pinned title. macOS gets extra clearance — the launcher
/// title sits right under the window titlebar and the settings/add-host sheets have no titlebar
/// at all. The other values follow the console shell's rhythm (title top = 18 design units,
/// k-floored to 10 for a landscape phone): the title needs air to the screen edge or the whole
/// header reads pressed against the bezel, which the tab strip's extra band made obvious.
func gamepadTitleTopPadding(compact: Bool) -> CGFloat {
    #if os(macOS)
    26
    #elseif os(tvOS)
    24
    #else
    compact ? 18 : 28
    #endif
}

/// Padding under a gamepad screen's pinned header block (title, and the tab strip where there is
/// one) before the content: the console leaves ~14 units of air under its tab pills, and without
/// it the first row sits shoulder-to-shoulder with the header.
func gamepadTitleBottomPadding(compact: Bool) -> CGFloat {
    #if os(tvOS)
    16
    #else
    compact ? 8 : 12
    #endif
}

/// Spacing between a header's stacked elements (title over tab strip / subtitle).
func gamepadHeaderSpacing(compact: Bool) -> CGFloat {
    #if os(tvOS)
    13
    #else
    compact ? 6 : 10
    #endif
}

/// Point size for a gamepad screen's pinned title: TV-large on tvOS (read from the couch), the
/// in-hand compact-aware sizes elsewhere. Sized as a proper screen heading — the field verdict
/// on the smaller first cut was "way too small" once the title moved off-centre.
func gamepadTitleSize(compact: Bool) -> CGFloat {
    #if os(tvOS)
    44
    #else
    compact ? 24 : 34
    #endif
}

/// Metrics shared by the gamepad form screens' glass rows (GamepadSettingsView,
/// GamepadAddHostView) — one set of numbers so the screens read as the same surface, at the size
/// the screen they are on calls for.
///
/// Three tiers, not two. The phone numbers used to serve every non-TV device, so an iPad Pro drew
/// a settings list at iPhone scale in the middle of a 13" display — the field verdict was that the
/// sizing "does not adapt to larger screens". `pad` sits between the in-hand and 10-foot sets.
///
/// Chosen from the SIZE CLASSES rather than the device idiom, so an iPad running a narrow Stage
/// Manager or Split View window correctly gets the in-hand numbers — the window is what the user
/// is reading, not the panel it sits on.
struct GamepadFormMetrics {
    /// Which set this is, for the few things that are a KIND of layout rather than a number.
    enum Tier { case phone, pad, tv }

    let tier: Tier
    let headerFont: CGFloat
    let labelFont: CGFloat
    let valueFont: CGFloat
    let iconFont: CGFloat
    let iconWidth: CGFloat
    let chevronFont: CGFloat
    let rowHPad: CGFloat
    let rowVPad: CGFloat
    let rowCorner: CGFloat
    let rowMaxWidth: CGFloat
    let detailFont: CGFloat
    /// The option band's (GamepadOptionBand) fixed stage inside a choice row.
    let bandWidth: CGFloat
    /// The settings screen's section-tab pills.
    let tabFont: CGFloat
    /// The pinned controls legend (GamepadHintBar).
    let hintGlyphFont: CGFloat
    let hintTextFont: CGFloat
    let hintPad: CGFloat

    /// In-hand: a phone, or any window narrow enough to read like one.
    static let phone = GamepadFormMetrics(
        tier: .phone,
        headerFont: 12, labelFont: 16, valueFont: 15, iconFont: 17, iconWidth: 28,
        chevronFont: 12, rowHPad: 16, rowVPad: 13, rowCorner: 14, rowMaxWidth: 620,
        detailFont: 13, bandWidth: 240,
        tabFont: 13, hintGlyphFont: 19, hintTextFont: 14, hintPad: 13)

    /// A tablet-sized window — an arm's length away rather than in the palm.
    static let pad = GamepadFormMetrics(
        tier: .pad,
        headerFont: 14, labelFont: 20, valueFont: 19, iconFont: 21, iconWidth: 34,
        chevronFont: 14, rowHPad: 20, rowVPad: 16, rowCorner: 16, rowMaxWidth: 820,
        detailFont: 16, bandWidth: 320,
        tabFont: 16, hintGlyphFont: 23, hintTextFont: 17, hintPad: 15)

    /// 10-foot.
    static let tv = GamepadFormMetrics(
        tier: .tv,
        headerFont: 17, labelFont: 23, valueFont: 21, iconFont: 24, iconWidth: 40,
        chevronFont: 16, rowHPad: 24, rowVPad: 19, rowCorner: 18, rowMaxWidth: 920,
        detailFont: 19, bandWidth: 380,
        tabFont: 17, hintGlyphFont: 27, hintTextFont: 20, hintPad: 18)

    /// What a screen gets before anything publishes a tier — and the only tier tvOS and macOS ever
    /// use (an Apple TV is always 10-foot; a Mac window is read at desk distance).
    static var platformDefault: GamepadFormMetrics {
        #if os(tvOS)
        tv
        #else
        phone
        #endif
    }

    #if os(iOS)
    /// The tier a window's size classes call for. REGULAR on both axes is the tablet case.
    static func forWindow(
        h: UserInterfaceSizeClass?, v: UserInterfaceSizeClass?
    ) -> GamepadFormMetrics {
        h == .regular && v == .regular ? .pad : .phone
    }
    #endif
}

private struct GamepadMetricsKey: EnvironmentKey {
    static let defaultValue = GamepadFormMetrics.platformDefault
}

extension EnvironmentValues {
    /// The form metrics for the screen currently drawing. Published from ContentView — the app
    /// ROOT — rather than only from `gamepadPaletteInk`, because a screen that applies that
    /// modifier itself sits ABOVE its own copy: its `@Environment` resolves against its parent, so
    /// it would read the bare default instead of its own window's tier.
    var gamepadMetrics: GamepadFormMetrics {
        get { self[GamepadMetricsKey.self] }
        set { self[GamepadMetricsKey.self] = newValue }
    }
}

private struct DisplayBottomInsetKey: EnvironmentKey {
    static let defaultValue: CGFloat = 0
}

extension EnvironmentValues {
    /// The display's bottom safe-area inset — the home-indicator strip — measured by
    /// `DisplayBottomInsetProbe` and published from ContentView. 0 until UIKit's first callback
    /// lands (the legend keeps its plain margin for that first frame) and always 0 on
    /// macOS/tvOS, where nothing publishes it.
    var displayBottomInset: CGFloat {
        get { self[DisplayBottomInsetKey.self] }
        set { self[DisplayBottomInsetKey.self] = newValue }
    }
}

#if os(iOS)
/// Reports the hosting window's bottom safe-area inset from UIKit's OWN callbacks — never
/// during a SwiftUI render.
///
/// This number has a history of wrong spellings, each failing silently:
///   - a `GeometryReader` carrying `.ignoresSafeArea()` — a proxy reports NO insets for an edge it
///     has been told to ignore, so that spelling can only ever answer 0;
///   - `.ignoresSafeArea(.container, edges: .bottom)` on `safeAreaInset` CONTENT, which does not
///     move content the inset mechanism itself placed; and
///   - asking UIKit for the key window (`UIApplication.shared.connectedScenes…`) DURING body,
///     which answered correctly and then KILLED the calling view: on an iPad (never the
///     simulator) the walk re-enters UIKit layout mid-render and the view's update graph is
///     silently severed — every later `@State` write lands in storage without ever re-running
///     `body` again, which is how Settings and Add Host stopped opening while their triggers
///     kept firing. No AttributeGraph warning, no log line; found by bisecting builds on glass.
/// So: UIKit tells THIS view when the window or its insets change, on UIKit's schedule, and the
/// answer hops out of the current update before anyone in SwiftUI reads it.
struct DisplayBottomInsetProbe: UIViewRepresentable {
    let onChange: (CGFloat) -> Void

    func makeUIView(context: Context) -> ProbeView {
        let view = ProbeView()
        view.onChange = onChange
        // Mounted as a full-size `.background`; it must never eat a touch meant for the UI.
        view.isUserInteractionEnabled = false
        return view
    }

    func updateUIView(_ view: ProbeView, context: Context) {
        view.onChange = onChange
    }

    final class ProbeView: UIView {
        var onChange: ((CGFloat) -> Void)?
        private var last: CGFloat?

        override func didMoveToWindow() {
            super.didMoveToWindow()
            report()
        }

        override func safeAreaInsetsDidChange() {
            super.safeAreaInsetsDidChange()
            report()
        }

        // Rotation reshuffles the window's insets without necessarily touching this view's own.
        override func layoutSubviews() {
            super.layoutSubviews()
            report()
        }

        private func report() {
            // The WINDOW's inset, not this view's: the probe sits inside the safe area, so its
            // own inset is 0 — the number the legend needs is the strip the window reserves.
            guard let bottom = window?.safeAreaInsets.bottom, bottom != last else { return }
            last = bottom
            let onChange = onChange
            // Out of the current UIKit/SwiftUI update before any state write.
            DispatchQueue.main.async { onChange?(bottom) }
        }
    }
}
#endif

/// The bottom padding that puts a pinned legend the same distance from the bottom of the DISPLAY
/// as it sits from the leading edge — so it lands on the diagonal of the display's rounded corner,
/// which is what the corner asks for.
///
/// A `safeAreaInset` places its content INSIDE the safe area, so a plain margin stacks on top of
/// the device's own bottom inset and the pill ends up two to three times further from the bottom
/// than from the left. On a tablet this therefore goes NEGATIVE, pulling the pill back down
/// through the home-indicator strip; the pill is left-aligned and an iPad's indicator is a short
/// bar in the middle, so the two never meet.
///
/// Phones keep the plain margin. Their inset is the taller indicator bar and their legend runs
/// most of the width, so sitting it that low would cross the indicator rather than tuck beside it.
///
/// `displayBottom` is `\.displayBottomInset` — measured by `DisplayBottomInsetProbe`, NEVER asked
/// of UIKit here: this runs during body, and a key-window walk mid-render severs the calling
/// view's updates (see the probe's comment). Pure arithmetic only.
func gamepadLegendBottomPadding(
    _ margin: CGFloat, tier: GamepadFormMetrics.Tier, displayBottom: CGFloat
) -> CGFloat {
    guard tier == .pad else { return margin }
    // Floored at -inset: at worst the pill sits flush with the physical edge, never past it.
    return max(-displayBottom, margin - displayBottom)
}

/// One glyph + label cell in a hint bar.
struct GamepadHint: Identifiable {
    let glyph: String
    let text: String
    /// What tapping/clicking this cell does — the same thing its button does. Optional because a
    /// few legend cells NAME an input rather than an action ("↔ Adjust" is the stick itself;
    /// there is no single thing a tap on it could mean), and those stay inert labels.
    var action: (() -> Void)? = nil
    var id: String { glyph + text }
}

/// The pinned controls legend every gamepad screen shows bottom-leading (via `.safeAreaInset`).
/// Same font/spacing everywhere so the legend reads as system chrome, not per-screen decoration —
/// worn as a self-contained Liquid Glass pill (like the top-bar controller chip) so it floats over
/// the backdrop instead of dissolving into it.
struct GamepadHintBar: View {
    @Environment(\.gamepadInk) private var ink
    /// Sized with the screen it pins to — a legend at phone scale on a 13" iPad is the same
    /// mismatch the form rows had (see GamepadFormMetrics).
    @Environment(\.gamepadMetrics) private var metrics
    let hints: [GamepadHint]

    var body: some View {
        HStack(spacing: 18) {
            ForEach(hints) { hint in
                cell(hint)
            }
        }
        .font(.geist(metrics.hintTextFont, .semibold, relativeTo: .subheadline))
        .foregroundStyle(ink.fg(0.85))
        .padding(metrics.hintPad)
        .consoleGlass(Capsule())
        // The hairline is DECORATION and sits on top of the cells, so it must never take a touch.
        // Spelled out rather than left to defaults, because a swallowed touch in this bar is
        // invisible — the legend simply stops doing anything.
        .overlay(Capsule().strokeBorder(ink.fg(0.12), lineWidth: 1).allowsHitTesting(false))
    }

    /// A cell is a button where it has somewhere to go, and a plain label otherwise (see the type
    /// comment for why tvOS is always the latter).
    @ViewBuilder private func cell(_ hint: GamepadHint) -> some View {
        #if os(tvOS)
        label(hint)
        #else
        if let action = hint.action {
            Button(action: action) { label(hint) }
                .buttonStyle(HintCellStyle())
                .accessibilityLabel(hint.text)
        } else {
            label(hint)
        }
        #endif
    }

    private func label(_ hint: GamepadHint) -> some View {
        HStack(spacing: 7) {
            Image(systemName: hint.glyph)
                .font(.system(size: metrics.hintGlyphFont))
                .foregroundStyle(ink.fg)
            Text(hint.text)
        }
        .fixedSize() // keep glyph + label together; never truncate a hint mid-word
        // The tappable area covers the gap between glyph and label, not just their painted
        // pixels — a legend cell is small enough already.
        .contentShape(Rectangle())
    }
}

#if !os(tvOS)
/// Press feedback for a legend cell. Deliberately quiet — the bar is chrome, and a cell that lit
/// up like a primary button would pull the eye off the content it describes.
///
/// `contentShape` sits BELOW the scale so the hit region stays the unscaled layout bounds: a press
/// animation that shrinks the artwork must never move the target out from under a resting finger,
/// or the touch-up lands outside and SwiftUI discards the tap.
private struct HintCellStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .opacity(configuration.isPressed ? 0.55 : 1)
            .scaleEffect(configuration.isPressed ? 0.94 : 1)
            .animation(.smooth(duration: 0.14), value: configuration.isPressed)
            .contentShape(Rectangle())
    }
}
#endif

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
    /// How far toward the form screens' quiet the field sits: 0 = the launcher's full aurora,
    /// 1 = calm, fractional mid-chase. Continuous (not a Bool) so the in-place shell can CHASE
    /// it during a push/pop — the console does the same with its `bg_mix` — and every
    /// calm-dependent factor below rides an `.opacity` modifier, which animates reliably where
    /// re-built gradient stops do not.
    var calmMix: Double

    /// The Bool spelling every non-shell call site uses (see the type comment for `calm`).
    init(calm: Bool = false) {
        calmMix = calm ? 1 : 0
    }

    init(calmMix: Double) {
        self.calmMix = calmMix
    }

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
                // Calm = col·0.6 + ground·0.4. Over the OPAQUE ground beneath, `.opacity` already
                // lerps toward it, so this layer alone IS the whole calm mix.
                .opacity(1 - 0.4 * calmMix)
            // A further plusLighter wash of the ground, which lets a DARK palette's bright pools
            // come down to meet its ground rather than merely fading toward it.
            //
            // Suppressed on a pale palette (the factor goes to 0), because there it was destroying
            // the setting: a pale ground is near-white, so ADDING 0.4 of it on top of a field
            // already mixed 0.4 toward that same ground saturated the form screens to flat white —
            // the field ask was "in bright mode the sub-screens are basically just white". Written
            // as a factor rather than an `if` so the layer stays mounted and the calm chase keeps
            // animating instead of popping when a screen is pushed.
            Self.color(palette.ground)
                .opacity(0.4 * calmMix * (palette.light ? 0 : 1))
                .blendMode(.plusLighter)
            // Cinematic vignette: the edges settle toward the scrim so the cards sit in the
            // pooled light. Soft (extends past the frame) so the corners deepen rather than
            // crush. Halved under calm: a launcher's cards sit in the pooled centre, but a form
            // screen's rows run out toward the edges, where crushing them just eats the list.
            EllipticalGradient(
                colors: [.clear, scrim.opacity(0.42 * strength)],
                center: .center, startRadiusFraction: 0.25, endRadiusFraction: 1.15)
                .opacity(1 - 0.5 * calmMix)
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
