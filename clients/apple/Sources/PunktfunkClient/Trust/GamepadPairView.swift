// The gamepad-driven PIN pairing screen (iOS/iPadOS/macOS) — the controller counterpart of
// PairSheet, and the reason a console-UI user can pair at all.
//
// PairSheet is a `Form` with two `TextField`s. On tvOS the focus engine drives those natively, but
// on iOS/macOS a controller cannot reach a text field, type into it, or press the button
// underneath — so for anyone in the console UI, pairing (the ONE thing standing between a fresh
// install and a first stream) ended at "now touch the screen". This screen is the same ceremony
// wearing the gamepad UI's own vocabulary: the vertical focus list from the settings/add-host
// screens, A on a field to open GamepadKeyboard in a bottom tray, B to peel one layer.
//
// Structure deliberately mirrors GamepadAddHostView field for field — the two screens are the same
// interaction (a short form, typed with a pad, committed by an action row) and a user who has
// added a host should recognise this immediately. The ceremony itself is shared with PairSheet
// (`PairCeremony`), so the two presentations can never disagree about what a wrong PIN means.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)

struct GamepadPairView: View {
    /// Resolved from the stored palette, NOT from `\.gamepadInk` — this screen publishes that
    /// value itself and so sits above its own copy (see `GamepadInk.stored`).
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.gamepadMetrics) private var metrics
    @Environment(\.displayBottomInset) private var displayBottomInset
    @Environment(\.dismiss) private var dismiss
    @Environment(\.gamepadHostedInShell) private var hostedInShell
    let host: StoredHost
    /// Called with the verified host fingerprint after a successful ceremony — the caller pins it
    /// and connects (ContentView's `handlePaired`).
    let onPaired: (Data) -> Void
    /// How the in-place shell (iOS) closes this screen; nil (the macOS sheet) falls back to the
    /// environment dismiss.
    var close: (() -> Void)?
    /// Whether this screen owns the controller — false while the shell is mid-transition or the
    /// connect takeover is up (see GamepadAddHostView's twin).
    var controllerActive = true

    #if os(iOS)
    /// `.compact` in a landscape phone window — tighter chrome so the keyboard tray still fits.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the sheet is sized to fit the tray
    #endif

    @StateObject private var ceremony = PairCeremony()
    @State private var pin = ""
    // Same source the connect path knocks with — see the note in `PairSheet`.
    @State private var clientName = DeviceName.current
    @State private var focusID: String?
    /// The field row the keyboard tray is editing; nil ⇒ the row list owns the controller.
    @State private var editing: String?
    /// The edited row's flight between its place in the list and its seat above the keyboard.
    @Namespace private var fieldFlight

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onActivate: { activate(id: $0.id) },
            onBack: { performClose() },
            // A ceremony in flight also takes the list out of the loop: `pair()` blocks on a
            // background thread and its result rewrites this screen, so letting B peel a layer
            // or A fire a second ceremony underneath it would race the completion.
            isActive: controllerActive && editing == nil && !ceremony.busy
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: metrics.rowMaxWidth)
                .padding(.horizontal, 24)
                // While the tray edits this row, the row IS the one seated above the keyboard
                // (see `bottomTray`); its slot here stays empty and keeps the list's layout.
                .opacity(editing == row.id ? 0 : 1)
                // The flight's origin/destination: an invisible frame-provider that exists only
                // while the row is HERE. When editing starts it unmounts and the seated row is
                // inserted with the same id, so SwiftUI animates the seated row in FROM this
                // frame; when editing ends it returns and the seated row's removal flies back to
                // it. Exactly one matched view per id at any time — two live ones with the
                // source flag swapped sent the invisible list row flying instead.
                .overlay {
                    if editing != row.id {
                        Color.clear.matchedGeometryEffect(id: row.id, in: fieldFlight)
                    }
                }
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            header
                .padding(.horizontal, 24)
                .padding(.top, gamepadTitleTopPadding(compact: compact))
                .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
                .frame(maxWidth: .infinity, alignment: .leading)
                .background { GamepadTrayBlur(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            bottomTray
                // Equal distance from the left and bottom edges for the legend pill (see
                // GamepadHomeView).
                .padding(.horizontal, compact ? 12 : 18)
                .padding(
                    .bottom,
                    gamepadLegendBottomPadding(
                        compact ? 12 : 18, tier: metrics.tier, displayBottom: displayBottomInset))
                .padding(.top, compact ? 6 : 10)
                .background { GamepadTrayBlur(edge: .bottom) }
        }
        // Hosted in the shell, the field is the shell's own (see GamepadAddHostView's twin).
        .background {
            if !hostedInShell { GamepadFormBackground() }
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a
        // pale palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
        // A PIN is short; cap it so the row can't grow absurd on a stuck key.
        .onChange(of: pin) { _, value in
            if value.count > Self.maxPINLength { pin = String(value.prefix(Self.maxPINLength)) }
        }
        // Any dismissal path abandons an in-flight ceremony — a late success must not pin and
        // connect to a host the user backed out of.
        .onDisappear { ceremony.abandon() }
        // The visible close ✕ is gone (a gamepad UI exits with B) — this keeps a hardware
        // keyboard's Esc and the macOS sheet's cancel working without chrome.
        .background {
            // Not while the keyboard tray is up: Esc is the tray's Done then (see
            // GamepadKeyboard), and a shortcut here would fire first and close the whole screen.
            if editing == nil {
                Button("Cancel") { performClose() }
                    .keyboardShortcut(.cancelAction)
                    .buttonStyle(.plain)
                    .frame(width: 0, height: 0)
                    .opacity(0)
                    .accessibilityHidden(true)
            }
        }
    }

    /// Generous next to the host's 4 digits: the PIN length is the HOST's business (a future one
    /// may well be longer), so this is a runaway guard, not a validator. Rejecting a correct PIN
    /// locally would be a far worse failure than sending a wrong one, which the host just refuses.
    private static let maxPINLength = 12

    private var header: some View {
        VStack(alignment: .leading, spacing: gamepadHeaderSpacing(compact: compact)) {
            // Leading, like every gamepad heading — and no close chrome (B is the exit).
            Text("Pair with \(host.displayName)")
                .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            if !compact {
                Text("The PIN is shown in the host's web console (port 47992 → Pairing). "
                    + "Pairing verifies both sides at once — no fingerprint comparison needed.")
                    .font(.geist(metrics.detailFont, relativeTo: .caption))
                    .foregroundStyle(ink.fg(0.55))
                    .multilineTextAlignment(.leading)
                    .frame(maxWidth: metrics.rowMaxWidth * 0.72, alignment: .leading)
            }
        }
    }

    /// The keyboard tray while editing, the status line + controls legend otherwise.
    @ViewBuilder private var bottomTray: some View {
        if let editing {
            VStack(spacing: 10) {
                // The edited row sits directly above the keys, flown in from the list (see
                // GamepadAddHostView's twin) — what the keyboard covers no longer matters.
                if let row = rows.first(where: { $0.id == editing }) {
                    rowView(row, focused: true)
                        .frame(maxWidth: metrics.rowMaxWidth)
                        .padding(.horizontal, 24)
                        .matchedGeometryEffect(id: row.id, in: fieldFlight)
                        .frame(maxWidth: .infinity)
                        .transition(.opacity)
                }
                VStack(spacing: 10) {
                    GamepadKeyboard(
                        text: editingBinding(editing),
                        allowed: allowedCharacters(editing),
                        onDone: { closeKeyboard() })
                        // Fresh keyboard per field (see GamepadAddHostView) — the tray's input
                        // wiring captured the previous binding on appear.
                        .id(editing)
                    GamepadHintBar(hints: [
                        .init(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Type"),
                        .init(
                            glyph: buttonGlyph(\.buttonX, fallback: "x.circle"), text: "Delete",
                            action: { backspace(editing) }),
                        .init(
                            glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done",
                            action: { closeKeyboard() }),
                    ])
                    .frame(maxWidth: .infinity, alignment: .leading)
                }
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        } else {
            VStack(alignment: .leading, spacing: 8) {
                statusLine
                GamepadHintBar(hints: [
                    .init(
                        glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Select",
                        action: { if let focusID { activate(id: focusID) } }),
                    .init(
                        glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Cancel",
                        action: { performClose() }),
                ])
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// What the ceremony is doing, in the slot the settings screen gives its detail line. Reserves
    /// its space so the legend never jumps when a failure arrives.
    @ViewBuilder private var statusLine: some View {
        Group {
            if ceremony.busy {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small).tint(ink.fg(0.7))
                    Text("Pairing with \(host.displayName)…").foregroundStyle(ink.fg(0.7))
                }
            } else if let error = ceremony.errorText {
                Text(error).foregroundStyle(.red)
            } else {
                // Placeholder keeps the reserved height honest under `lineLimit(2)`.
                Text(" ").foregroundStyle(.clear)
            }
        }
        .font(.geist(metrics.detailFont, relativeTo: .caption))
        .lineLimit(2, reservesSpace: true)
        .multilineTextAlignment(.leading)
        .frame(maxWidth: metrics.rowMaxWidth, alignment: .leading)
        .animation(.smooth(duration: 0.2), value: ceremony.errorText)
    }

    /// Close this screen through whichever mechanism presents it: the shell's layer pop on iOS,
    /// the environment dismiss under a macOS sheet.
    private func performClose() {
        ceremony.abandon()
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
            Row(id: "pin", label: "PIN", value: pin, placeholder: "Shown in the web console"),
            Row(
                id: "name", label: "Device name", value: clientName,
                placeholder: "How the host lists this device"),
            Row(id: "pair", label: "Pair & Connect", isAction: true),
        ]
    }

    private func rowView(_ row: Row, focused: Bool) -> some View {
        let m = metrics
        return HStack(spacing: 14) {
            if row.isAction {
                Label("Pair & Connect", systemImage: "lock.shield")
                    .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                    .foregroundStyle(canPair ? ink.accent : ink.fg(0.35))
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
                    .truncationMode(.head) // keep the end of a long name visible while typing
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
        guard !ceremony.busy else { return }
        switch id {
        case "pair":
            guard canPair else {
                // Not pairable yet — jump straight to what's missing instead of a dead press,
                // matching the add-host screen's Add row.
                focusID = "pin"
                openKeyboard("pin")
                return
            }
            ceremony.run(host: host.address, port: host.port, pin: pin, clientName: clientName) {
                fingerprint in
                onPaired(fingerprint)
                // NOT `performClose()`: that abandons the ceremony, and this IS the ceremony's
                // success. Closing is all that's left to do.
                if let close { close() } else { dismiss() }
            }
        default:
            openKeyboard(id)
        }
    }

    private var canPair: Bool {
        !pin.trimmingCharacters(in: .whitespaces).isEmpty && !ceremony.busy
    }

    private func openKeyboard(_ id: String) {
        withAnimation(.spring(response: 0.32, dampingFraction: 0.86)) { editing = id }
    }

    private func closeKeyboard() {
        withAnimation(.spring(response: 0.32, dampingFraction: 0.86)) { editing = nil }
    }

    private func editingBinding(_ id: String) -> Binding<String> {
        id == "pin" ? $pin : $clientName
    }

    /// The legend's Delete cell — see GamepadAddHostView's twin for why this edits the binding
    /// rather than reaching into the keyboard.
    private func backspace(_ id: String) {
        let binding = editingBinding(id)
        guard !binding.wrappedValue.isEmpty else { return }
        binding.wrappedValue.removeLast()
    }

    /// What the keyboard may type per field: a PIN is digits; a device name is free-form.
    private func allowedCharacters(_ id: String) -> CharacterSet? {
        id == "pin" ? CharacterSet(charactersIn: "0123456789") : nil
    }
}
#endif
