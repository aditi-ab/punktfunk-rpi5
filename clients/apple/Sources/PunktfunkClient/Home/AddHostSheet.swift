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
    #if !os(tvOS)
    /// This host's DEFAULT settings profile — what a plain click/tap uses. Empty = Default
    /// settings (design/client-settings-profiles.md §5.2). Changing it here is the only way the
    /// default moves; "Connect with ▸" is deliberately a one-off.
    @State private var profileID: String
    /// Profiles pinned as their own cards for this host (§5.2a) — presentation only, and
    /// independent of the default above.
    @State private var pinnedIDs: Set<String>
    @ObservedObject private var profiles = ProfileStore.shared
    #endif
    #if !os(tvOS)
    /// Share the clipboard with this host (design clipboard-and-file-transfer.md §5.3). Off by
    /// default; honored only when the host advertises the capability at connect. Absent on tvOS,
    /// which has no pasteboard to share.
    @State private var clipboardSync: Bool
    #endif
    #if os(tvOS)
    private enum EditField: String, Identifiable {
        case name, address, port, mac
        var id: String { rawValue }
    }
    @State private var editingField: EditField?
    #endif

    /// A field's placeholder, which is not the same job on both platforms.
    ///
    /// macOS draws a `TextField`'s title as a leading label, so the prompt only has to hint at the
    /// VALUE. On iOS that title becomes an accessibility label the moment a prompt exists and
    /// nothing is drawn — the prompt is the field's entire visible identity, so it has to name the
    /// field as well as hint at it. Writing one string for both leaves you a Mac panel that says
    /// everything twice or a phone form of unlabelled boxes.
    private static func prompt(touch: String, desktop: String) -> Text {
        #if os(macOS)
        Text(desktop)
        #else
        Text(touch)
        #endif
    }

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
        #if !os(tvOS)
        _clipboardSync = State(initialValue: existing?.clipboardSync ?? false)
        #endif
        #if !os(tvOS)
        _profileID = State(initialValue: existing?.profileID ?? "")
        _pinnedIDs = State(initialValue: Set(existing?.pinnedProfileIDs ?? []))
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
            TVFieldRow(
                label: "MAC address", value: mac,
                placeholder: "For Wake-on-LAN, optional") { editingField = .mac }
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
                TextField(
                    "Name", text: $name,
                    prompt: Self.prompt(
                        touch: "Name (optional, e.g. Living Room)",
                        desktop: "Optional — e.g. Living Room"))
                TextField("Address", text: $address, prompt: Text("IP or hostname"))
                TextField("Port", value: $port, format: .number.grouping(.never))
                TextField(
                    "MAC address", text: $mac,
                    prompt: Self.prompt(
                        touch: "MAC address (for Wake-on-LAN, optional)",
                        desktop: "For Wake-on-LAN, optional"))
                    .autocorrectionDisabled()
                    #if os(iOS)
                    .textInputAutocapitalization(.never)
                    #endif
                #if !os(tvOS)
                Toggle("Share clipboard with this host", isOn: $clipboardSync)
                #endif
                profileRows
            }
            #if !os(tvOS)
            .formStyle(.grouped)
            #endif
            #if os(iOS)
            // As before: the sheet is sized to its content, so there is nothing to scroll. Only
            // the edit sheet's profile rows can outgrow that, and only they turn it back on.
            .scrollDisabled(!showsProfileRows)
            #endif
            #if os(macOS)
            // macOS ONLY: the grouped form's default system text is oversized next to the app's
            // Geist typography, so the panel reads out of place at full size. iOS keeps the app's
            // body size — a touch target and a field label there are not a Mac panel's, and 12pt
            // made this the one sheet in the app you had to squint at.
            .font(.geist(12, relativeTo: .callout))
            .controlSize(.small)
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
        // Sized to its content (see `sheetHeight`).
        .presentationDetents([.height(sheetHeight)])
        .presentationDragIndicator(.visible)
        #endif
        #if os(macOS)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        #endif
    }

    #if os(iOS)
    /// Four fields, the clipboard toggle, and the action row. The edit sheet's profile rows are
    /// the only thing that can outgrow it, and they say by how much; a single fixed number is what
    /// clipped them.
    private var sheetHeight: CGFloat {
        var height: CGFloat = 392 + 44 // the fields and action row, plus the clipboard toggle
        if showsProfileRows {
            height += 116 // the Profile picker and its footnote
            height += 96 + CGFloat(profiles.profiles.count) * 44 // the pins, their header + footer
        }
        return height
    }
    #endif

    /// Whether this sheet shows the per-host profile rows at all.
    ///
    /// Only when EDITING, and only once profiles exist. Adding a host is about reaching it — the
    /// address, and whether we can wake it; which settings it streams with is a decision for the
    /// host you already have, and stacking it onto the add flow made the first thing a new user
    /// sees a longer form than the one they came for. Editing is one context-menu item away.
    private var showsProfileRows: Bool {
        #if os(tvOS)
        return false
        #else
        return isEditing && !profiles.profiles.isEmpty
        #endif
    }

    /// The per-host profile rows: which profile this host uses by default, and which extra ones
    /// get their own card in the grid.
    ///
    /// Two plain sections, no disclosure. A collapsed group had to animate its own height AND the
    /// sheet's, and got both wrong; with a profile or three these are a couple of rows, and rows
    /// that are simply there can't be clipped or fail to expand.
    @ViewBuilder private var profileRows: some View {
        #if !os(tvOS)
        if showsProfileRows {
            Section {
                Picker("Profile", selection: $profileID) {
                    Text("Default settings").tag("")
                    ForEach(profiles.profiles) { profile in
                        Text(profile.name).tag(profile.id)
                    }
                    // A binding whose profile was deleted resolves as Default settings anyway;
                    // saying so beats an empty picker, and saving cleans the field up.
                    if !profileID.isEmpty, profiles.profile(id: profileID) == nil {
                        Text("Default settings (profile deleted)").tag(profileID)
                    }
                }
            } footer: {
                Text("The settings a plain tap on this host streams with.")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
            Section {
                ForEach(profiles.profiles) { profile in
                    Toggle(profile.name, isOn: Binding(
                        get: { pinnedIDs.contains(profile.id) },
                        set: { on in
                            if on {
                                pinnedIDs.insert(profile.id)
                            } else {
                                pinnedIDs.remove(profile.id)
                            }
                        }))
                }
            } header: {
                Text("Pinned cards")
            } footer: {
                Text("A pinned profile gets its own card next to this host — one tap, no menu.")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
        }
        #endif
    }

    private func save() {
        var host = existing ?? StoredHost(name: "", address: "")
        host.name = name.trimmingCharacters(in: .whitespaces)
        host.address = address.trimmingCharacters(in: .whitespaces)
        host.port = UInt16(clamping: port)
        host.macAddresses = Self.parseMacs(mac)
        #if !os(tvOS)
        // nil when off: the key stays absent from the saved JSON (forward-compat, and "never
        // opted in" and "opted out" read the same — off).
        host.clipboardSync = clipboardSync ? true : nil
        #endif
        #if !os(tvOS)
        // nil rather than "" for the same forward-compat reason, and a dangling binding is
        // cleaned up on this save (§6) instead of lingering as a stale id forever.
        host.profileID = profiles.profile(id: profileID) == nil ? nil : profileID
        // Keep the stored ORDER (it is card order) and drop what this sheet unpinned; anything
        // newly ticked goes on the end.
        let kept = (host.pinnedProfileIDs ?? []).filter { pinnedIDs.contains($0) }
        let added = pinnedIDs.filter { !kept.contains($0) }.sorted()
        let pins = kept + added
        host.pinnedProfileIDs = pins.isEmpty ? nil : pins
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
