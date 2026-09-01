// The quick-action ring's editor (design/touch-client-overlay.md §3.3): the editor IS the ring —
// the in-stream `RingOverlay`, the same type, full size over a backdrop that runs the real twist
// (`DialCatcher`). Tap a slot to pick its action from the catalogue, drag a disc onto another to
// swap, tap the centre to see depth two; the shortcuts list and the reset sit under it. A
// shortcut is edited on its own sheet: a name, the modifiers as chips, the key on a keyboard
// you tap, and the disc as it will look. It edits whichever layer the settings surface is on —
// the binding comes from `scoped(SettingsFields.overlayActions)` — so a profile that touches it
// owns the whole ring (D10).
//
// On the Mac the same editor, minus the twist: the backdrop is inert and the ring simply sits
// open on it, a mouse drags the discs where a finger did, and there is no on-screen controller
// to configure. A shortcut is removed from its own sheet — a macOS Form has no swipe.

#if os(iOS) || os(macOS)
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
        .init(id: "touch_mode", label: "Touch mode", note: noMacNote),
        .init(id: "keyboard", label: "Keyboard", note: macKeyboardNote),
        .init(id: "pad", label: "Virtual controller",
              note: noMacNote ?? "Shows or hides the on-screen controller"),
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

#if os(macOS)
/// The slots a Mac cannot serve, said once in the catalogue so a dimmed disc is never a surprise
/// found after picking it. The profile still syncs — an iPhone on the same profile runs them.
private let noMacNote: String? = "Not on a Mac — no touch screen"
private let macKeyboardNote: String? = "Not on a Mac — use its own keyboard"
/// The pointer verb, so the instructions name what the reader is actually holding.
private let pickVerb = "Click"
#else
private let noMacNote: String? = nil
private let macKeyboardNote: String? = nil
private let pickVerb = "Tap"
#endif

private let modifierKeys = ["ctrl", "alt", "shift", "win"]

/// The keys a chord can end on, as the keyboard the editor draws lays them out — every name
/// `keyVk` knows, grouped the way a keyboard groups them.
private let keyGroups: [(title: String, keys: [String])] = [
    ("Function", ["escape"] + (1...12).map { "f\($0)" }),
    ("Letters", "qwertyuiopasdfghjklzxcvbnm".map(String.init)),
    ("Numbers", (1...9).map(String.init) + ["0"]),
    ("Editing", ["tab", "space", "enter", "backspace", "delete", "insert"]),
    ("Navigation", ["home", "end", "pageup", "pagedown", "up", "down", "left", "right"]),
    ("Other", ["printscreen", "pause", "capslock"]),
]

private struct PickSlot: Identifiable {
    let k: Int
    var id: Int { k }
}

/// A shortcut on the editing sheet, new or existing.
private struct ShortcutDraft: Identifiable {
    var id: String
    var label: String
    var keys: [String]
    var isNew: Bool
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
    @State private var editingShortcut: ShortcutDraft?
    #if os(iOS)
    @State private var editingLayout = false
    #endif
    /// The backdrop's middle, where the ring opens and re-opens.
    @State private var centre = CGPoint.zero

    private var cfg: OverlayConfig {
        #if os(macOS)
        OverlayConfig.parse(blob, platform: .desktop)
        #else
        OverlayConfig.parse(blob)
        #endif
    }

    var body: some View {
        Form {
            Section {
                GeometryReader { geo in
                    ZStack {
                        // The Form's own cell colour, resolved dark (the scheme below), so the
                        // backdrop reads as one more field rather than a stage.
                        #if os(macOS)
                        Color(nsColor: .controlBackgroundColor)
                            // No twist on a Mac, so the backdrop carries only the sheet dismiss
                            // the DialCatcher's tap carries on iOS.
                            .onTapGesture { ring.sheet = false }
                        #else
                        Color(.secondarySystemGroupedBackground)
                        // The backdrop runs the real twist. Its tap only dismisses the preview
                        // sheet: UIKit hands a disc tap to this view as well as to the SwiftUI
                        // button above it, so closing the ring here closed it on every pick.
                        DialCatcher(onDial: { ring.handle($0) }) { ring.sheet = false }
                        #endif
                        RingOverlay(state: ring, cfg: cfg, actions: preview,
                                    editing: RingEditing(pick: { picking = PickSlot(k: $0) }, swap: swap))
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
                Text(overridden
                     ? "\(pickVerb) a button to change it, drag one onto another to swap. "
                       + "This profile has its own quick actions; the default ring no longer reaches it."
                     : "\(pickVerb) a button to change it, drag one onto another to swap.")
            }
            #if os(iOS)
            // The virtual controller's preset and look (§4.3), written to the blob's `pad`
            // through the same binding the ring uses. Absent on the Mac: there is no touch screen
            // to draw it on, and a `pad` block written from here would configure nothing.
            Section {
                Picker("Layout", selection: Binding(get: { cfg.pad.layout }, set: { l in setPad { $0.layout = l } })) {
                    Text("Full").tag("full")
                    Text("Sticks and shoulders").tag("sticks")
                    Text("D-pad and face buttons").tag("dpad")
                }
                PadSlider(label: "Opacity", value: cfg.pad.opacity, range: VirtualPad.opacityRange,
                          caption: "How strongly the controls draw over the picture; higher hides more of the game.") { v in
                    setPad { $0.opacity = v }
                }
                PadSlider(label: "Scale", value: cfg.pad.scale, range: VirtualPad.scaleRange,
                          caption: "How large the controls are; larger ones cover more of the picture.") { v in
                    setPad { $0.scale = v }
                }
                Button("Edit layout") { editingLayout = true }
            } header: {
                Text("Virtual controller")
            } footer: {
                Text("Shown from the ring's Virtual controller button. A finger on one of its controls "
                     + "drives the game; a finger anywhere else drives the touch mode. Layout picks which "
                     + "controls it shows; fewer controls leave more of the picture uncovered. "
                     + "Edit layout moves, sizes or hides each control by hand; wide and upright screens "
                     + "keep separate layouts.")
            }
            #endif
            Section {
                ForEach(cfg.shortcuts, id: \.id) { sc in
                    Button {
                        editingShortcut = ShortcutDraft(id: sc.id, label: sc.label, keys: sc.keys, isNew: false)
                    } label: {
                        HStack(spacing: 12) {
                            KeycapDisc(keys: sc.keys, size: 40)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(sc.label.isEmpty ? chordChip(sc.keys) : sc.label)
                                if !sc.label.isEmpty {
                                    Text(chordChip(sc.keys)).font(.footnote).foregroundStyle(.secondary)
                                }
                            }
                            Spacer(minLength: 8)
                            Image(systemName: "chevron.right")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(.tertiary)
                                .accessibilityHidden(true)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
                #if os(iOS)
                .onDelete { offsets in
                    for id in offsets.map({ cfg.shortcuts[$0].id }) { remove(id) }
                }
                #endif
                Button {
                    let next = (cfg.shortcuts.compactMap { Int($0.id.dropFirst()) }.max() ?? 0) + 1
                    editingShortcut = ShortcutDraft(id: "s\(next)", label: "", keys: [], isNew: true)
                } label: {
                    Label("Add shortcut", systemImage: "plus")
                }
            } header: {
                Text("Shortcuts")
            } footer: {
                #if os(macOS)
                Text("A chord the ring sends to the host. A new one takes the first empty slot; "
                     + "click one to change or remove it.")
                #else
                Text("A chord the ring sends to the host. A new one takes the first empty slot; "
                     + "tap one to change it, swipe to remove it.")
                #endif
            }
            Section {
                Button("Reset to default", role: .destructive, action: reset)
            } footer: {
                Text("Restores the platform ring and removes the shortcuts.")
            }
        }
        #if os(macOS)
        // The settings tabs' own style, so the editor reads as one more preferences page rather
        // than the plain two-column form a bare macOS Form draws.
        .formStyle(.grouped)
        #endif
        .navigationTitle("Quick actions")
        .sheet(item: $picking) { p in
            SlotPicker(groups: groups, current: cfg.ring[p.k]?.id ?? "") { id in
                set(p.k, id)
                picking = nil
            }
            // A macOS sheet sizes to its content, and a List's content is no size at all.
            #if os(macOS)
            .frame(width: 420, height: 520)
            #endif
        }
        .sheet(item: $editingShortcut) { draft in
            ShortcutEditor(draft: draft, save: save, delete: { remove(draft.id) })
                #if os(macOS)
                .frame(width: 460, height: 600)
                #endif
        }
        #if os(iOS)
        // Full screen deliberately, not a sheet: settings live in a sheet, and on an iPad a
        // sheet is a card — its geometry (and even the wide/narrow layout class) would lie
        // about the stream the layout is for.
        .fullScreenCover(isPresented: $editingLayout) {
            PadLayoutEditor(pad: cfg.pad) { p in setPad { $0 = p } }
        }
        #endif
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
            padAvailable: { true }, padShown: { false }, togglePad: {},
            currentMode: { (1920, 1080, 60) }, requestMode: { _, _, _ in })
    }

    private var groups: [SlotGroup] {
        var g = builtinGroups
        if !cfg.shortcuts.isEmpty {
            g.append(SlotGroup(id: "Shortcuts", options: cfg.shortcuts.map {
                SlotOption(id: "shortcut:\($0.id)", label: $0.label.isEmpty ? chordChip($0.keys) : $0.label,
                           note: $0.label.isEmpty ? nil : chordChip($0.keys))
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

    private func setPad(_ change: (inout PadConfig) -> Void) {
        var c = cfg
        change(&c.pad)
        blob = c.toJSON()
    }

    private func swap(_ a: Int, _ b: Int) {
        var c = cfg
        c.ring.swapAt(a, b)
        blob = c.toJSON()
    }

    private func save(_ d: ShortcutDraft) {
        var c = cfg
        let sc = OverlayShortcut(id: d.id, label: d.label, keys: d.keys)
        if let i = c.shortcuts.firstIndex(where: { $0.id == d.id }) {
            c.shortcuts[i] = sc
        } else {
            c.shortcuts.append(sc)
            if let k = c.ring.firstIndex(where: { $0 == nil }) { c.ring[k] = .shortcut(sc.id) }
        }
        blob = c.toJSON()
    }

    private func remove(_ id: String) {
        var c = cfg
        c.shortcuts.removeAll { $0.id == id }
        // `parse` would empty a dangling slot on the next read; write it empty now so the
        // ring shows it at once.
        c.ring = c.ring.map { s in
            if case .shortcut(let sid) = s, sid == id { return nil }
            return s
        }
        blob = c.toJSON()
    }
}

/// A slider that names its value as a percentage and writes it when the finger lifts, not per frame.
private struct PadSlider: View {
    let label: String
    let value: Float
    let range: ClosedRange<Float>
    let caption: String
    let commit: (Float) -> Void
    @State private var live: Float = 0

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("\(label) · \(Int((live * 100).rounded()))%")
            Slider(value: $live, in: range) { editing in
                if !editing { commit(live) }
            }
            Text(caption).font(.footnote).foregroundStyle(.secondary)
        }
        .onAppear { live = value }
        .onChange(of: value) { _, v in live = v }
    }
}

/// A chord on a disc the size the ring draws it, for lists and the editing sheet.
private struct KeycapDisc: View {
    let keys: [String]
    var size: CGFloat = 56

    var body: some View {
        ZStack {
            Circle().fill(Color(white: 0.22))
            Circle().strokeBorder(Color.white.opacity(0.18), lineWidth: 1)
            if keys.isEmpty {
                Image(systemName: "questionmark").font(.system(size: size * 0.35, weight: .semibold))
            } else {
                ChordKeycap(keys: keys).scaleEffect(size / 56)
            }
        }
        .foregroundStyle(.white)
        .frame(width: size, height: size)
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

/// One shortcut: a name, the modifiers held as chips, the key it ends on picked from a keyboard,
/// and the disc as the ring will draw it.
private struct ShortcutEditor: View {
    @State var draft: ShortcutDraft
    let save: (ShortcutDraft) -> Void
    let delete: () -> Void
    @Environment(\.dismiss) private var dismiss

    private var mods: [String] { draft.keys.filter { modifierKeys.contains($0) } }
    private var key: String? { draft.keys.first { !modifierKeys.contains($0) } }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    HStack(spacing: 14) {
                        KeycapDisc(keys: draft.keys)
                        VStack(alignment: .leading, spacing: 3) {
                            Text(draft.label.isEmpty ? (key == nil ? "Pick a key" : chordChip(draft.keys)) : draft.label)
                                .font(.geist(15, .medium))
                            Text(key == nil ? "The disc as the ring will draw it" : chordChip(draft.keys))
                                .font(.footnote)
                                .foregroundStyle(.secondary)
                        }
                    }
                    .padding(.vertical, 4)
                    TextField("Name (optional)", text: $draft.label)
                }
                Section("Hold") {
                    HStack(spacing: 8) {
                        ForEach(modifierKeys, id: \.self) { m in
                            let on = mods.contains(m)
                            Button(keyLegend(m)) { toggle(m) }
                                .buttonStyle(.bordered)
                                .tint(on ? Color.brand : Color.secondary)
                                .fontWeight(on ? .semibold : .regular)
                                .accessibilityAddTraits(on ? .isSelected : [])
                        }
                    }
                }
                ForEach(keyGroups, id: \.title) { group in
                    Section(group.title) {
                        // Word keys (Backspace, PgDn, PrtSc) get wider cells; every cap keeps
                        // one line at one height and shrinks its text rather than growing.
                        let wide = group.keys.contains { keyLegend($0).count > 2 }
                        LazyVGrid(columns: [GridItem(.adaptive(minimum: wide ? 80 : 44), spacing: 6)], spacing: 6) {
                            ForEach(group.keys, id: \.self) { k in
                                let on = key == k
                                Button {
                                    pick(k)
                                } label: {
                                    Text(keyLegend(k))
                                        .font(.geistFixed(13, on ? .semibold : .medium))
                                        .lineLimit(1)
                                        .minimumScaleFactor(0.6)
                                        .frame(maxWidth: .infinity)
                                        .frame(height: 30)
                                }
                                .buttonStyle(.bordered)
                                .tint(on ? Color.brand : Color.secondary)
                                .accessibilityAddTraits(on ? .isSelected : [])
                            }
                        }
                        .padding(.vertical, 4)
                    }
                }
                if !draft.isNew {
                    Section {
                        Button("Remove shortcut", role: .destructive) {
                            delete()
                            dismiss()
                        }
                    }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .navigationTitle(draft.isNew ? "New shortcut" : "Shortcut")
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button(draft.isNew ? "Add" : "Save") {
                        save(draft)
                        dismiss()
                    }
                    .disabled(key == nil)
                }
            }
        }
    }

    /// Modifiers first in keyboard order, then the key — the order the chord is sent.
    private func rebuild(mods: [String], key: String?) {
        draft.keys = modifierKeys.filter { mods.contains($0) } + (key.map { [$0] } ?? [])
    }

    private func toggle(_ m: String) {
        var next = mods
        if let i = next.firstIndex(of: m) { next.remove(at: i) } else { next.append(m) }
        rebuild(mods: next, key: key)
    }

    private func pick(_ k: String) {
        rebuild(mods: mods, key: k)
    }
}
#endif
