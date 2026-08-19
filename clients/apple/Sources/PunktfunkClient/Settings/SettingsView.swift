// App settings. The host creates a virtual output at exactly the chosen size/refresh; the only
// deliberate resample is the opt-in Render Scale (the host renders at size × scale and this device
// downscales — supersampling for sharpness, or under-rendering for a lighter host/link).
//
// Navigation differs per platform, but all three follow the same category map (General =
// session/app behavior, Display = everything about the picture, Input, Audio, Controllers,
// About — see SettingsCategory): macOS uses a tabbed preferences window; iOS/iPadOS uses an
// adaptive NavigationSplitView — a category sidebar + detail pane on iPad, auto-collapsing to
// a hierarchical push list on iPhone (the system Settings idiom on each); tvOS uses a
// focus-native pushed-picker layout in the same order. The individual sections
// (`resolutionSection`, `audioSection`, …) are shared across all three so a setting is defined
// exactly once — they live in SettingsView+Sections.swift, with their helpers (including the
// per-field `described` caption idiom) in SettingsView+Support.swift.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    // Which LAYER this surface is editing (SettingsView+Scope): the global defaults, or one
    // profile's overrides. tvOS keeps defaults-only in v1 — controller-first surfaces honor
    // profiles and render pinned cards, but don't edit them (design §5.4).
    @ObservedObject var profiles = ProfileStore.shared
    @State var scope: SettingsScope = .defaults
    /// The profile editor (create / duplicate / edit), when it is open, and the profile a delete
    /// is being confirmed for.
    @State var profileDraft: ProfileDraft?
    @State var profilePendingDelete: StreamProfile?
    /// The menu's icons are rasterised per appearance (see `MenuIcon`), so the surface has to know
    /// which one it is drawing in.
    @Environment(\.colorScheme) var colorScheme
    #if os(macOS)
    @State private var macTab: MacTab = .general
    #endif
    @AppStorage(DefaultsKey.streamWidth) var width = 1920
    @AppStorage(DefaultsKey.streamHeight) var height = 1080
    @AppStorage(DefaultsKey.streamHz) var hz = 60
    // Opt-in (default OFF): the explicit mode below is used and never auto-resized. When ON, a
    // windowed session instead streams at the window's native pixels (1:1, no scaling) so it stays
    // pixel-exact rather than the presenter resampling a fixed-mode frame into the window.
    @AppStorage(DefaultsKey.matchWindow) var matchWindow = false
    // Render-resolution multiplier: the host renders/encodes at chosen-resolution × this, and the
    // presenter downscales (> 1 = supersampling for sharpness) or upscales (< 1 = a lighter host /
    // link). 1.0 = Native (the prior behaviour).
    @AppStorage(DefaultsKey.renderScale) var renderScale = 1.0
    @AppStorage(DefaultsKey.compositor) var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) var gamepadType = 0
    @AppStorage(DefaultsKey.gamepadForwarding) var gamepadForwarding = true
    @AppStorage(DefaultsKey.bitrateKbps) var bitrateKbps = 0
    @AppStorage(DefaultsKey.presentPriority) var presentPriority =
        SettingsOptions.presentPriorityDefault
    @AppStorage(DefaultsKey.smoothBuffer) var smoothBuffer = 0
    #if os(macOS)
    @AppStorage(DefaultsKey.vsync) var vsync = false
    @AppStorage(DefaultsKey.windowedSafePresent) var windowedSafePresent = true
    #endif
    #if !os(tvOS)
    @AppStorage(DefaultsKey.allowVRR) var allowVRR = true
    #endif
    @AppStorage(DefaultsKey.hdrEnabled) var hdrEnabled = true
    @AppStorage(DefaultsKey.enable444) var enable444 = false
    @AppStorage(DefaultsKey.libraryEnabled) var libraryEnabled = true
    /// The gamepad library's arrangement and its collections-first switch — device preferences,
    /// stored as the cross-client `library_view` / `library_collections` values.
    @AppStorage(DefaultsKey.libraryView) var libraryViewRaw = LibraryArrangement.shelf.stored
    @AppStorage(DefaultsKey.libraryCollections) var libraryCollections = false
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.micEnabled) var micEnabled = true
    @AppStorage(DefaultsKey.echoCancel) var echoCancel = true
    @AppStorage(DefaultsKey.audioChannels) var audioChannels = 2
    @AppStorage(DefaultsKey.audioFormat) var audioFormat = AudioFormatChoice.opus.rawValue
    @AppStorage(DefaultsKey.codec) var codec = "auto"
    // The overlay tier's raw string (the pickers tag by rawValue); the absent-key default runs
    // the legacy-hudEnabled migration (same pattern as ContentView/StreamCommands).
    @AppStorage(DefaultsKey.statsVerbosity) var statsVerbosityRaw = StatsVerbosity.current.rawValue
    @AppStorage(DefaultsKey.hudPlacement) var hudPlacement = HUDPlacement.topTrailing.rawValue
    @ObservedObject var gamepads = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) var gamepadUIEnabled = true
    /// When the switch above takes over — read (and shown) only while it is on.
    @AppStorage(DefaultsKey.gamepadUIMode) var gamepadUIMode =
        GamepadUIEnvironment.modeWhenConnected
    /// The gamepad UI's background palette. Edited here on tvOS only (see `tvBody`) — every other
    /// platform reaches it through the gamepad settings screen, which an Apple TV without a
    /// controller cannot open.
    @AppStorage(DefaultsKey.uiPalette) var uiPalette = "violet"
    @AppStorage(DefaultsKey.autoWake) var autoWakeEnabled = true
    @AppStorage(DefaultsKey.backgroundKeepAlive) var backgroundKeepAlive = false
    @AppStorage(DefaultsKey.backgroundTimeoutMinutes) var backgroundTimeoutMinutes = 10
    #if !os(tvOS)
    // Keyboard & mouse forwarding (macOS + a hardware keyboard/mouse on iPad). Invert-scroll flips
    // both wheel axes; modifier-layout relocates the ⌥/⌘ → Alt/Super roles by physical position.
    @AppStorage(DefaultsKey.invertScroll) var invertScroll = false
    @AppStorage(DefaultsKey.modifierLayout) var modifierLayout = ModifierLayout.mac.rawValue
    #endif
    #if DEBUG && !os(tvOS)
    @State var showControllerTest = false
    #endif
    #if os(iOS)
    @AppStorage(DefaultsKey.pointerCapture) var pointerCapture = true
    @AppStorage(DefaultsKey.touchMode) var touchMode = TouchInputMode.trackpad.rawValue
    @AppStorage(DefaultsKey.rumbleOnDevice) var rumbleOnDevice = false
    @AppStorage(DefaultsKey.gyroFromDevice) var gyroFromDevice = false
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
    @AppStorage(DefaultsKey.mouseMode) var mouseMode = MouseInputMode.capture.rawValue
    /// Cross-client `inhibit_shortcuts` — here, the ⌘-chord passthrough (⌘Q & co. reach the host
    /// instead of the app menu while captured). macOS-only: it is the one platform whose window
    /// system hands a plain app no keyboard grab, so the client has to claim the chords itself.
    @AppStorage(DefaultsKey.inhibitShortcuts) var inhibitShortcuts = true
    /// Accessibility granted? Gates the system-shortcut half of `inhibit_shortcuts` (⌘Space, ⌘Tab…
    /// need the event tap). Re-read whenever the app comes back to the front — that is when the
    /// user returns from flipping the switch in System Settings.
    @State var accessibilityTrusted = InputCapture.systemShortcutsAvailable
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
    /// The preferences tabs, tagged so the scope control can sit out the one page that isn't a
    /// settings layer at all.
    private enum MacTab: Hashable {
        case general, display, input, audio, controllers, about
    }

    private var macBody: some View {
        // The scope control heads the window — above the tabs, because it is about which layer
        // every tab is editing, not about any one of them. About is the exception: it edits
        // nothing, and directly above the acknowledgements the switcher read as belonging to
        // them.
        VStack(alignment: .leading, spacing: 0) {
            if macTab != .about {
                scopeSwitcher
                    .padding(.horizontal, 20)
                    .padding(.top, 14)
                    .padding(.bottom, 10)
            }
            macTabs
        }
        .frame(width: 500, height: 580)
    }

    private var macTabs: some View {
        // Tab map mirrors SettingsCategory: General = session/app behavior, Display = the whole
        // picture (resolution lives here), Input = keyboard & mouse.
        TabView(selection: $macTab) {
            Form {
                sessionSection
                overlaySection
                librarySection
            }
            .formStyle(.grouped)
            .tabItem { Label("General", systemImage: "gearshape") }
            .tag(MacTab.general)

            Form {
                resolutionSection
                qualitySection
                presentationSection
                hostOutputSection
            }
            .formStyle(.grouped)
            .tabItem { Label("Display", systemImage: "display") }
            .tag(MacTab.display)

            Form {
                inputSection
            }
            .formStyle(.grouped)
            .tabItem { Label("Input", systemImage: "keyboard") }
            .tag(MacTab.input)

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
            .tag(MacTab.audio)

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
            .tag(MacTab.controllers)

            AboutView()
                .tabItem { Label("About", systemImage: "info.circle") }
                .tag(MacTab.about)
        }
    }
    #endif

    // MARK: - iOS / iPadOS: adaptive split view

    #if os(iOS)
    private var iosBody: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            List(selection: $settingsSelection) {
                // The scope control heads the category list: on iPhone this is the screen you
                // start on, and on iPad it stays visible beside whichever category is open — so
                // the layer being edited is never off screen while you edit it. The caption is
                // the section's FOOTER; in the row it wrapped to four lines.
                Section {
                    scopeRow
                } footer: {
                    // A List footer inherits the app's body font unless it's told otherwise, and
                    // 17pt Geist next to the 12pt footers everywhere else read as a different
                    // kind of text. Same style as every other footer in this surface.
                    Text(scopeCaption)
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                }
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
                sessionSection
                overlaySection
                librarySection
            }
            .formStyle(.grouped)
            .navigationTitle("General")
            .navigationBarTitleDisplayMode(.inline)
        case .display:
            Form {
                resolutionSection
                qualitySection
                presentationSection
                hostOutputSection
            }
            .formStyle(.grouped)
            .navigationTitle("Display")
            .navigationBarTitleDisplayMode(.inline)
        case .input:
            Form {
                pointerSection
                inputSection
            }
            .formStyle(.grouped)
            .navigationTitle("Input")
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
        case .about:
            // The identity card; the license wall is one push further in. Inline title to match
            // the five sibling detail pages (it would otherwise inherit the large title from the
            // "Settings" sidebar root).
            AboutView()
                .navigationTitle("About")
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

    /// One cluster caption, TV-legible — the 10-foot analogue of the touch/desktop per-row
    /// `described` captions (per-row text doesn't scale to TV type sizes).
    private func tvCaption(_ text: String) -> some View {
        Text(text)
            .font(.geist(20, relativeTo: .caption))
            .foregroundStyle(.secondary)
            .multilineTextAlignment(.center)
            .padding(.top, 8)
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
        // Row order mirrors the touch/desktop category map: Display (mode → quality →
        // presentation → host output), then Audio, General, Statistics, Controllers — with one
        // short caption per cluster (per-row captions don't scale to 10-foot type sizes).
        return ScrollView {
            VStack(spacing: 16) {
                TVSelectionRow(title: "Stream mode", options: options, selection: modeTag)
                TVSelectionRow(
                    title: "Render scale",
                    options: RenderScale.presets.map { (label: RenderScale.label($0), tag: $0) },
                    selection: $renderScale)
                TVSelectionRow(
                    title: "Bitrate",
                    options: SettingsOptions.bitrateOptions(current: bitrateKbps),
                    selection: $bitrateKbps)
                if bitrateKbps > 1_000_000 {
                    Label(Self.gigabitWarning, systemImage: "exclamationmark.triangle.fill")
                        .font(.geist(20, relativeTo: .caption)) // TV-legible caption size
                        .foregroundStyle(.orange)
                        .multilineTextAlignment(.center)
                }
                TVSelectionRow(
                    title: "10-bit HDR",
                    options: [("On", "on"), ("Off", "off")], selection: hdrEnabledTag)
                TVSelectionRow(
                    title: "Prioritize",
                    options: SettingsOptions.presentPriorities,
                    selection: $presentPriority)
                if presentPriority == "smooth" {
                    TVSelectionRow(
                        title: "Smoothness buffer",
                        options: SettingsOptions.smoothBuffers(refreshHz: hz),
                        selection: $smoothBuffer)
                }
                TVSelectionRow(
                    title: "Compositor", options: SettingsOptions.compositors,
                    selection: $compositor)
                tvCaption("The host drives a real output at exactly the chosen mode. "
                    + "\(Self.bitrateFooter) Lowest latency shows frames immediately; "
                    + "Smoothness buffers a few. A compositor is honored only if available. "
                    + "Applies from the next session.")
                TVSelectionRow(
                    title: "Audio channels",
                    options: SettingsOptions.audioChannels,
                    selection: $audioChannels)
                // Offered at every channel count — the lossless plane is no longer stereo-only,
                // because the frame ladder is sized per channel count and surround simply
                // negotiates a shorter frame (see SettingsOptions.audioFormats).
                TVSelectionRow(
                    title: "Audio quality",
                    options: SettingsOptions.audioFormats,
                    selection: $audioFormat)
                tvCaption("Lossless sends bit-exact PCM — 2.3 Mbps at 48 kHz, up to 8.5 at "
                    + "176.4. Falls back to Standard if the host or this TV declines it.")
                TVSelectionRow(
                    title: "Auto-wake on connect",
                    options: [("On", "on"), ("Off", "off")], selection: autoWakeEnabledTag)
                tvCaption("Sends Wake-on-LAN to a sleeping saved host and waits for it.")
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
                // Hidden while the switch above is off — see the touch settings' identical gate.
                if gamepadUIEnabled {
                    TVSelectionRow(
                        title: "Show it",
                        options: SettingsOptions.gamepadUIModes, selection: $gamepadUIMode)
                    // The Apple TV's ONLY route to the shared `ui_palette`. Everywhere else the
                    // Background row lives on the gamepad settings screen, which is reached from
                    // the gamepad launcher — and on tvOS that launcher needs an extended-profile
                    // controller, so an Apple TV driven by the Siri Remote alone could not reach
                    // the palettes at all. It belongs beside "Show it" because both describe the
                    // same interface: this row is what that interface looks like once it is up.
                    TVSelectionRow(
                        title: "Background",
                        options: GamepadPalette.all.map { (label: $0.name, tag: $0.id) },
                        selection: $uiPalette)
                }
                tvCaption(Self.controllersFooter)
                NavigationLink("About") { AboutView() }
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
