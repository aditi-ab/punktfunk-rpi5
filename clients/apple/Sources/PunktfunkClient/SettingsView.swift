// App settings (⌘,): the stream mode, the host compositor, and controllers. The host
// creates a native virtual output at exactly this size/refresh — there is no scaling
// anywhere in the pipeline.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @AppStorage("punktfunk.compositor") private var compositor = 0
    @AppStorage("punktfunk.gamepadType") private var gamepadType = 0
    @AppStorage("punktfunk.bitrateKbps") private var bitrateKbps = 0
    @AppStorage("punktfunk.presenter") private var presenter = "stage1"
    @AppStorage("punktfunk.micEnabled") private var micEnabled = true
    @ObservedObject private var gamepads = GamepadManager.shared
    #if os(macOS)
    @AppStorage("punktfunk.speakerUID") private var speakerUID = ""
    @AppStorage("punktfunk.micUID") private var micUID = ""
    @State private var outputDevices: [AudioDevice] = []
    @State private var inputDevices: [AudioDevice] = []
    #endif

    var body: some View {
        #if os(tvOS)
        // Native tv pattern: no inline text entry (typing numbers with a remote is
        // miserable and the inline field chrome fights the focus system). The mode is
        // a preset picker; pickers push selection lists like the system Settings app.
        tvBody
        #else
        sharedBody
        #endif
    }

    #if os(tvOS)
    private static let presets: [(label: String, tag: String)] = [
        ("720p @ 60", "1280x720x60"),
        ("1080p @ 60", "1920x1080x60"),
        ("4K @ 60", "3840x2160x60"),
    ]

    private var modeTag: Binding<String> {
        Binding(
            get: { "\(width)x\(height)x\(hz)" },
            set: { tag in
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 3 else { return }
                width = parts[0]
                height = parts[1]
                hz = parts[2]
            })
    }

    private var tvBody: some View {
        let currentTag = "\(width)x\(height)x\(hz)"
        let bounds = UIScreen.main.nativeBounds
        let nativeTag = "\(Int(max(bounds.width, bounds.height)))x"
            + "\(Int(min(bounds.width, bounds.height)))x\(UIScreen.main.maximumFramesPerSecond)"
        var options = Self.presets
        if !options.contains(where: { $0.tag == nativeTag }) {
            options.insert(("This TV (native)", nativeTag), at: 0)
        }
        if !options.contains(where: { $0.tag == currentTag }) {
            options.insert(("Custom (\(width)×\(height) @ \(hz))", currentTag), at: 0)
        }
        let compositors: [(label: String, tag: Int)] = [
            ("Automatic", 0),
            ("KWin (KDE Plasma)", 1),
            ("wlroots (Sway / Hyprland)", 2),
            ("Mutter (GNOME)", 3),
            ("gamescope", 4),
        ]
        return ScrollView {
            VStack(spacing: 16) {
                TVSelectionRow(title: "Stream mode", options: options, selection: modeTag)
                TVSelectionRow(
                    title: "Bitrate", options: bitrateOptions, selection: $bitrateKbps)
                if bitrateKbps > 1_000_000 {
                    Label(Self.gigabitWarning, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .multilineTextAlignment(.center)
                }
                TVSelectionRow(
                    title: "Compositor", options: compositors, selection: $compositor)
                TVSelectionRow(
                    title: "Presenter",
                    options: [("Stage 1 (default)", "stage1"), ("Stage 2 (experimental)", "stage2")],
                    selection: $presenter)
                Text("The host creates a virtual output at exactly this mode — native "
                    + "resolution, no scaling. \(Self.bitrateFooter) A specific compositor "
                    + "is honored only if available on the host.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
                ForEach(gamepads.controllers) { controller in
                    controllerRow(controller)
                        .padding(.horizontal, 24)
                }
                TVSelectionRow(
                    title: "Use controller", options: controllerOptions,
                    selection: $gamepads.preferredID)
                TVSelectionRow(
                    title: "Controller type", options: Self.padTypes, selection: $gamepadType)
                Text(Self.controllersFooter)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
            }
            .frame(maxWidth: 1000)
            .frame(maxWidth: .infinity)
            .padding(60)
        }
        .navigationTitle("Settings")
        .onAppear {
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        .onDisappear { gamepads.stopDiscovery() }
    }
    #endif

    // MARK: - Bitrate

    /// Slider domain, log-scale: the useful range spans three orders of magnitude
    /// (a few Mbps … 3 Gbps) — linear would cram everything below 100 Mbps into the
    /// first pixels.
    private static let minSliderKbps = 2_000.0
    private static let maxSliderKbps = 3_000_000.0

    private static let bitrateFooter =
        "Automatic uses the host's default bitrate (20 Mbps); the host clamps any choice "
        + "to its supported range. Run a speed test from a host card's context menu to "
        + "pick an informed value. Applies from the next session."

    private static let gigabitWarning =
        "Above 1 Gbps — test the network speed first (a host card's context menu → "
        + "Test Network Speed…). A bitrate beyond what the link sustains causes loss "
        + "and stutter."

    /// `bitrateKbps == 0` is Automatic; switching to manual lands on the host default.
    private var automaticBitrate: Binding<Bool> {
        Binding(
            get: { bitrateKbps == 0 },
            set: { bitrateKbps = $0 ? 0 : 20_000 })
    }

    /// Slider position 0...1 ↔ kbps on the log scale, snapped to two significant figures
    /// so the readout shows round numbers instead of 47_322.
    private var bitrateSlider: Binding<Double> {
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

    #if os(tvOS)
    /// tvOS has no Slider — the focus-native control is the pushed picker (the same
    /// pattern as the stream mode), so the rates are presets here, up to the same 3 Gbps
    /// ceiling, plus a custom entry so a non-preset stored value stays visible.
    private static let bitratePresets: [(label: String, tag: Int)] = [
        ("Automatic", 0),
        ("10 Mbps", 10_000),
        ("20 Mbps", 20_000),
        ("40 Mbps", 40_000),
        ("80 Mbps", 80_000),
        ("150 Mbps", 150_000),
        ("300 Mbps", 300_000),
        ("500 Mbps", 500_000),
        ("1 Gbps", 1_000_000),
        ("1.5 Gbps", 1_500_000),
        ("2 Gbps", 2_000_000),
        ("3 Gbps", 3_000_000),
    ]

    private var bitrateOptions: [(label: String, tag: Int)] {
        var options = Self.bitratePresets
        if !options.contains(where: { $0.tag == bitrateKbps }) {
            options.insert(
                (SpeedTestSheet.mbpsLabel(kbps: bitrateKbps) + " (custom)", bitrateKbps), at: 1)
        }
        return options
    }
    #endif

    // MARK: - Controllers

    private static let padTypes: [(label: String, tag: Int)] = [
        ("Automatic", 0),
        ("Xbox 360", 1),
        ("DualSense", 2),
    ]

    private static let controllersFooter =
        "One controller is forwarded to the host, as player 1 — Automatic picks the most "
        + "recently connected one. The type is the virtual pad the host creates: Automatic "
        + "matches the controller (a DualSense gets adaptive triggers, lightbar, touchpad "
        + "and motion), and changes apply from the next session. Two identical controllers "
        + "may swap a manual selection after reconnecting."

    /// "Use controller" choices: Automatic, every forwardable controller, and — so a stale
    /// pin stays visible instead of leaving the Picker selection tag-less — any pinned id
    /// that is NOT among the selectable (extended) entries, present-but-unusable included.
    private var controllerOptions: [(label: String, tag: String)] {
        let selectable = gamepads.controllers.filter(\.isExtended)
        var options: [(label: String, tag: String)] = [("Automatic", "")]
        options += selectable.map { ($0.name, $0.id) }
        if !gamepads.preferredID.isEmpty,
           !selectable.contains(where: { $0.id == gamepads.preferredID }) {
            options.append(("Unavailable controller", gamepads.preferredID))
        }
        return options
    }

    private func controllerRow(_ controller: GamepadManager.DiscoveredController) -> some View {
        HStack(spacing: 10) {
            Image(systemName: controller.isDualSense ? "playstation.logo" : "gamecontroller.fill")
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
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
            Spacer()
            if gamepads.active?.id == controller.id {
                Text("In use")
                    .font(.caption2.weight(.semibold))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .background(Capsule().fill(.green.opacity(0.2)))
                    .foregroundStyle(.green)
            }
        }
    }

    private var sharedBody: some View {
        Form {
            Section {
                HStack {
                    TextField("Resolution", value: $width, format: .number.grouping(.never))
                    Text("×")
                    TextField("", value: $height, format: .number.grouping(.never))
                        .labelsHidden()
                }
                TextField("Refresh rate (Hz)", value: $hz, format: .number.grouping(.never))
                LabeledContent("") {
                    Button("Use this display's mode") { fillFromMainScreen() }
                }
                // (sharedBody is unused on tvOS — its body still compiles there, and
                // Slider doesn't exist on tvOS; the tv path has its own preset picker.)
                #if !os(tvOS)
                Toggle("Automatic bitrate", isOn: automaticBitrate)
                if bitrateKbps != 0 {
                    HStack(spacing: 12) {
                        Slider(value: bitrateSlider, in: 0...1) {
                            Text("Bitrate")
                        }
                        Text(SpeedTestSheet.mbpsLabel(kbps: bitrateKbps))
                            .monospacedDigit()
                            .foregroundStyle(.secondary)
                            .frame(minWidth: 76, alignment: .trailing)
                    }
                    if bitrateKbps > 1_000_000 {
                        Label(Self.gigabitWarning, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
                #endif
            } header: {
                Text("Stream mode")
            } footer: {
                Text("The host creates a virtual output at exactly this mode — "
                    + "native resolution, no scaling. \(Self.bitrateFooter)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            #if !os(tvOS)
            Section {
                #if os(macOS)
                Picker("Speaker", selection: $speakerUID) {
                    Text("System default").tag("")
                    ForEach(outputDevices) { device in
                        Text(device.name).tag(device.uid)
                    }
                    if !speakerUID.isEmpty,
                       !outputDevices.contains(where: { $0.uid == speakerUID }) {
                        Text("Unavailable device").tag(speakerUID)
                    }
                }
                #endif
                #if !os(tvOS)
                Toggle("Send microphone to the host", isOn: $micEnabled)
                #endif
                #if os(macOS)
                Picker("Microphone", selection: $micUID) {
                    Text("System default").tag("")
                    ForEach(inputDevices) { device in
                        Text(device.name).tag(device.uid)
                    }
                    if !micUID.isEmpty,
                       !inputDevices.contains(where: { $0.uid == micUID }) {
                        Text("Unavailable device").tag(micUID)
                    }
                }
                .disabled(!micEnabled)
                #endif
            } header: {
                Text("Audio")
            } footer: {
                Text("Host audio plays through the speaker; the microphone feeds the "
                    + "host's virtual mic. System default follows macOS device changes. "
                    + "Applies from the next session.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            #endif
            Section {
                Picker("Compositor", selection: $compositor) {
                    Text("Automatic").tag(0)
                    Text("KWin (KDE Plasma)").tag(1)
                    Text("wlroots (Sway / Hyprland)").tag(2)
                    Text("Mutter (GNOME)").tag(3)
                    Text("gamescope").tag(4)
                }
            } header: {
                Text("Host compositor")
            } footer: {
                Text("Which compositor drives the virtual output on the host. A specific "
                    + "choice is honored only if that backend is available there — "
                    + "otherwise the host falls back to auto-detection.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                Picker("Presenter", selection: $presenter) {
                    Text("Stage 1 (default)").tag("stage1")
                    Text("Stage 2 (experimental)").tag("stage2")
                }
            } header: {
                Text("Video presenter")
            } footer: {
                Text("Stage 1 feeds compressed video to the system display layer (known-good). "
                    + "Stage 2 decodes explicitly and presents through Metal with a display "
                    + "link — it adds a capture→present (glass-to-glass) latency line in the HUD "
                    + "and shortens the present tail. Applies from the next session.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                if gamepads.controllers.isEmpty {
                    Text("No controllers detected")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(gamepads.controllers) { controller in
                        controllerRow(controller)
                    }
                }
                Picker("Use controller", selection: $gamepads.preferredID) {
                    ForEach(controllerOptions, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
                Picker("Controller type", selection: $gamepadType) {
                    ForEach(Self.padTypes, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            } header: {
                Text("Controllers")
            } footer: {
                Text(Self.controllersFooter)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear {
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        .onDisappear { gamepads.stopDiscovery() }
        #if os(macOS)
        .frame(width: 380)
        .fixedSize()
        .onAppear {
            outputDevices = AudioDevices.outputs()
            inputDevices = AudioDevices.inputs()
        }
        #endif
    }

    private func fillFromMainScreen() {
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
        #endif
    }
}

extension Double {
    /// The log-scale slider mapping needs a bounded input (Automatic stores 0).
    fileprivate func clamped(_ lo: Double, _ hi: Double) -> Double {
        Swift.min(Swift.max(self, lo), hi)
    }
}
