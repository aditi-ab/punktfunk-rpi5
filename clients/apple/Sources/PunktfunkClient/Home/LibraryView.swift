// Experimental game-library browser (plan step 3, gated behind DefaultsKey.libraryEnabled).
// Renders a poster grid of the host's library fetched over the management API. Read-only:
// launching a chosen title is a later step. Reached from a host card's "Browse Library…"
// context-menu action, which only appears when the feature flag is on.

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
    @Environment(\.dismiss) private var dismiss
    /// Resolves a pinned shelf's profile NAME for the title (the target carries only its id).
    @ObservedObject private var profiles = ProfileStore.shared

    /// The host this shelf belongs to — every fetch, every poster URL and the launch itself address
    /// it, and a pinned shelf is the same host seen through one of its cards.
    private var host: StoredHost { target.host }

    @State private var games: [GameEntry] = []
    @State private var loading = false
    @State private var errorText: String?
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
            .navigationTitle("\(target.title(in: profiles)) — Library")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                #if os(macOS)
                ToolbarItemGroup { reloadButton }
                #else
                ToolbarItem(placement: .primaryAction) { reloadButton }
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
                    LibraryBackCatcher(active: controllerActive) { (onClose ?? { dismiss() })() }
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
                LibraryCoverflowView(
                    games: games, artLoader: artLoader, onLaunch: onLaunch,
                    onDismiss: { (onClose ?? { dismiss() })() },
                    controllerActive: controllerActive)
            } else {
                grid
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
    @ViewBuilder private func consoleField(_ view: some View) -> some View {
        #if os(iOS) || os(macOS) || os(tvOS)
        view.background {
            if gamepadUIActive, !hostedInShell { GamepadScreenBackground() }
        }
        #else
        view
        #endif
    }

    private var grid: some View {
        // Design D4: launcher entries get their own section above the titles, never interleaved.
        // Both headers appear only when both groups exist, so a library without launcher entries
        // renders exactly as it did before.
        let launchers = games.filter(\.isLauncher)
        let titles = games.filter { !$0.isLauncher }
        let both = !launchers.isEmpty && !titles.isEmpty
        return ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    if !launchers.isEmpty {
                        if both { sectionHeader("Launchers") }
                        tiles(launchers)
                    }
                    if !titles.isEmpty {
                        if both { sectionHeader("Games") }
                        tiles(titles)
                    }
                }
                .padding()
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
                    guard let next = gridNav(launchers: launchers, titles: titles)
                        .move(from: keyCursor, direction) else { return }
                    keyCursor = next
                    withAnimation(.easeOut(duration: 0.18)) { proxy.scrollTo(next, anchor: .center) }
                },
                onConfirm: {
                    guard let onLaunch, let id = keyCursor else { return }
                    onLaunch(id)
                })
            #endif
        }
    }

    #if os(iOS) || os(macOS)
    /// The keyboard cursor's model over the two grid sections. Rebuilt per press from the live
    /// sections so it can never point into a stale list.
    private func gridNav(launchers: [GameEntry], titles: [GameEntry]) -> LibraryGridNav {
        LibraryGridNav(
            sections: [launchers, titles].filter { !$0.isEmpty }.map { $0.map(\.id) },
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
                if let onLaunch {
                    Button { onLaunch(game.id) } label: {
                        GameCard(game: game, artLoader: artLoader, selected: isKeyCursor(game))
                    }
                    .buttonStyle(.plain)
                    .id(game.id)
                } else {
                    GameCard(game: game, artLoader: artLoader, selected: isKeyCursor(game))
                        .id(game.id)
                }
            }
        }
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

    private func load() async {
        loading = true
        errorText = nil
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
        do {
            // `launchersFirst` groups launcher entries ahead of titles once, here, so the grid and
            // the gamepad coverflow both inherit the D4 ordering.
            games = try await LibraryClient.fetch(
                address: current.address,
                port: current.effectiveMgmtPort,
                certPEM: identity.certPEM,
                keyPEM: identity.keyPEM,
                hostFingerprint: current.pinnedSHA256
            ).launchersFirst
            artLoader = try LibraryArtLoader(
                address: current.address,
                port: current.effectiveMgmtPort,
                certPEM: identity.certPEM,
                keyPEM: identity.keyPEM,
                hostFingerprint: current.pinnedSHA256)
        } catch {
            games = []
            errorText = (error as? LibraryError)?.errorDescription ?? error.localizedDescription
        }
        loading = false
    }
}

#if os(iOS) || os(macOS)
/// Zero-size controller listener for the library's pre-coverflow states — B backs out. The same
/// shape as ConnectOverlay's `ConnectControllerInput`; `GamepadMenuInput.needsSnapshot` swallows
/// the held press that opened the screen. Unmounts the moment the coverflow (and its own B) is up.
private struct LibraryBackCatcher: View {
    let active: Bool
    let onBack: () -> Void
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .onAppear {
                input.onBack = onBack
                if active { input.start() }
            }
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
            Text(game.title)
                .font(.geist(12, relativeTo: .caption))
                .lineLimit(2)
                .foregroundStyle(.secondary)
        }
    }
}
