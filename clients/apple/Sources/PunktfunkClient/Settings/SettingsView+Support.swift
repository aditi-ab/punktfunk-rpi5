// SettingsView's footers and stateful helpers, used by both the section builders
// (SettingsView+Sections.swift) and the per-platform bodies (SettingsView.swift). The option
// LISTS live in SettingsOptions — they're shared with the gamepad settings screen too.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

/// How wide a `described` row's caption may run. TEXT only — the override marker's Reset is a
/// control and belongs on the row's trailing edge, not in the caption's column.
///
/// Two limits, because they solve different problems. The 360pt CAP is about reading: past roughly
/// 46 characters a line stops scanning well, and on a wide Mac window or an iPad detail pane an
/// uncapped caption runs the full cell. The trailing INSET is about collision: an iOS grouped row
/// puts its switch or value in from the trailing edge, and a caption laid out to the full cell
/// width runs its last line straight under that control — which is what the cap alone never
/// fixed, because an iPhone cell is narrower than the cap in the first place.
struct CaptionWidth: ViewModifier {
    #if os(iOS)
    /// Reserve the control column. A `UISwitch` is 51pt, and the rest is breathing room — the
    /// caption should stop visibly short of the control, not graze it.
    private static let trailingInset: CGFloat = 76
    #else
    // macOS lays the control out inline and its cells are wider than the cap; nothing to clear.
    private static let trailingInset: CGFloat = 0
    #endif

    func body(content: Content) -> some View {
        content
            // Order matters: the padding shrinks what the text is offered, THEN the cap applies —
            // so a narrow phone cell reserves the control column and a wide pane still caps.
            .padding(.trailing, Self.trailingInset)
            .frame(maxWidth: 360, alignment: .leading)
    }
}

extension SettingsView {
    // MARK: - Described rows (the 2026-07 revamp's field idiom)

    /// A control with its explanation attached to the SAME cell: the field, then a tight caption
    /// directly under it. This replaced the per-section footer paragraphs — a description the eye
    /// can't match to its field is one nobody reads. Keep captions to one or two sentences; when
    /// a picker's meaning depends on the selection, pass a DYNAMIC string describing the current
    /// choice.
    /// `field` is the overlay's name for this row (see `SettingsField`). Passing it puts the
    /// override marker + Reset in the caption line while a profile is being edited — with the row
    /// it belongs to, which is the only place the state is legible.
    @ViewBuilder
    func described<Content: View>(
        _ caption: String, field: String? = nil, @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            content()
            Text(caption)
                .font(.geist(13, relativeTo: .footnote))
                .foregroundStyle(.secondary)
                // Wrap, never truncate, in Form cells.
                .fixedSize(horizontal: false, vertical: true)
                .modifier(CaptionWidth())
            if let field {
                // Full cell width, deliberately: the marker's Reset is a CONTROL, and it belongs
                // on the same trailing edge as the row's own control above it. Sharing the
                // caption's reserved column left it stranded mid-row.
                overrideMarker(field)
            }
        }
        .padding(.vertical, 2)
    }

    // MARK: - Bitrate

    /// Slider domain, log-scale: the useful range spans three orders of magnitude
    /// (a few Mbps … 3 Gbps) — linear would cram everything below 100 Mbps into the
    /// first pixels.
    private static let minSliderKbps = 2_000.0
    private static let maxSliderKbps = 3_000_000.0

    /// tvOS's cluster caption (the touch/desktop forms describe bitrate per-row instead).
    ///
    /// Deliberately says nothing about the speed test: it is reached from a host card's context
    /// menu, and tvOS has no context menus — the old wording sent Apple TV users after a control
    /// that does not exist there.
    static let bitrateFooter = "Automatic bitrate uses the host's default, 20 Mbps."

    static let gigabitWarning =
        "Above 1 Gbps — more than the link sustains causes loss and stutter. Speed-test first."

    /// `bitrateKbps == 0` is Automatic; switching to manual lands on the host default. Scoped, so
    /// flipping it in a profile records the override there rather than moving the global.
    var automaticBitrate: Binding<Bool> {
        let bitrate = scoped(SettingsFields.bitrateKbps)
        return Binding(
            get: { bitrate.wrappedValue == 0 },
            set: { bitrate.wrappedValue = $0 ? 0 : 20_000 })
    }

    /// Slider position 0...1 ↔ kbps on the log scale, snapped to two significant figures
    /// so the readout shows round numbers instead of 47_322.
    var bitrateSlider: Binding<Double> {
        let bitrate = scoped(SettingsFields.bitrateKbps)
        return Binding(
            get: {
                let v = Double(bitrate.wrappedValue)
                    .clamped(Self.minSliderKbps, Self.maxSliderKbps)
                return log(v / Self.minSliderKbps)
                    / log(Self.maxSliderKbps / Self.minSliderKbps)
            },
            set: { pos in
                let raw = Self.minSliderKbps
                    * pow(Self.maxSliderKbps / Self.minSliderKbps, pos)
                let mag = pow(10, floor(log10(raw)) - 1)
                bitrate.wrappedValue = Int((raw / mag).rounded() * mag)
            })
    }

    // MARK: - Statistics

    static var statisticsDescription: String {
        // No "Live session stats in a corner overlay" preamble: it restates the row's own label.
        let base = "Compact is a one-line pill; Detailed adds the latency breakdown."
        #if os(macOS)
        return base + " ⌃⌥⇧S cycles it."
        #elseif os(iOS)
        return base + " ⌃⌥⇧S or a three-finger tap cycles it."
        #else
        return base
        #endif
    }

    // MARK: - Controllers

    /// tvOS's cluster caption (the touch/desktop form describes each row inline instead).
    static let controllersFooter =
        "Automatic uses the newest pad as player 1 and matches its type on the host. "
        + "Applies from the next session."

    /// "Use controller" choices for this view's manager (see `SettingsOptions.controllerOptions`).
    var controllerOptions: [(label: String, tag: String)] {
        SettingsOptions.controllerOptions(gamepads)
    }

    func controllerRow(_ controller: GamepadManager.DiscoveredController) -> some View {
        HStack(spacing: 10) {
            Image(systemName: controller.hasTouchpadAndMotion ? "playstation.logo" : "gamecontroller.fill")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(controller.name)
                HStack(spacing: 8) {
                    if !controller.isExtended {
                        Text(controller.productCategory)
                    }
                    if controller.hasAdaptiveTriggers {
                        Image(systemName: "r2.button.roundedtop.horizontal")
                    }
                    if controller.hasLight {
                        Image(systemName: "lightbulb.fill")
                    }
                    if controller.hasMotion {
                        Image(systemName: "gyroscope")
                    }
                    if controller.hasHaptics {
                        Image(systemName: "waveform")
                    }
                    if let level = controller.batteryLevel {
                        Text("\(Int(level * 100))%")
                        if controller.isCharging {
                            Image(systemName: "bolt.fill")
                        }
                    }
                }
                .font(.geist(11, relativeTo: .caption2))
                .foregroundStyle(.secondary)
            }
            Spacer()
            // Every forwarded controller is surfaced (not just the primary `active`) with its
            // wire pad index as a player number — a pin forwards only one, Automatic forwards all.
            if let pad = gamepads.padIndex(for: controller) {
                Text("Player \(pad + 1)")
                    .font(.geist(11, .semibold, relativeTo: .caption2))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .background(Capsule().fill(.green.opacity(0.2)))
                    .foregroundStyle(.green)
            }
        }
    }

    func fillFromMainScreen() {
        #if os(macOS)
        guard let screen = NSScreen.main else { return }
        let scale = screen.backingScaleFactor
        setResolution(
            width: Int(screen.frame.width * scale), height: Int(screen.frame.height * scale))
        scoped(SettingsFields.refreshHz).wrappedValue = screen.maximumFramesPerSecond
        #else
        // nativeBounds is portrait-oriented pixels — streams are landscape.
        let bounds = UIScreen.main.nativeBounds
        setResolution(
            width: Int(max(bounds.width, bounds.height)),
            height: Int(min(bounds.width, bounds.height)))
        scoped(SettingsFields.refreshHz).wrappedValue = UIScreen.main.maximumFramesPerSecond
        #if os(iOS)
        // The native mode is the "This device" wheel row, so leave Custom mode if it was on.
        customMode = false
        #endif
        #endif
    }
}
