// The gamepad-driven home screen (iOS/iPadOS only): a distinct, "10-foot" console-style host
// launcher shown INSTEAD of HomeView while GamepadUIEnvironment is active — a separate screen built
// around a center-snapping carousel of hosts, driven from the couch with a controller. No touch is
// required (a tap still works as a fallback). Scope: browse saved + discovered hosts, connect, and
// — when the library flag is on — jump into a saved host's library (Y).
//
// All the scrolling/snapping/navigation/haptics live in GamepadCarousel; this file is the launcher's
// chrome. Layout discipline (so nothing is EVER clipped, portrait or landscape): the gradient is a
// `.background` modifier — NOT a ZStack sibling — because an `.ignoresSafeArea()` sibling expands the
// stack to full-screen and hands the GeometryReader the full height, laying content out under the
// status bar / home indicator. As a background it draws behind without affecting layout, so the
// GeometryReader is sized to the safe area. The title and the controller-glyph hints are pinned with
// `.safeAreaInset` (top / bottom-leading) — guaranteed inside the safe area and out of the carousel's
// vertical budget — and the card is sized off the remaining height. tvOS/macOS never mount this view.

import PunktfunkKit
import SwiftUI
#if os(iOS)
import GameController

/// One navigable tile: a saved host or a discovered-but-unsaved one. Hashable so it can be the
/// carousel's scroll-position identity.
private enum GamepadHomeTarget: Hashable {
    case saved(UUID)
    case discovered(String)
}

/// A fully-resolved launcher tile — display fields + the activate action, built fresh each render
/// from the live stores so nothing goes stale.
private struct HomeTile: Identifiable {
    let id: GamepadHomeTarget
    let title: String
    let subtitle: String
    let isOnline: Bool
    let isPaired: Bool
    let isConnecting: Bool
    /// Saved (solid monogram) vs. discovered-but-unsaved (tinted outline).
    let filled: Bool
    /// Only saved hosts have a library (matches the touch grid's context-menu gate).
    let hasLibrary: Bool
    let activate: () -> Void
}

struct GamepadHomeView: View {
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    @Binding var libraryTarget: StoredHost?
    let connect: (StoredHost) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void

    /// Same experimental gate the touch grid's "Browse Library…" context-menu item uses.
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = false
    /// `.compact` in a landscape phone window — drives tighter chrome so everything still fits.
    @Environment(\.verticalSizeClass) private var vSizeClass
    @State private var selection: GamepadHomeTarget?
    @State private var breathe = false

    private var compact: Bool { vSizeClass == .compact }

    var body: some View {
        GeometryReader { geo in
            hero(for: geo.size)
        }
        // Pinned inside the safe area, out of the carousel's vertical budget — never clipped.
        .safeAreaInset(edge: .top, spacing: 0) {
            titleView
                .padding(.top, compact ? 4 : 10)
                .padding(.bottom, compact ? 4 : 8)
                .frame(maxWidth: .infinity)
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            if !tiles.isEmpty {
                hintBar
                    .padding(.leading, 22)
                    .padding(.vertical, compact ? 6 : 10)
            }
        }
        .background { background }
        .onAppear {
            discovery.start()
            withAnimation(.easeInOut(duration: 4).repeatForever(autoreverses: true)) { breathe = true }
        }
        .onDisappear { discovery.stop() }
        .alert(
            "Connection failed",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } })
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(model.errorMessage ?? "")
        }
    }

    // MARK: - Hero (carousel + detail), sized to fit the space between the pinned title and hints

    @ViewBuilder private func hero(for size: CGSize) -> some View {
        if tiles.isEmpty {
            emptyState.frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            let cardWidth = min(340, size.width * 0.84)
            // 96 ≈ the carousel's own vertical breathing (+40) plus the detail line (~54); clamp so
            // the strip + detail always fit the region the safe-area insets leave.
            let cardHeight = min(compact ? 170 : 210, max(118, size.height - 96))
            VStack(spacing: compact ? 8 : 10) {
                Spacer(minLength: 0)
                carousel(cardWidth: cardWidth, cardHeight: cardHeight)
                detailPanel
                Spacer(minLength: 0)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    // MARK: - Chrome

    private var background: some View {
        ZStack {
            LinearGradient(
                colors: [.black, Color.brand.opacity(0.22), .black],
                startPoint: .top, endPoint: .bottom)
            // A soft brand orb behind the strip gives the flat gradient depth; it breathes slowly.
            Circle()
                .fill(RadialGradient(
                    colors: [Color.brand.opacity(0.55), .clear],
                    center: .center, startRadius: 0, endRadius: 300))
                .frame(width: 560, height: 560)
                .blur(radius: 70)
                .scaleEffect(breathe ? 1.08 : 0.92)
                .opacity(breathe ? 0.5 : 0.32)
                .offset(y: -20)
        }
        .ignoresSafeArea()
    }

    private var titleView: some View {
        Text("Select a Host")
            .font(.geist(compact ? 20 : 30, .bold, relativeTo: .title))
            .foregroundStyle(.white)
    }

    private var emptyState: some View {
        VStack(spacing: 14) {
            Image(systemName: "gamecontroller")
                .font(.system(size: 46, weight: .light))
                .foregroundStyle(Color.brand)
            Text("No hosts yet")
                .font(.geist(20, .semibold, relativeTo: .title3))
                .foregroundStyle(.white)
            Text("Add one with touch first — it'll show up here for the controller.")
                .font(.geist(15, relativeTo: .body))
                .foregroundStyle(.white.opacity(0.6))
                .multilineTextAlignment(.center)
                .frame(maxWidth: 320)
        }
    }

    // MARK: - Carousel

    private func carousel(cardWidth: CGFloat, cardHeight: CGFloat) -> some View {
        GamepadCarousel(
            items: tiles,
            selection: $selection,
            itemWidth: cardWidth,
            spacing: 30,
            onActivate: { $0.activate() },
            onSecondary: { openLibraryForSelected() },
            // Stop consuming the controller while the library is presented on top — otherwise the
            // launcher navigates behind it (invisibly on iPhone, visibly on iPad's page sheet).
            isActive: libraryTarget == nil
        ) { tile in
            hostCard(tile, size: CGSize(width: cardWidth, height: cardHeight))
        }
        .frame(height: cardHeight + 40)
    }

    /// The host tile plus its focus treatment. Every continuous visual reads the scroll view's own
    /// per-frame `phase` (real distance-from-centered), so the look always matches what's on screen
    /// mid-scroll. `.shadow`/`.overlay` aren't part of `VisualEffect`, so the focus pop is scale +
    /// brightness/saturation + a depth blur on the recessed neighbors.
    private func hostCard(_ tile: HomeTile, size: CGSize) -> some View {
        GamepadHostTile(tile: tile, size: size)
            .scrollTransition { content, phase in
                let d = CGFloat(min(abs(phase.value), 1))
                let scale = 1 - d * 0.12
                let bright = Double(-d * 0.24)
                let sat = Double(1 - d * 0.42)
                let soft = d * 3
                let fade = Double(1 - d * 0.22)
                return content
                    .scaleEffect(scale)
                    .brightness(bright)
                    .saturation(sat)
                    .blur(radius: soft)
                    .opacity(fade)
            }
    }

    /// The "now focused" host, spelled out below the strip — empty (not hidden) so the layout
    /// doesn't jump as the selection changes.
    @ViewBuilder private var detailPanel: some View {
        let tile = tiles.first { $0.id == selection }
        VStack(spacing: 6) {
            Text(tile?.title ?? " ")
                .font(.geist(22, .bold, relativeTo: .title2))
                .foregroundStyle(.white)
                .lineLimit(1)
            HStack(spacing: 10) {
                Text(tile?.subtitle ?? " ")
                    .font(.geist(13, relativeTo: .caption))
                    .foregroundStyle(.white.opacity(0.6))
                if let tile {
                    statusPill(online: tile.isOnline, paired: tile.isPaired)
                }
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 24)
        .animation(.smooth(duration: 0.25), value: selection)
    }

    private func statusPill(online: Bool, paired: Bool) -> some View {
        HStack(spacing: 6) {
            Circle()
                .fill(online ? Color.green : Color.white.opacity(0.35))
                .frame(width: 6, height: 6)
            Text(online ? "ONLINE" : "OFFLINE")
            if paired { Text("· PAIRED") }
        }
        .font(.geist(11, .medium, relativeTo: .caption2))
        .tracking(0.8)
        .foregroundStyle(.white.opacity(0.55))
    }

    // MARK: - Hint bar (pinned bottom-leading via safeAreaInset)

    private var hintBar: some View {
        HStack(spacing: 18) {
            hint(glyph: buttonGlyph(\.buttonA, fallback: "a.circle"), text: "Connect")
            if showsLibraryHint {
                hint(glyph: buttonGlyph(\.buttonY, fallback: "y.circle"), text: "Library")
            }
        }
        .font(.geist(14, .semibold, relativeTo: .subheadline))
        .foregroundStyle(.white.opacity(0.85))
    }

    private func hint(glyph: String, text: String) -> some View {
        HStack(spacing: 7) {
            Image(systemName: glyph)
                .font(.system(size: 19))
                .foregroundStyle(.white)
            Text(text)
        }
        .fixedSize() // keep glyph + label together; never truncate a hint mid-word
    }

    private var showsLibraryHint: Bool {
        guard libraryEnabled else { return false }
        return tiles.first { $0.id == selection }?.hasLibrary ?? false
    }

    /// The active controller's real glyph for a button (Xbox "A", DualSense ✕, …) via
    /// `sfSymbolsName`; a generic fallback before a controller profile resolves.
    private func buttonGlyph(
        _ button: KeyPath<GCExtendedGamepad, GCControllerButtonInput>, fallback: String
    ) -> String {
        GamepadManager.shared.active?.controller.extendedGamepad?[keyPath: button].sfSymbolsName
            ?? fallback
    }

    // MARK: - Data + actions

    /// Built fresh each render from the live stores (no stale value capture) — saved hosts first,
    /// then discovered-but-unsaved ones.
    private var tiles: [HomeTile] {
        let saved = store.hosts.map { host in
            HomeTile(
                id: .saved(host.id),
                title: host.displayName,
                subtitle: "\(host.address):\(String(host.port))",
                isOnline: isOnline(host),
                isPaired: host.pinnedSHA256 != nil,
                isConnecting: model.phase == .connecting && model.activeHost?.id == host.id,
                filled: true,
                hasLibrary: true,
                activate: { connect(host) })
        }
        let discovered = discoveredUnsaved.map { d in
            HomeTile(
                id: .discovered(d.id),
                title: d.name,
                subtitle: "\(d.host):\(String(d.port))",
                isOnline: true,
                isPaired: false,
                isConnecting: false,
                filled: false,
                hasLibrary: false,
                activate: { connectDiscovered(d) })
        }
        return saved + discovered
    }

    /// Only saved hosts have a library — matches the touch grid, where "Browse Library…" is a
    /// `HostCardView`-only action never offered on `DiscoveredCardView`.
    private func openLibraryForSelected() {
        guard libraryEnabled, case .saved(let id) = selection,
              let host = store.hosts.first(where: { $0.id == id })
        else { return }
        libraryTarget = host
    }

    private func isOnline(_ host: StoredHost) -> Bool {
        discovery.hosts.contains { host.matches($0) }
    }

    private var discoveredUnsaved: [DiscoveredHost] {
        discovery.hosts.filter { d in !store.hosts.contains { $0.matches(d) } }
    }
}

/// One "console tile" in the host carousel — a dark-glass landscape card, bigger and bolder than the
/// touch grid's `HostCardView`. Renders only its base look; the centered-tile pop is layered on by
/// the caller's `.scrollTransition` so it always tracks the real scroll position.
private struct GamepadHostTile: View {
    let tile: HomeTile
    let size: CGSize

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .top) {
                monogramBadge
                Spacer(minLength: 0)
                if tile.isOnline {
                    Circle()
                        .fill(Color.green)
                        .frame(width: 9, height: 9)
                        .shadow(color: .green.opacity(0.7), radius: 5)
                }
            }
            Spacer(minLength: 0)
            Text(tile.title)
                .font(.geist(23, .bold, relativeTo: .title2))
                .foregroundStyle(.white)
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            Text(tile.subtitle)
                .font(.geist(13, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.55))
                .lineLimit(1)
                .padding(.top, 2)
        }
        .padding(20)
        .frame(width: size.width, height: size.height, alignment: .leading)
        .background {
            RoundedRectangle(cornerRadius: 26, style: .continuous)
                .fill(.ultraThinMaterial)
                .environment(\.colorScheme, .dark)
        }
        .overlay {
            RoundedRectangle(cornerRadius: 26, style: .continuous)
                .strokeBorder(
                    LinearGradient(
                        colors: [.white.opacity(0.22), .white.opacity(0.04)],
                        startPoint: .top, endPoint: .bottom),
                    style: StrokeStyle(lineWidth: 1, dash: tile.filled ? [] : [6, 5]))
        }
        .clipShape(RoundedRectangle(cornerRadius: 26, style: .continuous))
        .shadow(color: .black.opacity(0.45), radius: 20, y: 14)
    }

    private var monogramBadge: some View {
        let shape = RoundedRectangle(cornerRadius: 15, style: .continuous)
        return ZStack {
            shape.fill(tile.filled
                ? AnyShapeStyle(LinearGradient(
                    colors: [Color.brand, Color.brand.opacity(0.68)],
                    startPoint: .top, endPoint: .bottom))
                : AnyShapeStyle(Color.brand.opacity(0.16)))
            if tile.isConnecting {
                ProgressView().tint(.white)
            } else {
                Text(monogram(tile.title))
                    .font(.geistFixed(25, .bold))
                    .foregroundStyle(tile.filled ? .white : Color.brand)
            }
        }
        .frame(width: 52, height: 52)
        .overlay {
            if !tile.filled {
                shape.strokeBorder(Color.brand.opacity(0.5), lineWidth: 1)
            }
        }
    }

    private func monogram(_ name: String) -> String {
        guard let first = name.trimmingCharacters(in: .whitespacesAndNewlines).first else { return "•" }
        return String(first).uppercased()
    }
}
#endif
