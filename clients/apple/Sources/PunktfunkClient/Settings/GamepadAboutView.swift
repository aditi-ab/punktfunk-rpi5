// The gamepad UI's About page — the controller-first counterpart of `AboutView`, reached from the
// settings screen's About tab. Same content in the same order as the touch page (icon, name,
// version, then the ways out: documentation, community, source, licenses), restyled as a console
// screen and navigable with a stick: the identity card is the header, everything else is a row.
//
// It also carries the SHORTCUTS reference, which is why this page exists now rather than whenever
// an About page happened to get written. The controls used to be announced by a 6-second banner at
// the start of every session (ContentView's `showShortcutHint`) — a message that interrupts you
// once, while you are looking at the stream you just opened, and is unavailable forever after.
// Moving the words here trades a notification for a destination: nothing flashes over the stream,
// and the answer is reachable at the moment the question is actually asked.
//
// Both sub-pages are IN-PLACE layers, not pushes — the same "one layer, B peels it back" rule the
// settings pin picker and GamepadAddHostView follow, so the focus list's controller wiring and the
// tvOS focus engine carry over untouched.
//
// Acknowledgements re-uses `Licenses.chunked` — the chunking that exists so tvOS can page a
// ~885 KB license wall by focus steps. Fed to `GamepadMenuList` as one row per chunk, the wall
// scrolls with the stick on every platform and needs no scrolling machinery of its own.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)

struct GamepadAboutView: View {
    /// Resolved from the stored palette, NOT from `\.gamepadInk` — this screen publishes that
    /// value itself and so sits above its own copy (see `GamepadInk.stored`).
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    @Environment(\.gamepadMetrics) private var metrics
    @Environment(\.displayBottomInset) private var displayBottomInset
    @Environment(\.gamepadHostedInShell) private var hostedInShell
    @Environment(\.openURL) private var openURL
    /// How the host screen closes this one; nil falls back to the environment dismiss.
    var close: (() -> Void)?
    /// Whether this screen owns the controller — false while the shell is mid-transition.
    var controllerActive = true
    /// Gates the mic row of the shortcuts reference, exactly as the old banner gated it.
    var micAvailable = true
    @Environment(\.dismiss) private var dismiss

    /// Which layer the row list is showing. Depth is 1 by construction: neither sub-page opens
    /// anything further, so there is no stack to model (the same reasoning as `GamepadScreen`).
    private enum Layer: Equatable {
        case main
        case shortcuts
        case licenses
    }

    @State private var layer: Layer = .main
    @State private var focusID: String?

    #if os(iOS)
    /// `.compact` in a landscape phone window — tighter chrome so more rows fit.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false
    #endif

    private enum Destination {
        static let docs = URL(string: "https://docs.punktfunk.unom.io")!
        static let community = URL(string: "https://discord.gg/kaPNvzMuGU")!
        static let source = URL(string: "https://git.unom.io/unom/punktfunk")!
    }

    private static let tagline =
        "Low-latency desktop and game streaming with first-class Linux and Windows hosts."

    var body: some View {
        GamepadMenuList(
            items: rows,
            focusID: $focusID,
            onActivate: { activate(id: $0.id) },
            onBack: { back() },
            isActive: controllerActive
        ) { row, focused in
            rowView(row, focused: focused)
                .frame(maxWidth: metrics.rowMaxWidth)
                .padding(.horizontal, 24)
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
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            GamepadHintBar(hints: hints)
                // Equal distance from the left and bottom edges (see GamepadHomeView).
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
        // The launcher's field, calmed — same base as the settings and add-host screens. Hosted in
        // the shell the field is the SHELL's (one persistent backdrop), so don't mount a second.
        .background {
            if !hostedInShell { GamepadFormBackground() }
        }
        .gamepadPaletteInk()
        #if !os(tvOS)
        // A gamepad UI exits with B; this keeps a hardware keyboard's Esc and the macOS sheet's
        // cancel working without putting close chrome on screen.
        .background {
            Button("Cancel") { back() }
                .keyboardShortcut(.cancelAction)
                .buttonStyle(.plain)
                .frame(width: 0, height: 0)
                .opacity(0)
                .accessibilityHidden(true)
        }
        #endif
    }

    // MARK: - Header

    /// The identity card on the main layer, a plain heading on the sub-pages. Laid out sideways
    /// (icon leading, text trailing) rather than centred like the touch page: a console screen
    /// spends its vertical budget on rows, and a centred stack of icon-name-version-tagline is
    /// most of a 10-foot screen before the first row appears.
    @ViewBuilder
    private var header: some View {
        switch layer {
        case .main:
            HStack(alignment: .center, spacing: compact ? 14 : 18) {
                AppIconView(side: iconSide)
                VStack(alignment: .leading, spacing: 2) {
                    Text("Punktfunk")
                        .font(.geist(
                            gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                        .foregroundStyle(ink.fg)
                    Text(Self.versionLine)
                        .font(.geist(metrics.detailFont, .medium, relativeTo: .caption))
                        .monospacedDigit()
                        .foregroundStyle(ink.fg(0.7))
                    if !compact {
                        Text(Self.tagline)
                            .font(.geist(metrics.detailFont, relativeTo: .caption))
                            .foregroundStyle(ink.fg(0.5))
                            .fixedSize(horizontal: false, vertical: true)
                            .frame(maxWidth: metrics.rowMaxWidth * 0.66, alignment: .leading)
                            .padding(.top, 2)
                    }
                }
                Spacer(minLength: 0)
            }
        case .shortcuts, .licenses:
            Text(title)
                .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                .foregroundStyle(ink.fg)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private var title: String {
        switch layer {
        case .main: return "About"
        case .shortcuts: return "Shortcuts"
        case .licenses: return "Acknowledgements"
        }
    }

    /// "Version 0.28.0 (1)" — the build number only when it says something the version doesn't.
    /// Mirrors `AboutView.versionLine`; a bug report is worth more with it.
    private static var versionLine: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "—"
        let build = info?["CFBundleVersion"] as? String
        guard let build, !build.isEmpty, build != short else { return "Version \(short)" }
        return "Version \(short) (\(build))"
    }

    private var iconSide: CGFloat {
        #if os(tvOS)
        return 108
        #else
        return compact ? 46 : 62
        #endif
    }

    // MARK: - Hints

    private var hints: [GamepadHint] {
        let backText = layer == .main ? "Done" : "Back"
        // The sub-pages are reading surfaces: nothing on them activates, so offering A would be
        // the same lie a dimmed row tells. Only the main layer's rows do anything.
        var hints: [GamepadHint] = []
        if layer == .main {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Open",
                action: { if let focusID { activate(id: focusID) } }))
        }
        hints.append(.init(
            glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: backText,
            action: { back() }))
        return hints
    }

    /// B peels one layer: a sub-page back to the About rows — focus returning to the row it came
    /// from — then the screen itself.
    private func back() {
        switch layer {
        case .main:
            performClose()
        case .shortcuts:
            layer = .main
            focusID = "shortcuts"
        case .licenses:
            layer = .main
            focusID = "licenses"
        }
    }

    private func performClose() {
        if let close {
            close()
        } else {
            dismiss()
        }
    }

    // MARK: - Rows

    /// One row. `detail` is nil on the reading surfaces, whose rows are prose rather than controls.
    private struct Row: Identifiable {
        let id: String
        var icon: String?
        let label: String
        var value: String = ""
        /// A section heading inside a reading surface (the shortcuts groups) — no glass, no glyph.
        var isHeading = false
        /// Monospaced, wrapping body text: a license chunk, or a shortcut's description.
        var isProse = false
        var activate: () -> Void = {}
    }

    private var rows: [Row] {
        switch layer {
        case .main: return mainRows
        case .shortcuts: return shortcutRows
        case .licenses: return licenseRows
        }
    }

    private var mainRows: [Row] {
        var list: [Row] = [
            Row(
                id: "shortcuts", icon: "command", label: "Shortcuts",
                value: "While streaming",
                activate: {
                    focusID = shortcutRows.first?.id
                    layer = .shortcuts
                }),
            Row(
                id: "licenses", icon: "text.document", label: "Acknowledgements",
                value: "MIT or Apache-2.0",
                activate: {
                    focusID = licenseRows.first?.id
                    layer = .licenses
                }),
        ]
        // tvOS has no browser and no `openURL`, so an address there is text to read off the
        // screen, not a link to nowhere — the same call the touch About page makes.
        list.append(contentsOf: [
            linkRow(id: "docs", icon: "book", label: "Documentation", url: Destination.docs),
            linkRow(
                id: "community", icon: "bubble.left.and.bubble.right", label: "Community",
                url: Destination.community),
            linkRow(
                id: "source", icon: "chevron.left.forwardslash.chevron.right",
                label: "Source code", url: Destination.source),
        ])
        return list
    }

    private func linkRow(id: String, icon: String, label: String, url: URL) -> Row {
        // Shown without the scheme: the host is what identifies the destination, and "https://"
        // is 8 characters of every row that never varies.
        let shown = url.absoluteString.replacingOccurrences(of: "https://", with: "")
        #if os(tvOS)
        return Row(id: id, icon: icon, label: label, value: shown)
        #else
        return Row(id: id, icon: icon, label: label, value: shown, activate: { openURL(url) })
        #endif
    }

    /// The shortcuts reference: a heading per group, then one prose row per shortcut. Same catalog
    /// the touch page renders, so the two never drift.
    private var shortcutRows: [Row] {
        ShortcutsCatalog.groups(micAvailable: micAvailable).flatMap { group -> [Row] in
            [Row(id: "group-\(group.title)", label: group.title, isHeading: true)]
                + group.items.map { item in
                    Row(
                        id: "sc-\(group.title)-\(item.keys)", label: item.keys, value: item.text,
                        isProse: true)
                }
        }
    }

    /// The license wall, one row per pre-chunked page (see `Licenses.chunked`). The header block
    /// the touch page draws in prose becomes the first two rows.
    private var licenseRows: [Row] {
        var list: [Row] = [
            Row(id: "lic-heading", label: "Punktfunk", isHeading: true),
            Row(
                id: "lic-summary",
                label: "Punktfunk's source is open under MIT or Apache-2.0. It ships the Geist "
                    + "typeface under the SIL Open Font License 1.1, and uses the third-party "
                    + "components below, each under its own license.",
                isProse: true),
        ]
        for (i, chunk) in Licenses.chunked(Licenses.appLicense).enumerated() {
            list.append(Row(id: "lic-app-\(i)", label: chunk, isProse: true))
        }
        list.append(Row(id: "lic-third-heading", label: "Third-party software", isHeading: true))
        for (i, chunk) in Licenses.thirdPartyNoticesChunks.enumerated() {
            list.append(Row(id: "lic-third-\(i)", label: chunk, isProse: true))
        }
        return list
    }

    private func activate(id: String) {
        rows.first { $0.id == id }?.activate()
    }

    // MARK: - Row rendering

    @ViewBuilder
    private func rowView(_ row: Row, focused: Bool) -> some View {
        let m = metrics
        if row.isHeading {
            Text(row.label)
                .font(.geist(m.labelFont, .bold, relativeTo: .headline))
                .foregroundStyle(ink.fg(0.75))
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, m.rowHPad)
                .padding(.top, 14)
                .padding(.bottom, 2)
        } else if row.isProse {
            // A reading row: the focus treatment is a quiet wash rather than the control rows'
            // full glass — focus here means "this is the part you are scrolled to", not "press A".
            VStack(alignment: .leading, spacing: 4) {
                Text(row.label)
                    .font(.geistFixed(m.valueFont, .medium))
                    .foregroundStyle(ink.fg(0.95))
                    .fixedSize(horizontal: false, vertical: true)
                if !row.value.isEmpty {
                    Text(row.value)
                        .font(.geist(m.detailFont, relativeTo: .caption))
                        .foregroundStyle(ink.fg(0.6))
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, m.rowHPad)
            .padding(.vertical, m.rowVPad * 0.7)
            .background {
                RoundedRectangle(cornerRadius: m.rowCorner, style: .continuous)
                    .fill(ink.fg(focused ? 0.08 : 0))
            }
            .animation(.smooth(duration: 0.18), value: focused)
        } else {
            HStack(spacing: 14) {
                if let icon = row.icon {
                    Image(systemName: icon)
                        .font(.system(size: m.iconFont))
                        .foregroundStyle(focused ? ink.accent : ink.fg(0.55))
                        .frame(width: m.iconWidth)
                }
                Text(row.label)
                    .font(.geist(m.labelFont, .semibold, relativeTo: .body))
                    .foregroundStyle(ink.fg)
                    .lineLimit(1)
                Spacer(minLength: 12)
                Text(row.value)
                    .font(.geist(m.valueFont, .medium, relativeTo: .callout))
                    .foregroundStyle(focused ? ink.fg(0.85) : ink.fg(0.55))
                    .lineLimit(1)
                    .truncationMode(.middle)
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
}
#endif
