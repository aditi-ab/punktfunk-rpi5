// The gamepad-driven "Add Host" screen (iOS/iPadOS/macOS/tvOS) — the controller counterpart of
// AddHostSheet, reached from the launcher's Add Host tile. Three field rows (name / address /
// port) plus the Add action, navigated with the same vertical focus list as the gamepad settings;
// A on a field opens GamepadKeyboard in a bottom tray, so a host can be registered end to end
// without touching the screen. Field edits are live (the row shows every keystroke); B closes the
// keyboard first, then cancels the screen — the same "back peels one layer" rule as a console UI.
// tvOS swaps the custom keyboard tray for the SYSTEM fullscreen keyboard (TVTextEntry): unlike
// iOS/macOS, tvOS HAS a first-class controller/remote-drivable text entry, so the native one wins.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

struct GamepadAddHostView: View {
    @Environment(\.gamepadInk) private var ink
    @Environment(\.dismiss) private var dismiss
    @Environment(\.gamepadHostedInShell) private var hostedInShell
    let onAdd: (StoredHost) -> Void
    /// How the in-place shell (iOS) closes this screen; nil (the macOS sheet, the tvOS cover)
    /// falls back to the environment dismiss. Declared AFTER `onAdd` so the existing trailing-
    /// closure call sites keep binding to it, not to this.
    var close: (() -> Void)?
    /// Whether this screen owns the controller — false while the shell is mid-transition or the
    /// connect takeover is up (see GamepadSettingsView's twin).
    var controllerActive = true

    #if os(iOS)
    /// `.compact` in a landscape phone window — tighter chrome so the keyboard tray still fits.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the sheet is sized to fit the tray
    #endif
    @State private var name = ""
    @State private var address = ""
    @State private var port = "9777"
    @State private var focusID: String?
    /// The field row the keyboard tray is editing; nil ⇒ the row list owns the controller.
    @State private var editing: String?

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onActivate: { activate(id: $0.id) },
            onBack: { performClose() },
            isActive: controllerActive && editing == nil
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: GamepadFormMetrics.rowMaxWidth)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            VStack(alignment: .leading, spacing: gamepadHeaderSpacing(compact: compact)) {
                // Leading, like every gamepad heading — and no close chrome (B is the exit).
                Text("Add Host")
                    .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                    .foregroundStyle(ink.fg)
                if !compact {
                    Text("Hosts on this network appear automatically — add one by address "
                        + "for everything else.")
                        .font(.geist(GamepadFormMetrics.detailFont, relativeTo: .caption))
                        .foregroundStyle(ink.fg(0.55))
                        .multilineTextAlignment(.leading)
                        .frame(maxWidth: GamepadFormMetrics.rowMaxWidth * 0.72, alignment: .leading)
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, gamepadTitleTopPadding(compact: compact))
            .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { GamepadTrayScrim(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            bottomTray
                // Equal distance from the left and bottom edges for the legend pill (see GamepadHomeView).
                .padding(.horizontal, compact ? 12 : 18)
                .padding(.bottom, compact ? 12 : 18)
                .padding(.top, compact ? 6 : 10)
                .background { GamepadTrayScrim(edge: .bottom) }
        }
        // No aurora — the same clean Liquid-Glass-over-dark base as the gamepad settings screen.
        // Hosted in the shell, the field is the shell's (see GamepadSettingsView's twin).
        .background {
            if !hostedInShell { GamepadFormBackground() }
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a
        // pale palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
        // A port can't exceed 5 digits — cap while typing so the row can't grow absurd.
        .onChange(of: port) { _, value in
            if value.count > 5 { port = String(value.prefix(5)) }
        }
        #if !os(tvOS)
        // The visible close ✕ is gone (a gamepad UI exits with B) — this keeps a hardware
        // keyboard's Esc and the macOS sheet's cancel working without chrome.
        .background {
            Button("Cancel") { performClose() }
                .keyboardShortcut(.cancelAction)
                .buttonStyle(.plain)
                .frame(width: 0, height: 0)
                .opacity(0)
                .accessibilityHidden(true)
        }
        #endif
        #if os(tvOS)
        // tvOS types with the SYSTEM fullscreen keyboard (TVTextEntry) instead of the custom
        // tray — the remote and the pad both drive it natively. Same `editing` state as the
        // tray, just a different presentation; done (or Menu, edits-stick) commits and returns.
        .fullScreenCover(isPresented: Binding(
            get: { editing != nil },
            set: { if !$0 { editing = nil } })
        ) {
            if let field = editing {
                TVTextEntry(
                    title: fieldTitle(field),
                    text: editingBinding(field).wrappedValue,
                    keyboardType: keyboardType(field)
                ) { value in
                    commitEntry(field, value)
                    editing = nil
                }
            }
        }
        #endif
    }

    /// The keyboard tray while editing, the controls legend otherwise. (tvOS never shows the
    /// tray — `editing` presents the system keyboard cover instead — so it's legend-only there.)
    @ViewBuilder private var bottomTray: some View {
        #if os(tvOS)
        GamepadHintBar(hints: [
            .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Select"),
            .init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Cancel"),
        ])
        .frame(maxWidth: .infinity, alignment: .leading)
        #else
        if let editing {
            VStack(spacing: 10) {
                GamepadKeyboard(
                    text: editingBinding(editing),
                    allowed: allowedCharacters(editing),
                    onDone: { closeKeyboard() })
                    // Fresh keyboard per field: a touch user can retarget the tray by tapping
                    // another field row, and the keyboard's input wiring captured the previous
                    // binding on appear — new identity forces a rewire to the new field.
                    .id(editing)
                GamepadHintBar(hints: [
                    .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Type"),
                    .init(glyph: buttonGlyph(\.buttonX, fallback: "x.circle"), text: "Delete"),
                    .init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done"),
                ])
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .transition(.move(edge: .bottom).combined(with: .opacity))
        } else {
            GamepadHintBar(hints: [
                .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Select"),
                .init(glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Cancel"),
            ])
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        #endif
    }

    /// Close this screen through whichever mechanism presents it: the shell's layer pop on iOS,
    /// the environment dismiss under a macOS sheet / tvOS cover.
    private func performClose() {
        if let close { close() } else { dismiss() }
    }

    // MARK: - Rows

    private struct Row: Identifiable {
        let id: String
        let label: String
        var value = ""
        var placeholder = ""
        var isAction = false
    }

    private var rows: [Row] {
        [
            Row(id: "name", label: "Name", value: name, placeholder: "Optional — e.g. Living Room"),
            Row(id: "address", label: "Address", value: address, placeholder: "IP or hostname"),
            Row(id: "port", label: "Port", value: port, placeholder: "9777"),
            Row(id: "add", label: "Add Host", isAction: true),
        ]
    }

    private func rowView(_ row: Row, focused: Bool) -> some View {
        let m = GamepadFormMetrics.self
        return HStack(spacing: 14) {
            if row.isAction {
                Label("Add Host", systemImage: "plus.circle.fill")
                    .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                    .foregroundStyle(canAdd ? ink.accent : ink.fg(0.35))
                    .frame(maxWidth: .infinity)
            } else {
                Text(row.label)
                    .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                    .foregroundStyle(ink.fg)
                Spacer(minLength: 12)
                Text(row.value.isEmpty ? row.placeholder : row.value)
                    .font(.geistFixed(m.valueFont, .medium))
                    .foregroundStyle(row.value.isEmpty ? ink.fg(0.35) : ink.fg)
                    .lineLimit(1)
                    .truncationMode(.head) // keep the end of a long address visible while typing
                if editing == row.id {
                    // The live-edit caret: this row is what the keyboard tray is typing into.
                    Rectangle()
                        .fill(ink.accent)
                        .frame(width: 2, height: m.labelFont + 2)
                }
            }
        }
        .padding(.horizontal, m.rowHPad)
        .padding(.vertical, m.rowVPad)
        // Liquid Glass rows, matching the settings screen; the focused (or actively edited) row
        // takes the brand wash, and the edited row keeps its brand caret border.
        .consoleGlass(
            RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous),
            tint: (focused || editing == row.id) ? ink.accent(0.30) : nil,
            interactive: focused)
        .overlay {
            RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                .strokeBorder(
                    editing == row.id ? ink.accent(0.7) : ink.fg(focused ? 0.28 : 0.06),
                    lineWidth: 1)
        }
        .scaleEffect(focused ? 1.0 : 0.98)
        .animation(.smooth(duration: 0.18), value: focused)
    }

    // MARK: - Actions

    private func activate(id: String) {
        switch id {
        case "add":
            guard canAdd else {
                // Not addable yet — jump straight to what's missing instead of a dead press.
                focusID = "address"
                openKeyboard("address")
                return
            }
            onAdd(StoredHost(
                name: name.trimmingCharacters(in: .whitespaces),
                address: address.trimmingCharacters(in: .whitespaces),
                port: UInt16(port) ?? 9777))
            performClose()
        default:
            openKeyboard(id)
        }
    }

    private var canAdd: Bool {
        !address.trimmingCharacters(in: .whitespaces).isEmpty
            && UInt16(port).map { $0 > 0 } == true
    }

    private func openKeyboard(_ id: String) {
        withAnimation(.spring(response: 0.32, dampingFraction: 0.86)) { editing = id }
    }

    private func closeKeyboard() {
        withAnimation(.spring(response: 0.32, dampingFraction: 0.86)) { editing = nil }
    }

    private func editingBinding(_ id: String) -> Binding<String> {
        switch id {
        case "name": return $name
        case "port": return $port
        default: return $address
        }
    }

    /// What the keyboard may type per field: a port is digits, an address never contains spaces;
    /// a name is free-form.
    private func allowedCharacters(_ id: String) -> CharacterSet? {
        switch id {
        case "port": return CharacterSet(charactersIn: "0123456789")
        case "address": return CharacterSet(charactersIn: " ").inverted
        default: return nil
        }
    }

    #if os(tvOS)
    // MARK: - System keyboard plumbing (see the fullScreenCover on `body`)

    private func fieldTitle(_ id: String) -> String {
        switch id {
        case "name": return "Name (optional)"
        case "port": return "Port"
        default: return "Address (IP or hostname)"
        }
    }

    /// .URL for the address (dots on the primary layer, no autocapitalize) — the closest tvOS
    /// keyboard to "hostname or IP".
    private func keyboardType(_ id: String) -> UIKeyboardType {
        switch id {
        case "port": return .numberPad
        case "address": return .URL
        default: return .default
        }
    }

    /// Apply a system-keyboard result, enforcing what `allowedCharacters` enforces per keystroke
    /// on the other platforms (the system keyboard will type anything).
    private func commitEntry(_ id: String, _ value: String) {
        switch id {
        case "port":
            editingBinding(id).wrappedValue = String(value.filter(\.isNumber).prefix(5))
        case "address":
            editingBinding(id).wrappedValue = value
                .replacingOccurrences(of: " ", with: "")
        default:
            editingBinding(id).wrappedValue = value
        }
    }
    #endif
}
#endif
