// Experimental game-library browser (plan step 3, gated behind DefaultsKey.libraryEnabled).
// Renders a poster grid of the host's library fetched over the management API. Read-only:
// launching a chosen title is a later step. Reached from a host card's "Browse Library…"
// context-menu action, which only appears when the feature flag is on.

import PunktfunkKit
import SwiftUI

struct LibraryView: View {
    @ObservedObject var store: HostStore
    let host: StoredHost
    /// Tapping a title starts a session that asks the host to launch it (the library id is passed
    /// through). `nil` ⇒ browse-only (cards aren't tappable).
    var onLaunch: ((String) -> Void)? = nil
    /// How the gamepad shell (GamepadLibraryScreen) closes this screen; nil — every sheet/cover
    /// presentation — falls back to the environment dismiss.
    var onClose: (() -> Void)? = nil
    /// Whether the gamepad coverflow owns the controller — the shell gates it during a push/pop
    /// and while the connect takeover is up. Presentations that cover the launcher keep the
    /// default (their being up IS the launcher's gate).
    var controllerActive = true
    @Environment(\.dismiss) private var dismiss

    @State private var games: [GameEntry] = []
    @State private var loading = false
    @State private var errorText: String?
    /// Cover-art loader (the same paired identity + host pinning as the list fetch, reused across
    /// every poster in the grid). Built alongside `games` in `load()`; dropped on disappear.
    @State private var artLoader: LibraryArtLoader?
    #if os(iOS) || os(macOS) || os(tvOS)
    // Gamepad-driven browsing — see ContentView's identical gate. With no controller (or the
    // setting off) every platform keeps the plain-grid presentation of this same view.
    @ObservedObject private var gamepadManager = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    private var gamepadUIActive: Bool {
        GamepadUIEnvironment.isActive(
            gamepadConnected: gamepadManager.active != nil, enabledSetting: gamepadUIEnabled)
    }
    #endif

    var body: some View {
        content
            .navigationTitle("\(host.displayName) — Library")
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
    }

    @ViewBuilder private var content: some View {
        if loading && games.isEmpty {
            ProgressView("Loading library…")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if let errorText, games.isEmpty {
            errorState(errorText)
        } else if games.isEmpty {
            emptyState
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

    private var grid: some View {
        // Design D4: launcher entries get their own section above the titles, never interleaved.
        // Both headers appear only when both groups exist, so a library without launcher entries
        // renders exactly as it did before.
        let launchers = games.filter(\.isLauncher)
        let titles = games.filter { !$0.isLauncher }
        let both = !launchers.isEmpty && !titles.isEmpty
        return ScrollView {
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
        }
    }

    private func tiles(_ entries: [GameEntry]) -> some View {
        LazyVGrid(columns: columns, spacing: 18) {
            ForEach(entries) { game in
                if let onLaunch {
                    Button { onLaunch(game.id) } label: { GameCard(game: game, artLoader: artLoader) }
                        .buttonStyle(.plain)
                } else {
                    GameCard(game: game, artLoader: artLoader)
                }
            }
        }
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
    let artLoader: LibraryArtLoader?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            PosterImage(candidates: game.art.posterCandidates, title: game.title, loader: artLoader)
                .aspectRatio(2.0 / 3.0, contentMode: .fit)
                .frame(maxWidth: .infinity)
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
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
