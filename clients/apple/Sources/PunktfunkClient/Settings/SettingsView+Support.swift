// SettingsView's footers and stateful helpers, used by both the section builders
// (SettingsView+Sections.swift) and the per-platform bodies (SettingsView.swift). The option
// LISTS live in SettingsOptions — they're shared with the gamepad settings screen too.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

extension SettingsView {
    // MARK: - Bitrate

    /// Slider domain, log-scale: the useful range spans three orders of magnitude
    /// (a few Mbps … 3 Gbps) — linear would cram everything below 100 Mbps into the
    /// first pixels.
    private static let minSliderKbps = 2_000.0
    private static let maxSliderKbps = 3_000_000.0

    static let bitrateFooter =
        "Automatic uses the host's default bitrate (20 Mbps); the host clamps any choice "
        + "to its supported range. Run a speed test from a host card's context menu to "
        + "pick an informed value. Applies from the next session."

    static let gigabitWarning =
        "Above 1 Gbps — test the network speed first (a host card's context menu → "
        + "Test Network Speed…). A bitrate beyond what the link sustains causes loss "
        + "and stutter."

    /// `bitrateKbps == 0` is Automatic; switching to manual lands on the host default.
    var automaticBitrate: Binding<Bool> {
        Binding(
            get: { bitrateKbps == 0 },
            set: { bitrateKbps = $0 ? 0 : 20_000 })
    }

    /// Slider position 0...1 ↔ kbps on the log scale, snapped to two significant figures
    /// so the readout shows round numbers instead of 47_322.
    var bitrateSlider: Binding<Double> {
        Binding(
            get: {
                let v = Double(bitrateKbps).clamped(Self.minSliderKbps, Self.maxSliderKbps)
                return log(v / Self.minSliderKbps)
                    / log(Self.maxSliderKbps / Self.minSliderKbps)
            },
            set: { pos in
                let raw = Self.minSliderKbps
                    * pow(Self.maxSliderKbps / Self.minSliderKbps, pos)
                let mag = pow(10, floor(log10(raw)) - 1)
                bitrateKbps = Int((raw / mag).rounded() * mag)
            })
    }

    // MARK: - Statistics

    static var statisticsFooter: String {
        let base = "Shows resolution, frame rate, throughput and latency in the chosen "
            + "corner while streaming."
        #if os(macOS) || os(iOS)
        return base + " Toggle it any time with ⌃⌥⇧S."
        #else
        return base
        #endif
    }

    // MARK: - Audio

    static var audioFooter: String {
        #if os(macOS)
        return "Host audio plays through the chosen speaker; your microphone feeds the host's "
            + "virtual mic. System default follows your Mac's device changes. Applies from the "
            + "next session."
        #else
        return "Host audio plays locally; your microphone feeds the host's virtual mic. "
            + "Applies from the next session."
        #endif
    }

    // MARK: - Controllers

    static let controllersFooter =
        "One controller is forwarded as player 1 — Automatic picks the most recently "
        + "connected. Type is the virtual pad the host creates; Automatic matches your "
        + "controller (a DualSense keeps adaptive triggers, lightbar, touchpad and motion). "
        + "Applies from the next session."

    #if !os(tvOS)
    static let gamepadUIFooter =
        "When a controller connects, the host list and library switch to a controller-"
        + "friendly layout — larger focus targets and a swipeable cover browser. Turn this "
        + "off to always use the standard layout."
    #endif

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
            if gamepads.active?.id == controller.id {
                Text("In use")
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
        width = Int(screen.frame.width * scale)
        height = Int(screen.frame.height * scale)
        hz = screen.maximumFramesPerSecond
        #else
        // nativeBounds is portrait-oriented pixels — streams are landscape.
        let bounds = UIScreen.main.nativeBounds
        width = Int(max(bounds.width, bounds.height))
        height = Int(min(bounds.width, bounds.height))
        hz = UIScreen.main.maximumFramesPerSecond
        #if os(iOS)
        // The native mode is the "This device" wheel row, so leave Custom mode if it was on.
        customMode = false
        #endif
        #endif
    }
}
