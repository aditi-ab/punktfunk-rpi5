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
                placeholder: "Optional — lets Punktfunk wake this host") { editingField = .mac }
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
                Section {
                    TextField("Name", text: $name, prompt: Text("Optional — e.g. Living Room"))
                    TextField("Address", text: $address, prompt: Text("IP or hostname"))
                    TextField("Port", value: $port, format: .number.grouping(.never))
                    // "MAC" alone said what the value IS and nothing about why it's here, and
                    // "Wake-on-LAN" as the placeholder read like the field wanted that typed in.
                    // The label names the value, the footer says what it buys.
                    TextField(
                        "MAC address", text: $mac,
                        prompt: Text("Optional — aa:bb:cc:dd:ee:ff"))
                        .autocorrectionDisabled()
                        #if os(iOS)
                        .textInputAutocapitalization(.never)
                        #endif
                    #if os(macOS)
                    Toggle("Share clipboard with this host", isOn: $clipboardSync)
                    #endif
                } footer: {
                    Text("The MAC address lets Punktfunk wake this host while it's asleep. "
                        + "It fills in by itself once the host has been seen on the network.")
                }
                profileRows
            }
            #if !os(tvOS)
            .formStyle(.grouped)
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
        // The sheet follows what it is actually showing (see `sheetHeight`), and `.large` is
        // always reachable so a large Dynamic Type size can never strand the action row.
        .presentationDetents([.height(sheetHeight), .large])
        .presentationDragIndicator(.visible)
        #endif
        #if os(macOS)
        .frame(width: 400)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        #endif
    }

    #if os(iOS)
    /// The sheet's height, derived from what it is actually showing rather than pinned to one
    /// number — which is what clipped the profile rows before: a sheet can't grow into a height
    /// nobody recomputed.
    ///
    /// Estimates, deliberately: the form scrolls and `.large` is offered alongside, so running
    /// short costs a scroll rather than a stranded row at accessibility text sizes.
    private var sheetHeight: CGFloat {
        var height: CGFloat = 440 // name, address, port, MAC + its footnote + the action row
        if showsProfileRows {
            height += 128 // the Profile picker and its footnote
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
        #if os(macOS)
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
