// The quick-action ring's editor (design/touch-client-overlay.md §3.3): the editor IS the ring —
// the in-stream `RingOverlay`, the same type, full size over a gradient backdrop that runs the
// real twist (`DialCatcher`). Tap a slot to pick its action from the catalogue, drag a disc onto
// another to swap, tap the centre to see depth two; the shortcuts list and the reset sit under
// it. It edits whichever layer the settings surface is on — the binding comes from
// `scoped(SettingsFields.overlayActions)` — so a profile that touches it owns the whole ring (D10).

#if os(iOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

private struct SlotOption: Identifiable {
    let id: String
    let label: String
    var note: String? = nil
}

private struct SlotGroup: Identifiable {
    /// The section title.
    let id: String
    let options: [SlotOption]
}

/// The catalogue by group (§3.3), with each entry's availability note. The profile's own
/// shortcuts and the empty slot are appended per config.
private let builtinGroups: [SlotGroup] = [
    .init(id: "Session", options: [
        .init(id: "end_stream", label: "End stream"),
        .init(id: "disconnect_linger", label: "Disconnect, keep the game running"),
    ]),
    .init(id: "Input", options: [
        .init(id: "touch_mode", label: "Touch mode"),
        .init(id: "keyboard", label: "Keyboard"),
        .init(id: "pad", label: "Virtual controller", note: "Arrives in a later release"),
        .init(id: "send_text", label: "Send text", note: "Not on this device yet"),
    ]),
    .init(id: "View", options: [.init(id: "stats", label: "Statistics")]),
    .init(id: "Audio", options: [.init(id: "mic", label: "Microphone")]),
    .init(id: "Host", options: [
        .init(id: "host:power.sleep", label: "Sleep host", note: "Only where the host offers it"),
        .init(id: "host:power.reboot", label: "Restart host", note: "Only where the host offers it"),
        .init(id: "host:power.shutdown", label: "Shut down host", note: "Only where the host offers it"),
    ]),
]

private let modifierKeys = ["ctrl", "alt", "shift", "win"]
/// The keys a chord can end on — names `keyVk` knows.
private let chordKeys: [String] =
    ["escape", "tab", "enter", "space", "backspace", "delete", "insert", "home", "end",
     "pageup", "pagedown", "up", "down", "left", "right", "printscreen", "pause"]
    + (1...12).map { "f\($0)" }
    + "abcdefghijklmnopqrstuvwxyz".map(String.init)
    + (0...9).map(String.init)

private struct PickSlot: Identifiable {
    let k: Int
    var id: Int { k }
}

/// The three power actions as the ring would show them on a host that offers all three; the
/// editor has no host on the line, and a dimmed "does not offer it" would lie about the slot.
private let previewHosts = [
    HostAction(id: "power.sleep", title: "Sleep"),
    HostAction(id: "power.reboot", title: "Restart", danger: true),
    HostAction(id: "power.shutdown", title: "Shut down", danger: true),
]

struct QuickActionsEditor: View {
    /// The `overlay_actions` blob of the layer being edited; empty is the platform default.
    @Binding var blob: String
    /// The edited profile owns its own ring (the row's override marker says so; this says it
    /// under the ring).
    let overridden: Bool
    /// Back to the platform ring: drops the override in profile scope, clears the global
    /// otherwise.
    let reset: () -> Void
    @StateObject private var ring = RingState()
    @State private var picking: PickSlot?
    @State private var adding = false
    /// The backdrop's middle, where the ring opens and re-opens.
    @State private var centre = CGPoint.zero

    private var cfg: OverlayConfig { OverlayConfig.parse(blob) }

    var body: some View {
        Form {
            Section {
                GeometryReader { geo in
                    ZStack {
                        // The Form's own cell colour, resolved dark (the scheme below), so the
                        // backdrop reads as one more field rather than a stage.
                        Color(.secondarySystemGroupedBackground)
                        // The backdrop runs the real twist. Its tap only dismisses the preview
                        // sheet: UIKit hands a disc tap to this view as well as to the SwiftUI
                        // button above it, so closing the ring here closed it on every pick.
                        DialCatcher(onDial: { ring.handle($0) }) { ring.sheet = false }
                        RingOverlay(state: ring, cfg: cfg, actions: preview,
                                    editing: RingEditing(pick: { picking = PickSlot(k: $0) }, swap: swap))
                        VStack {
                            Spacer()
                            Text("Tap a button to change it, drag one onto another to swap.")
                                .font(.geist(12, .medium, relativeTo: .caption))
                                .foregroundStyle(.white.opacity(0.9))
                                .padding(.horizontal, 12).padding(.vertical, 6)
                                .glassBackground(Capsule())
                                .padding(.bottom, 10)
                        }
                        .allowsHitTesting(false)
                    }
                    .environment(\.colorScheme, .dark)
                    .onAppear {
                        centre = CGPoint(x: geo.size.width / 2, y: geo.size.height / 2)
                        ring.openAt(centre)
                    }
                    // Whatever closes it — a twist wound back, a preview row that ends the
                    // stream — the editor's ring springs back once the wind-in has played, so
                    // there is never a dead editor with nothing to tap.
                    .onChange(of: ring.closing) { _, closing in
                        if !closing, !ring.committed, ring.progress == 0 { ring.openAt(centre) }
                    }
                }
                .frame(height: 400)
                .listRowInsets(EdgeInsets())
            } footer: {
                if overridden {
                    Text("This profile has its own quick actions; the default ring no longer reaches it.")
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
        .sheet(item: $picking) { p in
            SlotPicker(groups: groups, current: cfg.ring[p.k]?.id ?? "") { id in
                set(p.k, id)
                picking = nil
            }
        }
        .sheet(isPresented: $adding) { AddShortcut(done: add) }
    }

    /// The ring's commands with nothing behind them: the editor shows, it never fires (§3.3).
    private var preview: RingActions {
        RingActions(
            endStream: {}, disconnectLinger: {},
            touchMode: { .trackpad }, cycleTouchMode: {},
            keyboard: {},
            stats: { .compact }, cycleStats: {},
            micAvailable: { true }, micMuted: { false }, toggleMic: {},
            hostActions: { previewHosts }, invokeHost: { _ in },
            sendShortcut: { _ in },
            currentMode: { (1920, 1080, 60) }, requestMode: { _, _, _ in })
    }

    private var groups: [SlotGroup] {
        var g = builtinGroups
        if !cfg.shortcuts.isEmpty {
            g.append(SlotGroup(id: "Shortcuts", options: cfg.shortcuts.map {
                SlotOption(id: "shortcut:\($0.id)", label: $0.label.isEmpty ? chordChip($0.keys) : $0.label)
            }))
        }
        g.append(SlotGroup(id: "Empty", options: [SlotOption(id: "", label: "Empty slot")]))
        return g
    }

    private func set(_ k: Int, _ id: String) {
        var c = cfg
        c.ring[k] = SlotId.parse(id)
        blob = c.toJSON()
    }

    private func swap(_ a: Int, _ b: Int) {
        var c = cfg
        c.ring.swapAt(a, b)
        blob = c.toJSON()
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
        // ring shows it at once.
        c.ring = c.ring.map { s in
            if case .shortcut(let id) = s, gone.contains(id) { return nil }
            return s
        }
        blob = c.toJSON()
    }
}

/// The catalogue by group; the current pick is ticked.
private struct SlotPicker: View {
    let groups: [SlotGroup]
    let current: String
    let choose: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                ForEach(groups) { g in
                    Section(g.id) {
                        ForEach(g.options) { o in
                            Button {
                                choose(o.id)
                            } label: {
                                HStack {
                                    VStack(alignment: .leading, spacing: 2) {
                                        Text(o.label)
                                        if let note = o.note {
                                            Text(note).font(.footnote).foregroundStyle(.secondary)
                                        }
                                    }
                                    Spacer()
                                    if o.id == current { Image(systemName: "checkmark") }
                                }
                            }
                            .foregroundStyle(.primary)
                        }
                    }
                }
            }
            .navigationTitle("Slot action")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
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
