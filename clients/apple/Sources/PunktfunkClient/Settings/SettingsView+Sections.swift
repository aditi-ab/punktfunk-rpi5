// SettingsView's shared sections — each setting's Section is defined exactly once here and
// composed by the per-platform bodies in SettingsView.swift.
//
// 2026-07 settings revamp: every field carries its explanation DIRECTLY under it in the same
// cell (the `described` helper in SettingsView+Support) — the old per-section footer paragraphs
// collected several fields' explanations into one blob nobody could match back to its row.
// Where a picker's meaning depends on the selection (touch mode, modifier layout, prioritize),
// the description is DYNAMIC — it explains the current choice. The only footers left are the
// one-line "Applies from the next session." form notes.
//
// Category map (SettingsCategory): General = session/app behavior, Display = everything about
// the picture (resolution lives HERE), Input = touch/keyboard/mouse, Audio, Controllers, About.

#if os(iOS)
import CoreHaptics
#endif
import PunktfunkKit
import SwiftUI

extension SettingsView {
    // MARK: - Display: Resolution

    // NOTE: the Section content is deliberately split into the small named builders below — as one
    // inline expression the iOS branch (wheel + 3-way refresh + bitrate rows) blew Swift's
    // type-checker budget ("unable to type-check this expression in reasonable time"), which
    // failed exactly one slice: the iOS archive (macOS/tvOS never compile that branch).
    @ViewBuilder var resolutionSection: some View {
        Section("Resolution") {
            #if os(iOS) || os(macOS)
            // Match-window (design/midstream-resolution-resize.md D1): follow the session
            // window/scene, renegotiating the host mode on a resize. Off → the explicit mode below.
            described(matchWindow
                ? "The host resizes its output to follow this window — the picture stays "
                    + "pixel-exact (1:1) through every resize."
                : "Stream at the fixed mode below; a window at a different size shows it scaled.") {
                Toggle("Match window", isOn: $matchWindow)
            }
            #endif
            #if os(iOS)
            iosResolutionWheel
            iosRefreshRows
            Button("Use this display's mode") { fillFromMainScreen() }
            #elseif os(macOS)
            HStack {
                TextField("Resolution", value: $width, format: .number.grouping(.never))
                Text("×")
                TextField("", value: $height, format: .number.grouping(.never))
                    .labelsHidden()
            }
            described("The host drives a real virtual output at exactly this size and refresh — "
                + "true pixels, no scaling.") {
                TextField("Refresh rate (Hz)", value: $hz, format: .number.grouping(.never))
            }
            LabeledContent("") {
                Button("Use this display's mode") { fillFromMainScreen() }
            }
            #endif
        }
    }

    #if os(iOS)
    // MARK: - Display: Resolution (iOS wheel)

    /// Touch-first: a rotating wheel of common resolutions (this device's own mode first) — the
    /// same family as the Clock/Timer pickers. The host renders a virtual output at exactly the
    /// chosen mode, so these are real pixel sizes. The last wheel row, "Custom…", reveals
    /// width/height/refresh fields for an arbitrary mode (see `iosRefreshRows`).
    @ViewBuilder private var iosResolutionWheel: some View {
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
            Text("The host drives a real output at exactly this mode — true pixels, no scaling.")
                .font(.geist(13, relativeTo: .footnote))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: 360, alignment: .leading) // match the described-row caption cap
        }
    }

    /// Custom W×H(+Hz) fields, a segmented refresh picker, or a static single-rate row.
    @ViewBuilder private var iosRefreshRows: some View {
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
    }

    /// Sentinel wheel tag for the "Custom…" row. Real tags are "WxH" (digits + "x"), so this can't
    /// collide with a resolution.
    private static let customResolutionTag = "custom"

    /// Wheel rows: the resolution modes (device native first — see `SettingsOptions`), then a
    /// "Custom…" row that reveals the numeric fields.
    private var resolutionChoices: [(label: String, tag: String)] {
        SettingsOptions.resolutionModes()
            .map { (label: "\($0.name)  ·  \($0.w) × \($0.h)", tag: "\($0.w)x\($0.h)") }
            + [(label: "Custom…", tag: Self.customResolutionTag)]
    }

    private var presetResolutionTags: Set<String> {
        Set(SettingsOptions.resolutionModes().map { "\($0.w)x\($0.h)" })
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

    /// Refresh rates this device can display, plus any stored custom value (see `SettingsOptions`).
    private var refreshChoices: [Int] {
        SettingsOptions.refreshRates(including: hz)
    }
    #endif

    // MARK: - Display: Quality

    @ViewBuilder var qualitySection: some View {
        Section("Quality") {
            #if !os(tvOS)
            renderScaleRow
            bitrateRows
            #endif
            described("A preference — the host falls back if it can't encode it.") {
                Picker("Video codec", selection: $codec) {
                    ForEach(SettingsOptions.codecs, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
            described("HDR10, when the host has HDR content and this display supports it. "
                + "HEVC only; otherwise the stream stays SDR.") {
                Toggle("10-bit HDR", isOn: $hdrEnabled)
            }
            described("Sharper text and UI for desktop work, at more bandwidth. For games the "
                + "bits are better spent at 4:2:0. HEVC only.") {
                Toggle("Full chroma (4:4:4)", isOn: $enable444)
            }
        }
    }

    #if !os(tvOS)
    /// Render-scale picker + the resulting host resolution. > 1 supersamples (sharper, at more
    /// bandwidth AND client decode); < 1 renders under native (lighter). The presenter resamples the
    /// decoded frame to this display, so the multiplier is where the sharpness/cost trade-off lives.
    @ViewBuilder var renderScaleRow: some View {
        described(renderScaleDescription) {
            Picker("Render scale", selection: $renderScale) {
                ForEach(RenderScale.presets, id: \.self) { scale in
                    Text(RenderScale.label(scale)).tag(scale)
                }
            }
        }
    }

    /// Render scale explained, with the CONCRETE host resolution when it applies — the cost made
    /// legible. Only the explicit mode can show it (match-window derives the base from the live
    /// window, not these fields).
    private var renderScaleDescription: String {
        var text = "Above native supersamples for sharpness; below renders lighter on the host "
            + "and the link."
        if renderScale != 1.0, !matchWindow {
            let mode = RenderScale.apply(
                baseWidth: width, baseHeight: height,
                scale: renderScale,
                maxDimension: RenderScale.maxDimension(codec: codec))
            text += " Host renders \(Int(mode.width))×\(Int(mode.height)); this device scales "
                + "it to your display."
        }
        return text
    }

    /// The automatic-bitrate toggle + manual slider (and the >1 Gbps warning) rows.
    @ViewBuilder private var bitrateRows: some View {
        described("The host's default 20 Mbps, clamped to what it supports. Turn off to set a "
            + "fixed rate — a host card's context menu has a network speed test.") {
            Toggle("Automatic bitrate", isOn: automaticBitrate)
        }
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
    }
    #endif

    // MARK: - Display: Presentation

    // The presentation intent (design/apple-presentation-rebuild.md — replaced the visible
    // stage picker): latency (newest-wins, zero queue) vs smoothness (a small deliberate jitter
    // buffer). The stage ladder survives only as the hidden PUNKTFUNK_PRESENTER debug env lever.
    @ViewBuilder var presentationSection: some View {
        Section("Presentation") {
            described(presentPriority == "smooth"
                ? "A small frame buffer evens out network hiccups, at the buffer's worth of "
                    + "added display latency."
                : "Every frame shows the moment the display can take it — a network hiccup is "
                    + "an occasional repeated or skipped frame.") {
                Picker("Prioritize", selection: $presentPriority) {
                    ForEach(SettingsOptions.presentPriorities, id: \.tag) { option in
                        Text(option.tag == SettingsOptions.presentPriorityDefault
                            ? "\(option.label) (default)" : option.label)
                            .tag(option.tag)
                    }
                }
            }
            if presentPriority == "smooth" {
                described("Frames held back — each absorbs about one refresh of jitter and "
                    + "adds one refresh of delay.") {
                    Picker("Buffer", selection: $smoothBuffer) {
                        ForEach(SettingsOptions.smoothBuffers(refreshHz: hz), id: \.tag) { option in
                            Text(option.label).tag(option.tag)
                        }
                    }
                }
            }
            // Non-tvOS: the Apple TV drives a fixed HDMI mode, so there's no adaptive refresh.
            #if !os(tvOS)
            described("A ProMotion or adaptive-sync display follows the stream's rate — "
                + "smoother motion. No effect on fixed-refresh displays.") {
                Toggle("Allow VRR", isOn: $allowVRR)
            }
            #endif
            // macOS-only: iOS/tvOS layers always present on the display's vsync, so the choice
            // only exists on the Mac (the layer's own sync stays off — see MetalVideoPresenter).
            #if os(macOS)
            described("Flips align to the display's refresh — even pacing, up to one refresh "
                + "of added latency. Off shows frames as soon as they're ready.") {
                Toggle("V-Sync", isOn: $vsync)
            }
            // The DCP swapID-panic mitigation's user handle (see DefaultsKey.windowedSafePresent
            // for the saga). Default ON: turning it off re-arms a WHOLE-MACHINE kernel panic on
            // affected setups, so the caption says so in plain words.
            described(windowedSafePresent
                ? "Windowed streams present in step with the system compositor — avoids a macOS "
                    + "display-driver crash seen on high-refresh displays, at a small latency "
                    + "cost. Fullscreen always uses the fastest path."
                : "Windowed streams use the fastest present path. On some high-refresh setups "
                    + "this can crash macOS itself (kernel panic) — turn back on if your Mac "
                    + "restarts during windowed streaming.") {
                Toggle("Safe windowed presentation", isOn: $windowedSafePresent)
            }
            #endif
        }
    }

    // MARK: - Display: Host output

    @ViewBuilder var hostOutputSection: some View {
        Section {
            described("The backend the host uses for its virtual output. A specific choice "
                + "falls back to auto-detection when that backend isn't available.") {
                Picker("Compositor", selection: $compositor) {
                    ForEach(SettingsOptions.compositors, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
        } header: {
            Text("Host output")
        } footer: {
            // The one form-level note (deliberately not repeated on every row above).
            Text("Display changes apply from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    // MARK: - General: Session

    @ViewBuilder var sessionSection: some View {
        Section("Session") {
            #if os(macOS)
            described("Go fullscreen when a session starts; return to a window on the host "
                + "list.") {
                Toggle("Fullscreen while streaming", isOn: $fullscreenWhileStreaming)
            }
            #endif
            described("Connecting to a saved host that's offline sends Wake-on-LAN and waits "
                + "for it to boot. Turn off if hosts behind a VPN look offline when they "
                + "aren't.") {
                Toggle("Auto-wake on connect", isOn: $autoWakeEnabled)
            }
            #if os(iOS)
            described("Audio and the connection stay live after you switch away; video pauses "
                + "to save power and resumes instantly when you return. Off, backgrounding "
                + "freezes the session.") {
                Toggle("Keep streaming in background", isOn: $backgroundKeepAlive)
            }
            if backgroundKeepAlive {
                described("Ends a backgrounded session so it can't run down the battery.") {
                    Picker("Disconnect after", selection: $backgroundTimeoutMinutes) {
                        Text("1 minute").tag(1)
                        Text("5 minutes").tag(5)
                        Text("10 minutes").tag(10)
                        Text("30 minutes").tag(30)
                    }
                }
            }
            #endif
        }
    }

    // MARK: - General: Statistics overlay

    @ViewBuilder var overlaySection: some View {
        Section("Statistics") {
            described(Self.statisticsDescription) {
                Picker("Statistics overlay", selection: $statsVerbosityRaw) {
                    ForEach(StatsVerbosity.allCases, id: \.rawValue) { tier in
                        Text(tier.label).tag(tier.rawValue)
                    }
                }
            }
            Picker("Position", selection: $hudPlacement) {
                ForEach(HUDPlacement.allCases) { placement in
                    Text(placement.label).tag(placement.rawValue)
                }
            }
            .disabled(statsVerbosityRaw == StatsVerbosity.off.rawValue)
        }
    }

    // MARK: - General: Library

    @ViewBuilder var librarySection: some View {
        Section("Library") {
            described("Adds “Browse Library…” to paired hosts — list their Steam and custom "
                + "games and launch one directly. No extra host setup.") {
                Toggle("Show game library", isOn: $libraryEnabled)
            }
        }
    }

    // MARK: - Input

    #if os(iOS)
    /// Touch-input model (iPhone + iPad) plus the iPad-only pointer-capture toggle: lock the
    /// mouse/trackpad for relative movement (games) vs forward an absolute cursor position.
    @ViewBuilder var pointerSection: some View {
        Section("Touch & pointer") {
            described(touchModeDescription) {
                Picker("Touch input", selection: $touchMode) {
                    Text("Trackpad").tag(TouchInputMode.trackpad.rawValue)
                    Text("Direct pointer").tag(TouchInputMode.pointer.rawValue)
                    Text("Touch passthrough").tag(TouchInputMode.touch.rawValue)
                }
            }
            if UIDevice.current.userInterfaceIdiom == .pad {
                described("Locks a hardware mouse for relative mouse-look in games; off sends "
                    + "absolute positions. Needs the stream fullscreen and frontmost.") {
                    Toggle("Capture pointer for games", isOn: $pointerCapture)
                }
            }
        }
    }

    /// The SELECTED touch mode explained — dynamic, so the caption always describes what the
    /// picker currently does instead of narrating all three modes at once.
    private var touchModeDescription: String {
        switch TouchInputMode(rawValue: touchMode) ?? .trackpad {
        case .trackpad:
            return "Your finger drives the host cursor like a laptop trackpad — tap to click, "
                + "two-finger tap right-clicks, two-finger drag scrolls, tap-and-drag holds."
        case .pointer:
            return "The host cursor jumps to wherever you touch — tap is a click at that spot."
        case .touch:
            return "Real multi-touch reaches the host — for touch-native apps and games."
        }
    }
    #endif

    #if !os(tvOS)
    /// Keyboard & mouse forwarding — applies wherever a hardware keyboard/mouse drives the stream
    /// (always on macOS; an attached keyboard/mouse on iPad). Absent on tvOS (no such input path).
    @ViewBuilder var inputSection: some View {
        Section("Keyboard & mouse") {
            described((ModifierLayout(rawValue: modifierLayout) ?? .mac).detail) {
                Picker("Modifier keys", selection: $modifierLayout) {
                    ForEach(ModifierLayout.allCases, id: \.self) { layout in
                        Text(layout.label).tag(layout.rawValue)
                    }
                }
            }
            described("Reverses the wheel and trackpad scroll direction sent to the host.") {
                Toggle("Invert scroll direction", isOn: $invertScroll)
            }
        }
    }
    #endif

    // MARK: - Audio

    @ViewBuilder var audioSection: some View {
        Section {
            described("The speaker layout requested from the host.") {
                Picker("Audio channels", selection: $audioChannels) {
                    ForEach(SettingsOptions.audioChannels, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
            #if os(macOS)
            described("Host audio plays through this device; System default follows your "
                + "Mac's output changes.") {
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
            }
            #endif
            described("This device's microphone feeds the host's virtual mic.") {
                Toggle("Send microphone to the host", isOn: $micEnabled)
            }
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
            // Multi-channel interfaces only: the mic sits on ONE discrete input, so let the user
            // pick it. Auto sums every channel (a lone hot mic still passes at full level).
            if micChannelCount > 1 {
                described("Pick the input your mic is on; Auto sums every channel.") {
                    Picker("Microphone channel", selection: $micChannel) {
                        Text("Auto (all channels)").tag(0)
                        ForEach(1...micChannelCount, id: \.self) { ch in
                            Text("Channel \(ch)").tag(ch)
                        }
                    }
                    .disabled(!micEnabled)
                }
            }
            #endif
        } header: {
            Text("Audio")
        } footer: {
            Text("Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    // MARK: - Controllers

    @ViewBuilder var controllersSection: some View {
        Section {
            if gamepads.controllers.isEmpty {
                Text("No controllers detected")
                    .foregroundStyle(.secondary)
            } else {
                ForEach(gamepads.controllers) { controller in
                    controllerRow(controller)
                }
            }
            described("One controller is forwarded as player 1 — Automatic picks the most "
                + "recently connected.") {
                Picker("Use controller", selection: $gamepads.preferredID) {
                    ForEach(controllerOptions, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
            described("The virtual pad created on the host. Automatic matches your controller "
                + "— a DualSense keeps adaptive triggers, lightbar, touchpad and motion.") {
                Picker("Controller type", selection: $gamepadType) {
                    ForEach(SettingsOptions.padTypes, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
            #if os(iOS)
            // iPhone only in practice: hidden where the device itself can't play haptics (iPad).
            if CHHapticEngine.capabilitiesForHardware().supportsHaptics {
                described("Plays player 1's rumble on the phone's own Taptic Engine — for "
                    + "clip-on controllers without motors of their own.") {
                    Toggle("Rumble on this iPhone", isOn: $rumbleOnDevice)
                }
            }
            #endif
            #if !os(tvOS)
            described("With a controller connected, the host list and library switch to a "
                + "controller-friendly layout — larger focus targets, a swipeable cover "
                + "browser.") {
                Toggle("Gamepad-optimized browsing", isOn: $gamepadUIEnabled)
            }
            #endif
            #if DEBUG && !os(tvOS)
            Button("Test Controller…") { showControllerTest = true }
                .disabled(gamepads.active == nil)
                .sheet(isPresented: $showControllerTest) { ControllerTestView() }
            #endif
        } header: {
            Text("Controllers")
        } footer: {
            Text("Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }
}
