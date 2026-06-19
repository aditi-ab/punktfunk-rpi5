// Hosts grid ⇄ trust prompt ⇄ live stream. ContentView is the coordinator: it owns the session
// model, host store, and LAN discovery; switches between the home grid (HomeView) and the live
// session; and holds the connect logic (it reads the @AppStorage stream mode). The grid + cards
// (HomeView/HostCards), the trust prompt (TrustCardView), and the HUD (StreamHUDView) live in
// their own files.
//
// Two ways to establish trust on first contact: the TOFU prompt (host fingerprint over the
// live-but-blurred stream, compared with the host's log) or the PIN pairing ceremony — pairing
// verifies both sides at once and is the only way into hosts running --require-pairing. Once
// pinned, reconnects are silent and a changed host identity refuses to connect.

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
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) private var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.hudEnabled) private var hudEnabled = true
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @State private var showAddHost = false
    @State private var pairingTarget: StoredHost?
    @State private var speedTestTarget: StoredHost?
    @State private var libraryTarget: StoredHost?
    #if !os(macOS)
    @State private var showSettings = false
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
            // A session actually started — remember it on the card ("Connected … ago"
            // plus the accent ring on the most recent host).
            if case .streaming = phase, let host = model.activeHost {
                store.markConnected(host.id)
            }
        }
        .onDisappear { model.disconnect() } // window closed mid-session (Cmd+N spawns more)
        // Expose the session to the Scene-level Stream menu (Disconnect ⌘D works even when
        // the HUD is hidden). tvOS has no such menu.
        #if !os(tvOS)
        .focusedSceneValue(\.sessionFocus, SessionFocus(
            isStreaming: model.connection != nil,
            disconnect: { model.disconnect() }))
        #endif
        #if os(macOS)
        // Fullscreen only while a session is up (incl. the trust prompt over the blurred stream),
        // windowed on the host list — so the picker isn't forced fullscreen. Opt-out in Settings.
        .background(FullscreenController(active: fullscreenWhileStreaming && model.connection != nil))
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
        .sheet(item: $libraryTarget) { host in
            NavigationStack {
                LibraryView(store: store, host: host, onLaunch: { launchTitle(host, $0) })
            }
        }
        #endif
    }

    private var home: some View {
        #if os(macOS)
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: $showAddHost, pairingTarget: $pairingTarget,
            speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
            connect: { connect($0) }, connectDiscovered: connectDiscovered,
            onPaired: handlePaired, onLaunchTitle: launchTitle)
        #else
        HomeView(
            store: store, model: model, discovery: discovery,
            showAddHost: $showAddHost, pairingTarget: $pairingTarget,
            speedTestTarget: $speedTestTarget, libraryTarget: $libraryTarget,
            showSettings: $showSettings,
            connect: { connect($0) }, connectDiscovered: connectDiscovered,
            onPaired: handlePaired, onLaunchTitle: launchTitle)
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
        // Fill the whole display in fullscreen, INCLUDING behind the camera housing (notch).
        // Without this the stream is laid out in the safe area below the notch, so an
        // aspect-fit video at the display's native mode scales down and leaves black borders.
        // A fullscreen video behind the notch (a thin top-center strip occluded) is the
        // expected behavior — same edge-to-edge intent as the iOS/tvOS branches below. Inert
        // in windowed mode (no notch safe-area inset on a titled window).
        .ignoresSafeArea()
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
                    onFrame: { [meter = model.meter, latency = model.latency, offset = conn.clockOffsetNs] au in
                        meter.note(byteCount: au.data.count)
                        latency.record(ptsNs: au.ptsNs, offsetNs: offset)
                    },
                    onSessionEnd: { [weak model] in
                        Task { @MainActor in model?.sessionEnded() }
                    },
                    presentMeter: model.presentLatency
                )
                .overlay(alignment: placement.alignment) {
                    if captureEnabled && hudEnabled {
                        StreamHUDView(model: model, connection: conn, placement: placement)
                    }
                }
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
        // an unpinned host with no matching `pair=optional` advert routes to PIN pairing instead
        // of silently entering the trust prompt (rules 3b + 4). A pinned host ignores all of this.
        if host.pinnedSHA256 == nil {
            let tofuOK = allowTofu ?? discovery.hosts.contains {
                host.matches($0) && $0.allowsTofu
            }
            if !tofuOK {
                pairingTarget = host
                return
            }
        }
        // The gamepad-type setting resolves NOW (Automatic → match the active physical
        // controller): the host's virtual pad backend is fixed per session.
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
            launchID: launchID,
            allowTofu: host.pinnedSHA256 == nil)
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
    /// a `pair=required` host, or one with no/unknown `pair` field, goes straight to the PIN
    /// pairing ceremony (rule 3b). (A pinned discovered host connects silently inside `connect`.)
    private func connectDiscovered(_ d: DiscoveredHost) {
        guard !model.isBusy else { return }
        let host = StoredHost(name: d.name, address: d.host, port: d.port)
        store.add(host)
        if d.allowsTofu {
            connect(host, allowTofu: true)
        } else {
            pairingTarget = host
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
            autoTrust: true)
    }
}

#if os(macOS)
/// Drives the hosting window in/out of native fullscreen from SwiftUI state. Mounted invisibly in
/// the view tree; on each `active` change it captures the window and toggles fullscreen only when
/// the current state differs (so it never fights a toggle already in flight, and never touches a
/// window the user fullscreened manually unless `active` says otherwise).
private struct FullscreenController: NSViewRepresentable {
    let active: Bool

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        let want = active
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            let isFull = window.styleMask.contains(.fullScreen)
            if want != isFull { window.toggleFullScreen(nil) }
        }
    }
}
#endif
