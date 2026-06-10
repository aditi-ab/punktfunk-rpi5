// App settings (⌘,): the stream mode. The host creates a native virtual output at
// exactly this size/refresh — there is no scaling anywhere in the pipeline.

import AppKit
import SwiftUI

struct SettingsView: View {
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60

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
