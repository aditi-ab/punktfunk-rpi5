// The host's game library: a poster grid fetched over the management API, from which titles are
// launched. Reached by TAPPING a paired host's card — this is the primary destination for a host
// that has a library, with streaming the desktop as the card's menu action.
//
// Three behaviours here exist to make the round trip (browse → play → quit → browse) hold together,
// all from a 2026-08-16 field report: the catalog is cached so a sleeping host still shows its
// titles, opening the screen wakes that host so it is warm by the time one is picked, and the
// position in the grid survives the stream. Titles already running are marked and sorted first.
//
// Still gated behind `DefaultsKey.libraryEnabled` (default on) and, separately, on the host being
// PAIRED — see `HomeView.hostCard` for why the pin is load-bearing rather than cosmetic.

import PunktfunkKit
import SwiftUI

/// Which library shelf is open: a host, and — when it was opened from a PINNED host+profile card
/// (design/client-settings-profiles.md §5.2a) — that card's profile, which every title launched off
/// the shelf then runs with, exactly as the card's own tap would.
///
/// One value rather than a host plus a profile carried beside it: a host and its pinned cards are
/// different cards on the grid, so "which library" is not answered by the host alone. That is also
/// why `id` folds the profile in — a presentation keyed on the host would not re-present when you
/// move between a host's own shelf and one of its pins.
struct LibraryTarget: Identifiable, Hashable {
    let host: StoredHost
    /// `.inherit` from the host's own card (its binding decides, as it always has); `.profile` from
    /// a pinned card. `.defaults` never reaches here — nothing opens a library "with the globals".
    var profile: ProfileSelection = .inherit

    var id: String {
        switch profile {
        case .inherit: host.id.uuidString
        case .defaults: "\(host.id.uuidString)#defaults"
        case .profile(let id): "\(host.id.uuidString)#\(id)"
        }
    }

    /// The pinned profile's id, if this shelf belongs to a pinned card.
    var pinnedProfileID: String? {
        if case .profile(let id) = profile { return id }
        return nil
    }

    /// What the screen calls itself: the host, and the profile when a pinned card opened it — the
    /// same `host · profile` shape that card wears, so which shelf you are on is on screen rather
    /// than remembered from the card you pressed. A pin whose profile has since been deleted
    /// resolves as no profile everywhere else, and reads as the plain host here.
    @MainActor func title(in catalog: ProfileStore) -> String {
        guard let id = pinnedProfileID, let profile = catalog.profile(id: id) else {
            return host.displayName
        }
        return "\(host.displayName) \u{b7} \(profile.name)"
    }
}

struct LibraryView: View {
    @ObservedObject var store: HostStore
    /// The shelf being browsed — the host, plus the pinned profile when a pinned card opened it.
    let target: LibraryTarget
    /// Tapping a title starts a session that asks the host to launch it (the library id is passed
    /// through). `nil` ⇒ browse-only (cards aren't tappable). The PROFILE a launch runs with is the
    /// caller's to apply: it holds `target` and connects with `target.profile`.
    var onLaunch: ((String) -> Void)? = nil
    /// How the gamepad shell (GamepadLibraryScreen) closes this screen; nil — every sheet/cover
    /// presentation — falls back to the environment dismiss.
    var onClose: (() -> Void)? = nil
    /// Whether the gamepad coverflow owns the controller — the shell gates it during a push/pop
    /// and while the connect takeover is up. Presentations that cover the launcher keep the
    /// default (their being up IS the launcher's gate).
    var controllerActive = true
    /// The collection the gamepad shelf is drilled into (its label), or nil — reported so a host
    /// screen (GamepadLibraryScreen's pinned title) can read `host · profile · collection`.
    var onCollectionChanged: ((String?) -> Void)?
    /// The same, for this view's own navigation title (the sheet/cover presentations).
    @State private var collectionLabel: String?
    /// The touch grid's sort (the shared `library_sort` key, the same one the console's bar
    /// writes) and its grouping (touch-only — sections are the touch analogue of the console's
    /// Collections place).
    @AppStorage(DefaultsKey.librarySort) private var sortRaw = ""
    @AppStorage(DefaultsKey.libraryGroupBy) private var groupByRaw = ""
    @Environment(\.dismiss) private var dismiss
    /// Resolves a pinned shelf's profile NAME for the title (the target carries only its id).
    @ObservedObject private var profiles = ProfileStore.shared

    /// The host this shelf belongs to — every fetch, every poster URL and the launch itself address
    /// it, and a pinned shelf is the same host seen through one of its cards.
    private var host: StoredHost { target.host }

    @State private var games: [GameEntry] = []
    @State private var loading = false
    @State private var errorText: String?
    /// What the host has launched right now, keyed by library id — the `Resume` affordance. Empty
    /// on an older host, an unreachable one, or while the catalog is being served from cache.
    @State private var running: [String: RunningGame] = [:]
    /// When the catalog on screen was fetched, if it came from disk rather than from the host.
    /// Non-nil ⇒ these titles are a memory, not an observation, and the view says so.
    @State private var servedFromCacheAt: Date?
    /// Guards the one-shot scroll restore. `onAppear` fires again on every re-layout (and once per
    /// section), and re-scrolling after the player has started browsing would yank the grid out
    /// from under them — which is a worse bug than the one being fixed.
    @State private var restoredScroll = false
    /// Cover-art loader (the same paired identity + host pinning as the list fetch, reused across
    /// every poster in the grid). Built alongside `games` in `load()`; dropped on disappear.
    @State private var artLoader: (any LibraryArtSource)?
    #if os(iOS) || os(macOS)
    /// The plain grid's hardware-keyboard cursor (a game id), and the grid width the column count
    /// is derived from. nil until the first arrow press, so a touch user never sees a selection
    /// they didn't ask for.
    @State private var keyCursor: String?
    @State private var gridWidth: CGFloat = 0
    #endif
    #if os(iOS) || os(macOS) || os(tvOS)
    // Gamepad-driven browsing — see ContentView's identical gate. With no controller (or the
    // setting off) every platform keeps the plain-grid presentation of this same view.
    @ObservedObject private var gamepadManager = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    @AppStorage(DefaultsKey.gamepadUIMode) private var gamepadUIMode =
        GamepadUIEnvironment.modeWhenConnected
    private var gamepadUIActive: Bool {
        GamepadUIEnvironment.isActive(
            gamepadConnected: gamepadManager.active != nil, enabledSetting: gamepadUIEnabled,
            mode: gamepadUIMode)
    }
    /// True when the iOS shell already draws one persistent field behind its layers — mounting a
    /// second would double the mesh (the same rule the coverflow and the settings screen follow).
    @Environment(\.gamepadHostedInShell) private var hostedInShell
    #endif

    var body: some View {
        content
            .navigationTitle("\(shelfTitle) — Library")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                #if os(macOS)
                ToolbarItemGroup {
                    if !gamepadUIActive { sortMenu }
                    reloadButton
                }
                #else
                ToolbarItem(placement: .primaryAction) { reloadButton }
                // The console presentation carries its own sort/view bar; the plain grid gets a
                // menu in the bar it already has.
                if !gamepadUIActive {
                    ToolbarItem(placement: .primaryAction) { sortMenu }
                }
                #endif
                // A gamepad-only user can't swipe-to-dismiss the sheet this view is presented in
                // (ContentView's `.sheet(item: $libraryTarget)`) — give it a focusable, dpad-reachable
                // Close action. tvOS already has its own pushed-navigation back (Menu button).
                #if !os(tvOS)
                ToolbarItem(placement: .cancellationAction) {
                    Button("Close") { dismiss() }
                }
                #endif
            }
            .task { await load() }
            .onDisappear {
                // Hand the loader off before clearing it, so its pooled connections are closed
                // rather than left open on a screen the user has left.
                let leaving = artLoader
                artLoader = nil
                Task { await leaving?.close() }
            }
            #if os(iOS) || os(macOS)
            // B closes the library even before the coverflow exists (loading / error / empty):
            // the coverflow's carousel owns B once games render; until then this zero-size
            // listener does — without it a controller-only user is trapped on an error screen
            // (the gamepad screens carry no close chrome).
            .background {
                if gamepadUIActive && games.isEmpty {
                    LibraryBackCatcher(
                        active: controllerActive,
                        // A = the on-screen Retry button, only while there is an error to retry
                        // (a press during the load itself would start a second fetch).
                        onConfirm: errorText != nil && !loading ? { Task { await load() } } : nil,
                        onBack: { (onClose ?? { dismiss() })() })
                }
            }
            #endif
            #if os(iOS) || os(macOS) || os(tvOS)
            // Published HERE, not just inside the coverflow, because the coverflow is only one of
            // four things this view renders: the loading spinner, the error state and the empty
            // state sit above it, as do the navigation title and toolbar. On iOS those are wrapped
            // by GamepadLibraryScreen, which inks the whole thing; tvOS and macOS present this view
            // directly in a NavigationStack, so under a pale palette every one of them kept the
            // system's own (dark, on an Apple TV) chrome over a light field. Off when the gamepad
            // UI isn't drawing — the plain grid belongs to the system background.
            .gamepadPaletteInk(gamepadUIActive)
            #endif
    }

    /// Says the titles below are remembered rather than observed — shown only while that is true,
    /// and never as an error: a cached library is a working library, and a host that is still
    /// waking is the case this whole path exists to serve.
    @ViewBuilder private var staleNote: some View {
        if let text = staleness.text {
            HStack(spacing: 6) {
                Image(systemName: staleness.symbol)
                Text(text)
            }
            .font(.geist(12, relativeTo: .caption))
            .foregroundStyle(.secondary)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal)
            .padding(.top, 8)
        }
    }

    @ViewBuilder private var content: some View {
        if loading && games.isEmpty {
            consoleField(
                ProgressView("Loading library…")
                    .frame(maxWidth: .infinity, maxHeight: .infinity))
        } else if let errorText, games.isEmpty {
            consoleField(errorState(errorText))
        } else if games.isEmpty {
            consoleField(emptyState)
        } else {
            if gamepadUIActive {
                LibraryConsoleView(
                    games: ordered, artLoader: artLoader, onLaunch: launchAndRemember,
                    running: running,
                    staleness: staleness,
                    // The title last opened from this shelf — the coverflow assembles around it,
                    // the way the plain grid scrolls back to it, so the round trip browse → play →
                    // quit → browse lands where the player left rather than at the first cover.
                    initialSelection: LibraryScrollMemory.last(forHost: host.id.uuidString),
                    onDismiss: { (onClose ?? { dismiss() })() },
                    // Nil where there is nothing to copy into (tvOS), which is what drops the
                    // Options row and its hint rather than leaving a menu with nothing in it.
                    onCopyLink: LinkClipboard.isAvailable ? { copyLink($0) } : nil,
                    hostName: host.displayName,
                    controllerActive: controllerActive,
                    onCollectionChanged: { label in
                        collectionLabel = label
                        onCollectionChanged?(label)
                    })
            } else {
                // Above the grid rather than over it: the coverflow owns its whole surface and has
                // its own legend row, so the note rides the plain-grid presentation only.
                VStack(spacing: 0) {
                    staleNote
                    grid
                }
            }
        }
    }

    /// The console field behind the three states that are NOT the coverflow — loading, error,
    /// empty. The coverflow mounts its own backdrop; these mounted nothing, so wherever this view
    /// is a COVER over the launcher (tvOS, macOS) they drew straight onto it: the spinner and its
    /// label sat on the launcher's own aurora with the host tiles still showing through. The same
    /// field as the coverflow's (not the calmed form one), so nothing shifts under the content when
    /// the titles land and the coverflow takes over.
    ///
    /// Only in gamepad mode: the plain grid's states belong on the system background, as before.
    ///
    /// In gamepad mode these states also carry the legend the coverflow carries — `A Retry` on an
    /// error, `B Back` always — because a controller-only user on an error screen otherwise had
    /// no visible way out (the desktop console shows the same two hints there).
    @ViewBuilder private func consoleField(_ view: some View) -> some View {
        #if os(iOS) || os(macOS) || os(tvOS)
        view
            .background {
                if gamepadUIActive, !hostedInShell { GamepadScreenBackground() }
            }
            .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
                if gamepadUIActive {
                    GamepadHintBar(hints: stateHints)
                        .padding(.leading, 22)
                        .padding(.vertical, 10)
                }
            }
        #else
        view
        #endif
    }

    #if os(iOS) || os(macOS) || os(tvOS)
    /// The legend under the loading / error / empty states: A retries a failed fetch (the same
    /// action as the on-screen Retry button), B backs out. Read at press time, like every hint.
    private var stateHints: [GamepadHint] {
        var hints: [GamepadHint] = []
        if errorText != nil, !loading {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Retry",
                action: { Task { await load() } }))
        }
        hints.append(.init(
            glyph: buttonGlyph(\.buttonB, fallback: "b.circle"), text: "Back",
            action: { (onClose ?? { dismiss() })() }))
        return hints
    }
    #endif

    /// The grid's sections: the catalog collated by the shared rules — launchers lead (design
    /// D4), then one section per group under the chosen grouping (none = one section of games),
    /// each in the chosen sort. Headers only when there is more than one section, so an
    /// ungrouped, launcher-less library renders exactly as it always did.
    private var sections: [(label: String, games: [GameEntry])] {
        let groupBy: LibraryGroupBy?
        switch groupByRaw {
        case "platform": groupBy = .platform
        case "store": groupBy = .store
        default: groupBy = nil
        }
        return LibraryCollation.collate(ordered, sort: LibrarySortKey(stored: sortRaw), groupBy: groupBy)
            .map { group in
                // The ungrouped bucket names itself "All"; on this grid it has always been "Games".
                let label = (groupBy == nil && group.key != .launchers) ? "Games" : group.label
                return (label, group.indices.map { ordered[$0] })
            }
    }

    private var grid: some View {
        let sections = self.sections
        let showsHeaders = sections.count > 1
        return ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    ForEach(Array(sections.enumerated()), id: \.offset) { _, section in
                        if showsHeaders { sectionHeader(section.label) }
                        tiles(section.games)
                    }
                }
                .padding()
                // Put the player back where they were. Leaving a stream re-presents this view from
                // scratch, so a long library always came back at the top — meaning the round trip
                // "browse → play → quit → browse" lost your place every single time. The last title
                // opened from this shelf is remembered per host and scrolled back to once, without
                // animation, so it is simply already there rather than visibly moving.
                .onAppear {
                    guard !restoredScroll, let last = LibraryScrollMemory.last(forHost: host.id.uuidString),
                          ordered.contains(where: { $0.id == last })
                    else { return }
                    restoredScroll = true
                    proxy.scrollTo(last, anchor: .center)
                }
                #if os(iOS) || os(macOS)
                // The grid's own width, reported without affecting layout — a GeometryReader
                // SIBLING inside a ScrollView would claim the whole viewport. It's what tells the
                // keyboard cursor how many columns `.adaptive` actually produced, so it is only
                // measured where that cursor exists.
                .background {
                    GeometryReader { geo in
                        Color.clear
                            .onAppear { gridWidth = geo.size.width }
                            .onChange(of: geo.size.width) { _, w in gridWidth = w }
                    }
                }
                #endif
            }
            #if os(iOS) || os(macOS)
            // Hardware keyboard: arrows pick a title, Return launches it — a field ask from an
            // iPad user on a Magic Keyboard. The gamepad UI's coverflow has had this via the
            // controller all along; this is the same thing for the plain grid, which is what an
            // iPad with a keyboard and NO pad actually sees.
            .gamepadKeyNavigation(
                active: onLaunch != nil,
                onMove: { direction in
                    guard let next = gridNav(sections: sections.map(\.games))
                        .move(from: keyCursor, direction) else { return }
                    keyCursor = next
                    withAnimation(.easeOut(duration: 0.18)) { proxy.scrollTo(next, anchor: .center) }
                },
                onConfirm: {
                    guard let launch = launchAndRemember, let id = keyCursor else { return }
                    launch(id)
                })
            #endif
        }
    }

    #if os(iOS) || os(macOS)
    /// The keyboard cursor's model over the two grid sections. Rebuilt per press from the live
    /// sections so it can never point into a stale list.
    private func gridNav(sections: [[GameEntry]]) -> LibraryGridNav {
        LibraryGridNav(
            sections: sections.filter { !$0.isEmpty }.map { $0.map(\.id) },
            columns: columnCount)
    }

    /// How many columns `.adaptive(minimum:spacing:)` fits into the measured width — the same
    /// arithmetic the layout does, so up/down move exactly one visual row rather than a guess.
    /// Falls back to one column before the first measurement lands.
    private var columnCount: Int {
        let minimum: CGFloat = 130 // matches `columns` below on iOS/macOS
        let spacing: CGFloat = 18
        // The VStack's `.padding()` is inside the measured width, so take it back off.
        let usable = gridWidth - 32
        guard usable > 0 else { return 1 }
        return max(1, Int((usable + spacing) / (minimum + spacing)))
    }
    #endif

    private func tiles(_ entries: [GameEntry]) -> some View {
        LazyVGrid(columns: columns, spacing: 18) {
            ForEach(entries) { game in
                Group {
                    if let launch = launchAndRemember {
                        Button { launch(game.id) } label: {
                            GameCard(
                                game: game, artLoader: artLoader, selected: isKeyCursor(game),
                                isRunning: running[game.id] != nil)
                        }
                        .buttonStyle(.plain)
                    } else {
                        GameCard(
                            game: game, artLoader: artLoader, selected: isKeyCursor(game),
                            isRunning: running[game.id] != nil)
                    }
                }
                .id(game.id)
                // Right-click / long-press a poster for that TITLE's own actions — the same
                // gesture a host card answers to, one level down. `.contextMenu` doesn't exist
                // on tvOS, which is also the one platform with no clipboard to copy into.
                #if !os(tvOS)
                .contextMenu {
                    if LinkClipboard.isAvailable {
                        Button("Copy Link") { copyLink(game) }
                    }
                }
                #endif
            }
        }
    }

    /// Put this title's self-emitted `punktfunk://` link on the clipboard: the shelf's host,
    /// the pinned card's profile when a pin opened it, and the game's own `launch=` id — so
    /// the URL boots straight into the title, the way a host card's link opens the desktop
    /// (design/client-deep-links.md §5).
    ///
    /// Addressed to the STORE's current record rather than the one the shelf was opened with,
    /// so a host re-addressed while browsing hands out the address it actually has now.
    private func copyLink(_ game: GameEntry) {
        let current = store.hosts.first { $0.id == host.id } ?? host
        LinkClipboard.copy(
            DeepLink.forHost(current, launch: game.id, profile: target.pinnedProfileID).urlString)
    }

    /// Whether the keyboard cursor is on this tile (always false where there is no keyboard
    /// navigation to have moved it).
    private func isKeyCursor(_ game: GameEntry) -> Bool {
        #if os(iOS) || os(macOS)
        keyCursor == game.id
        #else
        false
        #endif
    }

    private func sectionHeader(_ text: String) -> some View {
        Text(text)
            .font(.geist(12, .semibold, relativeTo: .caption))
            .tracking(1.1)
            .foregroundStyle(.secondary)
    }

    private var columns: [GridItem] {
        #if os(tvOS)
        let minW: CGFloat = 220
        #else
        let minW: CGFloat = 130
        #endif
        return [GridItem(.adaptive(minimum: minW), spacing: 18)]
    }

    private func errorState(_ text: String) -> some View {
        VStack(spacing: 16) {
            Image(systemName: "exclamationmark.triangle")
                .font(.largeTitle)
                .foregroundStyle(.secondary)
            Text(text)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .frame(maxWidth: 420)
            Button("Retry") { Task { await load() } }
                .glassProminentButtonStyle()
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Image(systemName: "square.grid.2x2")
                .font(.largeTitle)
                .foregroundStyle(.secondary)
            Text("No games found on this host.")
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var reloadButton: some View {
        Button { Task { await load() } } label: {
            Label("Reload", systemImage: "arrow.clockwise")
        }
        .disabled(loading)
    }

    /// Sort and group for the plain grid — the console's bar, as a menu. The sort is the shared
    /// key (Default · A–Z · Platform · Store); the grouping is this grid's own (sections stand in
    /// for the console's Collections place).
    private var sortMenu: some View {
        Menu {
            Picker("Sort", selection: $sortRaw) {
                ForEach(LibrarySortKey.all, id: \.stored) { key in
                    Text(key.label).tag(key.stored)
                }
            }
            Picker("Group by", selection: $groupByRaw) {
                Text("None").tag("")
                Text("Platform").tag("platform")
                Text("Store").tag("store")
            }
        } label: {
            Label("Sort & group", systemImage: "line.3.horizontal.decrease.circle")
        }
    }

    private func load() async {
        loading = true
        errorText = nil
        // Dev hook, the twin of the desktop console's `PUNKTFUNK_FAKE_LIBRARY`: a file holding
        // the host's `/api/v1/library` JSON (or the shared collate vectors file, whose `library`
        // array is the same shape) stands in for the host, so the grid, the sort bar and the
        // collections can be exercised on a Mac with no host at all. No wake, no cache, no
        // `/status`, and no art — the posters are placeholders.
        if let fake = ProcessInfo.processInfo.environment["PUNKTFUNK_FAKE_LIBRARY"], !fake.isEmpty {
            loadFake(path: fake)
            return
        }
        let current = store.hosts.first { $0.id == host.id } ?? host
        // mTLS uses this client's persistent identity (the host paired it over QUIC). No identity
        // yet → the user hasn't connected/paired, which is also when there's nothing to browse.
        guard let identity = (try? ClientIdentityStore.shared.load())?.identity else {
            games = []
            errorText = "Connect to this host once first — the library uses the identity created "
                + "on pairing to authenticate."
            loading = false
            return
        }
        // Beyond the client identity, require the HOST's pinned fingerprint. MgmtTransport accepts
        // ANY cert for a pin-less host (self-signed, no SAN → system trust is bypassed), so browsing
        // one lets a LAN MITM serve a forged catalog and harvest this device's mTLS identity. A host
        // can hold a client identity yet no host pin (abandoned pairing, or after "Forget
        // Identity"), so this is a distinct check. security-review 2026-08-15 finding 8.
        guard current.pinnedSHA256 != nil else {
            games = []
            errorText = "Pair with this host before browsing its library."
            loading = false
            return
        }
        // Show the catalog we already have BEFORE talking to the host. A library is the screen a
        // player uses to decide what to play, and an empty one while a sleeping box boots is the
        // opposite of useful — so the last-known titles go up immediately, marked as remembered,
        // and are replaced the moment the host answers.
        if let cached = await LibraryCache.shared?.load(hostID: current.id.uuidString) {
            games = cached.games.launchersFirst
            servedFromCacheAt = cached.fetchedAt
        }
        // ...and wake the box while the player is still choosing. Waking has always been bound to
        // CONNECTING, which is too late to help: by then they have picked a title and are waiting
        // out a cold boot. Opening the library is the earliest honest signal that someone intends
        // to play.
        //
        // Sent up front and unconditionally rather than only when the host looks offline — the
        // same shape as the client core's own `orchestrate` path, and for the same reason: a magic
        // packet is a single fire-and-forget datagram that an already-awake machine ignores, so
        // waiting to find out whether it is needed costs more than sending it.
        let waking = !current.wakeMacs.isEmpty && PunktfunkConnection.wakeOnLANAvailable
        if waking {
            _ = PunktfunkConnection.wakeOnLAN(macs: current.wakeMacs, lastKnownIP: current.address)
        }

        // The art loader is built from the same identity whether or not the catalog fetch
        // succeeds, so cached posters render behind a cached catalog with the host still down.
        artLoader = try? LibraryArtLoader(
            address: current.address,
            port: current.effectiveMgmtPort,
            certPEM: identity.certPEM,
            keyPEM: identity.keyPEM,
            hostFingerprint: current.pinnedSHA256)

        // A woken box takes 20–60 s to answer, so one attempt would almost always land on a host
        // that is still POSTing. Retry across that window when we sent a packet; without one, ask
        // exactly once and report what happened, as before.
        let attempts = waking ? 12 : 1
        for attempt in 0..<attempts {
            if Task.isCancelled { break }
            do {
                // `launchersFirst` groups launcher entries ahead of titles once, here, so the grid
                // and the gamepad coverflow both inherit the D4 ordering.
                let fetched = try await LibraryClient.fetch(
                    address: current.address,
                    port: current.effectiveMgmtPort,
                    certPEM: identity.certPEM,
                    keyPEM: identity.keyPEM,
                    hostFingerprint: current.pinnedSHA256
                ).launchersFirst
                games = fetched
                servedFromCacheAt = nil
                errorText = nil
                await LibraryCache.shared?.store(fetched, hostID: current.id.uuidString)
                break
            } catch {
                // Anything other than "can't reach it" is settled — a rejected certificate does not
                // become acceptable by waiting, and retrying an unpaired host twelve times just
                // delays telling the user what is actually wrong.
                let unreachable: Bool
                if case .unreachable = error as? LibraryError { unreachable = true } else {
                    unreachable = false
                }
                let more = unreachable && attempt + 1 < attempts
                if !more {
                    // A cached catalog outranks the error: the titles on screen are still the right
                    // ones to choose from, and replacing them with a red message because the host
                    // is asleep is precisely what this cache exists to prevent. The staleness note
                    // carries the situation instead.
                    if games.isEmpty {
                        errorText = (error as? LibraryError)?.errorDescription
                            ?? error.localizedDescription
                    }
                    break
                }
                try? await Task.sleep(nanoseconds: 5 * NSEC_PER_SEC)
            }
        }

        // What's up on the host right now — never fatal, and deliberately after the catalog so a
        // slow `/status` can't hold the titles back.
        let live = await LibraryClient.running(
            address: current.address,
            port: current.effectiveMgmtPort,
            certPEM: identity.certPEM,
            keyPEM: identity.keyPEM,
            hostFingerprint: current.pinnedSHA256)
        running = Dictionary(
            live.filter(\.isUp).compactMap { g in g.appID.map { ($0, g) } },
            // Two sessions can have the same title up (the host admits concurrent sessions); for a
            // Resume badge either one is the same answer.
            uniquingKeysWith: { first, _ in first })
        loading = false
    }

    /// The `PUNKTFUNK_FAKE_LIBRARY` path: a plain `[GameEntry]` array, or a `{ "library": [...] }`
    /// wrapper (the shared vectors file). A bad file reads as an error state, not a crash.
    private func loadFake(path: String) {
        defer { loading = false }
        struct Wrapped: Decodable { let library: [GameEntry] }
        servedFromCacheAt = nil
        running = [:]
        guard let data = FileManager.default.contents(atPath: path) else {
            games = []
            errorText = "PUNKTFUNK_FAKE_LIBRARY: can't read \(path)"
            return
        }
        let decoder = JSONDecoder()
        if let list = try? decoder.decode([GameEntry].self, from: data) {
            games = list.launchersFirst
        } else if let wrapped = try? decoder.decode(Wrapped.self, from: data) {
            games = wrapped.library.launchersFirst
        } else {
            games = []
            errorText = "PUNKTFUNK_FAKE_LIBRARY: \(path) is not a library JSON"
        }
    }

    /// Every launch from this shelf goes through here, so the player's position is recorded on
    /// exactly one path however they picked the title — a tap, the keyboard, or the coverflow.
    /// `nil` in browse-only mode, which is what keeps the tiles untappable there.
    private var launchAndRemember: ((String) -> Void)? {
        guard let onLaunch else { return nil }
        return { id in
            LibraryScrollMemory.remember(id, forHost: host.id.uuidString)
            onLaunch(id)
        }
    }

    /// `host` → `host · profile` (a pinned card's shelf) → `host · profile · collection` (drilled
    /// into one group), joined with `·` — the desktop's title shape.
    private var shelfTitle: String {
        let base = target.title(in: profiles)
        guard let collectionLabel else { return base }
        return "\(base) \u{b7} \(collectionLabel)"
    }

    /// The catalog in display order — `LibraryOrder.display`, the desktop's `order()`: launcher
    /// entries lead, and anything already running leads WITHIN its band, so getting back into it
    /// is the first thing on the screen rather than something to scroll for. (Its predecessor put
    /// every running entry first, over `launchersFirst`, so a running game jumped ahead of the
    /// launcher prefix and the coverflow's heading read GAMES · LAUNCHERS · GAMES along the strip.)
    private var ordered: [GameEntry] {
        guard !running.isEmpty else { return games }
        return LibraryOrder.display(games, running: Set(running.keys))
    }

    /// Whether the titles on screen are remembered rather than observed, and what the host is
    /// doing about it — the three-state staleness both presentations show. Never an error: a
    /// cached library is a working library.
    private var staleness: LibraryStaleness {
        guard servedFromCacheAt != nil else { return .none }
        return loading ? .waking : .offline
    }
}

/// The catalog's provenance, as the shelf states it. Three states rather than a flag so "waking
/// the host…" can never be shown while nothing is happening — the same enum the desktop console
/// keeps (`Stale::{No, Waking, Offline}`), with its exact wording.
enum LibraryStaleness: Equatable {
    case none
    /// Served from disk; a fetch (and a wake) is in flight.
    case waking
    /// Served from disk; the host did not answer.
    case offline

    /// The note the shelf shows, or nil when the titles are live.
    var text: String? {
        switch self {
        case .none: return nil
        case .waking: return "Last known library — waking the host…"
        case .offline: return "Last known library — the host didn't answer"
        }
    }

    var symbol: String {
        self == .waking ? "arrow.clockwise" : "wifi.slash"
    }
}

#if os(iOS) || os(macOS)
/// Zero-size controller listener for the library's pre-coverflow states — B backs out, A retries
/// a failed fetch. The same shape as ConnectOverlay's `ConnectControllerInput`;
/// `GamepadMenuInput.needsSnapshot` swallows the held press that opened the screen. Unmounts the
/// moment the coverflow (and its own A/B) is up.
private struct LibraryBackCatcher: View {
    let active: Bool
    /// nil while there is nothing to retry — the press then does nothing, exactly like the
    /// legend, which shows no A cell in that state.
    var onConfirm: (() -> Void)?
    let onBack: () -> Void
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .onAppear {
                input.onBack = onBack
                input.onConfirm = onConfirm
                if active { input.start() }
            }
            // The retry closure comes and goes with the error; keep the poller's copy current.
            .onChange(of: onConfirm != nil) { _, _ in input.onConfirm = onConfirm }
            .onChange(of: active) { _, nowActive in
                if nowActive { input.start() } else { input.stop() }
            }
            .onDisappear { input.stop() }
    }
}
#endif

/// One poster tile. Steam vs custom is marked with a badge; the art walks the candidate URLs
/// (portrait → header → hero) and finally a text placeholder.
private struct GameCard: View {
    let game: GameEntry
    let artLoader: (any LibraryArtSource)?
    /// The hardware-keyboard cursor is on this tile — drawn as an accent ring, since the plain
    /// grid has no other way to say "Return launches THIS one".
    var selected = false
    /// This title is already up on the host, so picking it resumes rather than starts. Worth
    /// saying on the tile: the host has quietly adopted a running launch instead of starting a
    /// second copy for a while now, but nothing ever told the player that — so choosing a game
    /// they were already playing looked identical to starting one, and read as a relaunch.
    var isRunning = false

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            PosterImage(
                candidates: game.art.posterCandidates, title: game.title, loader: artLoader,
                icon: game.iconToken)
                .aspectRatio(2.0 / 3.0, contentMode: .fit)
                .frame(maxWidth: .infinity)
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay {
                    if selected {
                        RoundedRectangle(cornerRadius: 10, style: .continuous)
                            .strokeBorder(.tint, lineWidth: 3)
                    }
                }
                .overlay(alignment: .topLeading) {
                    StoreBadge(label: game.storeLabel, isLauncher: game.isLauncher)
                }
                // Opposite corner from the store badge so the two never collide on a narrow tile.
                .overlay(alignment: .topTrailing) {
                    if isRunning { RunningBadge(compact: true) }
                }
            Text(game.title)
                .font(.geist(12, relativeTo: .caption))
                .lineLimit(2)
                .foregroundStyle(.secondary)
        }
    }
}
