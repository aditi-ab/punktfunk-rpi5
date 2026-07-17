// SettingsView's shared sections — each setting's Section is defined exactly once here and
// composed by the per-platform bodies in SettingsView.swift.

#if os(iOS)
import CoreHaptics
#endif
import PunktfunkKit
import SwiftUI

extension SettingsView {
    // MARK: - Sections (shared)

    // NOTE: the Section content is deliberately split into the small named builders below — as one
    // inline expression the iOS branch (wheel + 3-way refresh + bitrate rows) blew Swift's
    // type-checker budget ("unable to type-check this expression in reasonable time"), which
    // failed exactly one slice: the iOS archive (macOS/tvOS never compile that branch).
    @ViewBuilder var streamModeSection: some View {
        Section {
            #if os(iOS) || os(macOS)
            // Match-window (design/midstream-resolution-resize.md D1): follow the session
            // window/scene, renegotiating the host mode on a resize. Off → the explicit mode below.
            Toggle("Match window", isOn: $matchWindow)
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
            TextField("Refresh rate (Hz)", value: $hz, format: .number.grouping(.never))
            LabeledContent("") {
                Button("Use this display's mode") { fillFromMainScreen() }
            }
            #endif
            #if !os(tvOS)
            renderScaleRow
            bitrateRows
            #endif
        } header: {
            Text("Stream mode")
        } footer: {
            Text(matchWindow
                ? "The stream follows this window — the host resizes its virtual output to match "
                    + "as you resize, so the picture stays pixel-exact (1:1) with no scaling. "
                    + "\(Self.bitrateFooter)"
                : "The host creates a virtual output at exactly this mode — native resolution, but "
                    + "a window that isn't this size is scaled to fit. \(Self.bitrateFooter)")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    #if !os(tvOS)
    /// Render-scale picker + the resulting host resolution. > 1 supersamples (sharper, at more
    /// bandwidth AND client decode); < 1 renders under native (lighter). The presenter resamples the
    /// decoded frame to this display, so the multiplier is where the sharpness/cost trade-off lives.
    @ViewBuilder var renderScaleRow: some View {
        Picker("Render scale", selection: $renderScale) {
            ForEach(RenderScale.presets, id: \.self) { scale in
                Text(RenderScale.label(scale)).tag(scale)
            }
        }
        // The concrete host resolution makes the cost legible. Only meaningful for the explicit mode
        // (match-window derives the base from the live window, not these fields).
        if renderScale != 1.0, !matchWindow {
            let mode = RenderScale.apply(
                baseWidth: width, baseHeight: height,
                scale: renderScale,
                maxDimension: RenderScale.maxDimension(codec: codec))
            Text("Host renders \(Int(mode.width))×\(Int(mode.height)); this device downscales it to your display.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    /// Keyboard & mouse forwarding — applies wherever a hardware keyboard/mouse drives the stream
    /// (always on macOS; an attached keyboard/mouse on iPad). Absent on tvOS (no such input path).
    @ViewBuilder var inputSection: some View {
        Section {
            Picker("Modifier keys", selection: $modifierLayout) {
                ForEach(ModifierLayout.allCases, id: \.self) { layout in
                    Text(layout.label).tag(layout.rawValue)
                }
            }
            Toggle("Invert scroll direction", isOn: $invertScroll)
        } header: {
            Text("Keyboard & mouse")
        } footer: {
            Text((ModifierLayout(rawValue: modifierLayout) ?? .mac).detail
                + " Invert scroll reverses the wheel/trackpad scroll direction sent to the host.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }
    #endif

    #if os(iOS)
    // MARK: - Stream mode (iOS wheel)

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

    #if !os(tvOS)
    /// The automatic-bitrate toggle + manual slider (and the >1 Gbps warning) rows.
    @ViewBuilder private var bitrateRows: some View {
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
    }
    #endif

    @ViewBuilder var audioSection: some View {
        Section {
            Picker("Audio channels", selection: $audioChannels) {
                ForEach(SettingsOptions.audioChannels, id: \.tag) { option in
                    Text(option.label).tag(option.tag)
                }
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
            // Multi-channel interfaces only: the mic sits on ONE discrete input, so let the user
            // pick it. Auto sums every channel (a lone hot mic still passes at full level).
            if micChannelCount > 1 {
                Picker("Microphone channel", selection: $micChannel) {
                    Text("Auto (all channels)").tag(0)
                    ForEach(1...micChannelCount, id: \.self) { ch in
                        Text("Channel \(ch)").tag(ch)
                    }
                }
                .disabled(!micEnabled)
            }
            #endif
        } header: {
            Text("Audio")
        } footer: {
            Text(Self.audioFooter)
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    #if os(iOS)
    /// Touch-input model (iPhone + iPad) plus the iPad-only pointer-capture toggle: lock the
    /// mouse/trackpad for relative movement (games) vs forward an absolute cursor position.
    @ViewBuilder var pointerSection: some View {
        Section {
            Picker("Touch input", selection: $touchMode) {
                Text("Trackpad").tag(TouchInputMode.trackpad.rawValue)
                Text("Direct pointer").tag(TouchInputMode.pointer.rawValue)
                Text("Touch passthrough").tag(TouchInputMode.touch.rawValue)
            }
            if UIDevice.current.userInterfaceIdiom == .pad {
                Toggle("Capture pointer for games", isOn: $pointerCapture)
            }
        } header: {
            Text("Touch & pointer")
        } footer: {
            Text(pointerFooterText)
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    /// Footer copy for `pointerSection`, built in plain `+=` statements. Deliberately NOT one big
    /// `+` chain (with a ternary) inside the ViewBuilder — that single expression blew Swift's
    /// type-checker budget and was what actually broke the iOS archive.
    private var pointerFooterText: String {
        var text = "Trackpad: your finger moves the host cursor like a laptop touchpad — tap "
        text += "to click, two-finger tap to right-click, two-finger drag to scroll, "
        text += "tap-and-drag to hold, three-finger tap for the stats overlay. Direct pointer: "
        text += "the cursor jumps to your finger. Touch passthrough: real multi-touch reaches "
        text += "the host. Applies from the next touch."
        if UIDevice.current.userInterfaceIdiom == .pad {
            text += " Pointer capture locks a hardware mouse for relative mouse-look; off sends "
            text += "absolute positions. Needs the stream full-screen and frontmost."
        }
        return text
    }
    #endif

    @ViewBuilder var compositorSection: some View {
        Section {
            Picker("Compositor", selection: $compositor) {
                ForEach(SettingsOptions.compositors, id: \.tag) { option in
                    Text(option.label).tag(option.tag)
                }
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

    /// Auto-wake on connect — fire Wake-on-LAN + wait for a sleeping saved host to come back before
    /// giving up. Now available on every platform (the iOS/tvOS multicast entitlement is granted).
    @ViewBuilder var wakeSection: some View {
        Section {
            Toggle("Auto-wake on connect", isOn: $autoWakeEnabled)
        } header: {
            Text("Wake-on-LAN")
        } footer: {
            Text("Connecting to a saved host that isn't on the network yet sends a Wake-on-LAN "
                + "packet and waits for it to come back before streaming. Turn off if a host that's "
                + "already on just isn't visible here (e.g. over a VPN), so connects go straight "
                + "through instead of waiting out the wake. A host's “Wake” action still works either "
                + "way.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder var windowSection: some View {
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

    // Non-tvOS: the Apple TV drives a fixed HDMI mode, so there's no adaptive refresh to allow.
    @ViewBuilder var vrrSection: some View {
        #if !os(tvOS)
        Section {
            Toggle("Allow VRR", isOn: $allowVRR)
        } header: {
            Text("Variable refresh rate")
        } footer: {
            Text("Let a ProMotion or adaptive-sync display vary its refresh rate to match the "
                + "stream — smoother motion without tearing. No effect on fixed-refresh displays. "
                + "Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
        #endif
    }

    // macOS-only: iOS/tvOS layers always present on the display's vsync, so the choice only
    // exists on the Mac (where the layer's own sync must stay off — see MetalVideoPresenter).
    @ViewBuilder var vsyncSection: some View {
        #if os(macOS)
        Section {
            Toggle("V-Sync", isOn: $vsync)
        } header: {
            Text("Presentation")
        } footer: {
            Text("Off (default): each frame is shown as soon as it's ready — lowest latency, "
                + "but frame timing can look uneven and fullscreen may tear. On: frames flip "
                + "in step with the display's refresh — evenly paced, up to one refresh of "
                + "added latency. Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
        #endif
    }

    // Stage-2 (Metal/VTDecompressionSession, present on frame arrival) is the proven default;
    // stage-3 is the same pipeline with glass-gated present pacing — a user-visible A/B while the
    // pacing work settles (see Stage2Pipeline's PresentPacing for the queue-saturation rationale).
    // Stage-1 (compressed video straight to the system layer) stays a DEBUG-only diagnostic — it
    // freezes hard on a lost HEVC reference.
    @ViewBuilder var presenterSection: some View {
        Section {
            Picker("Presenter", selection: $presenter) {
                Text("Stage 2 (default)").tag("stage2")
                Text("Stage 3 (experimental)").tag("stage3")
                #if DEBUG
                Text("Stage 1 (debug)").tag("stage1")
                #endif
            }
        } header: {
            Text("Video presenter")
        } footer: {
            Text("Stage 2: each frame is shown the moment it's decoded — proven, but on displays "
                + "running near the stream's frame rate, queued frames can add two to three "
                + "refreshes of display latency that never drains. Stage 3: presents are paced "
                + "to the display — at most one undisplayed frame in flight, always the freshest, "
                + "dropping late frames instead of queueing them. Watch the statistics overlay's "
                + "display time to compare. Applies from the next session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder var hdrSection: some View {
        Section {
            Picker("Video codec", selection: $codec) {
                ForEach(SettingsOptions.codecs, id: \.tag) { option in
                    Text(option.label).tag(option.tag)
                }
            }
            Toggle("10-bit HDR", isOn: $hdrEnabled)
            Toggle("Full chroma (4:4:4)", isOn: $enable444)
        } header: {
            Text("Video quality")
        } footer: {
            Text("Codec is a preference; the host falls back if it can't encode your choice. "
                + "HDR (HDR10) and full chroma (4:4:4) are HEVC-only, and each engages only when "
                + "both this device and the host support it — otherwise the stream stays 8-bit "
                + "4:2:0 SDR. 4:4:4 (off by default) sharpens text and UI — best for desktop "
                + "work; for games the bits are better spent at 4:2:0. Applies from the next "
                + "session.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    @ViewBuilder var statisticsSection: some View {
        Section {
            Picker("Statistics overlay", selection: $statsVerbosityRaw) {
                ForEach(StatsVerbosity.allCases, id: \.rawValue) { tier in
                    Text(tier.label).tag(tier.rawValue)
                }
            }
            Picker("Position", selection: $hudPlacement) {
                ForEach(HUDPlacement.allCases) { placement in
                    Text(placement.label).tag(placement.rawValue)
                }
            }
            .disabled(statsVerbosityRaw == StatsVerbosity.off.rawValue)
        } header: {
            Text("Statistics")
        } footer: {
            Text(Self.statisticsFooter)
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

    /// iOS/iPadOS only: keep a backgrounded session alive (audio background mode). Empty elsewhere
    /// (tvOS backgrounding semantics differ; macOS isn't gated by the mode) so the shared `.general`
    /// detail can reference it unconditionally.
    @ViewBuilder var keepAliveSection: some View {
        #if os(iOS)
        Section {
            Toggle("Keep streaming in background", isOn: $backgroundKeepAlive)
            if backgroundKeepAlive {
                Picker("Disconnect after", selection: $backgroundTimeoutMinutes) {
                    Text("1 minute").tag(1)
                    Text("5 minutes").tag(5)
                    Text("10 minutes").tag(10)
                    Text("30 minutes").tag(30)
                }
            }
        } header: {
            Text("Background")
        } footer: {
            Text("Off by default: backgrounding the app freezes the session. When on, audio keeps "
                + "playing and the connection stays live (video is dropped to save power) after you "
                + "switch away — and the session auto-disconnects after the time above so it can't "
                + "run down your battery. Returning to the app resumes video instantly.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
        #endif
    }

    @ViewBuilder var experimentalSection: some View {
        Section {
            Toggle("Show game library", isOn: $libraryEnabled)
        } header: {
            Text("Experimental")
        } footer: {
            Text("Adds a “Browse Library…” action to each host that lists its games "
                + "(Steam + custom); tap a title to launch it. Works once you've paired — no "
                + "extra host setup.")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
        }
    }

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
            Picker("Use controller", selection: $gamepads.preferredID) {
                ForEach(controllerOptions, id: \.tag) { option in
                    Text(option.label).tag(option.tag)
                }
            }
            Picker("Controller type", selection: $gamepadType) {
                ForEach(SettingsOptions.padTypes, id: \.tag) { option in
                    Text(option.label).tag(option.tag)
                }
            }
            #if os(iOS)
            // iPhone only in practice: hidden where the device itself can't play haptics (iPad).
            if CHHapticEngine.capabilitiesForHardware().supportsHaptics {
                Toggle("Rumble on this iPhone", isOn: $rumbleOnDevice)
            }
            #endif
            #if !os(tvOS)
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
            // The gamepad-UI blurb is appended here, not merged into the shared
            // `controllersFooter` constant — tvOS's `tvBody` reuses that exact string (line ~348)
            // for its own footer and has no such toggle to describe.
            VStack(alignment: .leading, spacing: 6) {
                Text(Self.controllersFooter)
                #if os(iOS)
                if CHHapticEngine.capabilitiesForHardware().supportsHaptics {
                    Text(Self.deviceRumbleFooter)
                }
                #endif
                #if !os(tvOS)
                Text(Self.gamepadUIFooter)
                #endif
            }
            .font(.geist(12, relativeTo: .caption))
            .foregroundStyle(.secondary)
        }
    }
}
