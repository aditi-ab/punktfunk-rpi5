// Add / edit a host: name (optional) + address + port + Wake-on-LAN MAC → a card in the grid.
// The MAC prefills from what we already know — the host's stored MAC, or the live mDNS advert's if
// it hasn't been learned yet — so it's usually already correct; type/paste it for a host we've
// never seen advertise. The first actual connection still runs the trust-on-first-use prompt.

import PunktfunkKit
import SwiftUI

struct AddHostSheet: View {
    @Environment(\.dismiss) private var dismiss

    /// nil = add a new host; non-nil = edit this one (fields prefilled, identity/pin preserved).
    let existing: StoredHost?
    /// MAC(s) to offer when the host has none stored yet — the live advert's, so the field is
    /// prefilled the moment the host is on the network, even before a connect has learned it.
    let suggestedMacs: [String]
    let onSave: (StoredHost) -> Void

    @State private var name: String
    @State private var address: String
    @State private var port: Int
    @State private var mac: String
    #if os(macOS)
    /// Share the clipboard with this host (macOS sessions only; design
    /// clipboard-and-file-transfer.md §5.3). Off by default; honored only when the host
    /// advertises the capability at connect.
    @State private var clipboardSync: Bool
    #endif
    #if os(tvOS)
    private enum EditField: String, Identifiable {
        case name, address, port, mac
        var id: String { rawValue }
    }
    @State private var editingField: EditField?
    #endif

    private var isEditing: Bool { existing != nil }
    private var actionTitle: String { isEditing ? "Save" : "Add Host" }
    private var canSave: Bool { !address.trimmingCharacters(in: .whitespaces).isEmpty }

    init(existing: StoredHost? = nil, suggestedMacs: [String] = [], onSave: @escaping (StoredHost) -> Void) {
        self.existing = existing
        self.suggestedMacs = suggestedMacs
        self.onSave = onSave
        _name = State(initialValue: existing?.name ?? "")
        _address = State(initialValue: existing?.address ?? "")
        _port = State(initialValue: Int(existing?.port ?? 9777))
        let stored = existing?.macAddresses ?? []
        _mac = State(initialValue: (stored.isEmpty ? suggestedMacs : stored).joined(separator: ", "))
        #if os(macOS)
        _clipboardSync = State(initialValue: existing?.clipboardSync ?? false)
        #endif
    }

    var body: some View {
        #if os(tvOS)
        // No inline text editing on tvOS — Settings-style value rows; pressing one
        // raises the SYSTEM fullscreen keyboard (TVTextEntry).
        VStack(spacing: 24) {
            TVFieldRow(label: "Name", value: name, placeholder: "Optional") { editingField = .name }
            TVFieldRow(label: "Address", value: address, placeholder: "IP or hostname") { editingField = .address }
            TVFieldRow(label: "Port", value: String(port), placeholder: "") { editingField = .port }
            TVFieldRow(label: "MAC", value: mac, placeholder: "Wake-on-LAN — auto-filled when known") { editingField = .mac }
            HStack(spacing: 32) {
                Button("Cancel", role: .cancel) { dismiss() }
                Button(actionTitle) { save() }.disabled(!canSave)
            }
            .padding(.top, 12)
        }
        .frame(maxWidth: 1000)
        .padding(60)
        .navigationTitle(isEditing ? "Edit Host" : "Add Host")
        .fullScreenCover(item: $editingField) { field in
            switch field {
            case .name:
                TVTextEntry(title: "Name (optional, e.g. Living Room)", text: name) {
                    name = $0
                    editingField = nil
                }
            case .address:
                TVTextEntry(title: "IP or hostname", text: address) {
                    address = $0.trimmingCharacters(in: .whitespaces)
                    editingField = nil
                }
            case .port:
                TVTextEntry(title: "Port", text: String(port), keyboardType: .numberPad) {
                    if let value = Int($0), (1...65535).contains(value) { port = value }
                    editingField = nil
                }
            case .mac:
                TVTextEntry(title: "MAC address(es), comma-separated — aa:bb:cc:dd:ee:ff", text: mac) {
                    mac = $0.trimmingCharacters(in: .whitespaces)
                    editingField = nil
                }
            }
        }
        #else
        VStack(spacing: 0) {
            Form {
                TextField("Name", text: $name, prompt: Text("Optional — e.g. Living Room"))
                TextField("Address", text: $address, prompt: Text("IP or hostname"))
                TextField("Port", value: $port, format: .number.grouping(.never))
                TextField("MAC", text: $mac, prompt: Text("Wake-on-LAN — auto-filled when known"))
                    .autocorrectionDisabled()
                    #if os(iOS)
                    .textInputAutocapitalization(.never)
                    #endif
                #if os(macOS)
                Toggle("Share clipboard with this host", isOn: $clipboardSync)
                #endif
            }
            #if !os(tvOS)
            .formStyle(.grouped)
            // The grouped form's default system text is oversized next to the app's Geist
            // typography — bring it down and on-brand so the panel doesn't read out of place.
            .font(.geist(12, relativeTo: .callout))
            .controlSize(.small)
            #endif
            #if os(iOS)
            .scrollDisabled(true)
            #endif
            #if os(macOS)
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button(actionTitle) { save() }
                    .glassProminentButtonStyle()
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canSave)
            }
            .padding(16)
            #else
            Button { save() } label: {
                Text(actionTitle).frame(maxWidth: .infinity)
            }
                .glassProminentButtonStyle()
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .disabled(!canSave)
                .padding(16)
            #endif
        }
        #if os(iOS)
        // Four fields + the action row — a touch taller than the 3-field add sheet used to be.
        .presentationDetents([.height(392)])
        .presentationDragIndicator(.visible)
        #endif
        #if os(macOS)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        #endif
    }

    private func save() {
        var host = existing ?? StoredHost(name: "", address: "")
        host.name = name.trimmingCharacters(in: .whitespaces)
        host.address = address.trimmingCharacters(in: .whitespaces)
        host.port = UInt16(clamping: port)
        host.macAddresses = Self.parseMacs(mac)
        #if os(macOS)
        // nil when off: the key stays absent from the saved JSON (forward-compat, and "never
        // opted in" and "opted out" read the same — off).
        host.clipboardSync = clipboardSync ? true : nil
        #endif
        onSave(host)
        dismiss()
    }

    /// Split comma/space/newline-separated MACs, keep only well-formed `aa:bb:cc:dd:ee:ff` (six hex
    /// octets, normalized lower-case); nil when none are valid, so clearing the field clears the
    /// stored MAC.
    static func parseMacs(_ s: String) -> [String]? {
        let macs = s
            .split(whereSeparator: { ",; \n\t".contains($0) })
            .map { $0.trimmingCharacters(in: .whitespaces).lowercased() }
            .filter { m in
                let parts = m.split(separator: ":")
                return parts.count == 6 && parts.allSatisfy { $0.count == 2 && UInt8($0, radix: 16) != nil }
            }
        return macs.isEmpty ? nil : macs
    }
}
