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
        #if os(tvOS)
        // No Form here: tvOS list rows add a full-width focus fill + row platter
        // behind the field's own pill. Standalone fields have exactly one pill.
        VStack(spacing: 28) {
            Text("Add Host")
                .font(.title3.weight(.semibold))
            TextField("Name", text: $name, prompt: Text("Optional — e.g. Living Room"))
                .labelsHidden()
            TextField("Address", text: $address, prompt: Text("IP or hostname"))
                .labelsHidden()
            TextField("Port", value: $port, format: .number.grouping(.never))
                .labelsHidden()
            HStack(spacing: 32) {
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Add Host") { add() }
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        #else
        VStack(spacing: 0) {
            Form {
                TextField("Name", text: $name, prompt: Text("Optional — e.g. Living Room"))
                TextField("Address", text: $address, prompt: Text("IP or hostname"))
                TextField("Port", value: $port, format: .number.grouping(.never))
                    #if os(tvOS)
                    // tvOS floats the label above a non-empty field INSIDE the pill,
                    // shoving the value off-center — the field is always prefilled
                    // here, so drop the label there.
                    .labelsHidden()
                    #endif
            }
            #if !os(tvOS)
        .formStyle(.grouped)
        #endif
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    #if !os(tvOS)
                    .keyboardShortcut(.cancelAction)
                    #endif
                Spacer()
                Button("Add Host") { add() }
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
        #endif
    }

    private func add() {
        onAdd(StoredHost(
            name: name.trimmingCharacters(in: .whitespaces),
            address: address.trimmingCharacters(in: .whitespaces),
            port: UInt16(clamping: port)))
        dismiss()
    }
}
