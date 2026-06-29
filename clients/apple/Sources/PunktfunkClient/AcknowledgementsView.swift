import PunktfunkKit
import SwiftUI

/// Open-source acknowledgements: punktfunk's own license (MIT OR Apache-2.0) followed by the
/// third-party software notices. Used as a pushed view on iOS/tvOS and a preferences tab on macOS.
struct AcknowledgementsView: View {
    private var version: String? {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                Text("punktfunk")
                    .font(.title2).bold()
                if let version {
                    Text("Version \(version)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text(Licenses.appLicense)
                    .font(.caption.monospaced())
                    .modifier(SelectableText())

                Divider()

                Text("Third-party software")
                    .font(.headline)
                Text(
                    "punktfunk uses the open-source components below, each under its own license. "
                        + "On some platforms FFmpeg is additionally bundled under the LGPL v2.1+ "
                        + "(dynamically linked, replaceable)."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                Text(Licenses.thirdPartyNotices)
                    .font(.caption2.monospaced())
                    .modifier(SelectableText())
            }
            .frame(maxWidth: 900, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding()
            #if os(tvOS)
                .padding(40)
            #endif
        }
        .navigationTitle("Acknowledgements")
    }
}

/// `textSelection(.enabled)` is unavailable on tvOS, so apply it only where it exists.
private struct SelectableText: ViewModifier {
    func body(content: Content) -> some View {
        #if os(tvOS)
            content
        #else
            content.textSelection(.enabled)
        #endif
    }
}
