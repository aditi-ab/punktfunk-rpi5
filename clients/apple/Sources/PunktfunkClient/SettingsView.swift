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
    }

    private func fillFromMainScreen() {
        guard let screen = NSScreen.main else { return }
        let scale = screen.backingScaleFactor
        width = Int(screen.frame.width * scale)
        height = Int(screen.frame.height * scale)
        hz = screen.maximumFramesPerSecond
    }
}
