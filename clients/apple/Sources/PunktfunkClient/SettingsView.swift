// App settings (⌘,): the stream mode + the host compositor. The host creates a native
// virtual output at exactly this size/refresh — there is no scaling anywhere in the
// pipeline.

import AppKit
import PunktfunkKit
import SwiftUI

struct SettingsView: View {
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @AppStorage("punktfunk.compositor") private var compositor = 0
    @AppStorage("punktfunk.speakerUID") private var speakerUID = ""
    @AppStorage("punktfunk.micUID") private var micUID = ""
    @AppStorage("punktfunk.micEnabled") private var micEnabled = true
    @State private var outputDevices: [AudioDevice] = []
    @State private var inputDevices: [AudioDevice] = []

    var body: some View {
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
            } header: {
                Text("Stream mode")
            } footer: {
                Text("The host creates a virtual output at exactly this mode — "
                    + "native resolution, no scaling.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
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
                Toggle("Send microphone to the host", isOn: $micEnabled)
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
            } header: {
                Text("Audio")
            } footer: {
                Text("Host audio plays through the speaker; the microphone feeds the "
                    + "host's virtual mic. System default follows macOS device changes. "
                    + "Applies from the next session.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
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
        }
        .formStyle(.grouped)
        .frame(width: 380)
        .fixedSize()
        .onAppear {
            outputDevices = AudioDevices.outputs()
            inputDevices = AudioDevices.inputs()
        }
    }

    private func fillFromMainScreen() {
        guard let screen = NSScreen.main else { return }
        let scale = screen.backingScaleFactor
        width = Int(screen.frame.width * scale)
        height = Int(screen.frame.height * scale)
        hz = screen.maximumFramesPerSecond
    }
}
