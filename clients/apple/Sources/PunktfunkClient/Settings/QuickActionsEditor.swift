// The quick-action ring's editor (design/touch-client-overlay.md §3.3, as a list): which action
// sits in each of the six slots, the custom shortcuts a slot can send, and the way back to the
// platform default. It edits whichever layer the settings surface is on — the binding comes from
// `scoped(SettingsFields.overlayActions)` — so a profile that touches it owns the whole ring (D10).

#if os(iOS)
import PunktfunkShared
import SwiftUI

private struct SlotOption: Identifiable {
    let id: String
    let label: String
}

/// What a slot can hold, in picker order: empty, the built-ins, the three host power actions by
/// their advertised ids, then (appended per config) the profile's own shortcuts.
private let builtinSlots: [SlotOption] = [
    .init(id: "", label: "Empty"),
    .init(id: "end_stream", label: "End stream"),
    .init(id: "disconnect_linger", label: "Disconnect, keep the game running"),
    .init(id: "touch_mode", label: "Touch mode"),
    .init(id: "keyboard", label: "Keyboard"),
    .init(id: "stats", label: "Statistics"),
    .init(id: "mic", label: "Microphone"),
    .init(id: "pad", label: "Virtual controller"),
    .init(id: "send_text", label: "Send text"),
    .init(id: "host:power.sleep", label: "Host: sleep"),
    .init(id: "host:power.reboot", label: "Host: reboot"),
    .init(id: "host:power.shutdown", label: "Host: shut down"),
]

private let modifierKeys = ["ctrl", "alt", "shift", "win"]
/// The keys a chord can end on — names `keyVk` knows.
private let chordKeys: [String] =
    ["escape", "tab", "enter", "space", "backspace", "delete", "insert", "home", "end",
     "pageup", "pagedown", "up", "down", "left", "right", "printscreen", "pause"]
    + (1...12).map { "f\($0)" }
    + "abcdefghijklmnopqrstuvwxyz".map(String.init)
    + (0...9).map(String.init)

private let clockLabels = ["12", "2", "4", "6", "8", "10"]

struct QuickActionsEditor: View {
    /// The `overlay_actions` blob of the layer being edited; empty is the platform default.
    @Binding var blob: String
    /// Back to the platform ring: drops the override in profile scope, clears the global
    /// otherwise.
    let reset: () -> Void
    @State private var adding = false

    private var cfg: OverlayConfig { OverlayConfig.parse(blob) }

    var body: some View {
        Form {
            Section("Ring") {
                ForEach(0..<OverlayConfig.ringSlots, id: \.self) { k in
                    Picker("\(clockLabels[k]) o'clock", selection: slot(k)) {
                        ForEach(options) { Text($0.label).tag($0.id) }
                    }
                }
            }
            Section {
                ForEach(cfg.shortcuts, id: \.id) { sc in
                    HStack {
                        Text(sc.label.isEmpty ? chordChip(sc.keys) : sc.label)
                        Spacer()
                        Text(chordChip(sc.keys))
                            .font(.geistFixed(13, .medium))
                            .foregroundStyle(.secondary)
                    }
                }
                .onDelete(perform: removeShortcuts)
                Button("Add shortcut") { adding = true }
            } header: {
                Text("Shortcuts")
            } footer: {
                Text("A new shortcut takes the first empty slot. Swipe one to remove it.")
            }
            Section {
                Button("Reset to default", role: .destructive, action: reset)
            } footer: {
                Text("Restores the platform ring and removes the shortcuts.")
            }
        }
        .navigationTitle("Quick actions")
        .sheet(isPresented: $adding) { AddShortcut(done: add) }
    }

    private var options: [SlotOption] {
        builtinSlots + cfg.shortcuts.map {
            SlotOption(id: "shortcut:\($0.id)", label: $0.label.isEmpty ? chordChip($0.keys) : $0.label)
        }
    }

    private func slot(_ k: Int) -> Binding<String> {
        Binding(
            get: { cfg.ring[k]?.id ?? "" },
            set: { id in
                var c = cfg
                c.ring[k] = SlotId.parse(id)
                blob = c.toJSON()
            })
    }

    private func add(label: String, keys: [String]) {
        var c = cfg
        let next = (c.shortcuts.compactMap { Int($0.id.dropFirst()) }.max() ?? 0) + 1
        let sc = OverlayShortcut(id: "s\(next)", label: label, keys: keys)
        c.shortcuts.append(sc)
        if let k = c.ring.firstIndex(where: { $0 == nil }) { c.ring[k] = .shortcut(sc.id) }
        blob = c.toJSON()
    }

    private func removeShortcuts(at offsets: IndexSet) {
        var c = cfg
        let gone = offsets.map { c.shortcuts[$0].id }
        c.shortcuts.remove(atOffsets: offsets)
        // `parse` would empty a dangling slot on the next read; write it empty now so the
        // picker shows it at once.
        c.ring = c.ring.map { s in
            if case .shortcut(let id) = s, gone.contains(id) { return nil }
            return s
        }
        blob = c.toJSON()
    }
}

/// One chord: a label, the modifiers held, and the key it ends on.
private struct AddShortcut: View {
    let done: (_ label: String, _ keys: [String]) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var label = ""
    @State private var mods: Set<String> = []
    @State private var key = "escape"

    private var keys: [String] { modifierKeys.filter { mods.contains($0) } + [key] }

    var body: some View {
        NavigationStack {
            Form {
                TextField("Label (optional)", text: $label)
                Section("Modifiers") {
                    ForEach(modifierKeys, id: \.self) { m in
                        Toggle(chordChip([m]), isOn: Binding(
                            get: { mods.contains(m) },
                            set: { on in if on { mods.insert(m) } else { mods.remove(m) } }))
                    }
                }
                Picker("Key", selection: $key) {
                    ForEach(chordKeys, id: \.self) { Text(chordChip([$0])).tag($0) }
                }
                Section("Chord") {
                    Text(chordChip(keys)).font(.geistFixed(15, .medium))
                }
            }
            .navigationTitle("New shortcut")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Add") {
                        done(label, keys)
                        dismiss()
                    }
                }
            }
        }
    }
}
#endif
