// App settings (⌘,): the stream mode + the host compositor. The host creates a native
// virtual output at exactly this size/refresh — there is no scaling anywhere in the
// pipeline.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @AppStorage("punktfunk.compositor") private var compositor = 0
    @AppStorage("punktfunk.micEnabled") private var micEnabled = true
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
        return Form {
            Section {
                Picker("Stream mode", selection: modeTag) {
                    ForEach(options, id: \.tag) { option in
                        Text(option.label).tag(option.tag)
                    }
                }
                .pickerStyle(.navigationLink)
                Picker("Compositor", selection: $compositor) {
                    Text("Automatic").tag(0)
                    Text("KWin (KDE Plasma)").tag(1)
                    Text("wlroots (Sway / Hyprland)").tag(2)
                    Text("Mutter (GNOME)").tag(3)
                    Text("gamescope").tag(4)
                }
                .pickerStyle(.navigationLink)
            } footer: {
                Text("The host creates a virtual output at exactly this mode — native "
                    + "resolution, no scaling. A specific compositor is honored only if "
                    + "available on the host.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Section {
                Button("Done") { dismiss() }
            }
        }
        .navigationTitle("Settings")
    }
    #endif

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
            } header: {
                Text("Stream mode")
            } footer: {
                Text("The host creates a virtual output at exactly this mode — "
                    + "native resolution, no scaling.")
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
        }
        .formStyle(.grouped)
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
