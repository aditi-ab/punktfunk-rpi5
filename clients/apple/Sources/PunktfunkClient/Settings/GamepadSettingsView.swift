// The gamepad-driven settings screen (iOS/iPadOS/macOS): the couch-relevant subset of SettingsView,
// restyled as a console settings page and fully navigable with a controller — up/down moves the
// focus bar, left/right steps the focused value, A cycles/toggles it, B closes. Shown from the
// gamepad home launcher (X); the touch SettingsView remains the full-fidelity editor (custom
// resolutions, the log bitrate slider, debug tools), and both write the same DefaultsKey storage,
// so values round-trip freely between the two.
//
// Rows are rebuilt from live @AppStorage on every render; the focus list dispatches adjust/
// activate back here BY ROW ID (see `adjust`/`activate`), so a stored input callback can never act
// on stale captured state. Left/right CLAMPS at a choice list's ends (the dull boundary thud tells
// the thumb it's the last option); A always cycles forward, wrapping, so every option is reachable
// with one button. Toggles read left = off, right = on — refusing a no-op with the same thud.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)
import GameController

struct GamepadSettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage(DefaultsKey.streamWidth) private var width = 1920
    @AppStorage(DefaultsKey.streamHeight) private var height = 1080
    @AppStorage(DefaultsKey.streamHz) private var hz = 60
    @AppStorage(DefaultsKey.compositor) private var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) private var gamepadType = 0
    @AppStorage(DefaultsKey.bitrateKbps) private var bitrateKbps = 0
    @AppStorage(DefaultsKey.audioChannels) private var audioChannels = 2
    @AppStorage(DefaultsKey.hdrEnabled) private var hdrEnabled = true
    @AppStorage(DefaultsKey.enable444) private var enable444 = true
    @AppStorage(DefaultsKey.codec) private var codec = "auto"
    @AppStorage(DefaultsKey.micEnabled) private var micEnabled = true
    @AppStorage(DefaultsKey.hudEnabled) private var hudEnabled = true
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = false
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    @ObservedObject private var gamepads = GamepadManager.shared

    #if os(iOS)
    /// `.compact` in a landscape phone window — tighter chrome so more rows fit.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the sheet is sized generously
    #endif
    @State private var focusID: String?

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onAdjust: { row, delta in adjust(id: row.id, by: delta) },
            onActivate: { activate(id: $0.id) },
            onBack: { dismiss() }
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: 620)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            Text("Settings")
                .font(.geist(compact ? 20 : 30, .bold, relativeTo: .title))
                .foregroundStyle(.white)
                .padding(.top, gamepadTitleTopPadding(compact: compact))
                .padding(.bottom, compact ? 4 : 8)
                .frame(maxWidth: .infinity)
                .overlay(alignment: .trailing) { closeButton.padding(.trailing, 20) }
                .background { GamepadTrayScrim(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Text(focusedDetail)
                    .font(.geist(13, relativeTo: .caption))
                    .foregroundStyle(.white.opacity(0.55))
                    .lineLimit(2, reservesSpace: true)
                    .animation(.smooth(duration: 0.2), value: focusID)
                GamepadHintBar(hints: [
                    .init(glyph: "arrow.left.and.right", text: "Adjust"),
                    .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Change"),
                    .init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done"),
                ])
            }
            // Equal distance from the left and bottom edges for the legend pill (see GamepadHomeView).
            .padding(.leading, compact ? 12 : 18)
            .padding(.trailing, 22)
            .padding(.bottom, compact ? 12 : 18)
            .padding(.top, compact ? 6 : 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { GamepadTrayScrim(edge: .bottom) }
        }
        // No aurora here — the settings read as clean Liquid Glass over a quiet dark base, so the
        // glass rows are the only material on the screen.
        .background { GamepadFormBackground() }
        .onAppear {
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        .onDisappear { gamepads.stopDiscovery() }
    }

    /// Touch/click fallback for closing — the controller path is B, a hardware keyboard's Esc
    /// rides the cancel action.
    private var closeButton: some View {
        Button { dismiss() } label: {
            Image(systemName: "xmark")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 34, height: 34)
                .glassBackground(Circle(), interactive: true)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .keyboardShortcut(.cancelAction)
        .accessibilityLabel("Close settings")
    }

    // MARK: - Row rendering

    private func rowView(_ row: Row, focused: Bool) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            if let header = row.header {
                Text(header)
                    .font(.geist(12, .semibold, relativeTo: .caption))
                    .tracking(1.4)
                    .foregroundStyle(.white.opacity(0.45))
                    .padding(.leading, 16)
                    .padding(.top, 14)
            }
            HStack(spacing: 14) {
                Image(systemName: row.icon)
                    .font(.system(size: 17))
                    .foregroundStyle(focused ? Color.brand : .white.opacity(0.55))
                    .frame(width: 28)
                Text(row.label)
                    .font(.geist(16, .semibold, relativeTo: .body))
                    .foregroundStyle(.white)
                    .lineLimit(1)
                Spacer(minLength: 12)
                HStack(spacing: 9) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(.white.opacity(focused ? 0.6 : 0))
                    Text(row.value)
                        .font(.geist(15, .medium, relativeTo: .callout))
                        .foregroundStyle(focused ? .white : .white.opacity(0.6))
                        .lineLimit(1)
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(.white.opacity(focused ? 0.6 : 0))
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 13)
            // Every row is Liquid Glass; the focused one takes a brand wash and reacts to press.
            .consoleGlass(
                RoundedRectangle(cornerRadius: 14, style: .continuous),
                tint: focused ? Color.brand.opacity(0.30) : nil,
                interactive: focused)
            .overlay {
                RoundedRectangle(cornerRadius: 14, style: .continuous)
                    .strokeBorder(.white.opacity(focused ? 0.28 : 0.06), lineWidth: 1)
            }
            .scaleEffect(focused ? 1.0 : 0.98)
            .animation(.smooth(duration: 0.18), value: focused)
        }
    }

    private var focusedDetail: String {
        rows.first { $0.id == focusID }?.detail ?? " "
    }

    // MARK: - Row model

    private struct Row: Identifiable {
        let id: String
        /// Section header drawn above this row (the first row of each group carries it).
        var header: String?
        let icon: String
        let label: String
        let value: String
        /// One-line explanation shown near the hint bar while this row is focused.
        let detail: String
        /// Left/right step; returns whether the value actually changed (false ⇒ boundary thud).
        let adjust: (Int) -> Bool
        /// A — cycle forward (wrapping) / flip.
        let activate: () -> Void
    }

    /// Dispatch by id so the focus list's stored input callbacks always act on freshly built rows
    /// (never on state captured at wire time).
    private func adjust(id: String, by delta: Int) -> Bool {
        rows.first { $0.id == id }?.adjust(delta) ?? false
    }

    private func activate(id: String) {
        rows.first { $0.id == id }?.activate()
    }

    private var rows: [Row] {
        let resolution = resolutionOptions
        let refresh = SettingsOptions.refreshRates(including: hz)
            .map { (label: "\($0) Hz", tag: $0) }
        let bitrate = SettingsOptions.bitrateOptions(current: bitrateKbps)
        let controllers = SettingsOptions.controllerOptions(gamepads)
        return [
            choiceRow(
                id: "resolution", header: "Stream", icon: "aspectratio",
                label: "Resolution",
                detail: "The host creates a virtual display at exactly this size — no scaling.",
                options: resolution, current: "\(width)x\(height)"
            ) { tag in
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 2 else { return }
                width = parts[0]
                height = parts[1]
            },
            choiceRow(
                id: "refresh", icon: "gauge.with.needle", label: "Refresh rate",
                detail: "Rates this display can actually show.",
                options: refresh, current: hz
            ) { hz = $0 },
            choiceRow(
                id: "bitrate", icon: "speedometer", label: "Bitrate",
                detail: "Automatic uses the host's default (20 Mbps). "
                    + "Run a speed test from the touch UI for an informed value.",
                options: bitrate, current: bitrateKbps
            ) { bitrateKbps = $0 },
            choiceRow(
                id: "compositor", icon: "macwindow", label: "Compositor",
                detail: "Which compositor drives the virtual output — honored only if "
                    + "available on the host.",
                options: SettingsOptions.compositors, current: compositor
            ) { compositor = $0 },

            choiceRow(
                id: "codec", header: "Video", icon: "film", label: "Video codec",
                detail: "A preference — the host falls back if it can't encode this one "
                    + "(10-bit and 4:4:4 are HEVC-only).",
                options: SettingsOptions.codecs, current: codec
            ) { codec = $0 },
            toggleRow(
                id: "hdr", icon: "sun.max", label: "10-bit HDR",
                detail: "HDR10 — engages when the host sends HDR content and this display "
                    + "supports it.",
                value: $hdrEnabled),
            toggleRow(
                id: "chroma", icon: "textformat", label: "Full chroma (4:4:4)",
                detail: "Sharper text and UI at more bandwidth — needs host opt-in and "
                    + "hardware decode.",
                value: $enable444),

            choiceRow(
                id: "audio", header: "Audio", icon: "speaker.wave.2", label: "Audio channels",
                detail: "The speaker layout requested from the host.",
                options: SettingsOptions.audioChannels, current: audioChannels
            ) { audioChannels = $0 },
            toggleRow(
                id: "mic", icon: "mic", label: "Microphone",
                detail: "Send this device's microphone to the host's virtual mic.",
                value: $micEnabled),

            choiceRow(
                id: "pad", header: "Controller", icon: "gamecontroller", label: "Use controller",
                detail: "Which pad is forwarded to the host, as player 1.",
                options: controllers, current: gamepads.preferredID
            ) { gamepads.preferredID = $0 },
            choiceRow(
                id: "padType", icon: "dpad", label: "Controller type",
                detail: "The virtual pad the host creates — Automatic matches this controller.",
                options: SettingsOptions.padTypes, current: gamepadType
            ) { gamepadType = $0 },

            toggleRow(
                id: "hud", header: "Interface", icon: "chart.bar", label: "Statistics overlay",
                detail: "Resolution, frame rate, throughput and latency while streaming.",
                value: $hudEnabled),
            choiceRow(
                id: "hudPlacement", icon: "rectangle.inset.topright.filled", label: "Overlay position",
                detail: "Which corner the statistics overlay sits in.",
                options: SettingsOptions.hudPlacements, current: hudPlacement
            ) { hudPlacement = $0 },
            toggleRow(
                id: "library", icon: "square.grid.2x2", label: "Game library",
                detail: "Browse and launch the host's games with \(buttonName(\.buttonY, "Y")) "
                    + "(experimental).",
                value: $libraryEnabled),
            toggleRow(
                id: "gamepadUI", icon: "hand.tap", label: "Controller-optimized UI",
                detail: "Turn off to use the touch interface even with a controller connected.",
                value: $gamepadUIEnabled),
        ]
    }

    /// Resolution choices as "WxH" tags — the current size is inserted when it's a custom mode
    /// (set via the touch settings), so cycling starts from it instead of jumping.
    private var resolutionOptions: [(label: String, tag: String)] {
        var options = SettingsOptions.resolutionModes()
            .map { (label: "\($0.name) · \($0.w) × \($0.h)", tag: "\($0.w)x\($0.h)") }
        let current = "\(width)x\(height)"
        if !options.contains(where: { $0.tag == current }) {
            options.insert((label: "Custom · \(width) × \(height)", tag: current), at: 0)
        }
        return options
    }

    /// The active controller's user-facing name for a button (for detail strings).
    private func buttonName(
        _ button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>, _ fallback: String
    ) -> String {
        gamepads.active?.controller.extendedGamepad?[keyPath: button].localizedName ?? fallback
    }

    // MARK: - Row builders

    private func choiceRow<T: Equatable>(
        id: String, header: String? = nil, icon: String, label: String, detail: String,
        options: [(label: String, tag: T)], current: T, write: @escaping (T) -> Void
    ) -> Row {
        let index = options.firstIndex { $0.tag == current }
        return Row(
            id: id, header: header, icon: icon, label: label,
            value: index.map { options[$0].label } ?? "—",
            detail: detail,
            adjust: { delta in
                // Unknown current value: snap to the first option on any step.
                guard let index else {
                    guard let first = options.first else { return false }
                    write(first.tag)
                    return true
                }
                let target = index + delta
                guard target >= 0, target < options.count else { return false }
                write(options[target].tag)
                return true
            },
            activate: {
                guard let index else { return write(options.first?.tag ?? current) }
                write(options[(index + 1) % options.count].tag)
            })
    }

    private func toggleRow(
        id: String, header: String? = nil, icon: String, label: String, detail: String,
        value: Binding<Bool>
    ) -> Row {
        Row(
            id: id, header: header, icon: icon, label: label,
            value: value.wrappedValue ? "On" : "Off",
            detail: detail,
            adjust: { delta in
                // Directional semantics: left = off, right = on; a no-op reads as a boundary.
                let target = delta > 0
                guard value.wrappedValue != target else { return false }
                value.wrappedValue = target
                return true
            },
            activate: { value.wrappedValue.toggle() })
    }
}

#endif
