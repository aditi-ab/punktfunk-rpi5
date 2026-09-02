// The gamepad-driven home screen: a distinct, "10-foot" console-style host
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
// (macOS has no fullScreenCover). tvOS mounts it as well, driven by the native focus engine
// (see GamepadCarousel's tvOS mode) so the Siri Remote works alongside the pad; Play/Pause
// mirrors X for Settings since the focus engine has no concept of that button.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(macOS) || os(tvOS)
import GameController

/// One navigable tile: a saved host, a discovered-but-unsaved one, or one of the trailing
/// actions. Hashable so it can be the carousel's scroll-position identity.
private enum GamepadHomeTarget: Hashable {
    /// A saved host's own tile, or one of its pinned host+profile cards (§5.2a) — which on a
    /// controller-first surface are THE profile affordance: focus and press, no menus.
    case saved(UUID, profile: String?)
    case discovered(String)
    case addHost
    case rescan
}

/// A fully-resolved launcher tile — display fields + the activate action, built fresh each render
/// from the live stores so nothing goes stale.
private struct HomeTile: Identifiable {
    let id: GamepadHomeTarget
    let title: String
    let subtitle: String
    /// The profile this tile connects with — shown as a tinted chip. Set on a pinned card; nil on
    /// a plain host tile unless the host is bound to one (then it answers "what will A do?").
    var profile: StreamProfile?
    /// This tile IS a pinned host+profile card, not a host wearing its binding's chip — the chip
    /// reads loud on the former (it is the reason the tile exists) and quiet on the latter.
    var isPinnedCard = false
    var isOnline = false
    var isPaired = false
    var isConnecting = false
    /// Saved (solid monogram) vs. discovered-but-unsaved / action (tinted outline).
    var filled = false
    /// Only saved hosts have a library (matches the touch grid's context-menu gate).
    var hasLibrary = false
    /// Shows this SF symbol in the badge instead of the title monogram (the Add Host tile).
    var icon: String?
    /// The host's OS-identity chain. When we ship art for it, the badge wears the OS mark instead
    /// of the title's initial — the same substitution the touch cards make.
    var osChain: String?
    /// Offline saved host we hold a MAC for (and WoL is available) — activating it wakes first.
    var canWake = false
    let activate: () -> Void
}

struct GamepadHomeView: View {
    /// Resolved from the stored palette, NOT from `\.gamepadInk` — this screen publishes that
    /// value itself and so sits above its own copy (see `GamepadInk.stored`).
    @AppStorage(DefaultsKey.uiPalette) private var paletteID = "violet"
    private var ink: GamepadInk { .stored(paletteID) }
    /// Published by ContentView at the app ROOT, so this reads its own window's tier — this screen
    /// applies `gamepadPaletteInk` itself and so sits above its own copy of the environment.
    @Environment(\.gamepadMetrics) private var metrics
    /// The home-indicator strip's height, measured by DisplayBottomInsetProbe and published from
    /// ContentView — an environment READ is safe in body; asking UIKit for it here is not (see
    /// the probe's comment: a key-window walk mid-render severed this very view's updates).
    @Environment(\.displayBottomInset) private var displayBottomInset
    @ObservedObject var store: HostStore
    @ObservedObject var model: SessionModel
    @ObservedObject var discovery: HostDiscovery
    @Binding var libraryTarget: LibraryTarget?
    /// The host awaiting a PIN ceremony, if any. Owned by ContentView (a connect attempt sets it,
    /// as does the trust card's "Pair with PIN instead"), presented here as a shell screen —
    /// PairSheet's `Form` is unreachable with a controller on iOS/macOS, which made pairing the
    /// one thing a console-UI user simply could not do. See GamepadPairView.
    @Binding var pairingTarget: StoredHost?
    /// Pin the verified fingerprint and connect — ContentView's `handlePaired`.
    let onPaired: (StoredHost, Data) -> Void
    /// Wake-and-wait driver — gates the carousel while its overlay is up, and the carousel's
    /// activate routes an offline+wakeable host through it (see ContentView.startSession).
    @ObservedObject var waker: HostWaker
    let connect: (StoredHost, ProfileSelection) -> Void
    let connectDiscovered: (DiscoveredHost) -> Void
    /// Launch a library title on a host — the in-place library layer's activate path (iOS; the
    /// cover/sheet presentations wire ContentView's `launchTitle` into LibraryView themselves).
    let launchTitle: (LibraryTarget, String) -> Void
    /// Wake a host WITHOUT connecting (ContentView's `wakeOnly`) — the host menu's Wake row. The
    /// tile's own A already wakes-and-connects; this is the other half, for bringing a machine up
    /// to look at it rather than to stream from it right now.
    let wakeOnly: (StoredHost) -> Void
    /// A console prompt (GamepadPromptView) is up over the home — it polls the same controller, so
    /// this screen must stand down for as long as it is. Same handoff contract as the connect
    /// takeover and the shell's own layers; without it the carousel keeps scrolling underneath the
    /// modal and a single A press reaches both.
    var promptActive = false

    /// The profile catalog — pinned host+profile combos render as their own tiles here, which is
    /// how a controller picks a profile: one focus-and-press instead of a menu (design §5.4).
    @ObservedObject private var profiles = ProfileStore.shared
    /// What each paired host says this device may do TO it (`design/host-actions.md` §7) —
    /// shared with the touch grid, so the two menus cannot disagree about what a host offers.
    @ObservedObject private var hostPower = HostPowerStore.shared
    /// Same gate the touch grid's "Browse Library…" context-menu item uses (default ON; the
    /// Settings "Game library" toggle opts out).
    @AppStorage(DefaultsKey.libraryEnabled) private var libraryEnabled = true
    /// Auto-wake on connect (default ON) — when off, activating an offline host just dials (no wake),
    /// so the tile drops its "Wake & Connect" affordance for a plain "Connect".
    @AppStorage(DefaultsKey.autoWake) private var autoWakeEnabled = true
    #if os(iOS)
    /// `.compact` in a landscape phone window — drives tighter chrome so everything still fits.
    @Environment(\.verticalSizeClass) private var vSizeClass

    private var compact: Bool { vSizeClass == .compact }
    #else
    private let compact = false // no size classes on macOS; the window minimum keeps room
    #endif
    @ObservedObject private var gamepads = GamepadManager.shared
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var selection: GamepadHomeTarget?
    @State private var showSettings = false
    @State private var showAddHost = false
    /// The card whose options menu is up (UP on a saved tile) — see GamepadHostOptionsView.
    @State private var hostOptionsTarget: HostOptionsTarget?
    /// The host being edited. Set from the options menu, which closes itself as it opens this so
    /// the two are never stacked — depth stays ≤ 1, which is what `GamepadScreen` assumes.
    @State private var editTarget: StoredHost?
    /// The console's input drop: true for the transition's 0.26 s, during which NO layer polls
    /// the controller — a double-tapped A can't push two screens, and the held button that
    /// caused the change is long released before the next poller starts (whose own
    /// `needsSnapshot` seed swallows it if not).
    @State private var transitioning = false
    /// Guards the gate's release against an interrupted transition: only the newest hold clears.
    @State private var transitionEpoch = 0

    var body: some View {
        // The in-place shell (see GamepadShell.swift): the launcher is the base layer, the
        // current sub-screen a transparent layer over it, both over ONE persistent backdrop
        // that never unmounts — a push slides the screen up out of a fade while the launcher
        // recedes underneath, the console's own choreography. On macOS/tvOS `topScreen` is
        // constantly nil and this ZStack degenerates to the plain launcher, presented over by
        // the sheets/covers below exactly as before.
        ZStack {
            homeLayer
                .opacity(covered ? 0 : 1)
                .scaleEffect(covered ? GamepadShellMotion.underScale : 1)
                // The covers used to swallow touch; the recessed layer must too.
                .allowsHitTesting(!covered)
            #if os(iOS)
            if let screen = topScreen {
                screenLayer(screen)
                    // Settle the screen's internal layout before the insertion animates, so
                    // descendants never lerp from a half-resolved first frame. (Not sufficient
                    // for the tray blurs on its own — safe-area expansion resolves outside a
                    // geometry group; GamepadTrayScrim pins its own geometry too.)
                    .geometryGroup()
                    .zIndex(1)
                    .id(screen.id)
                    // Reduce Motion: a crossfade — no slide, no scale (the desktop's rule).
                    .transition(
                        reduceMotion
                            ? .opacity
                            : .gamepadScreen(slide: GamepadShellMotion.slide(compact: compact)))
            }
            // Back mid-push (the desktop's interruptible transition): while a push is in flight
            // no layer owns the controller, so this zero-size listener takes B alone and turns
            // the entering screen around. A and the rest stay dropped until the spring has
            // passed 0.85 of its travel (`transitioning`), which keeps a double-tapped A from
            // pushing two screens.
            if transitioning, topScreen != nil {
                MidPushBackCatcher { backOutOfPush() }
            }
            #endif
        }
        // Value-keyed rather than `withAnimation` at the triggers: pushes originate outside
        // this view too (`model.returnToLibrary` writes `libraryTarget`), and keying on the
        // derived id catches every writer. Reduce Motion crossfades on the reduced spring.
        .animation(
            reduceMotion ? GamepadShellMotion.reducedScreen : GamepadShellMotion.screen,
            value: topScreenID)
        // ONE living field for every layer, still a `.background` (the layout rule in this
        // file's header). Its calm is CHASED between the launcher's aurora and the form
        // screens' quiet, never crossfaded per screen — the console's `bg_mix`.
        .background {
            GamepadScreenBackground(calmMix: calmTarget)
                .animation(reduceMotion ? nil : GamepadShellMotion.calm, value: calmTarget)
        }
        // Publish the palette's ink to this screen (text, glass, accent, scrims) — a
        // pale palette flips all of them, and no leaf should have to read the setting.
        .gamepadPaletteInk()
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
        #if os(iOS)
        .onChange(of: topScreenID) { _, _ in
            transitionEpoch += 1
            let epoch = transitionEpoch
            transitioning = true
            // The gate opens when the spring has passed 0.85 of its travel, not when it has
            // settled — the wall is gone, the double-tap protection stays.
            let hold = reduceMotion ? 0.05 : GamepadShellMotion.inputOpensAfter
            DispatchQueue.main.asyncAfter(deadline: .now() + hold) {
                if epoch == transitionEpoch { transitioning = false }
            }
        }
        #endif
        // The remote's Play/Pause mirrors the pad's X (Settings): the focus engine never surfaces
        // X, and historically tvOS maps a pad's X to this same press — the poll and this command
        // double-firing just sets the same Bool twice.
        #if os(tvOS)
        .onPlayPauseCommand { if homeOwnsController { showSettings = true } }
        #endif
        // The settings / add-host screens take over the controller (the carousel's `isActive`
        // gate above). macOS has no fullScreenCover — they are generously sized sheets over the
        // dimmed launcher; tvOS keeps its focus-engine covers. iOS needs nothing here: the
        // shell's layers above ARE the presentation.
        #if os(macOS)
        .sheet(isPresented: $showSettings) {
            GamepadSettingsView(store: store, micAvailable: model.micAvailable)
                .frame(width: 720, height: 640)
        }
        .sheet(isPresented: $showAddHost) {
            GamepadAddHostView { store.add($0) }
                .frame(width: 660, height: 620)
        }
        // Shorter than the forms above: a menu is five rows, and a sheet sized for a settings
        // screen would be mostly empty field under them.
        .sheet(item: $hostOptionsTarget) { target in
            hostOptionsView(target, active: true)
                .frame(width: 620, height: 460)
        }
        .sheet(item: $editTarget) { host in
            editHostView(host, active: true)
                .frame(width: 660, height: 620)
        }
        .frame(minWidth: 640, minHeight: 420)
        #elseif os(tvOS)
        .fullScreenCover(isPresented: $showSettings) {
            GamepadSettingsView(store: store, micAvailable: model.micAvailable)
        }
        .fullScreenCover(isPresented: $showAddHost) {
            GamepadAddHostView { store.add($0) }
        }
        .fullScreenCover(item: $hostOptionsTarget) { target in
            hostOptionsView(target, active: true)
        }
        .fullScreenCover(item: $editTarget) { host in
            editHostView(host, active: true)
        }
        #endif
    }

    // MARK: - The shell's layers (see GamepadShell.swift)

    /// The launcher itself — everything the pre-shell body was, minus the backdrop (hoisted to
    /// the shell) and the presentation modifiers (below).
    private var homeLayer: some View {
        GeometryReader { geo in
            hero(for: geo.size)
        }
        // Pinned inside the safe area, out of the carousel's vertical budget — never clipped.
        .safeAreaInset(edge: .top, spacing: 0) {
            titleBar
                .padding(.top, gamepadTitleTopPadding(compact: compact))
                .padding(.bottom, gamepadTitleBottomPadding(compact: compact))
        }
        .safeAreaInset(edge: .bottom, alignment: .leading, spacing: 0) {
            legend
        }
    }

    /// The pinned controls legend, sitting the SAME distance from the leading and bottom edges of
    /// the DISPLAY — see `gamepadLegendBottomPadding` for why the bottom number is not simply the
    /// margin, and why measuring the inset (rather than trying to opt out of it) is what finally
    /// worked.
    private var legend: some View {
        GamepadHintBar(hints: hints)
            .padding(.leading, legendMargin)
            .padding(
                .bottom,
                gamepadLegendBottomPadding(
                    legendMargin, tier: metrics.tier, displayBottom: displayBottomInset))
            .padding(.top, compact ? 4 : 8)
    }

    /// The legend pill's distance from the screen's leading and bottom edges.
    private var legendMargin: CGFloat { compact ? 12 : 18 }

    #if os(iOS)
    /// The screen the shell shows over the launcher — derived from the same triggers every
    /// platform sets, so `returnToLibrary`, the tiles, X and Y all keep writing what they wrote.
    private var topScreen: GamepadScreen? {
        // Pairing leads: it is a ceremony blocking a connect the user already asked for, and it
        // can be raised from ON TOP of the library (launching a title on an unpaired host), where
        // it has to win. Backing out of it reveals whatever it interrupted.
        if let host = pairingTarget { return .pair(host) }
        // Editing leads the menu that raised it: the menu clears itself on the way, so the two are
        // never both set, and if they somehow were, the screen the user asked for last should win.
        if let host = editTarget { return .editHost(host) }
        if let target = hostOptionsTarget { return .hostOptions(target) }
        if showSettings { return .settings }
        if showAddHost { return .addHost }
        if let shelf = libraryTarget { return .library(shelf) }
        return nil
    }

    @ViewBuilder private func screenLayer(_ screen: GamepadScreen) -> some View {
        // The layer owns the controller only once the push settles and nothing rides over the
        // shell (the connect/wake takeover is an overlay in ContentView, above these layers).
        let active = !transitioning && waker.waking == nil && model.phase != .connecting
        Group {
            switch screen {
            case .settings:
                GamepadSettingsView(
                    store: store,
                    close: { if !transitioning { showSettings = false } },
                    controllerActive: active,
                    micAvailable: model.micAvailable)
            case .addHost:
                GamepadAddHostView(
                    onAdd: { store.add($0) },
                    close: { if !transitioning { showAddHost = false } },
                    controllerActive: active)
            case .hostOptions(let target):
                hostOptionsView(target, active: active)
            case .editHost(let host):
                editHostView(host, active: active)
            case .pair(let host):
                GamepadPairView(
                    host: host,
                    onPaired: { onPaired(host, $0) },
                    close: { if !transitioning { pairingTarget = nil } },
                    controllerActive: active)
            case .library(let shelf):
                GamepadLibraryScreen(
                    store: store, target: shelf,
                    onLaunch: { launchTitle(shelf, $0) },
                    close: { if !transitioning { libraryTarget = nil } },
                    controllerActive: active)
            }
        }
        .environment(\.gamepadHostedInShell, true)
    }
    #endif

    private var covered: Bool {
        #if os(iOS)
        topScreen != nil
        #else
        false
        #endif
    }

    #if os(iOS)
    /// Back pressed while a push is still in flight: clear the trigger that raised the top
    /// screen, so the same spring carries it back down — the desktop retargets its NAV spring
    /// to 0 mid-push; SwiftUI does the equivalent when the identity flips back before the
    /// insertion has settled.
    private func backOutOfPush() {
        guard transitioning, let screen = topScreen else { return }
        switch screen {
        case .pair: pairingTarget = nil
        case .editHost: editTarget = nil
        case .hostOptions: hostOptionsTarget = nil
        case .settings: showSettings = false
        case .addHost: showAddHost = false
        case .library: libraryTarget = nil
        }
    }
    #endif

    private var topScreenID: String? {
        #if os(iOS)
        topScreen?.id
        #else
        nil
        #endif
    }

    /// The backdrop's calm target: 1 under a form screen, 0 under the launcher/library. The
    /// macOS sheets / tvOS covers mount their own calmed field, so the launcher behind them
    /// keeps its aurora — exactly what shipped.
    private var calmTarget: Double {
        #if os(iOS)
        topScreen?.isForm == true ? 1 : 0
        #else
        0
        #endif
    }

    /// Stop consuming the controller while another screen (or the connect/wake takeover) is on
    /// top — otherwise the launcher navigates behind it (invisibly on iPhone, visibly on iPad),
    /// and a second A during a dial would launch a concurrent connect. `.connecting` covers the
    /// takeover's Connecting phase; `waker.waking` its Waking phase. On iOS the shell adds the
    /// transition's input drop, during which NOBODY polls.
    private var homeOwnsController: Bool {
        #if os(iOS)
        topScreen == nil && !transitioning && !promptActive
            && waker.waking == nil && model.phase != .connecting
        #else
        // `pairingTarget` too: macOS presents the pair screen as a sheet and tvOS as a cover, and
        // either way the launcher underneath must stop consuming the pad — the pair screen's own
        // list is polling the same controller.
        libraryTarget == nil && pairingTarget == nil && !showSettings && !showAddHost
            && !promptActive && waker.waking == nil && model.phase != .connecting
        #endif
    }

    // MARK: - Hero (carousel + detail), sized to fit the space between the pinned title and hints

    @ViewBuilder private func hero(for size: CGSize) -> some View {
        #if os(tvOS)
        // 10-foot scale: the phone-sized card reads like a postage stamp from the couch.
        let cardWidth = min(560, size.width * 0.34)
        let cardHeight = min(350, max(240, size.height - 260))
        #else
        let cardWidth = min(340, size.width * 0.84)
        // 48 ≈ the carousel's own vertical breathing (+40) plus a small margin; clamp so the strip
        // always fits the region the pinned title / hints safe-area insets leave. (The old detail
        // line below the strip is gone — it only re-printed what the centered card already shows.)
        let cardHeight = min(compact ? 176 : 224, max(118, size.height - 48))
        #endif
        VStack(spacing: compact ? 8 : 10) {
            Spacer(minLength: 0)
            carousel(cardWidth: cardWidth, cardHeight: cardHeight)
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: - Chrome

    private var titleBar: some View {
        // Leading title (a console heading, not a floating label — field ask), chip trailing.
        // The old hidden-mirror trick existed only to keep a CENTRED title clear of the chip;
        // a leading title needs none of it — the flexible frame keeps the two apart, and the
        // title shrinks a little before it would ever truncate.
        HStack(spacing: 12) {
            Text("Select a Host")
                .font(.geist(gamepadTitleSize(compact: compact), .bold, relativeTo: .title))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
                .minimumScaleFactor(0.75)
                .frame(maxWidth: .infinity, alignment: .leading)
            statusChip
        }
        .padding(.horizontal, 24)
    }

    /// Which pad is driving this UI (name + battery) — quiet, and only where there's room; a
    /// compact-height phone gives the pixels to the carousel instead.
    @ViewBuilder private var statusChip: some View {
        if !compact, let active = gamepads.active {
            ControllerStatusChip(controller: active)
        }
    }

    private var cardSpacing: CGFloat {
        #if os(tvOS)
        44
        #else
        30
        #endif
    }

    // MARK: - Carousel

    private func carousel(cardWidth: CGFloat, cardHeight: CGFloat) -> some View {
        GamepadCarousel(
            items: tiles,
            selection: $selection,
            itemWidth: cardWidth,
            spacing: cardSpacing,
            onActivate: { $0.activate() },
            onSecondary: { openLibraryForSelected() },
            onTertiary: { showSettings = true },
            onUp: { openOptionsForSelected() },
            isActive: homeOwnsController
        ) { tile, entrance in
            hostCard(tile, size: CGSize(width: cardWidth, height: cardHeight), entrance: entrance)
        }
        .frame(height: cardHeight + 40)
    }

    /// The host tile plus its focus treatment. Every continuous visual reads the scroll view's own
    /// per-frame `phase` (real distance-from-centered), so the look always matches what's on screen
    /// mid-scroll. `.shadow`/`.overlay` aren't part of `VisualEffect`, so the focus pop is scale +
    /// brightness/saturation + a depth blur on the recessed neighbors.
    private func hostCard(
        _ tile: HomeTile, size: CGSize, entrance: CardEntrance
    ) -> some View {
        GamepadHostTile(tile: tile, size: size)
            // Beneath the scroll transition, never around it — see CardEntrance.
            .modifier(entrance)
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
        let action: String? = switch selected?.id {
        case .addHost: "Add Host"
        case .rescan: "Rescan"
        default: nil
        }
        // Every cell's action re-resolves the selection when it FIRES rather than closing over the
        // one this render saw: the legend is rebuilt on selection changes, but a tap landing in
        // the same frame as a carousel move would otherwise activate the tile that was selected a
        // moment ago — the one failure mode a launcher cannot afford.
        var hints = [GamepadHint(
            glyph: buttonGlyph(\.buttonA, fallback: "a.circle"),
            text: action ?? (selected?.canWake == true ? "Wake & Connect" : "Connect"),
            action: { tiles.first { $0.id == selection }?.activate() })]
        if libraryEnabled, selected?.hasLibrary == true {
            hints.append(.init(
                glyph: buttonGlyph(\.buttonY, fallback: "y.circle"), text: "Library",
                action: { openLibraryForSelected() }))
        }
        // Only a saved card has a menu, so the cell appears only where the press does something —
        // the same honesty rule the Library cell above follows. A direction, not a button, so it
        // is a plain arrow rather than a `buttonGlyph` (see the settings screen's "Adjust").
        if case .saved = selected?.id {
            hints.append(.init(
                glyph: "arrow.up", text: "Options",
                action: { openOptionsForSelected() }))
        }
        hints.append(.init(
            glyph: buttonGlyph(\.buttonX, fallback: "x.circle"), text: "Settings",
            action: { showSettings = true }))
        return hints
    }

    // MARK: - Data + actions

    /// Built fresh each render from the live stores (no stale value capture) — saved hosts first,
    /// then discovered-but-unsaved ones, then the Add Host action tile (so the strip is never
    /// empty and manual entry is always one press away).
    private var tiles: [HomeTile] {
        var saved: [HomeTile] = []
        for host in store.hosts {
            // Online = advertising on mDNS OR proven reachable by the probe (a routed/VPN host
            // never advertises); the wake item is offered only when neither holds.
            let online = discovery.advertises(host) || store.probedOnline.contains(host.id)
            let bound = profiles.binding(for: host)
            let connecting = model.phase == .connecting && model.activeHost?.id == host.id
            // The host's own tile, then one per pinned profile — the same order the touch grid
            // uses, so a pin reads as belonging to its host on both surfaces.
            for profile in [StreamProfile?.none] + profiles.pinned(for: host).map(Optional.some) {
                saved.append(HomeTile(
                    id: .saved(host.id, profile: profile?.id),
                    title: host.displayName,
                    subtitle: "\(host.address):\(String(host.port))",
                    profile: profile ?? bound,
                    isPinnedCard: profile != nil,
                    isOnline: online,
                    isPaired: host.pinnedSHA256 != nil,
                    isConnecting: connecting,
                    filled: true,
                    // A pinned card reaches the library too, and gets its OWN shelf: browsing is
                    // this card's connect with a title picked first, not a host-level action like
                    // wake or forget. Gated on a pinned identity: the library plane's MgmtTransport
                    // accepts any cert for an unpinned host, so an unpaired host must not expose a
                    // library affordance a LAN MITM could answer. security-review 2026-08-15 #8.
                    hasLibrary: host.pinnedSHA256 != nil,
                    osChain: host.osChain,
                    canWake: autoWakeEnabled && PunktfunkConnection.wakeOnLANAvailable
                        && !online && !host.wakeMacs.isEmpty,
                    activate: {
                        connect(host, profile.map { .profile($0.id) } ?? .inherit)
                    }))
            }
        }
        let discovered = discovery.unsaved(among: store.hosts).map { d in
            HomeTile(
                id: .discovered(d.id),
                title: d.name,
                subtitle: "\(d.host):\(String(d.port))",
                isOnline: true,
                osChain: d.osChain,
                activate: { connectDiscovered(d) })
        }
        let add = HomeTile(
            id: .addHost,
            title: "Add Host",
            subtitle: "Register a host by address",
            icon: "plus",
            activate: { showAddHost = true })
        // A controller surface has no toolbar and no pull-to-refresh, so the rescan the field
        // asked for is a tile like any other — one press from wherever the stick already is.
        let rescan = HomeTile(
            id: .rescan,
            title: "Rescan",
            subtitle: discovery.isScanning ? "Scanning…" : "Look for hosts on this network",
            icon: "arrow.clockwise",
            activate: { discovery.refresh() })
        return saved + discovered + [add, rescan]
    }

    /// Only saved hosts have a library — matches the touch grid, where "Browse Library…" is a
    /// `HostCardView`-only action never offered on `DiscoveredCardView`. A pinned card opens its
    /// own shelf: the selection already names which card Y was pressed on, and that card's profile
    /// is what its launches run with.
    /// The host menu, built once for all three presentations (the iOS shell layer, the macOS
    /// sheet, the tvOS cover) so the actions can't drift between them.
    ///
    /// Edit REPLACES this menu rather than stacking on it — `hostOptionsTarget` is cleared as
    /// `editTarget` is set — which is the desktop console's `Nav::Replace` and what keeps the
    /// shell's "depth ≤ 1 by construction" claim true.
    @ViewBuilder
    private func hostOptionsView(_ target: HostOptionsTarget, active: Bool) -> some View {
        let host = target.host
        GamepadHostOptionsView(
            host: host,
            pinnedProfile: target.profile,
            isOnline: discovery.advertises(host) || store.probedOnline.contains(host.id),
            canWake: autoWakeEnabled && PunktfunkConnection.wakeOnLANAvailable
                && !host.wakeMacs.isEmpty,
            onEdit: {
                guard !transitioning else { return }
                hostOptionsTarget = nil
                editTarget = host
            },
            onWake: {
                #if os(tvOS)
                hostOptionsTarget = nil // the wake takeover sits UNDER a tvOS cover
                #endif
                wakeOnly(host)
            },
            onForgetPairing: { store.forgetIdentity(host) },
            onRemove: { store.remove(host) },
            onUnpin: {
                guard let profile = target.profile else { return }
                store.setPinned(host.id, profileID: profile.id, pinned: false)
            },
            onSendLogs: host.pinnedSHA256 != nil ? { await SendLogs.toHost(host) } : nil,
            // A pinned card is a shortcut to one profile, not a second host, so it carries no
            // host actions — the same rule the touch grid and the console menu apply.
            hostActions: target.profile == nil ? hostPower.actions(for: host) : [],
            onHostAction: { action in await hostPower.invoke(action, on: host) },
            close: { if !transitioning { hostOptionsTarget = nil } },
            controllerActive: active)
    }

    /// The add-host form in edit mode. `store.update` writes the record back by id, so the
    /// fingerprint, MACs, pins and binding the form never shows are preserved.
    @ViewBuilder
    private func editHostView(_ host: StoredHost, active: Bool) -> some View {
        GamepadAddHostView(
            onAdd: { store.update($0) },
            close: { if !transitioning { editTarget = nil } },
            controllerActive: active,
            editingHost: host)
    }

    /// UP on a saved tile opens that card's menu. Only SAVED hosts have one: a discovered-but-
    /// unsaved host is not ours to rename or remove, and the two action tiles have nothing to
    /// offer — the same `HostOptionsScreen::available` gate the desktop console applies.
    private func openOptionsForSelected() {
        guard case .saved(let id, let profileID) = selection,
              let host = store.hosts.first(where: { $0.id == id })
        else { return }
        hostOptionsTarget = HostOptionsTarget(
            host: host,
            profile: profileID.flatMap { pid in profiles.profiles.first { $0.id == pid } })
    }

    private func openLibraryForSelected() {
        guard libraryEnabled, case .saved(let id, let profileID) = selection,
              let host = store.hosts.first(where: { $0.id == id })
        else { return }
        libraryTarget = LibraryTarget(host: host, profile: ProfileSelection(profileID: profileID))
    }
}

/// One "console tile" in the host carousel — a dark-glass landscape card, bigger and bolder than the
/// touch grid's `HostCardView`. Renders only its base look; the centered-tile pop is layered on by
/// the caller's `.scrollTransition` so it always tracks the real scroll position.
private struct GamepadHostTile: View {
    @Environment(\.gamepadInk) private var ink
    let tile: HomeTile
    let size: CGSize

    // 10-foot metrics on tvOS, in-hand metrics elsewhere — one tile, two viewing distances.
    #if os(tvOS)
    private static let titleFont: CGFloat = 33
    private static let subtitleFont: CGFloat = 19
    private static let statusFont: CGFloat = 15
    private static let pipSide: CGFloat = 12
    private static let badgeSide: CGFloat = 70
    private static let badgeCorner: CGFloat = 19
    private static let monogramFont: CGFloat = 34
    private static let iconFont: CGFloat = 32
    private static let pad: CGFloat = 28
    private static let corner: CGFloat = 30
    #else
    private static let titleFont: CGFloat = 23
    private static let subtitleFont: CGFloat = 13
    private static let statusFont: CGFloat = 11
    private static let pipSide: CGFloat = 9
    private static let badgeSide: CGFloat = 52
    private static let badgeCorner: CGFloat = 15
    private static let monogramFont: CGFloat = 25
    private static let iconFont: CGFloat = 24
    private static let pad: CGFloat = 20
    private static let corner: CGFloat = 26
    #endif

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
                            .font(.system(size: Self.statusFont, weight: .semibold))
                            .foregroundStyle(ink.fg(0.5))
                    }
                    if tile.isOnline {
                        // Status colours stay palette-independent (a pip must not change meaning
                        // with the wallpaper) — only the glow softens on a pale field, where it
                        // reads as a smudge at full strength.
                        Circle()
                            .fill(GamepadInk.onlineGreen)
                            .frame(width: Self.pipSide, height: Self.pipSide)
                            .shadow(
                                color: GamepadInk.onlineGreen.opacity(ink.isLight ? 0.45 : 0.7),
                                radius: 5)
                    }
                }
            }
            Spacer(minLength: 0)
            Text(tile.title)
                .font(.geist(Self.titleFont, .bold, relativeTo: .title2))
                .foregroundStyle(ink.fg)
                .lineLimit(1)
                .minimumScaleFactor(0.7)
            if let profile = tile.profile {
                ProfileChip(
                    profile: profile, size: Self.statusFont, prominent: tile.isPinnedCard)
                    .padding(.top, 4)
            }
            Text(tile.subtitle)
                .font(.geist(Self.subtitleFont, relativeTo: .caption))
                .foregroundStyle(ink.fg(0.55))
                .lineLimit(1)
                .padding(.top, 2)
        }
        .padding(Self.pad)
        .frame(width: size.width, height: size.height, alignment: .leading)
        // Console tile — a brand wash marks a saved host as primary; discovered / Add-Host tiles
        // stay neutral with a dashed edge. The surface clips to the shape itself.
        //
        // `forceMaterial`: these tiles are the one console surface that gets TRANSFORMED while it
        // animates — `CardEntrance` swings each card in on a `rotation3DEffect` under an opacity
        // ramp, and the carousel's `.scrollTransition` keeps scaling and rotating the neighbours
        // forever after. Liquid Glass samples the backdrop through its own layer and cannot do
        // that under a 3D transform, so it drew one way through the swing and snapped to another
        // as the card landed — on glass it read as the tiles being swapped out for different ones
        // at the end of their entrance. A material composites flat, so the card looks the same at
        // every frame of the travel. (tvOS already takes this path for its own reasons.)
        .consoleGlass(
            RoundedRectangle(cornerRadius: Self.corner, style: .continuous),
            tint: tile.filled ? ink.accent(0.20) : nil,
            forceMaterial: true)
        .overlay {
            RoundedRectangle(cornerRadius: Self.corner, style: .continuous)
                .strokeBorder(
                    LinearGradient(
                        colors: [ink.fg(0.22), ink.fg(0.04)],
                        startPoint: .top, endPoint: .bottom),
                    style: StrokeStyle(lineWidth: 1, dash: tile.filled ? [] : [6, 5]))
        }
        .shadow(color: ink.shadow(0.45), radius: 20, y: 14)
    }

    private var monogramBadge: some View {
        let shape = RoundedRectangle(cornerRadius: Self.badgeCorner, style: .continuous)
        // What the glyph is drawn ON: a filled badge IS the accent, so its mark takes `onAccent`
        // — the colour picked by the accent's own luminance — exactly as the settings screen's
        // selected tab pill does. It used to take `fg`, which is chosen against the FIELD, and the
        // two disagree at both ends of the set: a pale palette put near-black on a deep accent, and
        // Graphite (accent luma ≈ 0.80) put white on a light grey.
        let glyph = tile.filled ? ink.onAccent : ink.accent
        return ZStack {
            shape.fill(tile.filled
                ? AnyShapeStyle(LinearGradient(
                    colors: [ink.accent, ink.accent(0.68)],
                    startPoint: .top, endPoint: .bottom))
                : AnyShapeStyle(ink.accent(0.16)))
            if tile.isConnecting {
                ProgressView().tint(glyph)
            } else if let icon = tile.icon {
                Image(systemName: icon)
                    .font(.system(size: Self.iconFont, weight: .semibold))
                    .foregroundStyle(ink.accent)
            } else if let mark = osIconImage(for: tile.osChain) {
                // The OS mark stands in for the initial (template asset — tints like the text it
                // replaces), and carries the label, since nothing else on the tile names the OS.
                mark
                    .resizable()
                    .scaledToFit()
                    .frame(width: Self.monogramFont, height: Self.monogramFont)
                    .foregroundStyle(glyph)
                    .accessibilityLabel(tile.osChain ?? "")
            } else {
                Text(monogram(tile.title))
                    .font(.geistFixed(Self.monogramFont, .bold))
                    .foregroundStyle(glyph)
            }
        }
        .frame(width: Self.badgeSide, height: Self.badgeSide)
        .overlay {
            if !tile.filled {
                shape.strokeBorder(ink.accent(0.5), lineWidth: 1)
            }
        }
    }

    private func monogram(_ name: String) -> String {
        guard let first = name.trimmingCharacters(in: .whitespacesAndNewlines).first else { return "•" }
        return String(first).uppercased()
    }
}
#endif

#if os(iOS)
/// Zero-size controller listener for a push in flight — B alone. The same shape as LibraryView's
/// `LibraryBackCatcher`; `GamepadMenuInput.needsSnapshot` swallows the still-held A that pushed
/// the screen, so only a FRESH B mid-push counts. Unmounts the moment the gate opens.
private struct MidPushBackCatcher: View {
    let onBack: () -> Void
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .onAppear {
                input.onBack = onBack
                input.start()
            }
            .onDisappear { input.stop() }
    }
}
#endif
