// A saved host's own actions — Wake, Copy link, Edit…, Forget pairing, Remove — reached with UP on
// its carousel tile. The console's answer to the overflow menu the touch grid hangs off every host
// card (HostCardView's context menu), and the Apple port of `pf-console-ui`'s HostOptionsScreen.
//
// Until now the gamepad UI could add a host and connect to one, and that was all: a renamed machine
// or a host typed in with a fat-fingered address stayed wrong forever, because the only surface
// that could edit or remove one was the touch UI. The tile is where a host is, so the tile is where
// its actions belong.
//
// UP is the gesture because the carousel is horizontal — left/right are spoken for and up is free —
// and because the desktop console and the Android console already do exactly this, so the three are
// learned once. A pinned profile card offers only Unpin: it is a shortcut, not a second host, and
// offering to remove the host from it would blur precisely the distinction a pin exists to draw.
//
// Vocabulary note: this screen says "Forget pairing" and "Remove host" where the desktop console
// says one word, "Forget". The console has only the one action; Apple has both (HostCardView calls
// them `onForget` = drop the pinned fingerprint and `onRemove` = delete the record), and two
// different actions cannot share a name on the surface that offers both. The touch card's words
// win over the other consoles' here — a user meets both Apple surfaces, and only one of them is
// cross-platform.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

/// Which card the menu was opened on. Carries the host BY VALUE for the same reason the screen
/// does — the carousel is rebuilt on every discovery pass, and a target that re-resolved itself
/// could hand "Remove" a different host than the one the user was looking at.
struct HostOptionsTarget: Identifiable {
    let host: StoredHost
    /// Non-nil ⇒ a pinned profile card rather than the host's own tile.
    var profile: StreamProfile?

    /// Keyed on the CARD, not the host: a host and each of its pinned cards open different menus,
    /// and sharing an id would let one stand in for another mid-transition (the same rule
    /// `GamepadScreen.library` follows).
    var id: String { "\(host.id.uuidString)-\(profile?.id ?? "")" }
}

struct GamepadHostOptionsView: View {
    /// Resolved from the stored palette, NOT from `\.gamepadInk` — this screen publishes that
    /// value itself and so sits above its own copy (see `GamepadInk.stored`).
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.gamepadMetrics) private var metrics
    @Environment(\.displayBottomInset) private var displayBottomInset
    @Environment(\.gamepadHostedInShell) private var hostedInShell
    @Environment(\.dismiss) private var dismiss

    /// The host this menu was opened on, BY VALUE. Discovery rewrites the carousel on every
    /// service pass; holding an index or a live lookup would let the menu retarget itself onto
    /// whichever host slid into that slot, and "Remove" must never be able to do that.
    let host: StoredHost
    /// Non-nil ⇒ opened on a pinned profile card rather than the host's own tile.
    var pinnedProfile: StreamProfile?
    /// Whether the host is reachable right now — decides whether Wake is worth offering.
    var isOnline = false
    /// Whether waking is possible at all (the setting is on, WoL is available, a MAC is known).
    var canWake = false
    let onEdit: () -> Void
    let onWake: () -> Void
    /// Drop the pinned fingerprint — the host stays saved, and the next connect re-pairs.
    let onForgetPairing: () -> Void
    /// Delete the saved record outright.
    let onRemove: () -> Void
    let onUnpin: () -> Void
    var close: (() -> Void)?
    var controllerActive = true

    #if os(iOS)
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false
    #endif

    /// Removing is the one action here with no undo, so its row ARMS on the first press and only
    /// fires on the second. The touch grid removes behind a system confirmation dialog; a console
    /// is driven by a thumbstick from across a room, which is a good reason to be at least as
    /// strict as it is, and none at all to be looser.
    @State private var armed = false
    @State private var copied = false
    @State private var focusID: String?

    private enum Action: String {
        case wake
        case copyLink
        case edit
        case forgetPairing
        case remove
        case unpin
        case cancel
    }

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onActivate: { run($0.action) },
            onBack: { performClose() },
            isActive: controllerActive
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: metrics.rowMaxWidth)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            VStack(alignment: .leading, spacing: gamepadHeaderSpacing(compact: compact)) {
                Text(title)
                    .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                    .foregroundStyle(ink.fg)
                    .lineLimit(1)
                if !compact {
                    Text("\(host.address):\(String(host.port))")
                        .font(.geistFixed(metrics.detailFont, .medium))
                        .foregroundStyle(ink.fg(0.55))
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, gamepadTitleTopPadding(compact: compact))
            .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { GamepadTrayBlur(edge: .top) }
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Text(detail)
                    .font(.geist(metrics.detailFont, relativeTo: .caption))
                    .foregroundStyle(ink.fg(0.55))
                    .lineLimit(2, reservesSpace: true)
                    .animation(.smooth(duration: 0.2), value: focusID)
                GamepadHintBar(hints: hints)
            }
            .padding(.leading, compact ? 12 : 18)
            .padding(.trailing, 22)
            .padding(
                .bottom,
                gamepadLegendBottomPadding(
                    compact ? 12 : 18, tier: metrics.tier, displayBottom: displayBottomInset))
            .padding(.top, compact ? 6 : 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background { GamepadTrayBlur(edge: .bottom) }
        }
        .background {
            if !hostedInShell { GamepadFormBackground() }
        }
        .gamepadPaletteInk()
        // Moving the focus off the armed Remove row disarms it: an arming that outlives the row it
        // was made on is a trap, and the thumb that wandered away is exactly the hesitation the
        // two-press rule exists to catch.
        .onChange(of: focusID) { _, id in
            if id != Action.remove.rawValue { armed = false }
        }
        #if !os(tvOS)
        .background {
            Button("Cancel") { performClose() }
                .keyboardShortcut(.cancelAction)
                .buttonStyle(.plain)
                .frame(width: 0, height: 0)
                .opacity(0)
                .accessibilityHidden(true)
        }
        #endif
    }

    private var title: String {
        pinnedProfile.map { "\(host.displayName) · \($0.name)" } ?? host.displayName
    }

    // MARK: - Rows

    private struct Row: Identifiable {
        let action: Action
        let label: String
        var icon: String
        var isDestructive = false
        var id: String { action.rawValue }
    }

    private var rows: [Row] {
        // A pinned card is a shortcut, not a host: everything host-level is deliberately absent.
        if pinnedProfile != nil {
            return [
                Row(action: .unpin, label: "Unpin card", icon: "pin.slash"),
                Row(action: .copyLink, label: copied ? "Copied" : "Copy link", icon: "link"),
                Row(action: .cancel, label: "Cancel", icon: "xmark"),
            ]
        }
        var list: [Row] = []
        // Waking a host that is already answering would just sit there counting seconds.
        if canWake, !isOnline {
            list.append(Row(action: .wake, label: "Wake host", icon: "power"))
        }
        list.append(Row(action: .copyLink, label: copied ? "Copied" : "Copy link", icon: "link"))
        list.append(Row(action: .edit, label: "Edit\u{2026}", icon: "pencil"))
        // Only a paired host has a pairing to drop.
        if host.pinnedSHA256 != nil {
            list.append(Row(
                action: .forgetPairing, label: "Forget pairing", icon: "lock.open"))
        }
        list.append(Row(
            action: .remove,
            label: armed ? "Remove host \u{2014} press again" : "Remove host",
            icon: "trash", isDestructive: true))
        list.append(Row(action: .cancel, label: "Cancel", icon: "xmark"))
        return list
    }

    /// The explainer under the list — the same band the settings screen uses, and the only place a
    /// destructive action can say what it will actually do before it is pressed.
    private var detail: String {
        switch rows.first(where: { $0.id == focusID })?.action {
        case .wake:
            return "Send a Wake-on-LAN packet and wait for this host to answer."
        case .copyLink:
            return "Copy a punktfunk:// link to this host — paste it anywhere to connect."
        case .edit:
            return "Rename this host or change its address. Pairing and pinned cards are kept."
        case .forgetPairing:
            return "Drop the stored fingerprint. The host stays saved and the next connect "
                + "pairs again."
        case .remove:
            return armed
                ? "Press again to remove — this cannot be undone."
                : "Delete this host, its pairing and its pinned cards from this device."
        case .unpin:
            return "Remove this profile's card. The profile itself and the host are untouched."
        case .cancel, .none:
            return ""
        }
    }

    private var hints: [GamepadHint] {
        [
            .init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Select",
                action: { if let id = focusID, let row = rows.first(where: { $0.id == id }) {
                    run(row.action)
                } }),
            .init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
                action: { performClose() }),
        ]
    }

    // MARK: - Actions

    private func run(_ action: Action) {
        switch action {
        case .wake:
            onWake()
            performClose()
        case .copyLink:
            LinkClipboard.copy(
                DeepLink.forHost(host, profile: pinnedProfile?.id).urlString)
            // No toast machinery on this surface — the row says so itself, which is the same
            // acknowledgement in the place the user is already looking.
            withAnimation(.smooth(duration: 0.2)) { copied = true }
        case .edit:
            onEdit()
        case .forgetPairing:
            onForgetPairing()
            performClose()
        case .remove:
            guard armed else {
                withAnimation(.smooth(duration: 0.2)) { armed = true }
                return
            }
            onRemove()
            performClose()
        case .unpin:
            onUnpin()
            performClose()
        case .cancel:
            performClose()
        }
    }

    private func performClose() {
        if let close { close() } else { dismiss() }
    }

    // MARK: - Row rendering

    private func rowView(_ row: Row, focused: Bool) -> some View {
        let m = metrics
        // The destructive row wears the warning colour only once ARMED: red on a row that still
        // needs a second press reads as "this already happened".
        let danger = row.isDestructive && armed
        return HStack(spacing: 14) {
            Image(systemName: row.icon)
                .font(.system(size: m.iconFont))
                .foregroundStyle(
                    danger ? GamepadInk.warningRed : (focused ? ink.accent : ink.fg(0.55)))
                .frame(width: m.iconWidth)
            Text(row.label)
                .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                .foregroundStyle(danger ? GamepadInk.warningRed : ink.fg)
                .lineLimit(1)
            Spacer(minLength: 12)
        }
        .padding(.horizontal, m.rowHPad)
        .padding(.vertical, m.rowVPad)
        .consoleGlass(
            RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous),
            tint: focused ? (danger ? GamepadInk.warningRed.opacity(0.3) : ink.accent(0.30)) : nil,
            interactive: focused)
        .overlay {
            RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                .strokeBorder(
                    danger ? GamepadInk.warningRed.opacity(0.7) : ink.fg(focused ? 0.28 : 0.06),
                    lineWidth: 1)
        }
        .scaleEffect(focused ? 1.0 : 0.98)
        .animation(.smooth(duration: 0.18), value: focused)
        .animation(.smooth(duration: 0.18), value: armed)
    }
}
#endif
