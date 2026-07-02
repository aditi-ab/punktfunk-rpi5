// App settings. The host creates a native virtual output at exactly the chosen size/refresh —
// there is no scaling anywhere in the pipeline.
//
// Navigation differs per platform, but all three group the same categories (General, Display,
// Audio, Controllers, Advanced, About): macOS uses a tabbed preferences window; iOS/iPadOS uses
// an adaptive NavigationSplitView — a category sidebar + detail pane on iPad, auto-collapsing to
// a hierarchical push list on iPhone (the system Settings idiom on each); tvOS uses a
// focus-native pushed-picker layout. The individual sections (`streamModeSection`,
// `audioSection`, …) are shared across all three so a setting is defined exactly once.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage(DefaultsKey.streamWidth) private var width = 1920
    @AppStorage(DefaultsKey.streamHeight) private var height = 1080
    @AppStorage(DefaultsKey.streamHz) private var hz = 60
    @AppStorage(DefaultsKey.compositor) private var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) private var gamepadType = 0
    @AppStorage(DefaultsKey.bitrateKbps) private var bitrateKbps = 0
    @AppStorage(DefaultsKey.presenter) private var presenter = "stage2"
    @AppStorage(DefaultsKey.hdrEnabled) private var hdrEnabled = true
    @AppStorage(DefaultsKey.enable444) private var enable444 = true
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = false
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) private var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.micEnabled) private var micEnabled = true
    @AppStorage(DefaultsKey.audioChannels) private var audioChannels = 2
    @AppStorage(DefaultsKey.codec) private var codec = "auto"
    @AppStorage(DefaultsKey.hudEnabled) private var hudEnabled = true
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @ObservedObject private var gamepads = GamepadManager.shared
    #if DEBUG && !os(tvOS)
    @State private var showControllerTest = false
    #endif
    #if os(iOS)
    @AppStorage(DefaultsKey.pointerCapture) private var pointerCapture = true
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    // The sidebar selection drives the detail pane on iPad and the pushed sub-page on iPhone.
    // Width class decides the initial value: nil on iPhone (show the category list first),
    // General on iPad (a two-column layout should never open with an empty detail).
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @State private var settingsSelection: SettingsCategory?
    // Tracked so the detail can show its own Done whenever the sidebar (and its Done) is off screen
    // — not just on iPhone, but on any iPad layout that collapses the sidebar to an overlay. Starts
    // .doubleColumn so iPad reliably opens with the sidebar (and its Done) visible.
    @State private var columnVisibility: NavigationSplitViewVisibility = .doubleColumn
    // Sticky once the wheel lands on "Custom…", so editing a width/height that briefly equals a
    // preset doesn't snap the wheel back off Custom. A stored non-preset value reads as custom even
    // when this is false (see `isCustomResolution`), so it survives relaunches without persisting.
    @State private var customMode = false
    #endif
    #if os(macOS)
    @AppStorage(DefaultsKey.speakerUID) private var speakerUID = ""
    @AppStorage(DefaultsKey.micUID) private var micUID = ""
    @State private var outputDevices: [AudioDevice] = []
    @State private var inputDevices: [AudioDevice] = []
    #endif

    #if os(iOS)
    /// `initialCategory` is nil in the app (the list opens un-selected on iPhone; iPad lands on
    /// General via `onAppear`). The screenshot harness passes an explicit category so the captured
    /// shot opens on a real settings page (a populated detail) rather than the bare category list.
    init(initialCategory: SettingsCategory? = nil) {
        _settingsSelection = State(initialValue: initialCategory)
    }
    #endif

    var body: some View {
        #if os(tvOS)
        // Native tv pattern: no inline text entry (typing numbers with a remote is
        // miserable and the inline field chrome fights the focus system). Modes are
        // preset pickers that push selection lists like the system Settings app.
        tvBody
        #elseif os(macOS)
        macBody
        #else
        iosBody
        #endif
    }

    // MARK: - macOS: tabbed preferences

    #if os(macOS)
    private var macBody: some View {
        TabView {
            Form {
                streamModeSection
                compositorSection
            }
            .formStyle(.grouped)
            .tabItem { Label("General", systemImage: "gearshape") }

            Form {
                presenterSection
                hdrSection
                windowSection
                statisticsSection
            }
            .formStyle(.grouped)
            .tabItem { Label("Display", systemImage: "display") }

            Form {
                audioSection
            }
            .formStyle(.grouped)
            .onAppear {
                outputDevices = AudioDevices.outputs()
                inputDevices = AudioDevices.inputs()
            }
            .tabItem { Label("Audio", systemImage: "speaker.wave.2") }

            Form {
                controllersSection
            }
            .formStyle(.grouped)
            .onAppear {
                gamepads.refresh()
                gamepads.startDiscovery()
            }
            .onDisappear { gamepads.stopDiscovery() }
            .tabItem { Label("Controllers", systemImage: "gamecontroller") }

            Form {
                experimentalSection
            }
            .formStyle(.grouped)
            .tabItem { Label("Advanced", systemImage: "slider.horizontal.3") }

            AcknowledgementsView()
                .tabItem { Label("About", systemImage: "info.circle") }
        }
        .frame(width: 480, height: 460)
    }
    #endif

    // MARK: - iOS / iPadOS: adaptive split view

    #if os(iOS)
    private var iosBody: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            List(selection: $settingsSelection) {
                ForEach(SettingsCategory.allCases) { category in
                    // On iPhone the split view collapses to a push list, but a selection List
                    // draws no disclosure indicator of its own — add one in compact width for the
                    // expected drill-in affordance. On iPad the selected row highlights instead, so
                    // the chevron is omitted there.
                    HStack {
                        Label(category.title, systemImage: category.symbol)
                        if horizontalSizeClass == .compact {
                            Spacer()
                            Image(systemName: "chevron.forward")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(.tertiary)
                                // Purely a drill-in affordance — the row's button trait already
                                // conveys "opens"; keep it out of the VoiceOver announcement.
                                .accessibilityHidden(true)
                        }
                    }
                    .tag(category)
                }
            }
            .navigationTitle("Settings")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        } detail: {
            // NavigationSplitView hosts the detail in its own navigation context (its title bar),
            // so no inner NavigationStack — that would double the bar on iPad. On iPhone the split
            // view collapses to one stack and pushes this when a row is tapped. `?? .general` only
            // backs the brief pre-selection window; the list never auto-pushes on a nil selection.
            settingsDetail(settingsSelection ?? .general)
                // Keep a Done on the detail whenever the sidebar (and its Done) isn't on screen: the
                // iPhone push, or any iPad layout that collapsed the sidebar to an overlay. When the
                // sidebar is showing, its Done is the only one — so this stays hidden to avoid two.
                .toolbar {
                    if horizontalSizeClass == .compact || columnVisibility == .detailOnly {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { dismiss() }
                        }
                    }
                }
        }
        .onAppear {
            if horizontalSizeClass == .regular, settingsSelection == nil {
                settingsSelection = .general
            }
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        // A regular→regular launch sets the default above; this catches a compact→regular change
        // (e.g. an iPad leaving narrow split-screen multitasking) so the detail pane fills in.
        .onChange(of: horizontalSizeClass) { _, newValue in
            if newValue == .regular, settingsSelection == nil {
                settingsSelection = .general
            }
        }
        .onDisappear { gamepads.stopDiscovery() }
    }

    @ViewBuilder
    private func settingsDetail(_ category: SettingsCategory) -> some View {
        switch category {
        case .general:
            Form {
                streamModeSection
                pointerSection
                compositorSection
            }
            .formStyle(.grouped)
            .navigationTitle("General")
            .navigationBarTitleDisplayMode(.inline)
        case .display:
            Form {
                presenterSection
                hdrSection
                statisticsSection
            }
            .formStyle(.grouped)
            .navigationTitle("Display")
            .navigationBarTitleDisplayMode(.inline)
        case .audio:
            Form { audioSection }
                .formStyle(.grouped)
                .navigationTitle("Audio")
                .navigationBarTitleDisplayMode(.inline)
        case .controllers:
            Form { controllersSection }
                .formStyle(.grouped)
                .navigationTitle("Controllers")
                .navigationBarTitleDisplayMode(.inline)
        case .advanced:
            Form { experimentalSection }
                .formStyle(.grouped)
                .navigationTitle("Advanced")
                .navigationBarTitleDisplayMode(.inline)
        case .about:
            // Already a full scrollable view that sets its own "Acknowledgements" title; pin the
            // display mode inline to match the five sibling detail pages (it would otherwise inherit
            // the large title from the "Settings" sidebar root).
            AcknowledgementsView()
                .navigationBarTitleDisplayMode(.inline)
        }
    }
    #endif

    // MARK: - tvOS

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

    private var hudEnabledTag: Binding<String> {
        Binding(get: { hudEnabled ? "on" : "off" }, set: { hudEnabled = $0 == "on" })
    }

    private var hdrEnabledTag: Binding<String> {
        Binding(get: { hdrEnabled ? "on" : "off" }, set: { hdrEnabled = $0 == "on" })
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
                TVSelectionRow(
                    title: "Audio channels",
                    options: [("Stereo", 2), ("5.1 Surround", 6), ("7.1 Surround", 8)],
                    selection: $audioChannels)
                if bitrateKbps > 1_000_000 {
                    Label(Self.gigabitWarning, systemImage: "exclamationmark.triangle.fill")
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.orange)
                        .multilineTextAlignment(.center)
                }
                TVSelectionRow(
                    title: "Compositor", options: compositors, selection: $compositor)
                #if DEBUG
                TVSelectionRow(
                    title: "Presenter (debug)",
                    options: [("Stage 2 (default)", "stage2"), ("Stage 1 (debug)", "stage1")],
                    selection: $presenter)
                #endif
                TVSelectionRow(
                    title: "10-bit HDR",
                    options: [("On", "on"), ("Off", "off")], selection: hdrEnabledTag)
                Text("The host creates a virtual output at exactly this mode — native "
                    + "resolution, no scaling. \(Self.bitrateFooter) A specific compositor "
                    + "is honored only if available on the host.")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
                TVSelectionRow(
                    title: "Statistics overlay",
                    options: [("On", "on"), ("Off", "off")], selection: hudEnabledTag)
                TVSelectionRow(
                    title: "Statistics position", options: Self.placementOptions,
                    selection: $hudPlacement)
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
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
                NavigationLink("Acknowledgements") { AcknowledgementsView() }
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

    // MARK: - Sections (shared)

    @ViewBuilder private var streamModeSection: some View {
        Section {
            #if os(iOS)
            // Touch-first: a rotating wheel of common resolutions (this device's own mode first) and
            // a segmented refresh-rate control — the same family as the Clock/Timer pickers. The host
            // renders a virtual output at exactly the chosen mode, so these are real pixel sizes. The
            // last wheel row, "Custom…", reveals width/height/refresh fields for an arbitrary mode.
            VStack(alignment: .leading, spacing: 4) {
                Text("Resolution")
                    .font(.geist(15, relativeTo: .subheadline))
                    .foregroundStyle(.secondary)
                Picker("Resolution", selection: resolutionSelection) {
                    ForEach(resolutionChoices, id: \.tag) { choice in
                        Text(choice.label).tag(choice.tag)
                    }
                }
                .labelsHidden()
                .pickerStyle(.wheel)
                .frame(maxHeight: 140)
            }
            if isCustomResolution {
                // Arbitrary entry: type the exact width × height (and refresh) the host should drive.
                HStack {
                    TextField("Width", value: $width, format: .number.grouping(.never))
                        .keyboardType(.numberPad)
                    Text("×")
                    TextField("Height", value: $height, format: .number.grouping(.never))
                        .labelsHidden()
                        .keyboardType(.numberPad)
                }
                // A row built from an HStack of TextFields otherwise insets its bottom separator to
                // the inner content, clipping the hairline under "Width"; pin it to the cell edge.
                .alignmentGuide(.listRowSeparatorLeading) { _ in 0 }
                LabeledContent("Refresh rate") {
                    TextField("Hz", value: $hz, format: .number.grouping(.never))
                        .keyboardType(.numberPad)
                        .multilineTextAlignment(.trailing)
                }
            } else if refreshChoices.count > 1 {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Refresh rate")
                        .font(.geist(15, relativeTo: .subheadline))
                        .foregroundStyle(.secondary)
                    Picker("Refresh rate", selection: $hz) {
                        ForEach(refreshChoices, id: \.self) { rate in
                            Text("\(rate) Hz").tag(rate)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                }
            } else {
                // A device with a single supported rate (e.g. 60 Hz) has nothing to pick.
                LabeledContent("Refresh rate") {
                    Text("\(hz) Hz").foregroundStyle(.secondary)
                }
            }
            Button("Use this display's mode") { fillFromMainScreen() }
            #elseif os(macOS)
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
            #endif
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
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.orange)
                }
            }
            #endif
        } header: {
            Text("Stream mode")
        } footer: {
            Text("The host creates a virtual output at exactly this mode — "
                + "native resolution, no scaling. \(Self.bitrateFooter)")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    #if os(iOS)
    // MARK: - Stream mode (iOS wheel)

    /// Sentinel wheel tag for the "Custom…" row. Real tags are "WxH" (digits + "x"), so this can't
    /// collide with a resolution.
    private static let customResolutionTag = "custom"

    /// 16:9 then ultrawide presets; the device's native mode is prepended at runtime.
    private static let resolutionPresets: [(name: String, w: Int, h: Int)] = [
        ("720p", 1280, 720),
        ("1080p", 1920, 1080),
        ("1440p", 2560, 1440),
        ("4K", 3840, 2160),
        ("Ultrawide 1080p", 2560, 1080),
        ("Ultrawide 1440p", 3440, 1440),
        ("Super ultrawide", 5120, 1440),
    ]

    /// The non-custom wheel rows: this device's native mode first, then the presets, deduped by
    /// dimensions (native wins a tie).
    private var resolutionModes: [(name: String, w: Int, h: Int)] {
        let bounds = UIScreen.main.nativeBounds // portrait-oriented pixels
        let native = (w: Int(max(bounds.width, bounds.height)), h: Int(min(bounds.width, bounds.height)))
        let all = [(name: "This device", w: native.w, h: native.h)] + Self.resolutionPresets
        var seen = Set<String>()
        return all.filter { seen.insert("\($0.w)x\($0.h)").inserted }
    }

    /// Wheel rows: the resolution modes, then a "Custom…" row that reveals the numeric fields.
    private var resolutionChoices: [(label: String, tag: String)] {
        resolutionModes.map { (label: "\($0.name)  ·  \($0.w) × \($0.h)", tag: "\($0.w)x\($0.h)") }
            + [(label: "Custom…", tag: Self.customResolutionTag)]
    }

    private var presetResolutionTags: Set<String> {
        Set(resolutionModes.map { "\($0.w)x\($0.h)" })
    }

    /// True when the editable custom fields should show: the wheel is parked on "Custom…" (sticky),
    /// or the stored size simply isn't one of the presets (e.g. a value synced from a Mac) — so a
    /// non-preset mode stays editable across relaunches without a persisted flag.
    private var isCustomResolution: Bool {
        customMode || !presetResolutionTags.contains("\(width)x\(height)")
    }

    /// The wheel works in "WxH" tags so one selection drives both width and height; the custom
    /// sentinel toggles `customMode` instead of writing a size.
    private var resolutionSelection: Binding<String> {
        Binding(
            get: { isCustomResolution ? Self.customResolutionTag : "\(width)x\(height)" },
            set: { tag in
                if tag == Self.customResolutionTag {
                    customMode = true
                    return
                }
                customMode = false
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 2 else { return }
                width = parts[0]
                height = parts[1]
            })
    }

    /// Refresh rates the device can actually display (no point asking the host to render frames the
    /// screen can't show), plus any stored custom value so it stays selectable.
    private var refreshChoices: [Int] {
        let maxHz = UIScreen.main.maximumFramesPerSecond
        var rates = [60, 120, 240].filter { $0 <= maxHz }
        if rates.isEmpty { rates = [maxHz] }
        if !rates.contains(hz) { rates.append(hz) }
        return rates.sorted()
    }
    #endif

    @ViewBuilder private var audioSection: some View {
        Section {
            Picker("Audio channels", selection: $audioChannels) {
                Text("Stereo").tag(2)
                Text("5.1 Surround").tag(6)
                Text("7.1 Surround").tag(8)
            }
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
            Toggle("Send microphone to the host", isOn: $micEnabled)
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
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    #if os(iOS)
    /// iPad-only pointer-capture toggle: lock the mouse/trackpad for relative movement (games) vs
    /// forward an absolute cursor position (desktop). Empty on iPhone (no hardware-pointer lock —
    /// the mouse path there is always the absolute fallback).
    @ViewBuilder private var pointerSection: some View {
        if UIDevice.current.userInterfaceIdiom == .pad {
            Section {
                Toggle("Capture pointer for games", isOn: $pointerCapture)
            } header: {
                Text("Pointer")
            } footer: {
                Text("With a mouse or trackpad connected, lock the pointer and send relative "
                    + "movement — the expected behavior for games (mouse-look). Turn this off for "
                    + "desktop use to keep the pointer free and send its absolute position instead. "
                    + "The lock needs the stream full-screen and frontmost; it falls back to the "
                    + "absolute pointer automatically (Stage Manager, Slide Over). Finger touch is "
                    + "unaffected. Applies from the next session.")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
        }
    }
    #endif

    @ViewBuilder private var compositorSection: some View {
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
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var windowSection: some View {
        #if os(macOS)
        Section {
            Toggle("Fullscreen while streaming", isOn: $fullscreenWhileStreaming)
        } header: {
            Text("Window")
        } footer: {
            Text("Take the window fullscreen when a session starts and restore it on the host "
                + "list, so only the stream is fullscreen — not the picker.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
        #endif
    }

    // Stage-2 (Metal/VTDecompressionSession) is the default and only user-visible presenter — it
    // recovers from a wedged decoder, where stage-1's AVSampleBufferDisplayLayer freezes hard on a
    // lost HEVC reference. Stage-1 is kept reachable as a DEBUG-only override for diagnostics, like
    // the controller test. Empty in release builds (no presenter UI; stage-2 always).
    @ViewBuilder private var presenterSection: some View {
        #if DEBUG
        Section {
            Picker("Presenter", selection: $presenter) {
                Text("Stage 2 (default)").tag("stage2")
                Text("Stage 1 (debug)").tag("stage1")
            }
        } header: {
            Text("Video presenter · debug")
        } footer: {
            Text("Stage 2 (default) decodes explicitly and presents through Metal with a display "
                + "link — it adds a capture→present (glass-to-glass) latency line in the HUD and "
                + "self-recovers from decode stalls. Stage 1 feeds compressed video straight to the "
                + "system display layer; it freezes on a lost HEVC reference frame, so it's a debug "
                + "fallback only. Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
        #endif
    }

    @ViewBuilder private var hdrSection: some View {
        Section {
            Picker("Video codec", selection: $codec) {
                Text("Automatic").tag("auto")
                Text("HEVC (H.265)").tag("hevc")
                Text("H.264 (AVC)").tag("h264")
            }
            Toggle("10-bit HDR", isOn: $hdrEnabled)
            Toggle("Full chroma (4:4:4)", isOn: $enable444)
        } header: {
            Text("Video quality")
        } footer: {
            Text("Codec is a preference — the host falls back if it can't encode the one you pick "
                + "(and 10-bit/4:4:4 are HEVC-only). HDR requests a 10-bit BT.2020 PQ (HDR10) stream — "
                + "it only engages when the host is sending HDR content AND this display supports HDR. "
                + "4:4:4 requests full chroma (sharper text/UI, more bandwidth) — it only engages when "
                + "this device can hardware-decode it AND the host opted in. Otherwise the stream stays "
                + "8-bit 4:2:0 SDR. Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var statisticsSection: some View {
        Section {
            Toggle("Show statistics overlay", isOn: $hudEnabled)
            Picker("Position", selection: $hudPlacement) {
                ForEach(HUDPlacement.allCases) { placement in
                    Text(placement.label).tag(placement.rawValue)
                }
            }
            .disabled(!hudEnabled)
        } header: {
            Text("Statistics")
        } footer: {
            Text(Self.statisticsFooter)
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var experimentalSection: some View {
        Section {
            Toggle("Show game library", isOn: $libraryEnabled)
        } header: {
            Text("Experimental")
        } footer: {
            Text("Adds a “Browse Library…” action to each host that lists its games "
                + "(Steam + custom) via the host's management API; tap a title to launch it. "
                + "Works once you've paired with the host — the library is authorized by this "
                + "device's certificate, with no extra host setup.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private var controllersSection: some View {
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
            #if os(iOS)
            Toggle("Gamepad-optimized browsing", isOn: $gamepadUIEnabled)
            #endif
            #if DEBUG && !os(tvOS)
            Button("Test Controller…") { showControllerTest = true }
                .disabled(gamepads.active == nil)
                .sheet(isPresented: $showControllerTest) { ControllerTestView() }
            #endif
        } header: {
            Text("Controllers")
        } footer: {
            // The iOS-only gamepad-UI blurb is appended here, not merged into the shared
            // `controllersFooter` constant — tvOS's `tvBody` reuses that exact string (line ~348)
            // for its own footer and has no such toggle to describe.
            VStack(alignment: .leading, spacing: 6) {
                Text(Self.controllersFooter)
                #if os(iOS)
                Text(Self.gamepadUIFooter)
                #endif
            }
            .font(.geist(12, relativeTo: .caption))
            .foregroundStyle(.secondary)
        }
    }

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

    private static let placementOptions: [(label: String, tag: String)] =
        HUDPlacement.allCases.map { ($0.label, $0.rawValue) }
    #endif

    // MARK: - Statistics

    private static var statisticsFooter: String {
        let base = "The overlay shows resolution, frame rate, throughput and latency while "
            + "streaming, in the chosen corner."
        #if os(macOS) || os(iOS)
        return base + " Toggle it any time with ⌘⇧S."
        #else
        return base
        #endif
    }

    // MARK: - Controllers

    private static let padTypes: [(label: String, tag: Int)] = [
        ("Automatic", 0),
        ("Xbox 360", 1),
        ("Xbox One", 3),
        ("DualSense", 2),
        ("DualShock 4", 4),
    ]

    private static let controllersFooter =
        "One controller is forwarded to the host, as player 1 — Automatic picks the most "
        + "recently connected one. The type is the virtual pad the host creates: Automatic "
        + "matches the controller (a DualSense gets adaptive triggers, lightbar, touchpad "
        + "and motion; a DualShock 4 the same minus adaptive triggers), and changes apply "
        + "from the next session. Two identical controllers may swap a manual selection "
        + "after reconnecting."

    #if os(iOS)
    private static let gamepadUIFooter =
        "When a controller is connected, the host list and game library switch to a "
        + "controller-friendly layout — larger focus targets and a swipeable cover browser "
        + "for the library. Turn this off to always use the touch layout. (The system may "
        + "still move basic focus with a controller connected even with this off — that's "
        + "outside the app's control.)"
    #endif

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
        #if os(iOS)
        // The native mode is the "This device" wheel row, so leave Custom mode if it was on.
        customMode = false
        #endif
        #endif
    }
}

extension Double {
    /// The log-scale slider mapping needs a bounded input (Automatic stores 0).
    fileprivate func clamped(_ lo: Double, _ hi: Double) -> Double {
        Swift.min(Swift.max(self, lo), hi)
    }
}

#if os(iOS)
/// The settings groups, mirroring the macOS preference tabs. On iPad each is a sidebar row that
/// drives the detail pane; on iPhone the same list collapses to pushed sub-pages. Internal (not
/// private) so the screenshot harness can open SettingsView on a specific category.
enum SettingsCategory: String, CaseIterable, Identifiable {
    case general, display, audio, controllers, advanced, about

    var id: Self { self }

    var title: String {
        switch self {
        case .general: return "General"
        case .display: return "Display"
        case .audio: return "Audio"
        case .controllers: return "Controllers"
        case .advanced: return "Advanced"
        case .about: return "About"
        }
    }

    var symbol: String {
        switch self {
        case .general: return "gearshape"
        case .display: return "display"
        case .audio: return "speaker.wave.2"
        case .controllers: return "gamecontroller"
        case .advanced: return "slider.horizontal.3"
        case .about: return "info.circle"
        }
    }
}

extension View {
    /// Present the settings sheet large on iPad so the NavigationSplitView has room for its
    /// sidebar + detail — a default form sheet is too narrow and the split view would collapse to
    /// the iPhone push list. No-op on iPhone (the standard sheet is already right) and on iOS 17
    /// (no `presentationSizing` — it falls back to the default sheet, which still degrades cleanly
    /// to the push list).
    @ViewBuilder
    func settingsSheetSizing() -> some View {
        if UIDevice.current.userInterfaceIdiom == .pad, #available(iOS 18, *) {
            presentationSizing(.page)
        } else {
            self
        }
    }
}
#endif
