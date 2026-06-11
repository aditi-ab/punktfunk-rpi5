// "+" sheet: name (optional) + address + port → a card in the hosts grid. The first
// actual connection runs the trust-on-first-use fingerprint prompt.

import SwiftUI

struct AddHostSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var address = ""
    @State private var port = 9777

    let onAdd: (StoredHost) -> Void

    var body: some View {
        VStack(spacing: 0) {
            Form {
                TextField("Name", text: $name, prompt: Text("Optional — e.g. Living Room"))
                TextField("Address", text: $address, prompt: Text("IP or hostname"))
                TextField("Port", value: $port, format: .number.grouping(.never))
            }
            .formStyle(.grouped)
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    #if !os(tvOS)
                    .keyboardShortcut(.cancelAction)
                    #endif
                Spacer()
                Button("Add Host") {
                    onAdd(StoredHost(
                        name: name.trimmingCharacters(in: .whitespaces),
                        address: address.trimmingCharacters(in: .whitespaces),
                        port: UInt16(clamping: port)))
                    dismiss()
                }
                .buttonStyle(.borderedProminent)
                #if !os(tvOS)
                .keyboardShortcut(.defaultAction)
                #endif
                .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            #if os(iOS)
            .controlSize(.large)
            #endif
            .padding(16)
        }
        #if os(macOS)
        .frame(width: 380)
        .fixedSize(horizontal: false, vertical: true)
        #endif
    }
}
