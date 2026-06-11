// "+" sheet: name (optional) + address + port → a card in the hosts grid. The first
// actual connection runs the trust-on-first-use fingerprint prompt.

import SwiftUI

struct AddHostSheet: View {
    @Environment(\.dismiss) private var dismiss
    @State private var name = ""
    @State private var address = ""
    @State private var port = 9777
    #if os(tvOS)
    private enum EditField: String, Identifiable {
        case name, address, port
        var id: String { rawValue }
    }
    @State private var editing: EditField?
    #endif

    let onAdd: (StoredHost) -> Void

    var body: some View {
        #if os(tvOS)
        // No inline text editing on tvOS — Settings-style value rows; pressing one
        // raises the SYSTEM fullscreen keyboard (TVTextEntry).
        VStack(spacing: 24) {
            TVFieldRow(
                label: "Name", value: name, placeholder: "Optional"
            ) { editing = .name }
            TVFieldRow(
                label: "Address", value: address, placeholder: "IP or hostname"
            ) { editing = .address }
            TVFieldRow(
                label: "Port", value: String(port), placeholder: ""
            ) { editing = .port }
            HStack(spacing: 32) {
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Add Host") { add() }
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        .navigationTitle("Add Host")
        .fullScreenCover(item: $editing) { field in
            switch field {
            case .name:
                TVTextEntry(title: "Name (optional, e.g. Living Room)", text: name) {
                    name = $0
                    editing = nil
                }
            case .address:
                TVTextEntry(title: "IP or hostname", text: address) {
                    address = $0.trimmingCharacters(in: .whitespaces)
                    editing = nil
                }
            case .port:
                TVTextEntry(
                    title: "Port", text: String(port), keyboardType: .numberPad
                ) {
                    if let value = Int($0), (1...65535).contains(value) {
                        port = value
                    }
                    editing = nil
                }
            }
        }
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
