// SettingsView's shared sections — each setting's Section is defined exactly once here and
// composed by the per-platform bodies in SettingsView.swift.

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
            bitrateRows
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
        var text = "Trackpad: your finger nudges the host cursor like a laptop touchpad — tap to "
        text += "click, two-finger tap for a right click, two-finger drag to scroll, "
        text += "tap-then-drag to hold the button, three-finger tap for the stats overlay. "
        text += "Direct pointer: the cursor jumps to your finger. Touch passthrough: real "
        text += "multi-touch reaches the host, for apps that understand touch. Applies from "
        text += "the next touch."
        if UIDevice.current.userInterfaceIdiom == .pad {
            text += " Pointer capture locks a hardware mouse/trackpad for relative movement "
            text += "(mouse-look); off keeps the pointer free and sends absolute positions. "
            text += "The lock needs the stream full-screen and frontmost, and falls back "
            text += "automatically (Stage Manager, Slide Over)."
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

    // Stage-2 (Metal/VTDecompressionSession) is the default and only user-visible presenter — it
    // recovers from a wedged decoder, where stage-1's AVSampleBufferDisplayLayer freezes hard on a
    // lost HEVC reference. Stage-1 is kept reachable as a DEBUG-only override for diagnostics, like
    // the controller test. Empty in release builds (no presenter UI; stage-2 always).
    @ViewBuilder var presenterSection: some View {
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

    @ViewBuilder var statisticsSection: some View {
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

    @ViewBuilder var experimentalSection: some View {
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
                #if !os(tvOS)
                Text(Self.gamepadUIFooter)
                #endif
            }
            .font(.geist(12, relativeTo: .caption))
            .foregroundStyle(.secondary)
        }
    }
}
