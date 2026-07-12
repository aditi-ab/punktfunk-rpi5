// App settings. The host creates a native virtual output at exactly the chosen size/refresh —
// there is no scaling anywhere in the pipeline.
//
// Navigation differs per platform, but all three group the same categories (General, Display,
// Audio, Controllers, Advanced, About): macOS uses a tabbed preferences window; iOS/iPadOS uses
// an adaptive NavigationSplitView — a category sidebar + detail pane on iPad, auto-collapsing to
// a hierarchical push list on iPhone (the system Settings idiom on each); tvOS uses a
// focus-native pushed-picker layout. The individual sections (`streamModeSection`,
// `audioSection`, …) are shared across all three so a setting is defined exactly once — they
// live in SettingsView+Sections.swift, with their helpers in SettingsView+Support.swift.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage(DefaultsKey.streamWidth) var width = 1920
    @AppStorage(DefaultsKey.streamHeight) var height = 1080
    @AppStorage(DefaultsKey.streamHz) var hz = 60
    // Opt-in (default OFF): the explicit mode below is used and never auto-resized. When ON, a
    // windowed session instead streams at the window's native pixels (1:1, no scaling) so it stays
    // pixel-exact rather than the presenter resampling a fixed-mode frame into the window.
    @AppStorage(DefaultsKey.matchWindow) var matchWindow = false
    @AppStorage(DefaultsKey.compositor) var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) var gamepadType = 0
    @AppStorage(DefaultsKey.bitrateKbps) var bitrateKbps = 0
    @AppStorage(DefaultsKey.presenter) var presenter = SettingsOptions.presenterDefault
    #if os(macOS)
    @AppStorage(DefaultsKey.vsync) var vsync = false
    #endif
    #if !os(tvOS)
    @AppStorage(DefaultsKey.allowVRR) var allowVRR = true
    #endif
    @AppStorage(DefaultsKey.hdrEnabled) var hdrEnabled = true
    @AppStorage(DefaultsKey.enable444) var enable444 = false
    @AppStorage(DefaultsKey.libraryEnabled) var libraryEnabled = true
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.micEnabled) var micEnabled = true
    @AppStorage(DefaultsKey.audioChannels) var audioChannels = 2
    @AppStorage(DefaultsKey.codec) var codec = "auto"
    // The overlay tier's raw string (the pickers tag by rawValue); the absent-key default runs
    // the legacy-hudEnabled migration (same pattern as ContentView/StreamCommands).
    @AppStorage(DefaultsKey.statsVerbosity) var statsVerbosityRaw = StatsVerbosity.current.rawValue
    @AppStorage(DefaultsKey.hudPlacement) var hudPlacement = HUDPlacement.topTrailing.rawValue
    @ObservedObject var gamepads = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) var gamepadUIEnabled = true
    @AppStorage(DefaultsKey.autoWake) var autoWakeEnabled = true
    #if DEBUG && !os(tvOS)
    @State var showControllerTest = false
    #endif
    #if os(iOS)
    @AppStorage(DefaultsKey.pointerCapture) var pointerCapture = true
    @AppStorage(DefaultsKey.touchMode) var touchMode = TouchInputMode.trackpad.rawValue
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
    @State var customMode = false
    #endif
    #if os(macOS)
    @AppStorage(DefaultsKey.speakerUID) var speakerUID = ""
    @AppStorage(DefaultsKey.micUID) var micUID = ""
    @AppStorage(DefaultsKey.micChannel) var micChannel = 0
    @State var outputDevices: [AudioDevice] = []
    @State var inputDevices: [AudioDevice] = []
    // Input channels of the selected mic — drives the "Microphone channel" picker, which only
    // appears for a multi-channel interface (>1). 0 until the Audio tab loads it.
    @State var micChannelCount = 0
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
                wakeSection
            }
            .formStyle(.grouped)
            .tabItem { Label("General", systemImage: "gearshape") }

            Form {
                presenterSection
                hdrSection
                vrrSection
                vsyncSection
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
                micChannelCount = AudioDevices.inputChannelCount(forUID: micUID)
            }
            .onChange(of: micUID) { _, newUID in
                // A different mic → different channel count; drop a now-out-of-range pin to Auto.
                micChannelCount = AudioDevices.inputChannelCount(forUID: newUID)
                if micChannel > micChannelCount { micChannel = 0 }
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
                wakeSection
            }
            .formStyle(.grouped)
            .navigationTitle("General")
            .navigationBarTitleDisplayMode(.inline)
        case .display:
            Form {
                presenterSection
                hdrSection
                vrrSection
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

    private var hdrEnabledTag: Binding<String> {
        Binding(get: { hdrEnabled ? "on" : "off" }, set: { hdrEnabled = $0 == "on" })
    }

    /// The gamepad-UI switch as an on/off row (same shape as HDR above) — the escape hatch back
    /// to this focus-engine home for someone who prefers it with a controller connected.
    private var gamepadUIEnabledTag: Binding<String> {
        Binding(get: { gamepadUIEnabled ? "on" : "off" }, set: { gamepadUIEnabled = $0 == "on" })
    }

    private var autoWakeEnabledTag: Binding<String> {
        Binding(get: { autoWakeEnabled ? "on" : "off" }, set: { autoWakeEnabled = $0 == "on" })
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
        return ScrollView {
            VStack(spacing: 16) {
                TVSelectionRow(title: "Stream mode", options: options, selection: modeTag)
                TVSelectionRow(
                    title: "Bitrate",
                    options: SettingsOptions.bitrateOptions(current: bitrateKbps),
                    selection: $bitrateKbps)
                TVSelectionRow(
                    title: "Audio channels",
                    options: SettingsOptions.audioChannels,
                    selection: $audioChannels)
                if bitrateKbps > 1_000_000 {
                    Label(Self.gigabitWarning, systemImage: "exclamationmark.triangle.fill")
                        .font(.geist(20, relativeTo: .caption)) // TV-legible caption size
                        .foregroundStyle(.orange)
                        .multilineTextAlignment(.center)
                }
                TVSelectionRow(
                    title: "Compositor", options: SettingsOptions.compositors,
                    selection: $compositor)
                TVSelectionRow(
                    title: "Presenter",
                    options: SettingsOptions.presenters,
                    selection: $presenter)
                TVSelectionRow(
                    title: "10-bit HDR",
                    options: [("On", "on"), ("Off", "off")], selection: hdrEnabledTag)
                TVSelectionRow(
                    title: "Auto-wake on connect",
                    options: [("On", "on"), ("Off", "off")], selection: autoWakeEnabledTag)
                Text("The host creates a virtual output at exactly this mode — native "
                    + "resolution, no scaling. \(Self.bitrateFooter) A specific compositor "
                    + "is honored only if available on the host. Auto-wake sends Wake-on-LAN to a "
                    + "sleeping saved host and waits for it before streaming.")
                    .font(.geist(20, relativeTo: .caption))
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .padding(.top, 8)
                TVSelectionRow(
                    title: "Statistics overlay",
                    options: SettingsOptions.statsVerbosities, selection: $statsVerbosityRaw)
                TVSelectionRow(
                    title: "Statistics position", options: SettingsOptions.hudPlacements,
                    selection: $hudPlacement)
                ForEach(gamepads.controllers) { controller in
                    controllerRow(controller)
                        .padding(.horizontal, 24)
                }
                TVSelectionRow(
                    title: "Use controller", options: controllerOptions,
                    selection: $gamepads.preferredID)
                TVSelectionRow(
                    title: "Controller type", options: SettingsOptions.padTypes,
                    selection: $gamepadType)
                TVSelectionRow(
                    title: "Gamepad-optimized browsing",
                    options: [("On", "on"), ("Off", "off")], selection: gamepadUIEnabledTag)
                Text(Self.controllersFooter)
                    .font(.geist(20, relativeTo: .caption))
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
}
