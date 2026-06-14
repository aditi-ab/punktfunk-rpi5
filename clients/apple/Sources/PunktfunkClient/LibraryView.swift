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
    @State private var showConfig = false
    // Connection form state, seeded from the saved host.
    @State private var portText: String = ""
    @State private var tokenText: String = ""

    var body: some View {
        content
            .navigationTitle("\(host.displayName) — Library")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                #if os(macOS)
                ToolbarItemGroup {
                    connectionButton
                    reloadButton
                }
                #else
                ToolbarItem(placement: .primaryAction) { reloadButton }
                ToolbarItem(placement: .cancellationAction) { connectionButton }
                #endif
            }
            .sheet(isPresented: $showConfig) { connectionSheet }
            .task {
                seedForm()
                await load()
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
            Button("Connection Settings…") { showConfig = true }
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

    private var connectionButton: some View {
        Button { showConfig = true } label: {
            Label("Connection", systemImage: "network")
        }
    }

    private var connectionSheet: some View {
        NavigationStack {
            Form {
                Section {
                    LabeledContent("Host") { Text(host.address) }
                    TextField("Management port", text: $portText)
                        #if !os(macOS)
                        .keyboardType(.numberPad)
                        #endif
                    TextField("Management token", text: $tokenText)
                        .autocorrectionDisabled(true)
                        #if !os(macOS)
                        .textInputAutocapitalization(.never)
                        #endif
                } header: {
                    Text("Management API")
                } footer: {
                    Text("The host must expose its management API on the LAN: "
                        + "`serve --mgmt-bind 0.0.0.0 --mgmt-token <token>`. The default port "
                        + "is \(punktfunkDefaultMgmtPort). Enter the same token here.")
                }
            }
            .navigationTitle("Library Connection")
            #if os(iOS)
            .navigationBarTitleDisplayMode(.inline)
            #endif
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save") {
                        let port = UInt16(portText) ?? punktfunkDefaultMgmtPort
                        store.setMgmt(host.id, port: port, token: tokenText)
                        showConfig = false
                        Task { await load() }
                    }
                }
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { showConfig = false }
                }
            }
        }
    }

    private func seedForm() {
        // Always reflect the latest saved values (the host snapshot may predate a setMgmt).
        let current = store.hosts.first { $0.id == host.id } ?? host
        portText = String(current.effectiveMgmtPort)
        tokenText = current.mgmtToken ?? ""
    }

    private func load() async {
        loading = true
        errorText = nil
        let current = store.hosts.first { $0.id == host.id } ?? host
        do {
            games = try await LibraryClient.fetch(
                address: current.address,
                port: current.effectiveMgmtPort,
                token: current.mgmtToken)
        } catch {
            games = []
            if let libError = error as? LibraryError {
                errorText = libError.errorDescription
                // Token rejected — drop the user straight into the connection form.
                if case .unauthorized = libError { showConfig = true }
            } else {
                errorText = error.localizedDescription
            }
            // No credential entered yet → also straight to setup.
            if current.mgmtToken == nil { showConfig = true }
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
