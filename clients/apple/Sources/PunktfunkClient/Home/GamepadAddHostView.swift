// The gamepad-driven "Add Host" screen (iOS/iPadOS/macOS) — the controller counterpart of
// AddHostSheet, reached from the launcher's Add Host tile. Three field rows (name / address /
// port) plus the Add action, navigated with the same vertical focus list as the gamepad settings;
// A on a field opens GamepadKeyboard in a bottom tray, so a host can be registered end to end
// without touching the screen. Field edits are live (the row shows every keystroke); B closes the
// keyboard first, then cancels the screen — the same "back peels one layer" rule as a console UI.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)

struct GamepadAddHostView: View {
    @Environment(\.dismiss) private var dismiss
    let onAdd: (StoredHost) -> Void

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
            onBack: { dismiss() },
            isActive: editing == nil
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: 620)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            VStack(spacing: 4) {
                Text("Add Host")
                    .font(.geist(compact ? 20 : 30, .bold, relativeTo: .title))
                    .foregroundStyle(.white)
                if !compact {
                    Text("Hosts on this network appear automatically — add one by address "
                        + "for everything else.")
                        .font(.geist(13, relativeTo: .caption))
                        .foregroundStyle(.white.opacity(0.55))
                        .multilineTextAlignment(.center)
                        .frame(maxWidth: 440)
                }
            }
            .padding(.top, gamepadTitleTopPadding(compact: compact))
            .padding(.bottom, compact ? 4 : 8)
            .frame(maxWidth: .infinity)
            .overlay(alignment: .topTrailing) { closeButton.padding(.trailing, 20) }
            .background { GamepadTrayScrim(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            bottomTray
                .padding(.horizontal, 22)
                .padding(.vertical, compact ? 6 : 10)
                .background { GamepadTrayScrim(edge: .bottom) }
        }
        .background { GamepadScreenBackground() }
        // A port can't exceed 5 digits — cap while typing so the row can't grow absurd.
        .onChange(of: port) { _, value in
            if value.count > 5 { port = String(value.prefix(5)) }
        }
    }

    /// The keyboard tray while editing, the controls legend otherwise.
    @ViewBuilder private var bottomTray: some View {
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
    }

    /// Touch/click fallback for closing — the controller path is B, a hardware keyboard's Esc
    /// rides the cancel action.
    private var closeButton: some View {
        Button { dismiss() } label: {
            Image(systemName: "xmark")
                .font(.system(size: 14, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 34, height: 34)
                .glassBackground(Circle(), interactive: true)
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .keyboardShortcut(.cancelAction)
        .accessibilityLabel("Cancel")
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
        HStack(spacing: 14) {
            if row.isAction {
                Label("Add Host", systemImage: "plus.circle.fill")
                    .font(.geist(16, .semibold, relativeTo: .body))
                    .foregroundStyle(canAdd ? Color.brand : .white.opacity(0.35))
                    .frame(maxWidth: .infinity)
            } else {
                Text(row.label)
                    .font(.geist(16, .semibold, relativeTo: .body))
                    .foregroundStyle(.white)
                Spacer(minLength: 12)
                Text(row.value.isEmpty ? row.placeholder : row.value)
                    .font(.geistFixed(15, .medium))
                    .foregroundStyle(row.value.isEmpty ? .white.opacity(0.35) : .white)
                    .lineLimit(1)
                    .truncationMode(.head) // keep the end of a long address visible while typing
                if editing == row.id {
                    // The live-edit caret: this row is what the keyboard tray is typing into.
                    Rectangle()
                        .fill(Color.brand)
                        .frame(width: 2, height: 18)
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 13)
        .background {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(.white.opacity(focused || editing == row.id ? 0.1 : 0))
        }
        .overlay {
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .strokeBorder(
                    editing == row.id ? Color.brand.opacity(0.7) : .white.opacity(focused ? 0.22 : 0),
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
            dismiss()
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
}
#endif
