// A library title's own actions — reached with X on the shelf or the grid: the desktop console's
// per-title Options menu (`screens/options.rs`, `Subject::Game`), and the console answer to the
// touch grid's context menu on a poster. It holds Copy link and Cancel — deliberately not
// [Play, …]: the menu does not repeat the field's own A press, and Copy link leads so the cursor,
// which starts on row 0, is already on the row nearly everyone came for. Rendered as a layer over
// the field inside the library screen (it takes the controller while it is up), in the host
// options menu's idiom: a title band, glass rows, an explainer band, `A Select · B Back`.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

struct LibraryTitleOptionsView: View {
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.gamepadMetrics) private var metrics
    @Environment(\.displayBottomInset) private var displayBottomInset

    /// The title this menu was opened on, by value.
    let game: GameEntry
    /// The host's name, for the explainer ("Actions for this title on {host}").
    var hostName: String?
    /// Copy the title's `punktfunk://` link; nil where there is no clipboard (the row is then
    /// omitted, and the menu is Cancel alone — the caller hides X in that case anyway).
    var onCopyLink: ((GameEntry) -> Void)?
    let close: () -> Void
    var controllerActive = true

    #if os(iOS)
    @Environment(\.verticalSizeClass) private var vSizeClass
    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false
    #endif
    @State private var copied = false
    @State private var focusID: String?

    private enum Action: String {
        case copyLink
        case cancel
    }

    private struct Row: Identifiable {
        let action: Action
        let label: String
        let icon: String
        var id: String { action.rawValue }
    }

    private var rows: [Row] {
        var rows: [Row] = []
        if onCopyLink != nil {
            rows.append(Row(action: .copyLink, label: copied ? "Copied" : "Copy link", icon: "link"))
        }
        rows.append(Row(action: .cancel, label: "Cancel", icon: "xmark"))
        return rows
    }

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onActivate: { run($0.action) },
            onBack: { close() },
            isActive: controllerActive
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: metrics.rowMaxWidth)
                .padding(.horizontal, 24)
        }
        .frame(maxWidth: .infinity)
        .safeAreaInset(edge: .top, spacing: 0) {
            VStack(alignment: .leading, spacing: gamepadHeaderSpacing(compact: compact)) {
                Text(game.title)
                    .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                    .foregroundStyle(ink.fg)
                    .lineLimit(1)
                if !compact {
                    Text(blurb)
                        .font(.geist(metrics.detailFont, relativeTo: .caption))
                        .foregroundStyle(ink.fg(0.55))
                        .lineLimit(1)
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
        .task(id: copied) {
            guard copied else { return }
            try? await Task.sleep(for: .milliseconds(1600))
            withAnimation(.smooth(duration: 0.2)) { copied = false }
        }
    }

    /// The desktop's per-subject blurb: "Actions for this title on {host}."
    private var blurb: String {
        if let hostName, !hostName.isEmpty { return "Actions for this title on \(hostName)." }
        return "Actions for this title."
    }

    private var detail: String {
        switch rows.first(where: { $0.id == focusID })?.action {
        case .copyLink:
            return "Copy a punktfunk:// link that opens straight into this title."
        case .cancel, .none:
            return ""
        }
    }

    private var hints: [GamepadHint] {
        [
            .init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Select",
                action: {
                    if let id = focusID, let row = rows.first(where: { $0.id == id }) { run(row.action) }
                }),
            .init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
                action: { close() }),
        ]
    }

    private func run(_ action: Action) {
        switch action {
        case .copyLink:
            onCopyLink?(game)
            // No toast machinery on this surface — the row says so itself.
            withAnimation(.smooth(duration: 0.2)) { copied = true }
        case .cancel:
            close()
        }
    }

    private func rowView(_ row: Row, focused: Bool) -> some View {
        let m = metrics
        return HStack(spacing: 14) {
            Image(systemName: row.icon)
                .font(.system(size: m.iconFont))
                .foregroundStyle(focused ? ink.accent : ink.fg(0.55))
                .frame(width: m.iconWidth)
            Text(row.label)
                .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
            Spacer(minLength: 12)
        }
        .padding(.horizontal, m.rowHPad)
        .padding(.vertical, m.rowVPad)
        .consoleGlass(
            RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous),
            tint: focused ? ink.accent(0.30) : nil,
            interactive: focused)
        .overlay {
            RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                .strokeBorder(ink.fg(focused ? 0.28 : 0.06), lineWidth: 1)
        }
        .scaleEffect(focused ? 1.0 : 0.98)
        .animation(.smooth(duration: 0.18), value: focused)
    }
}
#endif
