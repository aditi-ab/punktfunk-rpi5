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

    @State private var games: [GameEntry] = []
    @State private var loading = false
    @State private var errorText: String?

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
            }
            .task { await load() }
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
            grid
        }
    }

    private var grid: some View {
        ScrollView {
            LazyVGrid(columns: columns, spacing: 18) {
                ForEach(games) { game in
                    if let onLaunch {
                        Button { onLaunch(game.id) } label: { GameCard(game: game) }
                            .buttonStyle(.plain)
                    } else {
                        GameCard(game: game)
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
                .buttonStyle(.borderedProminent)
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

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            PosterImage(candidates: game.art.posterCandidates, title: game.title)
                .aspectRatio(2.0 / 3.0, contentMode: .fit)
                .frame(maxWidth: .infinity)
                .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
                .overlay(alignment: .topLeading) { storeBadge }
            Text(game.title)
                .font(.caption)
                .lineLimit(2)
                .foregroundStyle(.secondary)
        }
    }

    private var storeBadge: some View {
        Text(game.isCustom ? "Custom" : "Steam")
            .font(.caption2.weight(.semibold))
            .padding(.horizontal, 6)
            .padding(.vertical, 3)
            .background(.ultraThinMaterial, in: Capsule())
            .padding(6)
    }
}

/// Sequentially tries cover-art URLs, advancing past any that fail to load, then a placeholder.
private struct PosterImage: View {
    let candidates: [URL]
    let title: String
    @State private var index = 0

    var body: some View {
        if index < candidates.count {
            AsyncImage(url: candidates[index]) { phase in
                switch phase {
                case .success(let image):
                    image.resizable().scaledToFill()
                case .failure:
                    // Advance to the next candidate on the next render pass.
                    Color.clear.onAppear { index += 1 }
                case .empty:
                    ZStack { placeholder; ProgressView() }
                @unknown default:
                    placeholder
                }
            }
            .id(index) // recreate AsyncImage so it loads the newly-selected URL
        } else {
            placeholder
        }
    }

    private var placeholder: some View {
        ZStack {
            Rectangle().fill(.quaternary)
            Text(title)
                .font(.headline)
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding(8)
        }
    }
}
