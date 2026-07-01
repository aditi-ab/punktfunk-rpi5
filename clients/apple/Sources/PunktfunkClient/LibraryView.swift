// Experimental game-library browser (plan step 3, gated behind DefaultsKey.libraryEnabled).
// Renders a poster grid of the host's library fetched over the management API. Read-only:
// launching a chosen title is a later step. Reached from a host card's "Browse Library…"
// context-menu action, which only appears when the feature flag is on.

import PunktfunkKit
import SwiftUI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

struct LibraryView: View {
    @ObservedObject var store: HostStore
    let host: StoredHost
    /// Tapping a title starts a session that asks the host to launch it (the library id is passed
    /// through). `nil` ⇒ browse-only (cards aren't tappable).
    var onLaunch: ((String) -> Void)? = nil
    @Environment(\.dismiss) private var dismiss

    @State private var games: [GameEntry] = []
    @State private var loading = false
    @State private var errorText: String?
    /// Authenticated session for cover-art fetches (the same paired identity + host pinning as the
    /// list fetch, reused across every poster in the grid). Built alongside `games` in `load()`;
    /// torn down on disappear since it isn't one-shot like `LibraryClient.fetch`'s own session.
    @State private var imageSession: URLSession?
    #if os(iOS)
    // Gamepad-driven browsing is iOS/iPadOS-only — see HomeView's identical gate. tvOS keeps its
    // existing plain-grid presentation of this same view unchanged.
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
                imageSession?.finishTasksAndInvalidate()
                imageSession = nil
            }
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
            #if os(iOS)
            if gamepadUIActive {
                LibraryCoverflowView(
                    games: games, imageSession: imageSession, onLaunch: onLaunch,
                    onDismiss: { dismiss() })
            } else {
                grid
            }
            #else
            grid
            #endif
        }
    }

    private var grid: some View {
        ScrollView {
            LazyVGrid(columns: columns, spacing: 18) {
                ForEach(games) { game in
                    if let onLaunch {
                        Button { onLaunch(game.id) } label: { GameCard(game: game, imageSession: imageSession) }
                            .buttonStyle(.plain)
                    } else {
                        GameCard(game: game, imageSession: imageSession)
                    }
                }
            }
            .padding()
        }
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
            games = try await LibraryClient.fetch(
                address: current.address,
                port: current.effectiveMgmtPort,
                certPEM: identity.certPEM,
                keyPEM: identity.keyPEM,
                hostFingerprint: current.pinnedSHA256)
            imageSession?.finishTasksAndInvalidate()
            imageSession = try LibraryImageLoader.session(
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

/// One poster tile. Steam vs custom is marked with a badge; the art walks the candidate URLs
/// (portrait → header → hero) and finally a text placeholder.
private struct GameCard: View {
    let game: GameEntry
    let imageSession: URLSession?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            PosterImage(candidates: game.art.posterCandidates, title: game.title, session: imageSession)
                .aspectRatio(2.0 / 3.0, contentMode: .fit)
                .frame(maxWidth: .infinity)
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(alignment: .topLeading) { StoreBadge(isCustom: game.isCustom) }
            Text(game.title)
                .font(.geist(12, relativeTo: .caption))
                .lineLimit(2)
                .foregroundStyle(.secondary)
        }
    }
}

/// The store-provenance badge (Steam vs. a user-curated custom entry) overlaid on a poster —
/// shared by the touch grid's `GameCard` and the gamepad coverflow's cover cell.
struct StoreBadge: View {
    let isCustom: Bool

    var body: some View {
        Text(isCustom ? "Custom" : "Steam")
            .font(.geist(11, .semibold, relativeTo: .caption2))
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(.ultraThinMaterial, in: Capsule())
            .padding(6)
    }
}

#if canImport(UIKit)
private typealias PlatformImage = UIImage
#elseif canImport(AppKit)
private typealias PlatformImage = NSImage
#endif

private extension Image {
    init(platformImage: PlatformImage) {
        #if canImport(UIKit)
        self.init(uiImage: platformImage)
        #elseif canImport(AppKit)
        self.init(nsImage: platformImage)
        #endif
    }
}

/// Sequentially tries cover-art URLs over `session` (so a paired client can reach the host's own
/// art proxy, not just public CDNs — see `LibraryImageLoader`), advancing past any that fail to
/// load, then a placeholder. The loaded image is hard-clipped to fill the card's actual frame
/// regardless of its own aspect ratio: a portrait capsule fills it as intended, but a fallback
/// banner (wide hero/header art, used when a title has no portrait capsule) would otherwise report
/// a much wider intrinsic size than the card and overflow into neighboring cards. Not `private` —
/// the gamepad coverflow (`LibraryCoverflowView`) reuses it directly rather than re-fetching art.
struct PosterImage: View {
    let candidates: [URL]
    let title: String
    let session: URLSession?
    @State private var index = 0
    @State private var image: PlatformImage?

    var body: some View {
        Group {
            if let image {
                Image(platformImage: image)
                    .resizable()
                    .scaledToFill()
            } else if index < candidates.count {
                ZStack { placeholder; ProgressView() }
            } else {
                placeholder
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .task(id: index) { await loadCurrent() }
    }

    private func loadCurrent() async {
        guard index < candidates.count else { return }
        guard let session, let data = try? await session.data(from: candidates[index]).0,
              let loaded = PlatformImage(data: data)
        else {
            index += 1 // advance to the next candidate (or past the end → placeholder)
            return
        }
        image = loaded
    }

    private var placeholder: some View {
        ZStack {
            Rectangle().fill(.quaternary)
            Text(title)
                .font(.geist(17, .semibold, relativeTo: .headline))
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding(8)
        }
    }
}
