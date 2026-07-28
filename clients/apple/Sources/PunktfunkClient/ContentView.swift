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
    // The dev auto-connect hook writes these three, so they stay observed here; every OTHER
    // stream setting reaches a session through `EffectiveSettings`, resolved once per connect.
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
    #if os(iOS)
    /// Owns the Live Activity for the running session (Lock Screen / Dynamic Island). Driven from
    /// the session model's published state below; iPhone/iPad only.
    @State private var liveActivity = SessionActivityController()
    #endif
    @State private var pairingTarget: StoredHost?
    /// A fresh `pair=required`/unknown host the user tapped: drives the choice between no-PIN
    /// delegated approval ("Request Access") and the SPAKE2 PIN ceremony (rule 3b).
    @State private var approvalChoice: ApprovalRequest?
    /// A delegated-approval connect is in flight (host parks it until the operator approves):
    /// drives the cancelable "Waiting for approval" prompt and the pin-as-paired on success.
    @State private var awaitingApproval: ApprovalRequest?
    @State private var speedTestTarget: StoredHost?
    @State private var libraryTarget: StoredHost?
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
    #if os(macOS) || os(tvOS)
    /// Shows the start-of-stream shortcut banner (the Windows client's discoverability
    /// pattern): raised on every transition to `.streaming`, dropped by the banner's own
    /// 6-second task. Independent of the stats HUD so the keys are discoverable even with
    /// statistics off. On tvOS it carries the ONLY exits (hold Back / the pad chord) plus
    /// the remote-as-pointer controls, so it must be seen at least once per session.
    @State private var showShortcutHint = false
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
    private var gamepadUIActive: Bool {
        GamepadUIEnvironment.isActive(
            gamepadConnected: gamepadManager.active != nil, enabledSetting: gamepadUIEnabled)
    }

    var body: some View {
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
                #if os(macOS) || os(tvOS)
                showShortcutHint = true // the 6 s shortcut banner, per session start
                #endif
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
                let store = store
                DispatchQueue.main.async {
                    store.markConnected(host.id)
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
            clipboardAvailable: model.connection?.hostSupportsClipboard == true,
            clipboardOn: model.clipboardEnabled,
            toggleClipboard: { model.toggleClipboardSync() },
            disconnect: { model.disconnect() }))
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
        // On the outer Group so the sheet survives the trust-prompt → home transition
        // (the "Pair with PIN instead" path disconnects first — the host's accept loop
        // is sequential, a pairing connection would queue behind the live session).
        #if !os(tvOS)
        .sheet(item: $pairingTarget) { host in
            PairSheet(host: host) { fingerprint in handlePaired(host, fingerprint: fingerprint) }
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
        .sheet(item: $libraryTarget) { host in
            NavigationStack {
                LibraryView(store: store, host: host, onLaunch: { launchTitle(host, $0) })
            }
            .frame(minWidth: 940, minHeight: 620)
        }
        #else
        .fullScreenCover(item: $libraryTarget) { host in
            NavigationStack {
                LibraryView(store: store, host: host, onLaunch: { launchTitle(host, $0) })
            }
        }
        #endif
        #endif
        // Fresh pair=required / unknown host: offer the two ways in. An action sheet (not an
        // alert) so it never collides with the wait alert below. "Request Access" is the no-PIN
        // delegated-approval path; "Pair with PIN…" runs the SPAKE2 ceremony. The follow-on
        // presentation is deferred a tick so this dialog is fully dismissed first.
        .confirmationDialog(
            "Pairing required",
            isPresented: Binding(
                get: { approvalChoice != nil },
                set: { if !$0 { approvalChoice = nil } }),
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
        // One "Connection failed" surface for every home screen (touch grid, gamepad launcher) and
        // platform — SessionModel funnels all connect/session errors into `errorMessage`.
        .alert(
            "Connection failed",
            isPresented: Binding(
                get: {
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
                },
                set: { if !$0 { model.errorMessage = nil } })
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(model.errorMessage ?? "")
        }
        // The delegated-approval wait: the host holds the connection open until the operator
        // approves it. Cancel returns the UI at once; the in-flight connect is left to time out
        // and its late result is discarded by SessionModel's connect guard (disconnect resets the
        // phase/host it checks).
        .alert(
            "Waiting for approval",
            isPresented: Binding(
                get: { awaitingApproval != nil },
                set: { if !$0 { awaitingApproval = nil } }),
            presenting: awaitingApproval
        ) { _ in
            Button("Cancel", role: .cancel) { model.disconnect() }
        } message: { req in
            Text("Approve \u{201C}\(localDeviceName)\u{201D} in \(req.host.displayName)'s web "
                + "console (port 47992 → Pairing). This device connects automatically once you "
                + "approve it — no need to reconnect.")
        }
        // Informational deep-link outcome (unknown host / already streaming). Not an error.
        .alert("Can't open", isPresented: deepLinkNoticePresented) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(deepLinkNotice ?? "")
        }
    }

    /// Presentation flag for the informational deep-link alert. Extracted from the `.alert` call so
    /// the manual get/set Binding type-checks on its own instead of inflating the body chain's
    /// budget (adding it inline tips SwiftUI's per-expression limit — see the split sections idiom).
    private var deepLinkNoticePresented: Binding<Bool> {
        Binding(get: { deepLinkNotice != nil }, set: { if !$0 { deepLinkNotice = nil } })
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
    /// The security posture is the parser's plus three rules that live here, and none of them
    /// bends: a URL never pairs and never trusts on its own (an unknown host becomes a
    /// confirmation, not a connect), never preempts a live session (same host → focus, different
    /// host → say so; NEVER tear one down on a background tap), and carries only references — a
    /// profile it can't honor refuses with a notice rather than streaming with the wrong settings.
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
        guard link.route == .connect else {
            // `wake` and `browse` are reserved in the grammar and parse today; this build routes
            // neither, and saying so beats silently connecting instead.
            deepLinkNotice = "Punktfunk links can't do “\(link.route.rawValue)” yet."
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
        switch link.resolveHost(in: store.hosts) {
        case .known(let host):
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
            connect(host, launchID: link.launch, profile: selection)
        case .unknown(let address, let port, let name, let fp):
            // Never a silent connect: hand the address, claimed name and pin to the add sheet so
            // the user makes the trust decision with their eyes on it.
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
                    libraryTarget: $libraryTarget, waker: waker,
                    connect: { connect($0, profile: $1) }, connectDiscovered: connectDiscovered)
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
                    libraryTarget: $libraryTarget, waker: waker,
                    connect: { connect($0, profile: $1) }, connectDiscovered: connectDiscovered)
                // On tvOS pairing/library normally present from HomeView's navigationDestinations
                // — which aren't mounted while the gamepad launcher is up. Give the launcher its
                // own presenters (exactly one of the two homes is mounted at a time, so these can
                // never double-present against HomeView's routes). Menu closes a cover the same
                // way B backs out elsewhere; PairSheet's own onDisappear cancels a live ceremony.
                #if os(tvOS)
                .fullScreenCover(item: $pairingTarget) { host in
                    PairSheet(host: host) { fingerprint in handlePaired(host, fingerprint: fingerprint) }
                        .onExitCommand { pairingTarget = nil }
                }
                .fullScreenCover(item: $libraryTarget) { host in
                    NavigationStack {
                        LibraryView(store: store, host: host, onLaunch: { launchTitle(host, $0) })
                    }
                    .onExitCommand { libraryTarget = nil }
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
                                split = model.latencySplit, queue = model.clientQueue,
                                offset = conn.clockOffsetNs] au in
                        meter.note(byteCount: au.data.count)
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
                #if os(macOS) || os(tvOS)
                // The start-of-stream shortcut banner (Windows-client parity): the platform's
                // reserved controls on a glass pill, bottom-centre, for the first 6 seconds of
                // every session — independent of the stats HUD, so the keys are discoverable
                // even with statistics off. The banner's own task drops it (cancelled cleanly
                // if the session view goes away first). On tvOS it carries the ONLY exits —
                // Menu/B is swallowed during a session (the `.onExitCommand {}` in the tvOS
                // session branch), so the hold gestures must be told to the user.
                .overlay(alignment: .bottom) {
                    if captureEnabled && showShortcutHint {
                        Text(Self.shortcutHintText)
                            .font(.geist(Self.shortcutHintFont, relativeTo: .caption))
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 14)
                            .padding(.vertical, 8)
                            .glassBackground(Capsule())
                            .padding(.bottom, 24)
                            .transition(.opacity)
                            .task {
                                try? await Task.sleep(for: .seconds(6))
                                withAnimation(.easeOut(duration: 0.6)) { showShortcutHint = false }
                            }
                    }
                }
                #endif
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
                        Button { model.disconnect() } label: {
                            Image(systemName: "xmark")
                                .font(.headline.weight(.semibold))
                                .frame(width: 36, height: 36)
                                // Floating glass disc over the frame (26+, material fallback).
                                // interactive: the disc IS the tap target, so the glass reacts
                                // to press.
                                .glassBackground(Circle(), interactive: true)
                                // Match the hit region to the visible disc so every tap also
                                // triggers the interactive-glass press highlight.
                                .contentShape(Circle())
                        }
                        .buttonStyle(.plain)
                        .padding(12)
                        .accessibilityLabel("Disconnect")
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

    #if os(macOS)
    private static let shortcutHintText =
        "Click the stream to capture · ⌃⌥⇧Q releases the mouse · ⌃⌥⇧D disconnects · ⌃⌥⇧S stats"
    private static let shortcutHintFont: CGFloat = 12
    #elseif os(tvOS)
    private static let shortcutHintText =
        "Hold the remote's Back button — or L1+R1+Start+Select on a controller — to disconnect"
        + " · Touch surface moves the pointer · press clicks · Play/Pause right-clicks"
    private static let shortcutHintFont: CGFloat = 22 // read from the couch
    #endif

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
    private func launchTitle(_ host: StoredHost, _ id: String) {
        libraryTarget = nil
        connect(host, launchID: id)
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
            macAddresses: d.macAddresses.isEmpty ? nil : d.macAddresses)
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
    private func autoConnectIfAsked() {
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
    }
}
