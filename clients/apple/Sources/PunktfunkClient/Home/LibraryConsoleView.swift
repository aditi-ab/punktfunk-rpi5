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
    /// Copy a title's `punktfunk://` link — offered from the title's Options menu (X). nil where
    /// the platform has no clipboard (tvOS), which drops the row and, with nothing else in the
    /// menu worth a press, the X hint.
    var onCopyLink: ((GameEntry) -> Void)?
    /// The host's name, for the Options menu's explainer.
    var hostName: String?
    /// Whether this screen owns the controller — the shell gates it mid-transition and under the
    /// connect takeover.
    var controllerActive = true
    /// The collection the shelf is filtered to (its label), or nil — the container reports it so
    /// the screen's title can read `host · profile · collection` like the desktop's.
    var onCollectionChanged: ((String?) -> Void)?
    /// Screenshot/dev overrides: force an arrangement, open with the bar focused, or start on
    /// the Collections tiles regardless of the setting.
    var arrangementOverride: LibraryArrangement?
    var barFocusedInitially = false
    var startInCollectionsOverride: Bool?
    /// Screenshot override: open the first title's Options menu on mount.
    var optionsInitially = false

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
    @AppStorage(DefaultsKey.libraryCollections) private var startInCollections = false
    /// Where the controller is inside this shelf: the plain shelf, the Collections tiles, or a
    /// filtered shelf — pushed and popped INSIDE this layer (see `LibraryPlaceStack`).
    @State private var places = LibraryPlaceStack(root: .shelf(filter: nil))
    /// The "start in collections" hand-over is decided ONCE per shelf.
    @State private var handoverDecided = false
    /// The focused Collections tile.
    @State private var focusedTileID: String?
    /// The bar owns the controller.
    @State private var barFocused = false
    /// The focused title (the strip's centred cover / the grid's cell), published by the
    /// arrangement — feeds the detail band and every hint, read at press time.
    @State private var focusID: String?
    /// The title whose Options menu is up (X) — a layer over the field that takes the controller.
    @State private var optionsFor: GameEntry?
    @State private var barInput = GamepadMenuInput(manager: .shared)
    @State private var barHaptics = MenuHaptics(manager: .shared)
    @State private var barBoundaryTick = 0

    private var sort: LibrarySortKey { LibrarySortKey(stored: sortRaw) }
    private var arrangement: LibraryArrangement {
        arrangementOverride ?? LibraryArrangement(stored: viewRaw)
    }
    /// The shelf, collated: launchers lead in host order, then the titles under `sort`
    /// (`filtered(nil)` flattens every group in collated order); on a drilled shelf, that
    /// group's titles alone.
    private var displayed: [GameEntry] {
        LibraryCollation.filtered(games, sort: sort, filter: places.top.filter).map { games[$0] }
    }
    /// The Collections tiles: group by platform under the current sort.
    private var groups: [LibraryGroup] {
        LibraryCollation.collate(games, sort: sort, groupBy: .platform)
    }
    private var focused: GameEntry? { displayed.first { $0.id == focusID } }
    /// The field owns the controller only while neither the bar nor a title's Options menu does.
    private var fieldActive: Bool { controllerActive && !barFocused && optionsFor == nil }
    /// Whether a title has an Options menu worth opening (today: only the Copy link row).
    private var offersOptions: Bool { onCopyLink != nil }
    /// Whether Y opens Collections here: an unfiltered root shelf over a library worth browsing.
    private var canOpenCollections: Bool {
        places.canOpenCollections && LibraryCollation.worthBrowsing(games)
    }

    var body: some View {
        // Keyed on the top place: a push or pop mounts the incoming place fresh (its own entrance,
        // its own cursor seeded on the focused title) and moves it with the shell's own push/pop
        // choreography — the same slide-out-of-a-fade the shell uses between its layers.
        let top = places.top
        ZStack {
            VStack(spacing: 0) {
            field
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            if !top.isCollections {
                detailPanel
                    .padding(.top, 8)
                    .padding(.bottom, compact ? 4 : 10)
            } else {
                // The tiles carry their own text; keep the band's height so the strip doesn't
                // jump when a place is pushed over the shelf.
                Color.clear.frame(height: compact ? 44 : 60)
            }
            }
            // A push/pop is a state change inside `placeMotion` (see the mutations below), so the
            // outgoing place leaves and the incoming one arrives with the shell's own transition.
            .id(top)
            .transition(
                reduceMotion
                    ? .opacity
                    : .gamepadScreen(slide: GamepadShellMotion.slide(compact: compact)))
            // Under a title's Options menu the field recedes exactly as the launcher does under a
            // shell layer (`covered` in GamepadHomeView): out of sight, a touch smaller, inert.
            .opacity(optionsFor == nil ? 1 : 0)
            .scaleEffect(optionsFor == nil ? 1 : GamepadShellMotion.underScale)
            .allowsHitTesting(optionsFor == nil)
            // The view/sort bar is a TRAY, not a band: it slides down over the field on ▲ (and
            // the legend's `Sort & view` cell) and back up on ▼/A/B, so the field keeps every
            // point of height it has — a fixed band ate a third of a landscape phone. While it
            // is down it owns the controller (`fieldActive`), and the legend says so.
            .overlay(alignment: .top) {
                if barFocused, optionsFor == nil {
                    LibraryBarView(
                        sort: sort, arrangement: arrangement, focused: true,
                        showsView: !top.isCollections, compact: compact,
                        onSort: { setSort($0) }, onArrangement: { setArrangement($0) }
                    )
                    .padding(.top, compact ? 4 : 8)
                    .padding(.bottom, compact ? 6 : 10)
                    .frame(maxWidth: .infinity)
                    .background { GamepadTrayBlur(edge: .top) }
                    .transition(.move(edge: .top).combined(with: .opacity))
                    .zIndex(1)
                }
            }
            // A title's Options menu rides over the field as its own layer, with its own title
            // band and legend; the field underneath goes inert until it closes.
            if let game = optionsFor {
                LibraryTitleOptionsView(
                    game: game, hostName: hostName, onCopyLink: onCopyLink,
                    close: { closeOptions() }, controllerActive: controllerActive)
                    .zIndex(2)
                    .transition(
                        reduceMotion
                            ? .opacity
                            : .gamepadScreen(slide: GamepadShellMotion.slide(compact: compact)))
            }
        }
        // The legend, over a bottom tray blur — rows scroll under it, so it needs the same
        // material the settings screen's tray has (bare, the grid drew straight through it).
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            if optionsFor == nil {
                GamepadHintBar(hints: barFocused ? barHints : (top.isCollections ? collectionHints : hints))
                    .padding(.leading, 22)
                    .padding(.vertical, compact ? 6 : 10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background { GamepadTrayBlur(edge: .bottom) }
            }
        }
        // Hosted in the shell, the field is the shell's own persistent aurora (the library is an
        // aurora screen — the calm mix simply stays 0, so nothing even chases).
        .background {
            if !hostedInShell { GamepadScreenBackground() }
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a pale
        // palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
        .onAppear {
            decideHandover()
            barFocused = barFocusedInitially
            if optionsInitially, offersOptions, let first = displayed.first { optionsFor = first }
            wireBar()
            if barFocused, controllerActive { barInput.start() }
        }
        .onChange(of: games.map(\.id)) { _, _ in
            // A later list (the host answering behind a cached catalog) may make the library
            // browsable — but only an undecided shelf may still move to the tiles.
            decideHandover()
        }
        .onChange(of: places) { _, stack in
            onCollectionChanged?(stack.top.filter?.label)
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
        if places.top.isCollections {
            LibraryCollectionsView(
                games: games, groups: groups, artLoader: artLoader, focusID: $focusedTileID,
                onOpen: { openCollection($0) },
                onAllTitles: places.offersAllTitles ? { openAllTitles() } : nil,
                onBack: { back() },
                onSortStep: { stepSort(by: $0, wrapping: true) },
                onUp: { enterBar() },
                controllerActive: fieldActive)
        } else {
            switch arrangement {
            case .shelf:
                LibraryCoverflowView(
                    games: displayed, artLoader: artLoader, focusID: $focusID, onLaunch: onLaunch,
                    running: running, initialSelection: focusID ?? initialSelection,
                    onBack: { back() },
                    onSecondary: { openCollections() },
                    onTertiary: offersOptions ? { openOptions() } : nil,
                    onUp: { enterBar() },
                    controllerActive: fieldActive)
            case .grid:
                LibraryGridView(
                    games: displayed, artLoader: artLoader, focusID: $focusID, onLaunch: onLaunch,
                    running: running, initialSelection: focusID ?? initialSelection,
                    onBack: { back() },
                    onSecondary: { openCollections() },
                    onTertiary: offersOptions ? { openOptions() } : nil,
                    onUp: { enterBar() },
                    controllerActive: fieldActive)
            }
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
        // Hidden when there is nothing to browse (one platform, one store) and on any drilled
        // shelf — the way back from those is B.
        if canOpenCollections {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonY, fallback: "y.circle"), text: "Collections",
                action: { openCollections() }))
        }
        // The desktop's `X Options` — a title's own actions live in a menu, not on a face button.
        if offersOptions {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonX, fallback: "x.circle"), text: "Options",
                action: { openOptions() }))
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

    /// The Collections legend: A Open · Y All titles (root only) · L1/R1 Sort · B Back.
    private var collectionHints: [GamepadHint] {
        var hints: [GamepadHint] = [
            .init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Open",
                action: {
                    if let id = focusedTileID,
                       let g = groups.first(where: { CollectionTile(group: $0).id == id }) {
                        openCollection(g.key)
                    }
                }),
        ]
        if places.offersAllTitles {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonY, fallback: "y.circle"), text: "All titles",
                action: { openAllTitles() }))
        }
        if showsShoulderHint {
            hints.append(.init(
                glyph: buttonGlyph(\.leftShoulder, fallback: "l1.rectangle.roundedbottom"),
                text: "Sort"))
        }
        hints.append(.init(
            glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
            action: { back() }))
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

    /// X: the FOCUSED title's Options menu — read at press time, inert with nothing focused.
    private func openOptions() {
        guard offersOptions, let game = focused else { return }
        leaveBar()
        barHaptics.move()
        withAnimation(placeMotion) { optionsFor = game }
    }

    private func closeOptions() {
        withAnimation(placeMotion) { optionsFor = nil }
    }

    private func setSort(_ key: LibrarySortKey) {
        guard key != sort else { return }
        sortRaw = key.stored
    }

    /// Step the sort by ±1 — clamped on the bar, WRAPPING on the Collections tiles.
    private func stepSort(by delta: Int, wrapping: Bool) {
        let all = LibrarySortKey.all
        guard let i = all.firstIndex(of: sort) else { return setSort(all[0]) }
        var target = i + delta
        if wrapping {
            target = (target % all.count + all.count) % all.count
        } else if !all.indices.contains(target) {
            return barBoundary()
        }
        barHaptics.move()
        setSort(all[target])
    }

    // MARK: - Places

    /// The push/pop choreography between places — the shell's own (`GamepadShellMotion`), a plain
    /// crossfade under Reduce Motion.
    private var placeMotion: Animation {
        reduceMotion ? GamepadShellMotion.reducedScreen : GamepadShellMotion.screen
    }

    /// The "start in collections" hand-over — decided once per shelf. This view is mounted only
    /// while there is a catalog (a cached one counts), so "ready" is true by construction here.
    private func decideHandover() {
        let on = startInCollectionsOverride ?? startInCollections
        switch CollectionsHandover.decide(
            settingOn: on, alreadyDecided: handoverDecided, drilled: !places.isRoot,
            ready: !games.isEmpty, worthBrowsing: LibraryCollation.worthBrowsing(games))
        {
        case .wait:
            return
        case .shelf:
            handoverDecided = true
        case .collections:
            handoverDecided = true
            // The Collections place REPLACES the shelf as the root (stack length unchanged),
            // exactly as the desktop's hand-over does.
            places = LibraryPlaceStack(root: .collections)
        }
    }

    /// Y on the shelf: push Collections — or a boundary pulse where it is refused (a drilled
    /// shelf, or nothing worth browsing).
    private func openCollections() {
        guard canOpenCollections else { return barBoundary() }
        barHaptics.move()
        leaveBar()
        withAnimation(placeMotion) { places.push(.collections) }
    }

    /// A on a tile: push that group as a filtered shelf.
    private func openCollection(_ key: LibraryGroupKey) {
        withAnimation(placeMotion) { places.push(.shelf(filter: key)) }
    }

    /// Y on the Collections root: the plain shelf, as a drill-in ("All titles").
    private func openAllTitles() {
        guard places.offersAllTitles else { return barBoundary() }
        withAnimation(placeMotion) { places.push(.shelf(filter: nil)) }
    }

    /// B: pop a place; at the root, dismiss the layer.
    private func back() {
        leaveBar()
        guard !places.isRoot else { return onDismiss?() ?? () }
        _ = withAnimation(placeMotion) { places.pop() }
    }

    private func setArrangement(_ view: LibraryArrangement) {
        guard view != arrangement else { return }
        viewRaw = view.stored
    }

    // MARK: - Bar input

    /// The tray's slide — the console's INDICATOR spring (the keyboard tray's, too); a plain
    /// fade under Reduce Motion.
    private var trayMotion: Animation {
        reduceMotion ? .easeOut(duration: 0.15) : .spring(response: 0.32, dampingFraction: 0.86)
    }

    private func enterBar() {
        guard !barFocused else { return }
        barHaptics.move()
        withAnimation(trayMotion) { barFocused = true }
    }

    private func leaveBar() {
        guard barFocused else { return }
        withAnimation(trayMotion) { barFocused = false }
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
            // Step the sort, CLAMPED — no wrap (the Collections tiles' shoulders wrap, this bar
            // does not; the desktop draws the same distinction).
            stepSort(by: direction == .right ? 1 : -1, wrapping: false)
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
