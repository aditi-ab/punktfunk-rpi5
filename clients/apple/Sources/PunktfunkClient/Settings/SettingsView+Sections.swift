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
// The SAME builders edit settings profiles (design/client-settings-profiles.md §5.1 —
// SettingsView+Scope): a control's binding comes from `scoped(...)` rather than `@AppStorage`, so
// it writes whichever layer the scope switcher selected, and `described(_:field:)` marks the row
// when the edited profile overrides it. Rows that are NOT profileable — tier G (this device's
// hardware and endpoints) and tier H (properties of a host) — are gated on `!inProfileScope` and
// simply don't render there; sections that would end up empty don't either.
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
            // NO marker here even though this toggle writes one: match-window, width and
            // height are ONE override (they are reset together), and hanging its marker off the
            // first of the two controls that drive it read as if the toggle alone were
            // overridden. It goes under the size control below, for the group.
            described(effective.matchWindow
                ? "The host resizes its output to follow this window — the picture stays "
                    + "pixel-exact (1:1) through every resize."
                : "Stream at the fixed mode below; a window at a different size shows it scaled.") {
                Toggle("Match window", isOn: scoped(SettingsFields.matchWindow))
            }
            #endif
            #if os(iOS)
            iosResolutionWheel
            overrideMarker(OverlayField.resolution)
            iosRefreshRows
            Button("Use this display's mode") { fillFromMainScreen() }
            #elseif os(macOS)
            HStack {
                TextField(
                    "Resolution", value: scoped(SettingsFields.width),
                    format: .number.grouping(.never))
                Text("×")
                TextField("", value: scoped(SettingsFields.height), format: .number.grouping(.never))
                    .labelsHidden()
            }
            overrideMarker(OverlayField.resolution)
            described("The host drives a real virtual output at exactly this size and refresh — "
                + "true pixels, no scaling.", field: "refresh_hz") {
                TextField(
                    "Refresh rate (Hz)", value: scoped(SettingsFields.refreshHz),
                    format: .number.grouping(.never))
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
                .modifier(CaptionWidth()) // the same reading cap + control column as `described`
        }
    }

    /// Custom W×H(+Hz) fields, a segmented refresh picker, or a static single-rate row.
    @ViewBuilder private var iosRefreshRows: some View {
        if isCustomResolution {
            // Arbitrary entry: type the exact width × height (and refresh) the host should drive.
            HStack {
                TextField("Width", value: scoped(SettingsFields.width),
                          format: .number.grouping(.never))
                    .keyboardType(.numberPad)
                Text("×")
                TextField("Height", value: scoped(SettingsFields.height),
                          format: .number.grouping(.never))
                    .labelsHidden()
                    .keyboardType(.numberPad)
            }
            // A row built from an HStack of TextFields otherwise insets its bottom separator to
            // the inner content, clipping the hairline under "Width"; pin it to the cell edge.
            .alignmentGuide(.listRowSeparatorLeading) { _ in 0 }
            LabeledContent("Refresh rate") {
                TextField("Hz", value: scoped(SettingsFields.refreshHz),
                          format: .number.grouping(.never))
                    .keyboardType(.numberPad)
                    .multilineTextAlignment(.trailing)
            }
        } else if refreshChoices.count > 1 {
            VStack(alignment: .leading, spacing: 6) {
                Text("Refresh rate")
                    .font(.geist(15, relativeTo: .subheadline))
                    .foregroundStyle(.secondary)
                Picker("Refresh rate", selection: scoped(SettingsFields.refreshHz)) {
                    ForEach(refreshChoices, id: \.self) { rate in
                        Text("\(rate) Hz").tag(rate)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                overrideMarker("refresh_hz")
            }
        } else {
            // A device with a single supported rate (e.g. 60 Hz) has nothing to pick.
            LabeledContent("Refresh rate") {
                Text("\(effective.refreshHz) Hz").foregroundStyle(.secondary)
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
    /// or the effective size simply isn't one of the presets (e.g. a value synced from a Mac, or a
    /// profile's own override) — so a non-preset mode stays editable without a persisted flag.
    private var isCustomResolution: Bool {
        customMode || !presetResolutionTags.contains("\(effective.width)x\(effective.height)")
    }

    /// The wheel works in "WxH" tags so one selection drives both width and height; the custom
    /// sentinel toggles `customMode` instead of writing a size.
    private var resolutionSelection: Binding<String> {
        Binding(
            get: {
                isCustomResolution
                    ? Self.customResolutionTag
                    : "\(effective.width)x\(effective.height)"
            },
            set: { tag in
                if tag == Self.customResolutionTag {
                    customMode = true
                    return
                }
                customMode = false
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 2 else { return }
                setResolution(width: parts[0], height: parts[1])
            })
    }

    /// Refresh rates this device can display, plus any stored custom value (see `SettingsOptions`).
    private var refreshChoices: [Int] {
        SettingsOptions.refreshRates(including: effective.refreshHz)
    }
    #endif

    // MARK: - Display: Quality

    @ViewBuilder var qualitySection: some View {
        Section("Quality") {
            #if !os(tvOS)
            renderScaleRow
            bitrateRows
            #endif
            described("A preference — the host falls back if it can't encode it.",
                      field: "codec") {
                Picker("Video codec", selection: scoped(SettingsFields.codec)) {
                    ForEach(SettingsOptions.codecs, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
            described("HDR10, when the host has HDR content and this display supports it. "
                + "HEVC only; otherwise the stream stays SDR.", field: "hdr_enabled") {
                Toggle("10-bit HDR", isOn: scoped(SettingsFields.hdrEnabled))
            }
            described("Sharper text and UI for desktop work, at more bandwidth. For games the "
                + "bits are better spent at 4:2:0. HEVC only.", field: "enable_444") {
                Toggle("Full chroma (4:4:4)", isOn: scoped(SettingsFields.enable444))
            }
        }
    }

    #if !os(tvOS)
    /// Render-scale picker + the resulting host resolution. > 1 supersamples (sharper, at more
    /// bandwidth AND client decode); < 1 renders under native (lighter). The presenter resamples the
    /// decoded frame to this display, so the multiplier is where the sharpness/cost trade-off lives.
    @ViewBuilder var renderScaleRow: some View {
        described(renderScaleDescription, field: "render_scale") {
            Picker("Render scale", selection: scoped(SettingsFields.renderScale)) {
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
        let settings = effective
        if settings.renderScale != 1.0, !settings.matchWindow {
            let mode = RenderScale.apply(
                baseWidth: settings.width, baseHeight: settings.height,
                scale: settings.renderScale,
                maxDimension: RenderScale.maxDimension(codec: settings.codec))
            text += " Host renders \(Int(mode.width))×\(Int(mode.height)); this device scales "
                + "it to your display."
        }
        return text
    }

    /// The automatic-bitrate toggle + manual slider (and the >1 Gbps warning) rows.
    @ViewBuilder private var bitrateRows: some View {
        described("The host's default 20 Mbps, clamped to what it supports. Turn off to set a "
            + "fixed rate — a host card's context menu has a network speed test.",
            field: "bitrate_kbps") {
            Toggle("Automatic bitrate", isOn: automaticBitrate)
        }
        if effective.bitrateKbps != 0 {
            HStack(spacing: 12) {
                Slider(value: bitrateSlider, in: 0...1) {
                    Text("Bitrate")
                }
                Text(SpeedTestSheet.mbpsLabel(kbps: effective.bitrateKbps))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .frame(minWidth: 76, alignment: .trailing)
            }
            if effective.bitrateKbps > 1_000_000 {
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
            described(effective.presentPriority == "smooth"
                ? "A small frame buffer evens out network hiccups, at the buffer's worth of "
                    + "added display latency."
                : "Every frame shows the moment the display can take it — a network hiccup is "
                    + "an occasional repeated or skipped frame.", field: "present_priority") {
                Picker("Prioritize", selection: scoped(SettingsFields.presentPriority)) {
                    ForEach(SettingsOptions.presentPriorities, id: \.tag) { option in
                        Text(option.tag == SettingsOptions.presentPriorityDefault
                            ? "\(option.label) (default)" : option.label)
                            .tag(option.tag)
                    }
                }
            }
            if effective.presentPriority == "smooth" {
                described("Frames held back — each absorbs about one refresh of jitter and "
                    + "adds one refresh of delay.", field: "smooth_buffer") {
                    Picker("Buffer", selection: scoped(SettingsFields.smoothBuffer)) {
                        ForEach(
                            SettingsOptions.smoothBuffers(refreshHz: effective.refreshHz),
                            id: \.tag
                        ) { option in
                            Text(option.label).tag(option.tag)
                        }
                    }
                }
            }
            // Non-tvOS: the Apple TV drives a fixed HDMI mode, so there's no adaptive refresh.
            #if !os(tvOS)
            described("A ProMotion or adaptive-sync display follows the stream's rate — "
                + "smoother motion. No effect on fixed-refresh displays.", field: "allow_vrr") {
                Toggle("Allow VRR", isOn: scoped(SettingsFields.allowVRR))
            }
            #endif
            // macOS-only: iOS/tvOS layers always present on the display's vsync, so the choice
            // only exists on the Mac (the layer's own sync stays off — see MetalVideoPresenter).
            #if os(macOS)
            described("Flips align to the display's refresh — even pacing, up to one refresh "
                + "of added latency. Off shows frames as soon as they're ready.", field: "vsync") {
                Toggle("V-Sync", isOn: scoped(SettingsFields.vsync))
            }
            // The DCP swapID-panic mitigation's user handle (see DefaultsKey.windowedSafePresent
            // for the saga). Default ON: turning it off re-arms a WHOLE-MACHINE kernel panic on
            // affected setups, so the caption says so in plain words.
            described(effective.windowedSafePresent
                ? "Windowed streams present in step with the system compositor — avoids a macOS "
                    + "display-driver crash seen on high-refresh displays, at a small latency "
                    + "cost. Fullscreen always uses the fastest path."
                : "Windowed streams use the fastest present path. On some high-refresh setups "
                    + "this can crash macOS itself (kernel panic) — turn back on if your Mac "
                    + "restarts during windowed streaming.", field: "windowed_safe_present") {
                Toggle("Safe windowed presentation", isOn: scoped(SettingsFields.windowedSafePresent))
            }
            #endif
        }
    }

    // MARK: - Display: Host output

    @ViewBuilder var hostOutputSection: some View {
        Section {
            described("The backend the host uses for its virtual output. A specific choice "
                + "falls back to auto-detection when that backend isn't available.",
                field: "compositor") {
                Picker("Compositor", selection: scoped(SettingsFields.compositor)) {
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

    /// Empty in profile scope everywhere but macOS: auto-wake is a property of the host and this
    /// network, and background keep-alive is a property of this device — neither is something
    /// "Game" and "Work" would ever differ on (§3, tiers H and G).
    private var showsSessionSection: Bool {
        #if os(macOS)
        return true
        #else
        return !inProfileScope
        #endif
    }

    @ViewBuilder var sessionSection: some View {
        if showsSessionSection {
            Section("Session") {
                #if os(macOS)
                described("Go fullscreen when a session starts; return to a window on the host "
                    + "list.", field: "fullscreen_on_stream") {
                    Toggle(
                        "Fullscreen while streaming",
                        isOn: scoped(SettingsFields.fullscreenWhileStreaming))
                }
                #endif
                if !inProfileScope {
                    described("Connecting to a saved host that's offline sends Wake-on-LAN and "
                        + "waits for it to boot. Turn off if hosts behind a VPN look offline "
                        + "when they aren't.") {
                        Toggle("Auto-wake on connect", isOn: $autoWakeEnabled)
                    }
                }
                #if os(iOS)
                if !inProfileScope {
                    described("Audio and the connection stay live after you switch away; video "
                        + "pauses to save power and resumes instantly when you return. Off, "
                        + "backgrounding freezes the session.") {
                        Toggle("Keep streaming in background", isOn: $backgroundKeepAlive)
                    }
                    if backgroundKeepAlive {
                        described("Ends a backgrounded session so it can't run down the "
                            + "battery.") {
                            Picker("Disconnect after", selection: $backgroundTimeoutMinutes) {
                                Text("1 minute").tag(1)
                                Text("5 minutes").tag(5)
                                Text("10 minutes").tag(10)
                                Text("30 minutes").tag(30)
                            }
                        }
                    }
                }
                #endif
            }
        }
    }

    // MARK: - General: Statistics overlay

    @ViewBuilder var overlaySection: some View {
        Section("Statistics") {
            described(Self.statisticsDescription, field: "stats_verbosity") {
                Picker("Statistics overlay", selection: scoped(SettingsFields.statsVerbosity)) {
                    ForEach(StatsVerbosity.allCases, id: \.rawValue) { tier in
                        Text(tier.label).tag(tier.rawValue)
                    }
                }
            }
            // Which corner the overlay sits in is a property of this device's screen, not of a
            // profile (tier G).
            if !inProfileScope {
                Picker("Position", selection: $hudPlacement) {
                    ForEach(HUDPlacement.allCases) { placement in
                        Text(placement.label).tag(placement.rawValue)
                    }
                }
                .disabled(effective.statsVerbosity == StatsVerbosity.off.rawValue)
            }
        }
    }

    // MARK: - General: Library

    @ViewBuilder var librarySection: some View {
        // An app-level feature switch for this device (tier G) — the whole section collapses in
        // profile scope rather than rendering an empty group.
        if !inProfileScope {
            Section("Library") {
                described("Adds “Browse Library…” to paired hosts — list their Steam and custom "
                    + "games and launch one directly. No extra host setup.") {
                    Toggle("Show game library", isOn: $libraryEnabled)
                }
            }
        }
    }

    // MARK: - Input

    #if os(iOS)
    /// Touch-input model (iPhone + iPad) plus the iPad-only pointer-capture toggle: lock the
    /// mouse/trackpad for relative movement (games) vs forward an absolute cursor position.
    @ViewBuilder var pointerSection: some View {
        Section("Touch & pointer") {
            described(touchModeDescription, field: "touch_mode") {
                Picker("Touch input", selection: scoped(SettingsFields.touchMode)) {
                    Text("Trackpad").tag(TouchInputMode.trackpad.rawValue)
                    Text("Direct pointer").tag(TouchInputMode.pointer.rawValue)
                    Text("Touch passthrough").tag(TouchInputMode.touch.rawValue)
                }
            }
            // Whether a hardware mouse attached to THIS iPad gets locked is a fact about this
            // device's input hardware (tier G), not about how a host is streamed.
            if !inProfileScope, UIDevice.current.userInterfaceIdiom == .pad {
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
        switch TouchInputMode(rawValue: effective.touchMode) ?? .trackpad {
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
            #if os(macOS)
            described(mouseModeDescription, field: "mouse_mode") {
                Picker("Mouse input", selection: scoped(SettingsFields.mouseMode)) {
                    Text("Capture (games)").tag(MouseInputMode.capture.rawValue)
                    Text("Desktop (absolute)").tag(MouseInputMode.desktop.rawValue)
                }
            }
            #endif
            described(
                (ModifierLayout(rawValue: effective.modifierLayout) ?? .mac).detail,
                field: "modifier_layout"
            ) {
                Picker("Modifier keys", selection: scoped(SettingsFields.modifierLayout)) {
                    ForEach(ModifierLayout.allCases, id: \.self) { layout in
                        Text(layout.label).tag(layout.rawValue)
                    }
                }
            }
            described("Reverses the wheel and trackpad scroll direction sent to the host.",
                      field: "invert_scroll") {
                Toggle("Invert scroll direction", isOn: scoped(SettingsFields.invertScroll))
            }
        }
    }

    #if os(macOS)
    /// The SELECTED mouse model explained — dynamic, like the touch-mode caption.
    private var mouseModeDescription: String {
        switch MouseInputMode(rawValue: effective.mouseMode) ?? .capture {
        case .capture:
            return "The pointer locks to the stream and sends relative motion — best for "
                + "games. ⌃⌥⇧M switches live; applies from the next capture otherwise."
        case .desktop:
            return "The pointer moves freely in and out of the stream and sends absolute "
                + "positions — best for remote desktop work. Unavailable on gamescope hosts."
        }
    }
    #endif
    #endif

    // MARK: - Audio

    @ViewBuilder var audioSection: some View {
        Section {
            described("The speaker layout requested from the host.", field: "audio_channels") {
                Picker("Audio channels", selection: scoped(SettingsFields.audioChannels)) {
                    ForEach(SettingsOptions.audioChannels, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
            }
            #if os(macOS)
            // Which speaker THIS Mac plays through is this device's audio routing (tier G).
            if !inProfileScope {
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
            }
            #endif
            described("This device's microphone feeds the host's virtual mic.",
                      field: "mic_enabled") {
                Toggle("Send microphone to the host", isOn: scoped(SettingsFields.micEnabled))
            }
            described(echoCancelCaption, field: "echo_cancel") {
                Toggle("Echo cancellation", isOn: scoped(SettingsFields.echoCancel))
                    .disabled(!effective.micEnabled)
            }
            #if os(macOS)
            if !inProfileScope {
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
                .disabled(!effective.micEnabled)
                // Multi-channel interfaces only: the mic sits on ONE discrete input, so let the
                // user pick it. Auto sums every channel (a lone hot mic still passes at full
                // level).
                if micChannelCount > 1 {
                    described("Pick the input your mic is on; Auto sums every channel.") {
                        Picker("Microphone channel", selection: $micChannel) {
                            Text("Auto (all channels)").tag(0)
                            ForEach(1...micChannelCount, id: \.self) { ch in
                                Text("Channel \(ch)").tag(ch)
                            }
                        }
                        .disabled(!effective.micEnabled)
                    }
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

    /// Honest about the macOS escape hatch: the voice processor only follows the system
    /// default devices, so hand-picked endpoints silently keep the raw path (see
    /// SessionAudio's topology note) — better said here than discovered mid-call.
    private var echoCancelCaption: String {
        let base = "Voice processing cancels the audio this device plays out of the mic "
            + "signal, so a speaker setup doesn't feed the game back to the host."
        #if os(macOS)
        return base + " Follows the system default devices — a hand-picked speaker, "
            + "microphone or input channel streams the raw mic instead."
        #else
        return base
        #endif
    }

    // MARK: - Controllers

    @ViewBuilder var controllersSection: some View {
        Section {
            // The master switch, above everything it governs. Profileable, so it renders in
            // both scopes: a "Work" profile can decline to forward what "Game" forwards.
            described("Sends controllers connected to this device to the host. Turn it off when "
                + "your controller already reaches the host another way — USB passthrough such "
                + "as VirtualHere, or a pad plugged into the host itself — so games don't see "
                + "two of them.",
                field: "gamepad_forwarding") {
                Toggle("Forward controllers", isOn: scoped(SettingsFields.gamepadForwarding))
            }
            // Which physical pad this device forwards, and what its own haptics do, are facts
            // about THIS device (tier G) — only the virtual pad the host creates is profileable.
            if !inProfileScope {
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
                    .disabled(!effective.gamepadForwarding)
                }
            }
            described("The virtual pad created on the host. Automatic matches your controller "
                + "— a DualSense keeps adaptive triggers, lightbar, touchpad and motion.",
                field: "gamepad") {
                Picker("Controller type", selection: scoped(SettingsFields.gamepadType)) {
                    ForEach(SettingsOptions.padTypes, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
                .disabled(!effective.gamepadForwarding)
            }
            #if os(iOS)
            // iPhone only in practice: hidden where the device itself can't play haptics (iPad).
            if !inProfileScope, CHHapticEngine.capabilitiesForHardware().supportsHaptics {
                described("Plays player 1's rumble on the phone's own Taptic Engine — for "
                    + "clip-on controllers without motors of their own.") {
                    Toggle("Rumble on this iPhone", isOn: $rumbleOnDevice)
                }
            }
            #endif
            #if !os(tvOS)
            if !inProfileScope {
                described("With a controller connected, the host list and library switch to a "
                    + "controller-friendly layout — larger focus targets, a swipeable cover "
                    + "browser.") {
                    Toggle("Gamepad-optimized browsing", isOn: $gamepadUIEnabled)
                }
            }
            #endif
            #if DEBUG && !os(tvOS)
            if !inProfileScope {
                Button("Test Controller…") { showControllerTest = true }
                    .disabled(gamepads.active == nil)
                    .sheet(isPresented: $showControllerTest) { ControllerTestView() }
            }
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
