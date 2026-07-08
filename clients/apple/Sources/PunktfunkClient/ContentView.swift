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
    @StateObject private var discovery = HostDiscovery()
    @AppStorage(DefaultsKey.streamWidth) private var width = 1920
    @AppStorage(DefaultsKey.streamHeight) private var height = 1080
    @AppStorage(DefaultsKey.streamHz) private var hz = 60
    @AppStorage(DefaultsKey.compositor) private var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) private var gamepadType = 0
    @AppStorage(DefaultsKey.bitrateKbps) private var bitrateKbps = 0
    @AppStorage(DefaultsKey.audioChannels) private var audioChannels = 2
    @AppStorage(DefaultsKey.codec) private var codec = "auto"
    @AppStorage(DefaultsKey.hdrEnabled) private var hdrEnabled = true
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) private var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.hudEnabled) private var hudEnabled = true
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    /// The `codec` setting as a `PUNKTFUNK_CODEC_*` soft-preference byte (`0` = auto).
    private var preferredCodecByte: UInt8 {
        switch codec {
        case "h264": return PunktfunkConnection.codecH264
        case "hevc": return PunktfunkConnection.codecHEVC
        case "av1": return PunktfunkConnection.codecAV1
        default: return 0
        }
    }
    @State private var showAddHost = false
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
    /// "Waking…" overlay). macOS-only in practice — WoL is gated off on iOS/tvOS.
    @StateObject private var waker = HostWaker()
    #if os(macOS)
    /// Whether the hosting window is native-fullscreen right now (reported by
    /// FullscreenController). Drives the session view's safe-area choice: fullscreen goes
    /// edge-to-edge (behind the notch); windowed respects the top inset so the title bar
    /// never covers the video.
    @State private var isFullscreen = false
    /// Shows the start-of-stream shortcut banner (the Windows client's discoverability
    /// pattern): raised on every transition to `.streaming`, dropped by the banner's own
    /// 6-second task. Independent of the stats HUD so the keys are discoverable even with
    /// statistics off.
    @State private var showShortcutHint = false
    #endif
    #if !os(macOS)
    @State private var showSettings = false
    #endif
    #if os(iOS) || os(macOS)
    // A connected controller (+ the Settings toggle) swaps the whole home screen for
    // GamepadHomeView instead of retrofitting HomeView's touch/desktop UI — see `home` below.
    @ObservedObject private var gamepadManager = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) private var gamepadUIEnabled = true
    private var gamepadUIActive: Bool {
        GamepadUIEnvironment.isActive(
            gamepadConnected: gamepadManager.active != nil, enabledSetting: gamepadUIEnabled)
    }
    #endif

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
        }
        .onChange(of: model.phase) { _, phase in
            switch phase {
            case .streaming:
                #if os(macOS)
                showShortcutHint = true // the 6 s shortcut banner, per session start
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
            disconnect: { model.disconnect() }))
        #endif
        #if os(macOS)
        // Fullscreen only while a session is up (incl. the trust prompt over the blurred stream),
        // windowed on the host list — so the picker isn't forced fullscreen. Opt-out in Settings.
        // The controller also reports the window's ACTUAL fullscreen state back into
        // `isFullscreen` (the user can toggle it manually), which drives the session view's
        // safe-area handling below.
        .background(FullscreenController(
            active: fullscreenWhileStreaming && model.connection != nil,
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
                + "device in the host's web console (port 3000 → Pairing) — no PIN needed. Or "
                + "pair with the 4-digit PIN it can display.")
        }
        // One "Connection failed" surface for every home screen (touch grid, gamepad launcher) and
        // platform — SessionModel funnels all connect/session errors into `errorMessage`.
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
                + "console (port 3000 → Pairing). This device connects automatically once you "
                + "approve it — no need to reconnect.")
        }
    }

    private var home: some View {
        // The "Waking…" overlay rides over BOTH home UIs (and the pre-connect window is still
        // `home`, so it covers the whole wake→online→connect sequence).
        homeBase.overlay { WakeOverlay(waker: waker) }
    }

    @ViewBuilder private var homeBase: some View {
        #if os(macOS)
        Group {
            if gamepadUIActive {
                GamepadHomeView(
                    store: store, model: model, discovery: discovery,
                    libraryTarget: $libraryTarget, waker: waker,
                    connect: { connect($0) }, connectDiscovered: connectDiscovered)
            } else {
                HomeView(
                    store: store, model: model, discovery: discovery,
                    showAddHost: $showAddHost, pairingTarget: $pairingTarget,
                    speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
                    connect: { connect($0) }, connectDiscovered: connectDiscovered,
                    onPaired: handlePaired, onLaunchTitle: launchTitle, wake: { wakeOnly($0) })
            }
        }
        #elseif os(iOS)
        Group {
            if gamepadUIActive {
                GamepadHomeView(
                    store: store, model: model, discovery: discovery,
                    libraryTarget: $libraryTarget, waker: waker,
                    connect: { connect($0) }, connectDiscovered: connectDiscovered)
            } else {
                HomeView(
                    store: store, model: model, discovery: discovery,
                    showAddHost: $showAddHost, pairingTarget: $pairingTarget,
                    speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
                    showSettings: $showSettings,
                    connect: { connect($0) }, connectDiscovered: connectDiscovered,
                    onPaired: handlePaired, onLaunchTitle: launchTitle, wake: { wakeOnly($0) })
            }
        }
        #else
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: $showAddHost, pairingTarget: $pairingTarget,
            speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
            showSettings: $showSettings,
            connect: { connect($0) }, connectDiscovered: connectDiscovered,
            onPaired: handlePaired, onLaunchTitle: launchTitle, wake: { wakeOnly($0) })
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
                .blur(radius: pendingFingerprint != nil ? 32 : 0)
                .overlay {
                    if pendingFingerprint != nil {
                        Color.black.opacity(0.45)
                    }
                }
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
        // Siri Remote MENU = disconnect (the idiomatic tvOS "back"). With no focusable
        // disconnect control during play, the controller's buttons flow to the host instead of
        // driving the focus engine. NOTE: a game controller's Menu is also forwarded to the
        // host as Start — the Siri Remote is the intended disconnect path.
        .onExitCommand { model.disconnect() }
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
                                split = model.latencySplit, offset = conn.clockOffsetNs] au in
                        meter.note(byteCount: au.data.count)
                        latency.record(ptsNs: au.ptsNs, offsetNs: offset)
                        // The same receipt, keyed by pts, awaiting its 0xCF host timing (the
                        // host/network split — drained by the 1 s stats tick).
                        split.recordReceipt(
                            ptsNs: au.ptsNs, receivedNs: au.receivedNs, offsetNs: offset)
                    },
                    onSessionEnd: { [weak model] in
                        Task { @MainActor in model?.sessionEnded() }
                    },
                    endToEndMeter: model.endToEnd,
                    decodeMeter: model.decodeStage,
                    displayMeter: model.displayStage
                )
                .overlay(alignment: placement.alignment) {
                    if captureEnabled && hudEnabled {
                        StreamHUDView(model: model, connection: conn, placement: placement)
                    }
                }
                #if os(macOS)
                // The start-of-stream shortcut banner (Windows-client parity): the full
                // reserved key set on a glass pill, bottom-centre, for the first 6 seconds of
                // every session — independent of the stats HUD, so the keys are discoverable
                // even with statistics off. The banner's own task drops it (cancelled cleanly
                // if the session view goes away first).
                .overlay(alignment: .bottom) {
                    if captureEnabled && showShortcutHint {
                        Text("Click the stream to capture · ⌃⌥⇧Q releases the mouse · "
                            + "⌃⌥⇧D disconnects · ⌃⌥⇧S stats")
                            .font(.geist(12, relativeTo: .caption))
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
                // Touch users have no menu / ⌘D, so when the HUD (and its Disconnect button)
                // is hidden, keep a minimal always-reachable exit in a corner. It rides a
                // material disc (like the HUD) so the glyph stays legible over a bright frame
                // — this is the sole touch disconnect path when stats are off.
                .overlay(alignment: .topLeading) {
                    if captureEnabled && !hudEnabled {
                        Button { model.disconnect() } label: {
                            Image(systemName: "xmark")
                                .font(.headline.weight(.semibold))
                                .frame(width: 36, height: 36)
                                // Sole touch exit when the HUD is off — a floating glass disc
                                // over the frame (26+, material fallback). interactive: the disc
                                // IS the tap target, so the glass reacts to press.
                                .glassBackground(Circle(), interactive: true)
                                // Match the hit region to the visible disc so every tap also
                                // triggers the interactive-glass press highlight.
                                .contentShape(Circle())
                        }
                        .buttonStyle(.plain)
                        .padding(12)
                        .accessibilityLabel("Disconnect")
                    }
                }
                #endif
            }
        }
    }

    // MARK: - Connect

    private func connect(_ host: StoredHost, launchID: String? = nil, allowTofu: Bool? = nil) {
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
        startSession(host, launchID: launchID, allowTofu: host.pinnedSHA256 == nil)
    }

    /// Resolve the @AppStorage stream mode + input prefs and hand off to the session model. The
    /// gamepad-type setting resolves NOW (Automatic → match the active physical controller): the
    /// host's virtual pad backend is fixed per session. `requestAccess` opens the no-PIN
    /// delegated-approval connect (host parks it until the operator approves).
    private func startSession(
        _ host: StoredHost, launchID: String? = nil,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil
    ) {
        let go = {
            startSessionDirect(
                host, launchID: launchID, allowTofu: allowTofu,
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
        if PunktfunkConnection.wakeOnLANAvailable, !host.wakeMacs.isEmpty, !discovery.advertises(host) {
            discovery.start() // so the wake-wait can observe it reappear
            startSessionDirect(
                host, launchID: launchID, allowTofu: allowTofu,
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
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil,
        onUnreachable: (@MainActor () -> Void)? = nil
    ) {
        prepareWake(for: host)
        // The delegated-approval wait prompt only makes sense once we're actually dialing — set it
        // here (after any wake), not before, so it never stacks under the "Waking…" overlay.
        if let approvalReq { awaitingApproval = approvalReq }
        model.connect(
            to: host,
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz),
            compositor: PunktfunkConnection.Compositor(
                rawValue: UInt32(clamping: compositor)) ?? .auto,
            gamepad: GamepadManager.shared.resolveType(
                setting: PunktfunkConnection.GamepadType(
                    rawValue: UInt32(clamping: gamepadType)) ?? .auto),
            bitrateKbps: UInt32(clamping: bitrateKbps),
            audioChannels: UInt8(clamping: audioChannels),
            hdrEnabled: hdrEnabled,
            preferredCodec: preferredCodecByte,
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
        } else if PunktfunkConnection.wakeOnLANAvailable, !host.wakeMacs.isEmpty {
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
        var pref = PunktfunkConnection.Compositor(
            rawValue: UInt32(clamping: compositor)) ?? .auto
        if let name = ProcessInfo.processInfo.environment["PUNKTFUNK_COMPOSITOR"],
           let c = PunktfunkConnection.Compositor(name: name) {
            pref = c
        }
        var pad = GamepadManager.shared.resolveType(
            setting: PunktfunkConnection.GamepadType(
                rawValue: UInt32(clamping: gamepadType)) ?? .auto)
        if let name = ProcessInfo.processInfo.environment["PUNKTFUNK_REMOTE_GAMEPAD"],
           let g = PunktfunkConnection.GamepadType(name: name) {
            pad = g
        }
        var bitrate = UInt32(clamping: bitrateKbps)
        if let kbps = ProcessInfo.processInfo.environment["PUNKTFUNK_BITRATE_KBPS"],
           let v = UInt32(kbps) {
            bitrate = v
        }
        model.connect(
            to: host,
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz),
            compositor: pref,
            gamepad: pad,
            bitrateKbps: bitrate,
            audioChannels: UInt8(clamping: audioChannels),
            hdrEnabled: hdrEnabled,
            preferredCodec: preferredCodecByte,
            autoTrust: true)
    }
}

#if os(macOS)
/// Drives the hosting window in/out of native fullscreen from SwiftUI state, and mirrors the
/// window's ACTUAL fullscreen state back into `isFullscreen` (the user can also toggle it with the
/// green button / ⌃⌘F — ContentView keys the session view's safe-area handling off the real state,
/// not the setting). Mounted invisibly in the view tree; on each `active` change it captures the
/// window and toggles fullscreen only when the current state differs (so it never fights a toggle
/// already in flight, and never touches a window the user fullscreened manually unless `active`
/// says otherwise).
private struct FullscreenController: NSViewRepresentable {
    let active: Bool
    @Binding var isFullscreen: Bool

    /// Holds the window's fullscreen-transition observers so they're rebound on a window change
    /// and removed on dismantle.
    final class Coordinator {
        var observers: [NSObjectProtocol] = []
        weak var observedWindow: NSWindow?
        deinit { observers.forEach(NotificationCenter.default.removeObserver(_:)) }
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        let want = active
        let isFullscreen = $isFullscreen
        let coordinator = context.coordinator
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            observeTransitions(of: window, coordinator: coordinator)
            let isFull = window.styleMask.contains(.fullScreen)
            if isFullscreen.wrappedValue != isFull { isFullscreen.wrappedValue = isFull }
            if want != isFull { window.toggleFullScreen(nil) }
        }
    }

    /// `willEnter` (not did) so the video goes edge-to-edge while the title bar is already
    /// animating away; `didExit` so the top inset returns only once the title bar is back —
    /// no black gap in either direction.
    private func observeTransitions(of window: NSWindow, coordinator: Coordinator) {
        guard coordinator.observedWindow !== window else { return }
        coordinator.observers.forEach(NotificationCenter.default.removeObserver(_:))
        coordinator.observers.removeAll()
        coordinator.observedWindow = window
        let isFullscreen = $isFullscreen
        for (name, value) in [
            (NSWindow.willEnterFullScreenNotification, true),
            (NSWindow.didExitFullScreenNotification, false),
        ] {
            coordinator.observers.append(NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { _ in
                isFullscreen.wrappedValue = value
            })
        }
    }
}
#endif

/// A fresh `pair=required`/unknown host pending a trust decision: drives both the "request access
/// vs. pair with PIN" choice and the subsequent approval wait. `advertisedFingerprint` is the
/// discovered host's advertised cert (nil for a manually-typed host → trust-on-first-use).
private struct ApprovalRequest {
    let host: StoredHost
    let advertisedFingerprint: Data?
}
