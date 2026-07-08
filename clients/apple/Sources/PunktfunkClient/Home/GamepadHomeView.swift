// The gamepad-driven home screen (iOS/iPadOS only): a distinct, "10-foot" console-style host
// launcher shown INSTEAD of HomeView while GamepadUIEnvironment is active — a separate screen built
// around a center-snapping carousel of hosts, driven from the couch with a controller. No touch is
// required anywhere: A connects, Y opens a saved host's library (when the flag is on), X opens the
// gamepad settings screen, and the carousel always ends in an Add Host tile that opens the
// controller-keyboard add flow. (A tap still works as a fallback for all of it.)
//
// All the scrolling/snapping/navigation/haptics live in GamepadCarousel; this file is the launcher's
// chrome. Layout discipline (so nothing is EVER clipped, portrait or landscape): the gradient is a
// `.background` modifier — NOT a ZStack sibling — because an `.ignoresSafeArea()` sibling expands the
// stack to full-screen and hands the GeometryReader the full height, laying content out under the
// status bar / home indicator. As a background it draws behind without affecting layout, so the
// GeometryReader is sized to the safe area. The title and the controller-glyph hints are pinned with
// `.safeAreaInset` (top / bottom-leading) — guaranteed inside the safe area and out of the carousel's
// vertical budget — and the card is sized off the remaining height. macOS mounts it too (the
// couch Mac-mini case) — same screen, with the settings/add-host covers presented as sheets
// (macOS has no fullScreenCover). tvOS never mounts this view (native focus engine instead).

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS)
import GameController

/// One navigable tile: a saved host, a discovered-but-unsaved one, or the trailing Add Host
/// action. Hashable so it can be the carousel's scroll-position identity.
private enum GamepadHomeTarget: Hashable {
    case saved(UUID)
    case discovered(String)
    case addHost
}

/// A fully-resolved launcher tile — display fields + the activate action, built fresh each render
/// from the live stores so nothing goes stale.
private struct HomeTile: Identifiable {
    let id: GamepadHomeTarget
    let title: String
    let subtitle: String
    var isOnline = false
    var isPaired = false
    var isConnecting = false
    /// Saved (solid monogram) vs. discovered-but-unsaved / action (tinted outline).
    var filled = false
    /// Only saved hosts have a library (matches the touch grid's context-menu gate).
    var hasLibrary = false
    /// Shows this SF symbol in the badge instead of the title monogram (the Add Host tile).
    var icon: String?
    /// Offline saved host we hold a MAC for (and WoL is available) — activating it wakes first.
    var canWake = false
    let activate: () -> Void
}

struct GamepadHomeView: View {
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    @Binding var libraryTarget: StoredHost?
    /// Wake-and-wait driver — gates the carousel while its overlay is up, and the carousel's
    /// activate routes an offline+wakeable host through it (see ContentView.startSession).
    @ObservedObject var waker: HostWaker
    let connect: (StoredHost) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void

    /// Same experimental gate the touch grid's "Browse Library…" context-menu item uses.
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = false
    #if os(iOS)
    /// `.compact` in a landscape phone window — drives tighter chrome so everything still fits.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the window minimum keeps room
    #endif
    @ObservedObject private var gamepads = GamepadManager.shared
    @State private var selection: GamepadHomeTarget?
    @State private var showSettings = false
    @State private var showAddHost = false

    var body: some View {
        GeometryReader { geo in
            hero(for: geo.size)
        }
        // Pinned inside the safe area, out of the carousel's vertical budget — never clipped.
        .safeAreaInset(edge: .top, spacing: 0) {
            titleBar
                .padding(.top, gamepadTitleTopPadding(compact: compact))
                .padding(.bottom, compact ? 4 : 8)
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            GamepadHintBar(hints: hints)
                // Equal distance from the left and bottom edges — the pill's corner inset was the
                // real asymmetry (leading 22 vs bottom 10), not its internal padding.
                .padding(.leading, compact ? 12 : 18)
                .padding(.bottom, compact ? 12 : 18)
                .padding(.top, compact ? 4 : 8)
        }
        .background { GamepadScreenBackground() }
        .onAppear { discovery.start() }
        .onDisappear { discovery.stop() }
        // Reachability sweep (mDNS-independent) so routed/VPN hosts that never advertise still show
        // Online — the console mirror of HomeView's `.task`; cancelled on disappear.
        .task {
            while !Task.isCancelled {
                await store.refreshReachability(discovery: discovery)
                try? await Task.sleep(for: .seconds(10))
            }
        }
        // The settings / add-host screens take over the controller (the carousel's `isActive`
        // gate above). iOS presents them full screen — the immersive console feel; macOS has no
        // fullScreenCover, so they become generously sized sheets over the dimmed launcher.
        #if os(macOS)
        .sheet(isPresented: $showSettings) {
            GamepadSettingsView()
                .frame(width: 720, height: 640)
        }
        .sheet(isPresented: $showAddHost) {
            GamepadAddHostView { store.add($0) }
                .frame(width: 660, height: 620)
        }
        .frame(minWidth: 640, minHeight: 420)
        #else
        .fullScreenCover(isPresented: $showSettings) { GamepadSettingsView() }
        .fullScreenCover(isPresented: $showAddHost) {
            GamepadAddHostView { store.add($0) }
        }
        #endif
    }

    // MARK: - Hero (carousel + detail), sized to fit the space between the pinned title and hints

    @ViewBuilder private func hero(for size: CGSize) -> some View {
        let cardWidth = min(340, size.width * 0.84)
        // 48 ≈ the carousel's own vertical breathing (+40) plus a small margin; clamp so the strip
        // always fits the region the pinned title / hints safe-area insets leave. (The old detail
        // line below the strip is gone — it only re-printed what the centered card already shows.)
        let cardHeight = min(compact ? 176 : 224, max(118, size.height - 48))
        VStack(spacing: compact ? 8 : 10) {
            Spacer(minLength: 0)
            carousel(cardWidth: cardWidth, cardHeight: cardHeight)
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: - Chrome

    private var titleBar: some View {
        Text("Select a Host")
            .font(.geist(compact ? 20 : 30, .bold, relativeTo: .title))
            .foregroundStyle(.white)
            .frame(maxWidth: .infinity)
            .overlay(alignment: .trailing) {
                // Which pad is driving this UI (name + battery) — quiet, and only where there's
                // room; a compact-height phone gives the pixels to the carousel instead.
                if !compact, let active = gamepads.active {
                    ControllerStatusChip(controller: active)
                        .padding(.trailing, 20)
                }
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
            onTertiary: { showSettings = true },
            // Stop consuming the controller while another screen (or the wake overlay) is on top —
            // otherwise the launcher navigates behind it (invisibly on iPhone, visibly on iPad).
            isActive: libraryTarget == nil && !showSettings && !showAddHost && waker.waking == nil
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

    // MARK: - Hint bar (pinned bottom-leading via safeAreaInset)

    private var hints: [GamepadHint] {
        let selected = tiles.first { $0.id == selection }
        var hints = [GamepadHint(
            glyph: buttonGlyph(\.buttonA, fallback: "a.circle"),
            text: selected?.id == .addHost ? "Add Host"
                : (selected?.canWake == true ? "Wake & Connect" : "Connect"))]
        if libraryEnabled, selected?.hasLibrary == true {
            hints.append(.init(glyph: buttonGlyph(\.buttonY, fallback: "y.circle"), text: "Library"))
        }
        hints.append(.init(glyph: buttonGlyph(\.buttonX, fallback: "x.circle"), text: "Settings"))
        return hints
    }

    // MARK: - Data + actions

    /// Built fresh each render from the live stores (no stale value capture) — saved hosts first,
    /// then discovered-but-unsaved ones, then the Add Host action tile (so the strip is never
    /// empty and manual entry is always one press away).
    private var tiles: [HomeTile] {
        let saved = store.hosts.map { host in
            HomeTile(
                id: .saved(host.id),
                title: host.displayName,
                subtitle: "\(host.address):\(String(host.port))",
                // Online = advertising on mDNS OR proven reachable by the probe (a routed/VPN host
                // never advertises); the wake item is offered only when neither holds.
                isOnline: discovery.advertises(host) || store.probedOnline.contains(host.id),
                isPaired: host.pinnedSHA256 != nil,
                isConnecting: model.phase == .connecting && model.activeHost?.id == host.id,
                filled: true,
                hasLibrary: true,
                canWake: PunktfunkConnection.wakeOnLANAvailable
                    && !discovery.advertises(host) && !store.probedOnline.contains(host.id)
                    && !host.wakeMacs.isEmpty,
                activate: { connect(host) })
        }
        let discovered = discovery.unsaved(among: store.hosts).map { d in
            HomeTile(
                id: .discovered(d.id),
                title: d.name,
                subtitle: "\(d.host):\(String(d.port))",
                isOnline: true,
                activate: { connectDiscovered(d) })
        }
        let add = HomeTile(
            id: .addHost,
            title: "Add Host",
            subtitle: "Register a host by address",
            icon: "plus",
            activate: { showAddHost = true })
        return saved + discovered + [add]
    }

    /// Only saved hosts have a library — matches the touch grid, where "Browse Library…" is a
    /// `HostCardView`-only action never offered on `DiscoveredCardView`.
    private func openLibraryForSelected() {
        guard libraryEnabled, case .saved(let id) = selection,
              let host = store.hosts.first(where: { $0.id == id })
        else { return }
        libraryTarget = host
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
            HStack(alignment: .top, spacing: 8) {
                monogramBadge
                Spacer(minLength: 0)
                // The status the removed detail panel used to spell out, now on the card itself: a
                // lock for a paired (pinned-identity) host + a green pip when it's live on the LAN.
                HStack(spacing: 7) {
                    if tile.isPaired {
                        Image(systemName: "lock.fill")
                            .font(.system(size: 11, weight: .semibold))
                            .foregroundStyle(.white.opacity(0.5))
                    }
                    if tile.isOnline {
                        Circle()
                            .fill(Color.green)
                            .frame(width: 9, height: 9)
                            .shadow(color: .green.opacity(0.7), radius: 5)
                    }
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
        // Liquid Glass console tile — a brand wash marks a saved host as primary; discovered /
        // Add-Host tiles stay neutral glass with a dashed edge. Glass clips to the shape itself.
        .consoleGlass(
            RoundedRectangle(cornerRadius: 26, style: .continuous),
            tint: tile.filled ? Color.brand.opacity(0.20) : nil)
        .overlay {
            RoundedRectangle(cornerRadius: 26, style: .continuous)
                .strokeBorder(
                    LinearGradient(
                        colors: [.white.opacity(0.22), .white.opacity(0.04)],
                        startPoint: .top, endPoint: .bottom),
                    style: StrokeStyle(lineWidth: 1, dash: tile.filled ? [] : [6, 5]))
        }
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
            } else if let icon = tile.icon {
                Image(systemName: icon)
                    .font(.system(size: 24, weight: .semibold))
                    .foregroundStyle(Color.brand)
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
