// The gamepad presentation of one library shelf — the chrome the desktop console's library
// screen owns and both of its arrangements feed: the view/sort bar across the top, the field
// (coverflow or grid, one persisted setting apart), the detail band under it, and the legend.
// `LibraryView` still owns the DATA (fetch, cache, wake, running, art loader); this view owns
// how the controller reads it.
//
// The sort and the arrangement are the cross-client `library_sort` / `library_view` keys, written
// here and by the Interface settings rows alike, so the two surfaces can never disagree. Both
// apply live: the collated shelf is re-derived from the setting and the focused TITLE survives —
// the strip's cursor keeps its item across a list change and the grid re-anchors by id — so a
// sort change never resets the cursor (the desktop's `sync` rule).
//
// Bar focus is routed here: ▲ from the field hands the controller to the bar (the field goes
// inert), ◀▶ step the sort clamped, L1/R1 pick the arrangement outright, ▼ / A / B hand it back.
// The legend swaps with it. This is the same "one owner of the controller at a time" hand-over
// the shell does between its layers.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController

struct LibraryConsoleView: View {
    /// Resolved from the stored palette, NOT from `\.gamepadInk` — this screen publishes that
    /// value itself and so sits above its own copy (see `GamepadInk.stored`).
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    /// The catalog in the model's order (`LibraryOrder.display`) — this view collates it.
    let games: [GameEntry]
    let artLoader: (any LibraryArtSource)?
    var onLaunch: ((String) -> Void)?
    var running: [String: RunningGame] = [:]
    var staleness: LibraryStaleness = .none
    /// The title last opened from this shelf — both arrangements open on it.
    var initialSelection: String?
    /// Button B at the shelf — dismisses the library screen.
    var onDismiss: (() -> Void)?
    /// Button X — copy the focused title's `punktfunk://` link. nil where the platform has no
    /// clipboard (tvOS), which also drops the hint.
    var onCopyLink: ((GameEntry) -> Void)?
    /// Whether this screen owns the controller — the shell gates it mid-transition and under the
    /// connect takeover.
    var controllerActive = true
    /// Screenshot/dev overrides: force an arrangement, open with the bar focused.
    var arrangementOverride: LibraryArrangement?
    var barFocusedInitially = false

    @Environment(\.gamepadHostedInShell) private var hostedInShell
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    #if os(iOS)
    @Environment(\.verticalSizeClass) private var vSizeClass
    @Environment(\.horizontalSizeClass) private var hSizeClass
    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false
    #endif
    @AppStorage(DefaultsKey.librarySort) private var sortRaw = ""
    @AppStorage(DefaultsKey.libraryView) private var viewRaw = ""
    /// The bar owns the controller.
    @State private var barFocused = false
    /// The focused title (the strip's centred cover / the grid's cell), published by the
    /// arrangement — feeds the detail band and every hint, read at press time.
    @State private var focusID: String?
    /// The copy hint's acknowledgement (there is no toast on this surface — the legend says it
    /// itself, and forgets it the moment the focus moves off the cover it was about).
    @State private var copied = false
    @State private var barInput = GamepadMenuInput(manager: .shared)
    @State private var barHaptics = MenuHaptics(manager: .shared)
    @State private var barBoundaryTick = 0

    private var sort: LibrarySortKey { LibrarySortKey(stored: sortRaw) }
    private var arrangement: LibraryArrangement {
        arrangementOverride ?? LibraryArrangement(stored: viewRaw)
    }
    /// The shelf, collated: launchers lead in host order, then the titles under `sort`
    /// (`filtered(nil)` flattens every group in collated order).
    private var displayed: [GameEntry] {
        LibraryCollation.filtered(games, sort: sort, filter: nil).map { games[$0] }
    }
    private var focused: GameEntry? { displayed.first { $0.id == focusID } }
    /// The field owns the controller only while the bar does not.
    private var fieldActive: Bool { controllerActive && !barFocused }

    var body: some View {
        VStack(spacing: 0) {
            LibraryBarView(
                sort: sort, arrangement: arrangement, focused: barFocused, compact: compact,
                onSort: { setSort($0) }, onArrangement: { setArrangement($0) }
            )
            .padding(.top, compact ? 2 : 6)
            .padding(.bottom, LibraryBarView.gap - 4)
            field
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            detailPanel
                .padding(.top, 8)
                .padding(.bottom, compact ? 4 : 10)
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            GamepadHintBar(hints: barFocused ? barHints : hints)
                .padding(.leading, 22)
                .padding(.vertical, compact ? 6 : 10)
        }
        // Hosted in the shell, the field is the shell's own persistent aurora (the library is an
        // aurora screen — the calm mix simply stays 0, so nothing even chases).
        .background {
            if !hostedInShell { GamepadScreenBackground() }
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a pale
        // palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
        .task(id: copied) {
            guard copied else { return }
            try? await Task.sleep(for: .milliseconds(1600))
            withAnimation(.smooth(duration: 0.2)) { copied = false }
        }
        .onChange(of: focusID) { _, _ in copied = false }
        .onAppear {
            barFocused = barFocusedInitially
            wireBar()
            if barFocused, controllerActive { barInput.start() }
        }
        .onChange(of: barFocused) { _, focusedNow in
            if focusedNow, controllerActive { barInput.start() } else { barInput.stop() }
        }
        .onChange(of: controllerActive) { _, active in
            if !active { barInput.stop() } else if barFocused { barInput.start() }
        }
        .onDisappear {
            barInput.stop()
            barHaptics.stop()
        }
        #if os(iOS) || os(macOS)
        // Hardware keyboard while the bar has focus: arrows step the sort, Return/Esc hand back.
        .gamepadKeyNavigation(
            active: barFocused && controllerActive,
            onMove: { barMove($0) },
            onConfirm: { leaveBar() },
            onBack: { leaveBar() })
        #endif
        .sensoryFeedback(.impact(flexibility: .rigid, intensity: 0.7), trigger: barBoundaryTick)
    }

    /// The arrangement — one shelf, two fields; the persisted setting picks. Keyed on the
    /// arrangement so a switch mounts the other field fresh (its own entrance, its own cursor
    /// seeded on the focused title).
    @ViewBuilder private var field: some View {
        switch arrangement {
        case .shelf:
            LibraryCoverflowView(
                games: displayed, artLoader: artLoader, focusID: $focusID, onLaunch: onLaunch,
                running: running, initialSelection: focusID ?? initialSelection,
                onBack: onDismiss,
                onTertiary: onCopyLink.map { copy in { copyFocused(copy) } },
                onUp: { enterBar() },
                controllerActive: fieldActive)
        case .grid:
            LibraryGridView(
                games: displayed, artLoader: artLoader, focusID: $focusID, onLaunch: onLaunch,
                running: running, initialSelection: focusID ?? initialSelection,
                onBack: onDismiss,
                onTertiary: onCopyLink.map { copy in { copyFocused(copy) } },
                onUp: { enterBar() },
                controllerActive: fieldActive)
        }
    }

    // MARK: - Detail band

    /// The focused title + its provenance — empty (not hidden) so the layout doesn't jump. The
    /// staleness note shares the subtitle line, leading, the way the desktop shelf draws it: the
    /// titles stay, the wording says where they came from.
    @ViewBuilder private var detailPanel: some View {
        let game = focused
        VStack(spacing: 6) {
            Text(game?.title ?? " ")
                .font(.geist(compact ? 22 : 25, .bold, relativeTo: .title))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
                .minimumScaleFactor(0.75)
                .multilineTextAlignment(.center)
            // Three columns of equal give: the note leads and truncates, the subtitle stays
            // centred, the trailing column is air — so on a narrow phone the two never overlap.
            HStack(spacing: 8) {
                Group {
                    if let note = staleness.text {
                        HStack(spacing: 5) {
                            Image(systemName: staleness.symbol)
                            Text(note)
                        }
                        .font(.geist(11, relativeTo: .caption2))
                        .foregroundStyle(ink.fg(0.55))
                        .lineLimit(1)
                        .truncationMode(.tail)
                    } else {
                        Color.clear.frame(height: 1)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                if let game {
                    Text(subtitle(for: game))
                        .font(.geist(11, .semibold, relativeTo: .caption2))
                        .tracking(1.2)
                        .foregroundStyle(ink.fg(0.5))
                        .lineLimit(1)
                        .fixedSize()
                }
                Color.clear.frame(maxWidth: .infinity, maxHeight: 1)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 24)
        .animation(.smooth(duration: 0.25), value: focusID)
    }

    /// `STORE · LAUNCHER` for a launcher, `STORE · PLATFORM` when the host named a platform, else
    /// `STORE` — the desktop's detail-band rule.
    private func subtitle(for game: GameEntry) -> String {
        let store = game.storeLabel.uppercased()
        if game.isLauncher { return "\(store) · LAUNCHER" }
        if let platform = game.platform?.trimmingCharacters(in: .whitespacesAndNewlines),
           !platform.isEmpty {
            return "\(store) · \(platform.uppercased())"
        }
        return store
    }

    // MARK: - Legend

    /// Whether the legend advertises the shoulder jump. Held back on an iPhone, whose legend is
    /// already at its width; never on tvOS (a Siri Remote has no shoulders) — the settings
    /// strip's own rule.
    private var showsShoulderHint: Bool {
        #if os(tvOS)
        false
        #elseif os(iOS)
        hSizeClass == .regular
        #else
        true
        #endif
    }

    /// The field's legend, in the desktop's order: A Resume|Open|Play · X Copy link · L1/R1 Jump
    /// · ▲ Sort & view · B Back. Every cell re-resolves the focused title at fire time.
    private var hints: [GamepadHint] {
        var hints: [GamepadHint] = []
        if let onLaunch {
            let game = focused
            let text: String
            if let game, running[game.id] != nil {
                text = "Resume"
            } else if game?.isLauncher == true {
                text = "Open"
            } else {
                text = "Play"
            }
            hints.append(.init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: text,
                action: { if let id = focusID { onLaunch(id) } }))
        }
        if let onCopyLink {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonX, fallback: "x.circle"),
                text: copied ? "Copied" : "Copy link",
                action: { copyFocused(onCopyLink) }))
        }
        if showsShoulderHint {
            hints.append(.init(
                glyph: buttonGlyph(\.leftShoulder, fallback: "l1.rectangle.roundedbottom"),
                text: "Jump"))
        }
        hints.append(.init(glyph: "arrow.up", text: "Sort & view", action: { enterBar() }))
        hints.append(.init(
            glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
            action: { onDismiss?() }))
        return hints
    }

    /// The legend while the bar has the controller — it REPLACES the field's.
    private var barHints: [GamepadHint] {
        [
            .init(glyph: "arrow.left.and.right", text: "Sort"),
            .init(
                glyph: buttonGlyph(\.leftShoulder, fallback: "l1.rectangle.roundedbottom"),
                text: "View"),
            .init(
                glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Done",
                action: { leaveBar() }),
        ]
    }

    // MARK: - Actions

    /// Hand the FOCUSED title to the copy action — read at press time, inert with nothing focused.
    private func copyFocused(_ copy: (GameEntry) -> Void) {
        guard let game = focused else { return }
        copy(game)
        withAnimation(.smooth(duration: 0.2)) { copied = true }
    }

    private func setSort(_ key: LibrarySortKey) {
        guard key != sort else { return }
        sortRaw = key.stored
    }

    private func setArrangement(_ view: LibraryArrangement) {
        guard view != arrangement else { return }
        viewRaw = view.stored
    }

    // MARK: - Bar input

    private func enterBar() {
        guard !barFocused else { return }
        barHaptics.move()
        withAnimation(.smooth(duration: 0.18)) { barFocused = true }
    }

    private func leaveBar() {
        guard barFocused else { return }
        withAnimation(.smooth(duration: 0.18)) { barFocused = false }
    }

    private func wireBar() {
        barInput.onMove = { barMove($0) }
        barInput.onConfirm = { leaveBar() }
        barInput.onBack = { leaveBar() }
        barInput.onSecondary = nil
        barInput.onTertiary = nil
        // L1 = Shelf, R1 = Grid — outright, no wrap, and a repeat press is a boundary, not a
        // flip-flop.
        barInput.onShoulder = { right in
            let want: LibraryArrangement = right ? .grid : .shelf
            guard want != arrangement else { return barBoundary() }
            barHaptics.move()
            setArrangement(want)
        }
    }

    private func barMove(_ direction: GamepadMenuInput.Direction) {
        switch direction {
        case .left, .right:
            // Step the sort, CLAMPED — no wrap (the collections screen's pill row wraps, this
            // bar does not; the desktop draws the same distinction).
            let all = LibrarySortKey.all
            guard let i = all.firstIndex(of: sort) else { return setSort(all[0]) }
            let target = i + (direction == .right ? 1 : -1)
            guard all.indices.contains(target) else { return barBoundary() }
            barHaptics.move()
            setSort(all[target])
        case .down:
            leaveBar()
        case .up:
            barBoundary()
        }
    }

    private func barBoundary() {
        barBoundaryTick &+= 1
        barHaptics.boundary()
    }
}
#endif
