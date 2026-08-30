// Hosts grid ⇄ trust prompt ⇄ live stream. ContentView is the coordinator: it owns the session
// model, host store, and LAN discovery; switches between the home grid (HomeView) and the live
// session; and holds the connect logic (it reads the @AppStorage stream mode). The grid + cards
// (HomeView/HostCards), the trust prompt (TrustCardView), and the HUD (StreamHUDView) live in
// their own files.
//
// Ways to establish trust on first contact: the TOFU prompt (host fingerprint over the
// live-but-blurred stream, compared with the host's log; only for a host advertising pair=optional),
// the PIN pairing ceremony (verifies both sides at once), or — for a host that requires pairing —
// delegated approval ("Request Access": a plain identified connect the host parks until the operator
// approves this device in its console, no PIN). Once pinned, reconnects are silent and a changed
// host identity refuses to connect.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

struct ContentView: View {
    @StateObject private var model = SessionModel()
    @StateObject private var store = HostStore()
    /// The settings-profile catalog (design/client-settings-profiles.md §4.2) — read at every
    /// connect to resolve the session's `EffectiveSettings`, and edited by the settings surface.
    @ObservedObject private var profiles = ProfileStore.shared
    @StateObject private var discovery = HostDiscovery()
    // The dev auto-connect hook (DEBUG-only — see `autoConnectIfAsked`) writes these three, so
    // they stay observed here; every OTHER stream setting reaches a session through
    // `EffectiveSettings`, resolved once per connect.
    @AppStorage(DefaultsKey.streamWidth) private var width = 1920
    @AppStorage(DefaultsKey.streamHeight) private var height = 1080
    @AppStorage(DefaultsKey.streamHz) private var hz = 60
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) private var fullscreenWhileStreaming = true
    // The raw string is what @AppStorage observes (so cycles from any surface re-render this
    // view); the absent-key default runs the legacy-hudEnabled migration once per init.
    @AppStorage(DefaultsKey.statsVerbosity) private var statsVerbosityRaw
        = StatsVerbosity.current.rawValue
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    /// The tier the overlay actually shows: the live session's (its profile's, then whatever the
    /// ⌃⌥⇧S/three-finger cycle moved it to) while streaming, the persisted global otherwise.
    private var statsVerbosity: StatsVerbosity {
        model.connection != nil
            ? model.statsVerbosity
            : (StatsVerbosity(rawValue: statsVerbosityRaw) ?? .normal)
    }
    /// Fullscreen-while-streaming is profileable (a Game profile goes fullscreen, a Work one
    /// doesn't), so a live session obeys ITS value and the host list obeys the global.
    private var fullscreenForSession: Bool {
        model.connection != nil ? model.settings.fullscreenWhileStreaming : fullscreenWhileStreaming
    }
    @State private var showAddHost = false
    /// A `punktfunk://` deep link (widget / Siri / Shortcuts) couldn't be honored — unknown host, or
    /// a live session is already up. Surfaced as an informational alert (distinct from the
    /// "Connection failed" one, which is for actual connect errors).
    @State private var deepLinkNotice: String?
    /// A `punktfunk://` deep link that named a saved host by something GUESSABLE — its display
    /// name, its address, or the `host=` recovery parameter — instead of by its stable record id.
    /// Anything that can open a URL can guess "Gaming PC", so the link's action waits for this
    /// confirmation; a link that names the id (every shortcut this app emits) still runs on its own.
    private struct DeepLinkConfirm {
        let host: StoredHost
        let launch: String?
        let profile: ProfileSelection
        /// A `browse` link: open the host's library instead of dialing it.
        let browse: Bool

        var actionTitle: String { browse ? "Open Library" : "Connect" }
        var message: String {
            let asked = browse
                ? "open \(host.displayName)'s game library"
                : "connect to \(host.displayName)"
                    + (launch.map { " and launch \u{201C}\($0)\u{201D}" } ?? "")
            return "A link asked to \(asked). It names the host by its label or address, which "
                + "anything that can open a link could guess — a shortcut made in Punktfunk names "
                + "the host's id and opens without asking."
        }
    }
    @State private var deepLinkConfirm: DeepLinkConfirm?
    #if os(iOS)
    /// Owns the Live Activity for the running session (Lock Screen / Dynamic Island). Driven from
    /// the session model's published state below; iPhone/iPad only.
    @State private var liveActivity = SessionActivityController()
    /// The window's bottom safe-area inset (the home-indicator strip), reported by
    /// DisplayBottomInsetProbe from UIKit's own callbacks and published as
    /// `\.displayBottomInset` for the screens that pin a legend to the display's corner. Held
    /// HERE and read through the environment because asking UIKit for it during a body severs
    /// the asking view's updates on device (see the probe).
    @State private var displayBottomInset: CGFloat = 0
    #endif
    @State private var pairingTarget: StoredHost?
    /// A fresh `pair=required`/unknown host the user tapped: drives the choice between no-PIN
    /// delegated approval ("Request Access") and the SPAKE2 PIN ceremony (rule 3b).
    @State private var approvalChoice: ApprovalRequest?
    /// A delegated-approval connect is in flight (host parks it until the operator approves):
    /// drives the cancelable "Waiting for approval" prompt and the pin-as-paired on success.
    @State private var awaitingApproval: ApprovalRequest?
    @State private var speedTestTarget: StoredHost?
    @State private var libraryTarget: LibraryTarget?
    /// Wakes a sleeping host and waits for it to come back online before connecting (drives the
    /// "Waking…" phase of the connect overlay). Available on every platform now that the iOS/tvOS
    /// multicast entitlement is granted (see PunktfunkConnection.wakeOnLANAvailable).
    @StateObject private var waker = HostWaker()
    #if os(macOS)
    /// Whether the hosting window is native-fullscreen right now (reported by
    /// FullscreenController). Drives the session view's safe-area choice: fullscreen goes
    /// edge-to-edge (behind the notch); windowed respects the top inset so the title bar
    /// never covers the video.
    @State private var isFullscreen = false
    #endif
    #if os(iOS)
    /// The stats-OFF tier's touch-exit disc window (see the overlay in `stream(captureEnabled:)`
    /// — the disc must LEAVE the hierarchy so nothing composites over the metal layer).
    @State private var showTouchExit = false
    #endif
    #if !os(macOS)
    @State private var showSettings = false
    #endif
    // A connected controller (+ the Settings toggle) swaps the whole home screen for
    // GamepadHomeView instead of retrofitting HomeView's touch/desktop UI — see `home` below.
    // On tvOS the same screens are focus-engine-driven, so the Siri Remote keeps working;
    // with no (extended) controller attached tvOS falls back to HomeView as before.
    @ObservedObject private var gamepadManager = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    /// When the switch above takes over — "connected" (default) or "always". See
    /// `GamepadUIEnvironment`.
    @AppStorage(DefaultsKey.gamepadUIMode) private var gamepadUIMode =
        GamepadUIEnvironment.modeWhenConnected
    /// Auto-wake on connect (Settings → General). On (default): a dial to an offline saved host
    /// fires Wake-on-LAN up front and falls into the "Waking…" wait if the dial fails. Off: connects
    /// go straight through with no wake. The explicit "Wake Host" action is unaffected either way.
    @AppStorage(DefaultsKey.autoWake) private var autoWakeEnabled = true
    /// Background keep-alive (Settings → General, iOS-only). Default OFF (today's freeze-on-background
    /// is the default). When on, backgrounding a live session keeps audio + the connection alive and
    /// drops video, auto-disconnecting after `backgroundTimeoutMinutes`.
    @AppStorage(DefaultsKey.backgroundKeepAlive) private var backgroundKeepAlive = false
    @AppStorage(DefaultsKey.backgroundTimeoutMinutes) private var backgroundTimeoutMinutes = 10
    /// scenePhase drives the keep-alive: use THIS, not the willResignActive observers — resign-active
    /// also fires for Control Center / app-switcher peeks, where the disconnect timer must not start.
    @Environment(\.scenePhase) private var scenePhase
    #if os(iOS)
    @Environment(\.horizontalSizeClass) private var hSizeClass
    @Environment(\.verticalSizeClass) private var vSizeClass
    #endif

    /// The gamepad UI's form-metric tier for this window, published from HERE — the app's root.
    /// A screen that applies `gamepadPaletteInk` itself sits ABOVE its own copy of the environment,
    /// so its `@Environment` resolves against its parent; publishing at the root is what makes
    /// every one of them (including the ones presented as sheets and covers, which inherit the
    /// environment) read its own window's tier instead of the bare default.
    private var gamepadMetrics: GamepadFormMetrics {
        #if os(iOS)
        .forWindow(h: hSizeClass, v: vSizeClass)
        #else
        .platformDefault
        #endif
    }
    private var gamepadUIActive: Bool {
        GamepadUIEnvironment.isActive(
            gamepadConnected: gamepadManager.active != nil, enabledSetting: gamepadUIEnabled,
            mode: gamepadUIMode)
    }

    // The body is split in two — `driven` (the screen plus its lifecycle drivers and sheets) and
    // the prompt chain below. Not a style choice: as ONE expression this blew Swift's
    // type-checker budget on the iOS slice ("unable to type-check in reasonable time"), which
    // macOS builds never reveal. Keep new modifiers on whichever half is shorter.
    var body: some View {
        driven
            // Fresh pair=required / unknown host: offer the two ways in. An action sheet (not an
            // alert) so it never collides with the wait alert below. "Request Access" is the
            // no-PIN delegated-approval path; "Pair with PIN…" runs the SPAKE2 ceremony. The
            // follow-on presentation is deferred a tick so this dialog is fully dismissed first.
            .confirmationDialog(
                "Pairing required",
                isPresented: approvalChoicePresented,
                titleVisibility: .visible,
                presenting: approvalChoice
            ) { req in
                Button("Request Access") {
                    DispatchQueue.main.async { requestAccess(req) }
                }
                Button("Pair with PIN…") {
                    DispatchQueue.main.async { pairingTarget = req.host }
                }
                Button("Cancel", role: .cancel) {}
            } message: { req in
                Text("\(req.host.displayName) requires pairing. Request access and approve this "
                    + "device in the host's web console (port 47992 → Pairing) — no PIN needed. Or "
                    + "pair with the 4-digit PIN it can display.")
            }
            // One "Connection failed" surface for every home screen (touch grid, gamepad launcher)
            // and platform — SessionModel funnels all connect/session errors into `errorMessage`.
            .alert("Connection failed", isPresented: connectionErrorPresented) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(model.errorMessage ?? "")
            }
            // The delegated-approval wait: the host holds the connection open until the operator
            // approves it. Cancel returns the UI at once; the in-flight connect is left to time out
            // and its late result is discarded by SessionModel's connect guard (disconnect resets
            // the phase/host it checks).
            .alert(
                "Waiting for approval",
                isPresented: awaitingApprovalPresented,
                presenting: awaitingApproval
            ) { _ in
                Button("Cancel", role: .cancel) { model.disconnect() }
            } message: { req in
                Text("Approve \u{201C}\(localDeviceName)\u{201D} in \(req.host.displayName)'s web "
                    + "console (port 47992 → Pairing). This device connects automatically once you "
                    + "approve it — no need to reconnect.")
            }
            // Informational deep-link outcome (unknown host, a refused profile, already
            // streaming). Not an error.
            .alert("Can't open", isPresented: deepLinkNoticePresented) {
                Button("OK", role: .cancel) {}
            } message: {
                Text(deepLinkNotice ?? "")
            }
            // A link that named a saved host by a guessable reference: the dial (or the library)
            // happens on the user's word rather than on the link's.
            .alert(
                "Open this link?",
                isPresented: deepLinkConfirmPresented,
                presenting: deepLinkConfirm
            ) { confirm in
                Button(confirm.actionTitle) { runDeepLinkConfirm(confirm) }
                Button("Cancel", role: .cancel) {}
            } message: { confirm in
                Text(confirm.message)
            }
    }

    /// The confirmed link's action: exactly what a `.known` (id-referenced) link would have done,
    /// one tap later.
    private func runDeepLinkConfirm(_ confirm: DeepLinkConfirm) {
        deepLinkConfirm = nil
        if confirm.browse {
            libraryTarget = LibraryTarget(host: confirm.host, profile: confirm.profile)
        } else {
            connect(confirm.host, launchID: confirm.launch, profile: confirm.profile)
        }
    }

    private var driven: some View {
        drivenBase
            .environment(\.gamepadMetrics, gamepadMetrics)
            #if os(iOS)
            .environment(\.displayBottomInset, displayBottomInset)
            // The probe is UIKit's, not any screen's: mounted once here as a background so the
            // legend-pinning screens can READ the inset from the environment without ever asking
            // UIKit during their own body (which severs their updates — see the probe).
            .background {
                DisplayBottomInsetProbe { displayBottomInset = $0 }
            }
            #endif
            #if os(iOS) || os(macOS)
            // The console's own modal, over WHICHEVER screen is up. Not attached to `home`, which
            // renders only while `model.connection == nil`: a connection exists through the
            // pair-required and approval handshakes, which is precisely when these prompts fire.
            // It sits above the connect takeover too — the delegated-approval wait is raised
            // DURING a dial and owns the only Cancel for it. (The takeover draws nothing in that
            // state: `connectingOverlayName` is nil while `awaitingApproval` is set, so the two
            // never poll the pad at once.)
            .overlay {
                if let prompt = consolePrompt {
                    GamepadPromptView(prompt: prompt)
                        .gamepadPaletteInk()
                        .transition(.opacity)
                }
            }
            #endif
    }

    private var drivenBase: some View {
        Group {
            // The stream view's structural identity MUST be stable across the
            // awaiting-trust → streaming transition: recreating it restarts the pump,
            // which has then already missed the opening IDR (infinite GOP — no other
            // keyframe ever comes) and decodes nothing. So: one branch per connection,
            // trust prompt as an overlay.
            if model.connection != nil {
                sessionView
            } else {
                home
            }
        }
        .onAppear {
            seedDefaultModeIfNeeded()
            autoConnectIfAsked()
            #if os(iOS)
            SessionActivityController.sweepOrphans() // end any Activity a prior killed launch left
            #endif
        }
        // Deep links (widget quick-launch, Siri/Shortcuts): route into the SAME connect path a card
        // tap uses, so trust policy / WoL / the approval sheet all come along. Never starts a
        // parallel session — this drives the one `model` ContentView owns.
        .onOpenURL { handleDeepLink($0) }
        // A live stats-overlay cycle from ANY surface (⌃⌥⇧S, the three-finger tap, the Stream
        // menu) writes the global; push it into the session so the overlay follows it from there
        // on, whatever tier the session's profile started on.
        .onChange(of: statsVerbosityRaw) { _, raw in
            model.setStatsVerbosity(StatsVerbosity(rawValue: raw) ?? .normal)
        }
        #if os(iOS) || os(tvOS)
        // Coming back to the app re-arms the LAN browse. The home's `onAppear`/`onDisappear` do
        // NOT fire across background/foreground, and a browse the system suspended while we were
        // away does not resume on its own — so the host grid came back empty and stayed empty
        // until the app was relaunched. No-op unless the browse is already running (mid-session
        // the home has deliberately torn it down).
        //
        // Mobile only: macOS never suspends the process, and its `scenePhase` flips on every
        // window focus change — re-arming there would rebuild the browser each time you alt-tab.
        // A Mac browse that genuinely breaks is caught by `HostDiscovery`'s own sweep instead.
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { discovery.refreshIfRunning() }
        }
        #endif
        #if os(iOS) || os(tvOS)
        // Backgrounding driver. Only .background/.active matter; .inactive (a transient peek) is
        // ignored so neither branch fires for a Control-Center pull.
        //
        // Backgrounding MUST end the session one way or the other: the app keeps running while
        // streaming (the `audio` background mode plus a live audio session), so its QUIC connection
        // keeps answering the host's keep-alives with the user long gone — the host has no way to
        // tell that apart from someone watching, and the session survived indefinitely. Either hold
        // it under the opt-in keep-alive (bounded by that path's own auto-disconnect timer) or end
        // it here.
        .onChange(of: scenePhase) { _, phase in
            switch phase {
            case .background:
                guard model.phase == .streaming else { break }
                if backgroundKeepAlive {
                    model.enterBackground(timeoutMinutes: backgroundTimeoutMinutes)
                } else {
                    // Not deliberate: the user may come straight back, so let the host linger the
                    // display for a fast reconnect instead of tearing it down.
                    model.disconnect(deliberate: false)
                }
            case .active:
                model.exitBackground()
            default:
                break
            }
        }
        #endif
        #if os(iOS)
        // Live Activity lifecycle, driven from the model's published state. iPhone/iPad only —
        // ActivityKit (and so `liveActivity`) does not exist on tvOS, which is why this stays in its
        // own os(iOS) block rather than riding the backgrounding driver's.
        .onChange(of: model.phase) { _, phase in
            switch phase {
            case .streaming:
                if let host = model.activeHost {
                    liveActivity.begin(
                        hostID: host.id, hostName: host.displayName,
                        launchTitle: nil, // no live foreground-app title mid-session (v1)
                        modeLine: currentModeLine(), startedAt: Date())
                }
            case .idle:
                liveActivity.end()
            default:
                break
            }
        }
        .onChange(of: model.isBackgrounded) { _, backgrounded in
            liveActivity.update {
                $0.stage = backgrounded ? .background : .streaming
                $0.backgroundDeadline = model.backgroundDeadline
            }
        }
        // The Live Activity's / Shortcuts' End button runs EndStreamIntent in-process, which posts
        // this — tear the session down deliberately (quit-close the host). iOS-only along with
        // the intent itself (LiveActivityIntent is ActivityKit's world).
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkEndActiveSession)) { _ in
            model.disconnect(deliberate: true)
        }
        #endif
        // Connect App Intent (Siri/Shortcuts/Spotlight): route its punktfunk:// URL through the
        // same handler a widget tap uses. NOT iOS-gated — the Connect intent compiles on macOS and
        // tvOS too, and an intent that posts to nobody would be a shortcut that silently does
        // nothing.
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkOpenDeepLink)) { note in
            if let url = note.object as? URL { handleDeepLink(url) }
        }
        .onChange(of: model.phase) { _, phase in
            switch phase {
            case .streaming:
                #if os(iOS)
                showTouchExit = true // the off-tier exit disc's 8 s window, per session start
                #endif
                // A session actually started — remember it on the card ("Connected … ago"
                // plus the accent ring on the most recent host).
                guard let host = model.activeHost else { break }
                // Delegated approval just succeeded: the operator let this device in, so pin the
                // host's observed fingerprint and remember it as paired — future connects are then
                // silent (rule 1), exactly like after a PIN/TOFU success. Dismisses the wait prompt.
                let approvedFingerprint = awaitingApproval?.host.id == host.id
                    ? model.connection?.hostFingerprint : nil
                if awaitingApproval?.host.id == host.id { awaitingApproval = nil }
                // Persist on the next runloop tick: HostStore is an ObservableObject, and mutating
                // its @Published from inside .onChange (a view-update callback) trips SwiftUI's
                // "Publishing changes from within view updates". A one-tick delay is imperceptible.
                // The session's own Welcome told us where this host's library lives — the one
                // source that does not need an mDNS advert, so it also covers a host reached by
                // address over a VPN. 0 = not advertised; updateMgmtPort ignores it.
                let liveMgmtPort = model.connection?.hostMgmtPort
                let store = store
                DispatchQueue.main.async {
                    store.markConnected(host.id)
                    store.updateMgmtPort(host.id, port: liveMgmtPort)
                    if let approvedFingerprint { store.pin(host.id, fingerprint: approvedFingerprint) }
                }
            case .idle:
                // The delegated-approval connect failed, timed out, or was cancelled — drop the
                // wait prompt (SessionModel surfaces any error via `errorMessage`).
                if awaitingApproval != nil { awaitingApproval = nil }
            default:
                break
            }
        }
        .onDisappear { model.disconnect() } // window closed mid-session (Cmd+N spawns more)
        // Expose the session to the Scene-level Stream menu (Disconnect ⌃⌥⇧D works even when
        // the HUD is hidden). tvOS has no such menu.
        #if !os(tvOS)
        .focusedSceneValue(\.sessionFocus, SessionFocus(
            isStreaming: model.connection != nil,
            // Host cap AND this device's CLIPBOARD grant (per-client access §7) — an
            // ungranted session's menu item greys out instead of inviting a refused enable.
            clipboardAvailable: model.connection.map {
                $0.hostSupportsClipboard && $0.canUseClipboard
            } == true,
            clipboardOn: model.clipboardEnabled,
            toggleClipboard: { model.toggleClipboardSync() },
            micAvailable: model.micAvailable,
            micMuted: model.micMuted,
            toggleMicMute: { model.toggleMicMute() },
            disconnect: { model.disconnect() }))
        // ⌃⌥⇧A fired while input was CAPTURED (InputCapture's chord path posts it — the menu's
        // identical equivalent can't reach a captured stream). Same toggle either way.
        .onReceive(NotificationCenter.default.publisher(for: .punktfunkToggleMicMute)) { _ in
            model.toggleMicMute()
        }
        #endif
        #if os(macOS)
        // Fullscreen only while a session is up (incl. the trust prompt over the blurred stream),
        // windowed on the host list — so the picker isn't forced fullscreen. Opt-out in Settings.
        // The controller also reports the window's ACTUAL fullscreen state back into
        // `isFullscreen` (the user can toggle it manually), which drives the session view's
        // safe-area handling below.
        .background(FullscreenController(
            active: fullscreenForSession && model.connection != nil,
            isFullscreen: $isFullscreen))
        #endif
        // A game launched from the library just exited, so the session ended on purpose: put the
        // player back in that host's library rather than on host selection. Set on the outer Group
        // (like the sheets below) so it survives the streaming → home transition the disconnect
        // drives, and consumed here — the model hands the host over once and we clear it, so a
        // later manual dismiss of the library can't be undone by a stale value.
        .onChange(of: model.returnToLibrary) { _, shelf in
            guard let shelf else { return }
            model.returnToLibrary = nil
            libraryTarget = shelf
        }
        // On the outer Group so the sheet survives the trust-prompt → home transition
        // (the "Pair with PIN instead" path disconnects first — the host's accept loop
        // is sequential, a pairing connection would queue behind the live session).
        #if !os(tvOS)
        // macOS presents BOTH pairing UIs from here, picking by mode (the console UI's screen is
        // gamepad-navigable; PairSheet's Form is not). iOS hides this sheet in gamepad mode
        // instead — there the pair screen is one of the shell's in-place layers, exactly like
        // settings and add-host (see `touchPairingTarget`).
        .sheet(item: touchPairingTarget) { host in
            #if os(macOS)
            if gamepadUIActive {
                GamepadPairView(host: host, onPaired: { handlePaired(host, fingerprint: $0) })
                    .frame(width: 660, height: 620)
            } else {
                PairSheet(host: host) { fingerprint in handlePaired(host, fingerprint: fingerprint) }
            }
            #else
            PairSheet(host: host) { fingerprint in handlePaired(host, fingerprint: fingerprint) }
            #endif
        }
        .sheet(item: $speedTestTarget) { host in
            SpeedTestSheet(host: host)
        }
        // The library is a full-screen presentation, not a sheet: on iPad a sheet is a centered page
        // card, but the gamepad coverflow is meant to be an immersive, full-bleed screen (and the
        // launcher behind it stops consuming the controller — see GamepadHomeView's `isActive`).
        // macOS has no `fullScreenCover`, so it keeps the sheet there — with an explicit size: a
        // macOS sheet takes its content's IDEAL size, and both library layouts are geometry-driven
        // (the coverflow is a GeometryReader, ideal ≈ zero), so without a frame it collapses to a
        // tiny panel.
        #if os(macOS)
        .sheet(item: $libraryTarget) { shelf in
            NavigationStack {
                LibraryView(store: store, target: shelf, onLaunch: { launchTitle(shelf, $0) })
            }
            .frame(minWidth: 940, minHeight: 620)
            // The stack draws the title, and it sits outside LibraryView's own ink — see the tvOS
            // cover. Gated, because this sheet is BOTH modes' library on macOS and the touch
            // grid's title belongs to the system background.
            .gamepadPaletteInk(gamepadUIActive)
        }
        #else
        // iOS: the cover is the TOUCH UI's presentation only. In gamepad mode the library is one
        // of GamepadHomeView's in-place layers (the console shell — no bottom-up cover), so the
        // proxy hides the target from the cover while that mode owns it; every writer (Y on a
        // tile, `returnToLibrary`) keeps writing the same `libraryTarget` either way, and a
        // controller arriving or leaving mid-browse hands the open library to whichever
        // presentation the new mode owns.
        .fullScreenCover(item: touchLibraryTarget) { shelf in
            NavigationStack {
                LibraryView(store: store, target: shelf, onLaunch: { launchTitle(shelf, $0) })
            }
        }
        #endif
        #endif
    }

    // Presentation flags for the prompt chain, extracted from their `.alert`/`.confirmationDialog`
    // calls so each manual get/set Binding type-checks on its own instead of inflating the body's
    // budget (inline, they tip SwiftUI's per-expression limit — see the split sections idiom).

    private var deepLinkNoticePresented: Binding<Bool> {
        Binding(
            get: { deepLinkNotice != nil && !consolePromptShowing },
            set: { if !$0 { deepLinkNotice = nil } })
    }

    private var deepLinkConfirmPresented: Binding<Bool> {
        Binding(
            get: { deepLinkConfirm != nil && !consolePromptShowing },
            set: { if !$0 { deepLinkConfirm = nil } })
    }

    /// True while the console prompt owns the modal state (see `consolePrompt`). Always false on
    /// tvOS, whose alerts the focus engine drives natively.
    private var consolePromptShowing: Bool {
        #if os(iOS) || os(macOS)
        consolePrompt != nil
        #else
        false
        #endif
    }

    #if os(iOS) || os(macOS)
    /// The modal state the console UI should present ITSELF, as a pad-navigable prompt, instead of
    /// letting a system alert take it. `.alert`/`.confirmationDialog` are UIKit/AppKit surfaces a
    /// controller cannot navigate, and these are not incidental prompts: "Pairing required" is the
    /// FIRST thing an unpaired host shows, "Connection failed" strands the console UI behind a
    /// modal only a finger can dismiss, and "Waiting for approval" owns the only Cancel for a
    /// connect that may never complete. One at a time, most-urgent first — a system alert stack
    /// would layer these, but a console shows one screen.
    ///
    /// Gated on not STREAMING, not on `model.connection == nil`: a connection object exists well
    /// before a stream does, through exactly the handshakes these prompts belong to. Streaming is
    /// the one case that must stay with the system alert — there the pad belongs to
    /// `GamepadCapture` and is being forwarded to the host.
    private var consolePrompt: GamepadPrompt? {
        guard gamepadUIActive, model.phase != .streaming else { return nil }
        if let req = approvalChoice {
            return GamepadPrompt(
                id: "pairing-required",
                title: "Pairing required",
                message: "\(req.host.displayName) requires pairing. Request access and approve "
                    + "this device in the host's web console (port 47992 → Pairing) — no PIN "
                    + "needed. Or pair with the 4-digit PIN it can display.",
                actions: [
                    // The follow-on presentation is deferred a tick exactly as the system dialog
                    // does it, so this prompt is fully torn down before the next screen mounts —
                    // two controller pollers overlapping for a frame is how one A press reaches
                    // both.
                    GamepadPromptAction(id: "request", title: "Request Access", isPrimary: true) {
                        approvalChoice = nil
                        DispatchQueue.main.async { requestAccess(req) }
                    },
                    GamepadPromptAction(id: "pin", title: "Pair with PIN…") {
                        approvalChoice = nil
                        DispatchQueue.main.async { pairingTarget = req.host }
                    },
                    GamepadPromptAction(id: "cancel", title: "Cancel", isCancel: true) {
                        approvalChoice = nil
                    },
                ])
        }
        if let req = awaitingApproval {
            return GamepadPrompt(
                id: "awaiting-approval",
                title: "Waiting for approval",
                message: "Approve \u{201C}\(localDeviceName)\u{201D} in \(req.host.displayName)'s "
                    + "web console (port 47992 → Pairing). This device connects automatically "
                    + "once you approve it — no need to reconnect.",
                actions: [
                    GamepadPromptAction(id: "cancel", title: "Cancel", isCancel: true) {
                        awaitingApproval = nil
                        model.disconnect()
                    },
                ],
                busy: true)
        }
        if connectionErrorReady {
            return GamepadPrompt(
                id: "connection-failed",
                title: "Connection failed",
                message: model.errorMessage ?? "",
                actions: [
                    GamepadPromptAction(id: "ok", title: "OK", isCancel: true) {
                        model.errorMessage = nil
                    },
                ])
        }
        if let confirm = deepLinkConfirm {
            return GamepadPrompt(
                id: "link-confirm",
                title: "Open this link?",
                message: confirm.message,
                actions: [
                    GamepadPromptAction(id: "go", title: confirm.actionTitle, isPrimary: true) {
                        runDeepLinkConfirm(confirm)
                    },
                    GamepadPromptAction(id: "cancel", title: "Cancel", isCancel: true) {
                        deepLinkConfirm = nil
                    },
                ])
        }
        if let notice = deepLinkNotice {
            return GamepadPrompt(
                id: "cant-open",
                title: "Can't open",
                message: notice,
                actions: [
                    GamepadPromptAction(id: "ok", title: "OK", isCancel: true) {
                        deepLinkNotice = nil
                    },
                ])
        }
        return nil
    }
    #endif

    /// The iOS library cover's item: `libraryTarget`, hidden while the gamepad shell presents
    /// the library in place (see the cover's comment).
    private var touchLibraryTarget: Binding<LibraryTarget?> {
        Binding(
            get: { gamepadUIActive ? nil : libraryTarget },
            set: { libraryTarget = $0 })
    }

    /// The pairing sheet's item. On iOS it hides while the gamepad shell presents the pair screen
    /// in place — the same proxy the library uses, and for the same reason: every writer keeps
    /// writing `pairingTarget`, and whichever presentation the current mode owns picks it up.
    /// macOS has no shell, so the sheet stays and switches its CONTENT by mode instead.
    private var touchPairingTarget: Binding<StoredHost?> {
        #if os(macOS)
        Binding(get: { pairingTarget }, set: { pairingTarget = $0 })
        #else
        Binding(
            get: { gamepadUIActive ? nil : pairingTarget },
            set: { pairingTarget = $0 })
        #endif
    }

    private var approvalChoicePresented: Binding<Bool> {
        Binding(
            get: { approvalChoice != nil && !consolePromptShowing },
            set: { if !$0 { approvalChoice = nil } })
    }

    private var awaitingApprovalPresented: Binding<Bool> {
        Binding(
            get: { awaitingApproval != nil && !consolePromptShowing },
            set: { if !$0 { awaitingApproval = nil } })
    }

    /// Whether the "Connection failed" state is ready to be shown at all — shared by the system
    /// alert and the console prompt so the two can never disagree about the macOS deferral below.
    private var connectionErrorReady: Bool {
        guard model.errorMessage != nil else { return false }
        #if os(macOS)
                // Defer the alert while a forced-fullscreen exit is still pending: a sheet
                // attached to a fullscreen window makes AppKit drop `-toggleFullScreen:`, so
                // presenting it now strands the window fullscreen on the home screen after a
                // session error (a deliberate disconnect sets no `errorMessage`, which is why
                // it never stuck). Tearing the session down already flipped `active`→false;
                // once the window leaves fullscreen and `isFullscreen` flips, the alert shows
                // over the windowed home UI. Not gated when fullscreen is the user's own manual
                // choice (opt-out setting) — nothing is auto-exiting there to conflict with.
        if fullscreenForSession && isFullscreen { return false }
        #endif
        return true
    }

    private var connectionErrorPresented: Binding<Bool> {
        Binding(
            get: { connectionErrorReady && !consolePromptShowing },
            set: { if !$0 { model.errorMessage = nil } })
    }

    #if os(iOS)
    /// The Live Activity mode line, e.g. "2560×1440 @120 · HEVC · HDR", from the live connection.
    private func currentModeLine() -> String {
        guard let c = model.connection else { return "" }
        let codec: String
        switch c.videoCodec {
        case .h264: codec = "H.264"
        case .hevc: codec = "HEVC"
        case .av1: codec = "AV1"
        case .pyrowave: codec = "PyroWave"
        }
        var line = "\(c.width)×\(c.height)"
        if c.refreshHz > 0 { line += " @\(c.refreshHz)" }
        line += " · \(codec)"
        if c.isHDR { line += " · HDR" }
        return line
    }
    #endif

    /// Route a `punktfunk://` deep link into the existing connect path — the whole §2 grammar
    /// (design/client-deep-links.md): a stable id, a unique host name or an `addr[:port]`, with
    /// `fp`/`host` recovery parameters and a one-off `profile`.
    ///
    /// The security posture is the parser's plus four rules that live here, and none of them
    /// bends: a URL never pairs and never trusts on its own (an unknown host becomes a
    /// confirmation, not a connect), never dials on a GUESSABLE reference (only the stable record
    /// id connects unattended — a label or an address becomes a confirmation), never preempts a
    /// live session (same host → focus, different host → say so; NEVER tear one down on a
    /// background tap), and carries only references — a profile it can't honor refuses with a
    /// notice rather than streaming with the wrong settings.
    private func handleDeepLink(_ url: URL) {
        let link: DeepLink
        do {
            link = try DeepLink(url: url)
        } catch DeepLinkError.notOurScheme {
            return // not ours — ignore it silently rather than warning about someone else's URL
        } catch {
            deepLinkNotice = (error as? DeepLinkError)?.message
                ?? "That link is malformed and was ignored."
            return
        }
        switch link.route {
        case .connect:
            break
        case .browse:
            // The reserved library route, now real: open the host's game library without starting
            // a session. `launch=`/`profile=` are meaningless on a browse (nothing streams until a
            // title is picked, and that connect resolves its own profile) — ignored, not refused,
            // per the unknown-parameter rule.
            openLibrary(from: link)
            return
        case .wake:
            // Still reserved: saying so beats silently connecting instead. (Shortcuts users have
            // the Wake Host intent, which never round-trips through a URL.)
            deepLinkNotice = "Punktfunk links can't do “wake” yet."
            return
        }
        // Resolve the one-off profile BEFORE anything happens: an unknown or ambiguous reference
        // must refuse, not degrade to the host's binding (§10.6).
        var selection = ProfileSelection.inherit
        if let reference = link.profile {
            let (profile, resolution) = profiles.catalog.resolve(reference)
            switch resolution {
            case .found:
                selection = .profile(profile?.id ?? "")
            case .notFound:
                deepLinkNotice = "No settings profile called “\(reference)” on this device."
                return
            case .ambiguous:
                deepLinkNotice = "More than one settings profile is called “\(reference)”. "
                    + "Rename one, or link to it by its id."
                return
            }
        }
        let resolution = link.resolveHost(in: store.hosts)
        switch resolution {
        // A saved record. `.known` (named by its unguessable id) dials straight away; `.confirm`
        // (named by its label or its address, which anything that can open a URL could guess)
        // takes the same dial one tap later.
        case .known(let host), .confirm(let host):
            guard !link.pinConflict(with: host) else {
                deepLinkNotice = "That link's fingerprint doesn't match the identity saved for "
                    + "\(host.displayName). It's out of date, or it isn't pointing where it says."
                return
            }
            guard model.phase == .idle else {
                guard model.activeHost?.id == host.id else {
                    let current = model.activeHost?.displayName ?? "a host"
                    deepLinkNotice = "Already streaming \(current). End that session first."
                    return
                }
                return // deep-linked to the host we're already on — nothing to do
            }
            if case .confirm = resolution {
                deepLinkConfirm = DeepLinkConfirm(
                    host: host, launch: link.launch, profile: selection, browse: false)
                return
            }
            connect(host, launchID: link.launch, profile: selection)
        case .unknown(let address, let port, let name, let fp):
            // Never a silent connect — an unsaved host is a trust decision, and a link is not
            // where it gets made. This only NAMES what the link pointed at; adding the host is a
            // deliberate trip to the + button, where the fingerprint is on screen. (Linux, Android
            // and Windows instead pre-fill their trust prompt from the link; the outcome is the
            // same — nothing connects until a person looks at it — but the sheet is not seeded
            // here, so don't read this as doing that.)
            guard model.phase == .idle else {
                deepLinkNotice = "Already streaming. End that session first."
                return
            }
            deepLinkNotice = "\(name ?? address) isn't saved on this device yet. "
                + "Add it with the + button — the link points at \(address):\(String(port))"
                + (fp == nil ? "." : ", and carries a fingerprint to verify it against.")
        case .ambiguous:
            deepLinkNotice = "More than one saved host is called “\(link.hostRef)”. "
                + "Rename one, or link to it by its address."
        case .unresolvable:
            deepLinkNotice = "That host isn't saved on this device."
        }
    }

    /// `punktfunk://browse/<host-ref>` — jump into a host's game library. Drives the SAME
    /// `libraryTarget` every internal surface writes, so the link lands in whichever presentation
    /// the current mode owns: the gamepad console's in-place library screen, the touch cover, the
    /// macOS sheet, or tvOS's cover. Connect's posture minus the connect itself: a pin conflict
    /// refuses, a live session is never preempted (same host → the open already foregrounded the
    /// app, which is all "focus it" can mean mid-stream; different host → say so), and an unsaved
    /// host can't be browsed — the library fetch rides the paired mTLS identity, so there is
    /// nothing to show before the host is saved (the notice says what to do instead).
    private func openLibrary(from link: DeepLink) {
        // A `profile=` on a browse link picks the shelf, exactly as it picks the settings on a
        // connect link — and refuses the same way (§10.6): an unknown or ambiguous reference must
        // never quietly degrade to the host's binding, which is a different shelf wearing the same
        // host's name.
        var selection = ProfileSelection.inherit
        if let reference = link.profile {
            let (profile, resolution) = profiles.catalog.resolve(reference)
            switch resolution {
            case .found:
                selection = .profile(profile?.id ?? "")
            case .notFound:
                deepLinkNotice = "No settings profile called “\(reference)” on this device."
                return
            case .ambiguous:
                deepLinkNotice = "More than one settings profile is called “\(reference)”. "
                    + "Rename one, or link to it by its id."
                return
            }
        }
        let resolution = link.resolveHost(in: store.hosts)
        switch resolution {
        // Same rule as a connect link: only the record id opens on the link's own say-so.
        case .known(let host), .confirm(let host):
            guard !link.pinConflict(with: host) else {
                deepLinkNotice = "That link's fingerprint doesn't match the identity saved for "
                    + "\(host.displayName). It's out of date, or it isn't pointing where it says."
                return
            }
            guard model.phase == .idle else {
                guard model.activeHost?.id == host.id else {
                    let current = model.activeHost?.displayName ?? "a host"
                    deepLinkNotice = "Already streaming \(current). End that session first."
                    return
                }
                return // browsing the host we're already streaming — nothing to do
            }
            if case .confirm = resolution {
                deepLinkConfirm = DeepLinkConfirm(
                    host: host, launch: nil, profile: selection, browse: true)
                return
            }
            libraryTarget = LibraryTarget(host: host, profile: selection)
        case .unknown(let address, _, let name, _):
            deepLinkNotice = "\(name ?? address) isn't saved on this device yet. "
                + "Add it with the + button first — a library can only be browsed on a saved host."
        case .ambiguous:
            deepLinkNotice = "More than one saved host is called “\(link.hostRef)”. "
                + "Rename one, or link to it by its address."
        case .unresolvable:
            deepLinkNotice = "That host isn't saved on this device."
        }
    }

    private var home: some View {
        // The full-screen connect takeover rides over BOTH home UIs (and the pre-connect window is
        // still `home`, so it covers the whole dial → wake → online → connect sequence): instant
        // "Connecting…" feedback on any dial, flowing seamlessly into the "Waking…" wait if the host
        // turns out to be asleep.
        homeBase.overlay {
            ConnectOverlay(
                connectingHostName: connectingOverlayName,
                waker: waker,
                gamepadUI: gamepadUIActive,
                onCancelConnect: { model.disconnect() })
                // The takeover mounts OUTSIDE the gamepad screens (it covers the whole home), so
                // it publishes the palette's ink itself rather than inheriting it.
                .gamepadPaletteInk()
        }
    }

    /// The host label for the connect takeover's "Connecting…" phase — a plain dial in flight. Nil
    /// during the delegated-approval wait (that has its own "Waiting for approval" prompt, so the
    /// takeover must not stack over it) and, of course, when idle or streaming.
    private var connectingOverlayName: String? {
        guard awaitingApproval == nil, model.phase == .connecting, let host = model.activeHost
        else { return nil }
        return host.displayName
    }

    @ViewBuilder private var homeBase: some View {
        #if os(macOS)
        Group {
            if gamepadUIActive {
                GamepadHomeView(
                    store: store, model: model, discovery: discovery,
                    libraryTarget: $libraryTarget, pairingTarget: $pairingTarget,
                    onPaired: handlePaired, waker: waker,
                    connect: { connect($0, profile: $1) }, connectDiscovered: connectDiscovered,
                    launchTitle: launchTitle,
                    wakeOnly: { wakeOnly($0) },
                    promptActive: consolePromptShowing)
            } else {
                HomeView(
                    store: store, model: model, discovery: discovery,
                    showAddHost: $showAddHost, pairingTarget: $pairingTarget,
                    speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
                    connect: { connect($0, profile: $1) }, connectDiscovered: connectDiscovered,
                    onPaired: handlePaired, onLaunchTitle: launchTitle, wake: { wakeOnly($0) })
            }
        }
        #else
        Group {
            if gamepadUIActive {
                GamepadHomeView(
                    store: store, model: model, discovery: discovery,
                    libraryTarget: $libraryTarget, pairingTarget: $pairingTarget,
                    onPaired: handlePaired, waker: waker,
                    connect: { connect($0, profile: $1) }, connectDiscovered: connectDiscovered,
                    launchTitle: launchTitle,
                    wakeOnly: { wakeOnly($0) },
                    promptActive: consolePromptShowing)
                // On tvOS pairing/library normally present from HomeView's navigationDestinations
                // — which aren't mounted while the gamepad launcher is up. Give the launcher its
                // own presenters (exactly one of the two homes is mounted at a time, so these can
                // never double-present against HomeView's routes). Menu closes a cover the same
                // way B backs out elsewhere; PairSheet's own onDisappear cancels a live ceremony.
                #if os(tvOS)
                .fullScreenCover(item: $pairingTarget) { host in
                    PairSheet(host: host) { fingerprint in handlePaired(host, fingerprint: fingerprint) }
                        .onExitCommand { pairingTarget = nil }
                        // A tvOS cover draws NO background of its own, and this one is attached
                        // outside the launcher's `gamepadPaletteInk` — so the pairing screen used
                        // to render the system's dark chrome directly over the launcher showing
                        // through it, which under a pale palette is white text on a bright field
                        // (the PIN prompt was all but invisible). Give it the console's own field
                        // and the palette's ink, like every other screen the launcher opens. Only
                        // this branch: `HomeView`'s route to the same sheet is the TOUCH UI, which
                        // sits on the system background and has no palette.
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .background { GamepadFormBackground() }
                        .gamepadPaletteInk()
                }
                .fullScreenCover(item: $libraryTarget) { shelf in
                    NavigationStack {
                        LibraryView(store: store, target: shelf, onLaunch: { launchTitle(shelf, $0) })
                    }
                    .onExitCommand { libraryTarget = nil }
                    // On the STACK, not just inside LibraryView: the navigation title is drawn by
                    // the stack, which wraps that view from outside its own `gamepadPaletteInk` —
                    // so the shelf's name stayed white over a pale field while the content below
                    // it had already gone dark. Unconditional here because this cover only exists
                    // in the launcher's branch, where the console UI is by definition drawing.
                    .gamepadPaletteInk()
                }
                #endif
            } else {
                HomeView(
                    store: store, model: model, discovery: discovery,
                    showAddHost: $showAddHost, pairingTarget: $pairingTarget,
                    speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
                    showSettings: $showSettings,
                    connect: { connect($0, profile: $1) }, connectDiscovered: connectDiscovered,
                    onPaired: handlePaired, onLaunchTitle: launchTitle, wake: { wakeOnly($0) })
            }
        }
        #endif
    }

    // MARK: - Session

    private var sessionView: some View {
        let pendingFingerprint: Data? = {
            if case .awaitingTrust(let fp) = model.phase { return fp }
            return nil
        }()
        return ZStack {
            stream(captureEnabled: pendingFingerprint == nil)
                // Blur the live stream during the trust prompt (heavy) and during a resize (lighter
                // — the deliberate "hold on" while the host rebuilds its pipeline and the decoder
                // re-inits on the new-mode IDR). Only the resize blur animates; the trust blur snaps
                // as before (its own overlay handles the transition).
                .blur(radius: pendingFingerprint != nil ? 32 : (model.resizing ? 16 : 0))
                .animation(.easeInOut(duration: 0.22), value: model.resizing)
                .overlay {
                    if pendingFingerprint != nil {
                        Color.black.opacity(0.45)
                    }
                }
                // The resize spinner rides over the (blurred) stream; suppressed under the trust
                // prompt, which owns the screen. It never hit-tests, so window-drag resizes keep
                // steering and the next click still reaches the stream. Mounted ONLY while a
                // resize is live: resident structure above the CAMetalLayer is what the stage-4
                // direct-to-display hunt is eliminating — composited presents reach glass a full
                // refresh later. The enter/exit fade rides the call-site transition + the
                // .animation(value: resizing) below (the view's internal `if active` fade can't
                // run when the whole view unmounts).
                .overlay {
                    if pendingFingerprint == nil, model.resizing {
                        ResizeIndicatorView(active: true)
                            .transition(.opacity.combined(with: .scale(scale: 0.92)))
                    }
                }
                .animation(.easeInOut(duration: 0.22), value: model.resizing)
            if let fp = pendingFingerprint {
                TrustCardView(
                    fingerprint: fp,
                    hostName: model.activeHost?.displayName ?? "host",
                    onCancel: { model.rejectTrust() },
                    onTrust: {
                        if let fp = model.confirmTrust(), let host = model.activeHost {
                            store.pin(host.id, fingerprint: fp)
                        }
                    },
                    onPairInstead: {
                        let host = model.activeHost
                        model.rejectTrust()
                        pairingTarget = host
                    })
            }
        }
        #if os(macOS)
        .frame(minWidth: 640, minHeight: 360)
        .background(Color.black)
        // FULLSCREEN fills the whole display, INCLUDING behind the camera housing (notch).
        // Without this the stream is laid out in the safe area below the notch, so an
        // aspect-fit video at the display's native mode scales down and leaves black borders.
        // A fullscreen video behind the notch (a thin top-center strip occluded) is the
        // expected behavior — same edge-to-edge intent as the iOS/tvOS branches below.
        // WINDOWED keeps the TOP inset: macOS 26 windows extend content under the (glass)
        // title bar and report its height as top safe area — ignoring it there put the top of
        // the video (and the HUD) underneath the title bar. The black `.background` above is a
        // ShapeStyle background, which always extends under every inset, so the strip behind
        // the title bar stays black rather than showing the video.
        .ignoresSafeArea(edges: isFullscreen ? .all : [.horizontal, .bottom])
        #elseif os(iOS)
        // Streaming is immersive: edge-to-edge under the status bar and home
        // indicator, both hidden for the session (they return with the hosts grid).
        .background(Color.black)
        .ignoresSafeArea()
        .statusBarHidden(true)
        .persistentSystemOverlays(.hidden)
        #else
        .background(Color.black)
        .ignoresSafeArea()
        // SWALLOW Menu/B during a session — a game controller's B button ALSO surfaces as this
        // UIKit menu press, so the old instant-disconnect here ended the session on every B
        // press in gameplay. The button still reaches the host via GamepadCapture; the
        // DELIBERATE exits are holding the remote's Back ≥ 1 s (SiriRemotePointer) and holding
        // L1+R1+Start+Select ≥ 1.5 s on a pad (GamepadCapture's escape chord), both surfaced by
        // the start-of-stream banner. The empty handler is what keeps the press from bubbling
        // out and suspending the app.
        .onExitCommand {}
        #endif
    }

    private func stream(captureEnabled: Bool) -> some View {
        let placement = HUDPlacement(rawValue: hudPlacement) ?? .topTrailing
        return Group {
            if let conn = model.connection {
                StreamView(
                    connection: conn,
                    captureEnabled: captureEnabled,
                    onCaptureChange: { [weak model] captured in
                        model?.mouseCaptured = captured
                    },
                    onDisconnectRequest: { [weak model] in
                        model?.disconnect() // the captured-state ⌃⌥⇧D combo
                    },
                    onFrame: { [meter = model.meter, latency = model.latency,
                                split = model.latencySplit, queue = model.clientQueue] au in
                        meter.note(byteCount: au.data.count)
                        // Read the offset PER AU (an atomic load), never in the capture list: a
                        // capture-list `offset =` froze the connect-time estimate for the whole
                        // session, and on a host whose wall clock steps (VM + NTP) that frozen
                        // value shifted hostnet/e2e by ~15 ms between sessions while the meter's
                        // impossible-sample guard hid the damage (field 2026-08-13). See
                        // `PunktfunkConnection.clockOffsetNs`.
                        let offset = conn.clockOffsetNs
                        latency.record(ptsNs: au.ptsNs, offsetNs: offset)
                        // The same receipt, keyed by pts, awaiting its 0xCF host timing (the
                        // host/network split — drained by the 1 s stats tick). receivedNs is
                        // the core's reassembly stamp (ABI v9), so the split's network term no
                        // longer contains the client-queue wait...
                        split.recordReceipt(
                            ptsNs: au.ptsNs, receivedNs: au.receivedNs, offsetNs: offset)
                        // ...which is measured as its own term instead (receipt→pull, both
                        // client-local).
                        queue.record(
                            ptsNs: UInt64(bitPattern: au.receivedNs), atNs: au.pulledNs,
                            offsetNs: 0)
                    },
                    onSessionEnd: { [weak model] in
                        Task { @MainActor in model?.sessionEnded() }
                    },
                    // Resize overlay START — the follower is main-actor, so this drives the blur
                    // + spinner synchronously the instant the window differs from the live mode.
                    onResizeTarget: { [weak model] w, h in
                        model?.resizeTargeted(width: w, height: h)
                    },
                    // Resize overlay END — the coded dims of each new-mode IDR, reported from the
                    // decode pump thread; hop to the main actor to clear the overlay.
                    onDecodedSize: { [weak model] w, h in
                        Task { @MainActor in model?.resizeDecoded(width: w, height: h) }
                    },
                    endToEndMeter: model.endToEnd,
                    decodeMeter: model.decodeStage,
                    displayMeter: model.displayStage,
                    presentFloorMeter: model.presentFloor
                )
                .overlay(alignment: placement.alignment) {
                    // The stats overlay MORPHS between tiers and SCALES UP on enter. With no `.id`, a
                    // verbosity change keeps the same StreamHUDView identity, so its one shared glass
                    // card animates its frame/shape to the new tier (a morph) instead of cross-fading a
                    // fresh card in. The `.transition` therefore fires only on the off↔on boundary — a
                    // scale-up (0.8→1) from the HUD's own corner. The ZStack is the stable host the
                    // `.animation` watches as the child enters/leaves and morphs.
                    ZStack {
                        if captureEnabled && statsVerbosity != .off {
                            StreamHUDView(
                                model: model, connection: conn, placement: placement,
                                verbosity: statsVerbosity)
                                .transition(
                                    .scale(scale: 0.8, anchor: placement.unitPoint)
                                        .combined(with: .opacity))
                        }
                    }
                    .animation(.smooth(duration: 0.28), value: statsVerbosity)
                }
                // The bottom-centre stack: the muted-microphone badge over the start-of-stream
                // shortcut banner. ONE overlay for both, so the two can never land on top of each
                // other in the seconds where they overlap.
                .overlay(alignment: .bottom) {
                    VStack(spacing: 8) {
                        // A forwarded pad has a gyro this session's virtual controller cannot
                        // carry. Shown briefly at every stats tier and with the overlay off: the
                        // failure is otherwise completely silent — the gyro just does nothing —
                        // and the fix is a setting, so the hint has to name it. Every platform,
                        // including tvOS, where a DualSense is an ordinary way to play.
                        if captureEnabled, model.motionUnreachableKind != nil {
                            MotionUnreachableBadge()
                                .transition(.opacity.combined(with: .scale(scale: 0.9)))
                        }
                        // The SC2 passthrough's claim edge (never true on tvOS — no capture
                        // there). Same transient contract as the motion hint above; without it
                        // the raw BLE capture engages with no visible trace anywhere in the app.
                        if captureEnabled, model.sc2CapturedHint {
                            Sc2CapturedBadge()
                                .transition(.opacity.combined(with: .scale(scale: 0.9)))
                        }
                        // The expiry-warning toast (T−5 m / T−1 m, per-client access §7) —
                        // transient, every platform, every tier: "the pad just died" must
                        // read as "the evening's access ended" while it can still be fixed.
                        if captureEnabled, let warning = model.accessWarning {
                            AccessWarningBadge(text: warning)
                                .transition(.opacity.combined(with: .scale(scale: 0.9)))
                        }
                        #if !os(tvOS)
                        // The access chip — up for a LIMITED session ("Controller only ·
                        // ends in 1 h 58 m") while the stats overlay is on. It rides the
                        // stats tier rather than standing for the whole stream: a pill that
                        // never goes away is chrome you read as distraction. Never mounted
                        // for a full-and-permanent session (every old host): today's look
                        // must not change there. tvOS states it as a line in the stats
                        // overlay instead (StreamHUDView).
                        if captureEnabled && statsVerbosity != .off && model.accessLimited {
                            AccessChipBadge(
                                label: model.accessLevel.label,
                                remainingSecs: model.accessRemainingSecs)
                                .transition(.opacity.combined(with: .scale(scale: 0.9)))
                        }
                        // Shown for as long as the mic is muted, at every stats tier and with the
                        // overlay off — see MicMutedBadge. tvOS has no microphone to mute.
                        if captureEnabled && model.micMuted {
                            MicMutedBadge { model.setMicMuted(false) }
                                .transition(.opacity.combined(with: .scale(scale: 0.9)))
                        }
                        #endif
                        // The start-of-stream shortcut banner used to sit here (macOS/tvOS): the
                        // platform's reserved controls on a glass pill for the first 6 seconds of
                        // every session. It is now a page you can OPEN — About ▸ Shortcuts, on
                        // both the touch and the controller surface (ShortcutsCatalog) — because
                        // a message that shows once, over the stream you have just connected to,
                        // is unavailable at the moment the question is actually asked. It also
                        // put a composited overlay above the stream for those 6 seconds, which on
                        // this path costs a refresh of display latency (see the iOS exit disc's
                        // note below); the reference page costs nothing during a session.
                    }
                    .padding(.bottom, 24)
                    .animation(.easeOut(duration: 0.2), value: model.micMuted)
                    .animation(.easeOut(duration: 0.2), value: model.accessWarning)
                    .animation(.easeOut(duration: 0.2), value: model.accessLimited)
                    // The access chip now rides the stats tier, so the tier is a visibility
                    // driver for this stack too — without it the chip pops on the toggle.
                    .animation(.easeOut(duration: 0.2), value: statsVerbosity)
                    // The motion hint was the one badge missing from this cluster — its
                    // `.transition` fired in an unanimated transaction and popped. One list,
                    // so every badge in the stack enters and exits the same way.
                    .animation(.easeOut(duration: 0.2), value: model.motionUnreachableKind)
                    .animation(.easeOut(duration: 0.2), value: model.sc2CapturedHint)
                }
                #if os(iOS)
                // Touch users have no menu / ⌘D, so when the HUD's Disconnect button isn't on
                // screen — the overlay off, or the compact pill (which carries no button) —
                // keep a minimal touch exit in a corner. It rides a material disc (like the
                // HUD) so the glyph stays legible over a bright frame.
                //
                // In the OFF tier the disc shows for the first 8 s of a session, then leaves
                // the hierarchy ENTIRELY (the shortcut-banner pattern): any composited overlay
                // above the stream — a glass one doubly so, its blur SAMPLES the video layer —
                // forces the CAMetalLayer through the compositor, costing ~a refresh of display
                // latency and blocking direct-to-display promotion. Off is the immersive/
                // measurement tier; after the fade, touch-only exits are backgrounding the app
                // or re-enabling the stats overlay. Compact keeps its disc permanently — that
                // tier composites a HUD pill anyway, so hiding the exit there wins nothing.
                .overlay(alignment: .topLeading) {
                    if captureEnabled,
                       statsVerbosity == .compact || (statsVerbosity == .off && showTouchExit) {
                        HStack(spacing: 10) {
                            Button { model.disconnect() } label: { touchDisc("xmark") }
                                .buttonStyle(.plain)
                                .accessibilityLabel("Disconnect")
                            // The mic toggle rides the same discs, for the same reason: in these
                            // tiers the HUD carries no buttons (compact is a stat pill, off is
                            // nothing), so this is a touch-only user's ONLY way to mute. Absent —
                            // not greyed — when the session sends no microphone at all.
                            if model.micAvailable {
                                Button { model.toggleMicMute() } label: {
                                    touchDisc(model.micMuted ? "mic.slash.fill" : "mic.fill")
                                }
                                .buttonStyle(.plain)
                                .accessibilityLabel(
                                    model.micMuted ? "Unmute microphone" : "Mute microphone")
                            }
                        }
                        .padding(12)
                        .transition(.opacity)
                        .task {
                            guard statsVerbosity == .off else { return }
                            try? await Task.sleep(for: .seconds(8))
                            withAnimation(.easeOut(duration: 0.6)) { showTouchExit = false }
                        }
                    }
                }
                #endif
            }
        }
    }

    #if os(iOS)
    /// One touch-control disc: an SF Symbol on a floating glass disc over the frame (26+,
    /// material fallback), sized as a comfortable tap target. `interactive`: the disc IS the tap
    /// target, so the glass reacts to press, and the hit region is matched to the visible disc so
    /// every tap triggers that press highlight.
    private func touchDisc(_ symbol: String) -> some View {
        Image(systemName: symbol)
            .font(.headline.weight(.semibold))
            .frame(width: 36, height: 36)
            .glassBackground(Circle(), interactive: true)
            .contentShape(Circle())
    }
    #endif

    // The two `shortcutHintText` strings that used to live here — one per platform, told once per
    // session by the banner above — are now `ShortcutsCatalog.groups`, which both About pages
    // render. The mic line is still conditional there for the same reason it was here: teaching a
    // shortcut for a microphone that isn't on would be a lie.

    // MARK: - Connect

    /// `profile` is this connect's one-off pick ("Connect with ▸", a pinned card, a link's
    /// `profile=`). `.inherit` — the default, and what a plain card tap passes — falls through to
    /// the host's binding. A one-off NEVER rebinds the host: rebinding is always an explicit act
    /// in the edit sheet (design §5.2).
    private func connect(
        _ host: StoredHost, launchID: String? = nil,
        profile: ProfileSelection = .inherit, allowTofu: Bool? = nil
    ) {
        // A pinned host connects on its stored fingerprint; an unpinned host may only TOFU when
        // the host's LIVE advert says `pair=optional` (rule 3a). When the caller doesn't already
        // know the policy (a saved-card tap / manual entry), resolve it from the current mDNS set:
        // an unpinned host with no matching `pair=optional` advert routes to the approval choice
        // (request access / pair with PIN) instead of silently entering the trust prompt (rules
        // 3b + 4). A pinned host ignores all of this.
        if host.pinnedSHA256 == nil {
            let tofuOK = allowTofu ?? discovery.hosts.contains {
                host.matches($0) && $0.allowsTofu
            }
            if !tofuOK {
                // pair=required / unknown policy / manual entry (rule 3b): never a silent
                // connect — offer no-PIN delegated approval or the PIN ceremony.
                approvalChoice = ApprovalRequest(
                    host: host, advertisedFingerprint: advertisedFingerprint(for: host))
                return
            }
        }
        startSession(
            host, launchID: launchID, profile: profile, allowTofu: host.pinnedSHA256 == nil)
    }

    /// Resolve the stream mode + input prefs and hand off to the session model. The gamepad-type
    /// setting resolves NOW (Automatic → match the active physical controller): the host's virtual
    /// pad backend is fixed per session. `requestAccess` opens the no-PIN delegated-approval
    /// connect (host parks it until the operator approves).
    private func startSession(
        _ host: StoredHost, launchID: String? = nil,
        profile: ProfileSelection = .inherit,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil
    ) {
        let go = {
            startSessionDirect(
                host, launchID: launchID, profile: profile, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq)
        }
        // Not advertising and we can wake it? DIAL FIRST anyway — no mDNS advert does NOT mean
        // unreachable: a host reached over a routed network (Tailscale/VPN/another subnet) is
        // mDNS-blind forever, and gating the dial on presence bricked exactly those reconnects
        // (the host log shows no connection attempt at all; the tile pip and this gate share the
        // LAN-only `advertises` predicate). `prepareWake` inside the dial already fires the magic
        // packet up front, so a genuinely-asleep host is waking while the connect times out; only
        // when that dial FAILS do we fall into the visible "Waking…" wait — a cold box takes far
        // longer to boot than a connect will sit — and redial once it's back on mDNS.
        if autoWakeEnabled, PunktfunkConnection.wakeOnLANAvailable,
           !host.wakeMacs.isEmpty, !discovery.advertises(host) {
            discovery.start() // so the wake-wait can observe it reappear
            startSessionDirect(
                host, launchID: launchID, profile: profile, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq,
                onUnreachable: {
                    waker.start(
                        host: host, connectsAfter: true, macs: host.wakeMacs, lastIP: host.address,
                        isOnline: { discovery.advertises(host) }, onOnline: go)
                })
        } else {
            go()
        }
    }

    /// The actual dial — reached directly when the host is awake, or from the waker once a woken
    /// host is back online. `prepareWake` still runs here to LEARN/refresh the MAC now that the host
    /// is advertising (and is a harmless no-op otherwise). `onUnreachable` hands a plain connect
    /// failure back to the caller (the wake-wait fallback) instead of the error alert.
    private func startSessionDirect(
        _ host: StoredHost, launchID: String? = nil,
        profile: ProfileSelection = .inherit,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil,
        onUnreachable: (@MainActor () -> Void)? = nil
    ) {
        prepareWake(for: host)
        // The delegated-approval wait prompt only makes sense once we're actually dialing — set it
        // here (after any wake), not before, so it never stacks under the "Waking…" overlay.
        if let approvalReq { awaitingApproval = approvalReq }
        // THE resolution point (design §4.4): the globals plus this connect's profile, once, here.
        // The model latches the result for the whole session, so nothing downstream can end up
        // applying a profile to half of it.
        let effective = EffectiveSettings.resolve(
            host: host, selection: profile, catalog: profiles.catalog)
        model.connect(
            to: host,
            effective: effective,
            gamepad: GamepadManager.shared.resolveType(
                setting: PunktfunkConnection.GamepadType(
                    rawValue: UInt32(clamping: effective.gamepadType)) ?? .auto),
            launchID: launchID,
            // Where a game exit returns to, when this connect launched a title: the shelf that
            // title was picked on — the host's own, or the pinned card whose profile this connect
            // is using. Ignored by the model unless there is a launchID.
            shelf: LibraryTarget(host: host, profile: profile),
            allowTofu: allowTofu,
            requestAccess: requestAccess,
            onUnreachable: onUnreachable)
    }

    /// Learn-while-awake, wake-while-asleep — run just before every connect:
    ///  • host currently advertising (awake) → refresh its stored Wake-on-LAN MAC(s) from the live
    ///    advert, so a later wake has an up-to-date target;
    ///  • host NOT advertising (likely asleep/off) and we have MAC(s) → fire a magic packet first.
    ///    The connect that follows already retries/times out long enough for a woken host to come
    ///    up; if it's genuinely off/unreachable the connect fails as before. Best-effort and
    ///    non-blocking (the send runs off the main thread).
    private func prepareWake(for host: StoredHost) {
        if let live = discovery.hosts.first(where: { host.matches($0) }) {
            store.updateMacs(host.id, macs: live.macAddresses) // learn — on every platform
            store.updateOsChain(host.id, chain: live.osChain) // ditto for the card's OS mark
            // ...and the mgmt port, so the library keeps working against a host that moved it once
            // this device can no longer see the advert (VPN, routed subnet, multicast-dead Wi-Fi).
            store.updateMgmtPort(host.id, port: live.mgmtPort)
        } else if autoWakeEnabled, PunktfunkConnection.wakeOnLANAvailable, !host.wakeMacs.isEmpty {
            // Auto-wake only: fire the up-front packet so a genuinely-asleep host is booting while the
            // dial times out. With auto-wake off, connects go straight through (no packet).
            let macs = host.wakeMacs
            let ip = host.address
            DispatchQueue.global(qos: .userInitiated).async {
                PunktfunkConnection.wakeOnLAN(macs: macs, lastKnownIP: ip)
            }
        }
    }

    /// The no-PIN delegated-approval flow: open an identified connect the host parks until the
    /// operator approves it in the console, showing the cancelable "Waiting for approval" prompt
    /// meanwhile. On success the SAME connection is admitted (no reconnect) and the host is pinned
    /// as paired (see the `.streaming` branch of `onChange`).
    private func requestAccess(_ req: ApprovalRequest) {
        guard !model.isBusy else { return }
        // Pin the advertised certificate for a discovered host (impostor defence during the long
        // wait); a manually-typed host has no advertised fingerprint, so trust-on-first-use.
        var host = req.host
        host.pinnedSHA256 = req.advertisedFingerprint
        // `awaitingApproval` is set inside startSessionDirect (after any wake), so it never stacks
        // under the "Waking…" overlay.
        startSession(host, allowTofu: false, requestAccess: true, approvalReq: req)
    }

    /// Explicit wake-only (the touch card's "Wake Host" menu item / a future gamepad action): fire
    /// the packet and wait for the host to come online, but don't connect — the user then sees it
    /// go online and can connect.
    private func wakeOnly(_ host: StoredHost) {
        guard PunktfunkConnection.wakeOnLANAvailable, !host.wakeMacs.isEmpty else { return }
        discovery.start()
        waker.start(
            host: host, connectsAfter: false, macs: host.wakeMacs, lastIP: host.address,
            isOnline: { discovery.advertises(host) }, onOnline: {})
    }

    /// Picked a title in the (experimental) library: dismiss the browser and start a session that
    /// asks the host to launch it.
    /// A title picked on a library shelf: dial its host, booting straight into that title — with
    /// the shelf's profile. A pinned card's shelf carries its card's profile as the one-off, so a
    /// launch made there streams with the profile the card promises; the host's own shelf carries
    /// `.inherit` and the binding decides, exactly as a plain card tap does.
    private func launchTitle(_ shelf: LibraryTarget, _ id: String) {
        libraryTarget = nil
        connect(shelf.host, launchID: id, profile: shelf.profile)
    }

    /// Tap a discovered host: save it (so the session has a stored identity and the trust pin
    /// persists), then connect or pair per the host's advertised policy. The host is the policy
    /// authority — TOFU is offered ONLY when it explicitly advertised `pair=optional` (rule 3a);
    /// a `pair=required` host, or one with no/unknown `pair` field, gets the approval choice
    /// (request access / pair with PIN) (rule 3b). (A pinned discovered host connects silently
    /// inside `connect`.)
    private func connectDiscovered(_ d: DiscoveredHost) {
        guard !model.isBusy else { return }
        let host = StoredHost(
            name: d.name, address: d.host, port: d.port,
            mgmtPort: d.mgmtPort,
            macAddresses: d.macAddresses.isEmpty ? nil : d.macAddresses,
            osChain: d.osChain.isEmpty ? nil : d.osChain)
        store.add(host)
        if d.allowsTofu {
            connect(host, allowTofu: true)
        } else {
            // pair=required / unknown policy (rule 3b): offer no-PIN delegated approval or PIN.
            approvalChoice = ApprovalRequest(
                host: host, advertisedFingerprint: pinFingerprint(d.fingerprintHex))
        }
    }

    /// Pairing ceremony succeeded — pin the host and connect. The guard backstops a stale
    /// ceremony surfacing after dismissal (PairSheet also self-discards those).
    private func handlePaired(_ host: StoredHost, fingerprint: Data) {
        guard pairingTarget?.id == host.id else { return }
        store.pin(host.id, fingerprint: fingerprint)
        var pinned = host
        pinned.pinnedSHA256 = fingerprint
        connect(pinned)
    }

    /// The certificate fingerprint a live mDNS advert carries for this saved host (advisory — see
    /// `HostDiscovery`), to pin during a delegated-approval wait. nil if the host isn't currently
    /// advertising or advertised no/invalid `fp`.
    private func advertisedFingerprint(for host: StoredHost) -> Data? {
        pinFingerprint(discovery.hosts.first { host.matches($0) }?.fingerprintHex)
    }

    /// Parse an advertised cert fingerprint (lowercase hex) into the 32-byte pin the connect
    /// expects; nil unless it's exactly a 32-byte (SHA-256) value, so a malformed advert falls
    /// back to trust-on-first-use rather than failing the connect closed.
    private func pinFingerprint(_ hex: String?) -> Data? {
        guard let hex, let data = Data(hexString: hex), data.count == 32 else { return nil }
        return data
    }

    /// How the host lists this device in its approval prompt (matches PairSheet's client name).
    private var localDeviceName: String {
        #if os(macOS)
        Host.current().localizedName ?? "Mac"
        #else
        UIDevice.current.name
        #endif
    }

    // MARK: - First-run + dev hooks

    /// First run on iOS: default the stream mode to this device's native screen so the
    /// video fills the display instead of letterboxing 1920×1080 onto a 4:3 iPad. (The
    /// compiled-in AppStorage defaults only apply until any value is saved; macOS keeps
    /// 1080p — a desktop window is not the screen.)
    private func seedDefaultModeIfNeeded() {
        #if !os(macOS)
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: DefaultsKey.streamWidth) == nil else { return }
        let bounds = UIScreen.main.nativeBounds // portrait-oriented pixels
        defaults.set(Int(max(bounds.width, bounds.height)), forKey: DefaultsKey.streamWidth)
        defaults.set(Int(min(bounds.width, bounds.height)), forKey: DefaultsKey.streamHeight)
        defaults.set(UIScreen.main.maximumFramesPerSecond, forKey: DefaultsKey.streamHz)
        #endif
    }

    /// PUNKTFUNK_AUTOCONNECT=host[:port] connects immediately (trust-on-first-use,
    /// auto-confirmed — dev only) at the saved or PUNKTFUNK_MODE=WxHxHz mode, without
    /// touching the saved host list. PUNKTFUNK_COMPOSITOR=kwin|gamescope|… overrides the
    /// compositor preference and PUNKTFUNK_REMOTE_GAMEPAD=xbox360|dualsense the virtual
    /// pad type (same names as the host env knobs). (IPv4/hostname only.)
    ///
    /// DEBUG-ONLY, and compiled out of a release build: it streams to whatever host an
    /// environment variable names with the trust prompt auto-confirmed, which is a dev lever
    /// (`swift run`, the shot harness), never something a shipped app should answer to.
    private func autoConnectIfAsked() {
        #if DEBUG
        guard let target = ProcessInfo.processInfo.environment["PUNKTFUNK_AUTOCONNECT"],
              !target.isEmpty, model.phase == .idle
        else { return }
        let parts = target.split(separator: ":")
        var host = StoredHost(name: "", address: String(parts[0]))
        if parts.count == 2, let p = UInt16(parts[1]) { host.port = p }
        if let mode = ProcessInfo.processInfo.environment["PUNKTFUNK_MODE"] {
            let dims = mode.split(separator: "x").compactMap { Int($0) }
            if dims.count == 3 {
                width = dims[0]
                height = dims[1]
                hz = dims[2]
            }
        }
        // The dev levers layer over the globals (no host record, so no binding to resolve).
        var effective = EffectiveSettings(defaults: .standard)
        if let name = ProcessInfo.processInfo.environment["PUNKTFUNK_COMPOSITOR"],
           let c = PunktfunkConnection.Compositor(name: name) {
            effective.compositor = Int(c.rawValue)
        }
        var pad = GamepadManager.shared.resolveType(
            setting: PunktfunkConnection.GamepadType(
                rawValue: UInt32(clamping: effective.gamepadType)) ?? .auto)
        if let name = ProcessInfo.processInfo.environment["PUNKTFUNK_REMOTE_GAMEPAD"],
           let g = PunktfunkConnection.GamepadType(name: name) {
            // Back through resolveType so the lever is adopted as the session's setting: the
            // per-pad arrivals declare it too, which is what the host actually builds from.
            pad = GamepadManager.shared.resolveType(setting: g)
        }
        if let kbps = ProcessInfo.processInfo.environment["PUNKTFUNK_BITRATE_KBPS"],
           let v = Int(kbps) {
            effective.bitrateKbps = v
        }
        model.connect(to: host, effective: effective, gamepad: pad, autoTrust: true)
        #endif
    }
}
