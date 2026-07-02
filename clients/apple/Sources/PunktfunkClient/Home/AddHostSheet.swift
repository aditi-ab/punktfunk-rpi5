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
            #if os(iOS)
            // The detent below is sized to fit all 3 rows + the action button exactly, so the
            // Form must NOT scroll/bounce inside it — lock it. (iOS 16+; safe at iOS 17.)
            .scrollDisabled(true)
            #endif
            #if os(macOS)
            // macOS: UNCHANGED — Cancel + Spacer + Add in an HStack, both wired to the
            // window's default/cancel keyboard actions. The 380-wide .fixedSize panel below
            // keeps this compact and centered.
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button("Add Host") { add() }
                    .glassProminentButtonStyle()
                    .keyboardShortcut(.defaultAction)
                    .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(16)
            #else
            // iOS / iPadOS: NO Cancel — the sheet is dismissed by the drag indicator,
            // swipe-down, or tap-outside. (AddHostSheet never sets interactiveDismissDisabled,
            // so all three are live; if anyone adds it later, restore a Cancel here or there is
            // no way back out.) A single FULL-WIDTH primary action reads as the one thing to do.
            // The fill must be on the LABEL, not the Button: .frame(maxWidth:.infinity) on the
            // Button only widens its hit area and leaves the styled capsule hugging the text —
            // stretching the label is what makes the glass/bordered pill itself go edge-to-edge.
            // .controlSize(.large) gives the tall, thumb-friendly height; .defaultAction lets a
            // hardware keyboard / iPad Return submit.
            Button { add() } label: {
                Text("Add Host").frame(maxWidth: .infinity)
            }
                .glassProminentButtonStyle()
                .controlSize(.large)
                .keyboardShortcut(.defaultAction)
                .disabled(address.trimmingCharacters(in: .whitespaces).isEmpty)
                .padding(16)
            #endif
        }
        #if os(iOS)
        // A short bottom sheet, not a full-screen modal. .height(320) hugs the 3-field grouped
        // Form + the full-width action row, instead of the half-screen .medium it used to rest
        // at. A single fixed detent is enough: the system keeps the content above the keyboard
        // when Address/Port is focused, and on iPadOS this renders as a short bottom sheet (not a
        // centered formSheet card). The Form itself is .scrollDisabled (above) so it can't
        // bounce/scroll inside this fixed detent. (.height(_:) is iOS 16+, safe at iOS 17.)
        .presentationDetents([.height(320)])
        .presentationDragIndicator(.visible)
        #endif
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
