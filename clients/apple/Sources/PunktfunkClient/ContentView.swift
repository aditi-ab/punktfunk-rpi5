// Hosts grid ⇄ trust prompt ⇄ live stream.
//
// Home is a grid of saved hosts (click to connect); "+" in the toolbar adds one; the
// stream mode lives in Settings (⌘,). Two ways to establish trust on first contact:
// the TOFU prompt (host fingerprint over the live-but-blurred stream, user compares it
// with the host's log) or the PIN pairing ceremony (right-click a card → "Pair with
// PIN…", or from the trust prompt itself) — pairing verifies both sides at once and is
// the only way into hosts running --require-pairing. Once pinned, reconnects are silent
// and a changed host identity refuses to connect.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI
#if os(tvOS)
import SwiftUINavigationTransitions
#endif

struct ContentView: View {
    @StateObject private var model = SessionModel()
    @StateObject private var store = HostStore()
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @AppStorage("punktfunk.compositor") private var compositor = 0
    @AppStorage("punktfunk.gamepadType") private var gamepadType = 0
    @State private var showAddHost = false
    @State private var pairingTarget: StoredHost?
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
        // On the outer Group so the sheet survives the trust-prompt → home transition
        // (the "Pair with PIN instead" path disconnects first — the host's accept loop
        // is sequential, a pairing connection would queue behind the live session).
        #if !os(tvOS)
        .sheet(item: $pairingTarget) { host in
            PairSheet(host: host) { fingerprint in
                // Backstop against a stale ceremony surfacing after dismissal (PairSheet
                // also self-discards those): only act while this host's sheet is up.
                guard pairingTarget?.id == host.id else { return }
                store.pin(host.id, fingerprint: fingerprint)
                var pinned = host
                pinned.pinnedSHA256 = fingerprint
                connect(pinned)
            }
        }
        #endif
    }

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
                trustCard(fp)
            }
        }
        #if os(macOS)
        .frame(minWidth: 640, minHeight: 360)
        .background(Color.black)
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
        #endif
    }

    // MARK: - Home (hosts grid)

    private var home: some View {
        NavigationStack {
            Group {
                if store.hosts.isEmpty {
                    emptyState
                } else {
                    ScrollView {
                        LazyVGrid(columns: gridColumns, spacing: gridSpacing) {
                            ForEach(store.hosts) { host in
                                hostCard(host)
                            }
                        }
                        .padding()
                        #if os(tvOS)
                        // Actions live below the hosts, not between them.
                        HStack(spacing: 32) {
                            Button {
                                showAddHost = true
                            } label: {
                                Label("Add Host", systemImage: "plus")
                            }
                            Button {
                                showSettings = true
                            } label: {
                                Label("Settings", systemImage: "gearshape")
                            }
                        }
                        .padding(.top, 24)
                        #endif
                    }
                }
            }
            .navigationTitle("Punktfunkempfänger")
            #if os(tvOS)
            // Pushed routes — the Settings-app navigation feel (push animation, Menu
            // pops) instead of modal overlays.
            .navigationDestination(isPresented: $showAddHost) {
                AddHostSheet { store.add($0) }
            }
            .navigationDestination(isPresented: $showSettings) {
                SettingsView()
            }
            .navigationDestination(item: $pairingTarget) { host in
                PairSheet(host: host) { fingerprint in
                    guard pairingTarget?.id == host.id else { return }
                    store.pin(host.id, fingerprint: fingerprint)
                    var pinned = host
                    pinned.pinnedSHA256 = fingerprint
                    connect(pinned)
                }
            }
            #endif
            #if !os(tvOS)
            .toolbar {
                #if os(iOS)
                // Adjacent trailing items share one glass pill (the system default).
                ToolbarItem(placement: .topBarTrailing) { settingsButton }
                ToolbarItem(placement: .topBarTrailing) { addHostButton }
                #else
                ToolbarItem(placement: .primaryAction) {
                    addHostButton
                        .help("Add a host")
                }
                ToolbarItem {
                    SettingsLink {
                        Label("Settings", systemImage: "gearshape")
                    }
                    .help("Stream mode and settings")
                }
                #endif
            }
            #endif
        }
        #if os(macOS)
        .frame(minWidth: 480, minHeight: 360)
        #endif
        #if os(tvOS)
        // The Settings-app slide for every push in this stack (top-level routes AND
        // the pickers' drill-ins) — SwiftUI's default on tvOS is a bare crossfade.
        // Spring-driven (UISpringTimingParameters): ~0.87 damping ratio — settles fast
        // with just a hint of life, no visible overshoot ping-pong.
        .customNavigationTransition(
            .slide.animation(.interpolatingSpring(stiffness: 300, damping: 30)))
        #endif
        #if !os(tvOS)
        .sheet(isPresented: $showAddHost) {
            AddHostSheet { store.add($0) }
        }
        #if os(iOS)
        .sheet(isPresented: $showSettings) {
            NavigationStack {
                SettingsView()
                    .navigationTitle("Settings")
                    .toolbar {
                        Button("Done") { showSettings = false }
                    }
            }
        }
        #endif
        #endif
        .alert(
            "Connection failed",
            isPresented: Binding(
                get: { model.errorMessage != nil },
                set: { if !$0 { model.errorMessage = nil } }
            )
        ) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(model.errorMessage ?? "")
        }
    }

    /// macOS caps card width (a huge window shouldn't yield huge cards); on iOS the
    /// columns FILL the width so the cards stay edge-aligned with the title and bars —
    /// sized touch-first: one column on iPhone portrait, 3–4 generous cards on iPad.
    private var gridColumns: [GridItem] {
        #if os(macOS)
        [GridItem(.adaptive(minimum: 180, maximum: 240), spacing: 16)]
        #elseif os(tvOS)
        [GridItem(.adaptive(minimum: 320), spacing: 48)]
        #else
        [GridItem(.adaptive(minimum: 280), spacing: 16)]
        #endif
    }

    private var gridSpacing: CGFloat {
        #if os(tvOS)
        48 // the focused card scales up — give it room instead of overlapping siblings
        #else
        16
        #endif
    }

    private var addHostButton: some View {
        Button {
            showAddHost = true
        } label: {
            Label("Add Host", systemImage: "plus")
        }
    }

    #if !os(macOS)
    private var settingsButton: some View {
        Button {
            showSettings = true
        } label: {
            Label("Settings", systemImage: "gearshape")
        }
    }
    #endif

    private var emptyState: some View {
        ContentUnavailableView {
            Label("No Hosts", systemImage: "rectangle.connected.to.line.below")
        } description: {
            Text("Add your punktfunk host with the + button.")
        } actions: {
            Button("Add Host") { showAddHost = true }
                .buttonStyle(.borderedProminent)
                #if os(iOS)
                .controlSize(.large)
                #endif
            #if os(tvOS)
            Button("Settings") { showSettings = true }
            #endif
        }
    }

    private func hostCard(_ host: StoredHost) -> some View {
        let isConnecting = model.phase == .connecting && model.activeHost?.id == host.id
        #if os(iOS)
        let iconSize: CGFloat = 56
        let iconBox: CGFloat = 76
        let cardPadding: CGFloat = 28
        let nameFont = Font.title3.weight(.semibold)
        #else
        let iconSize: CGFloat = 42
        let iconBox: CGFloat = 56
        let cardPadding: CGFloat = 18
        let nameFont = Font.headline
        #endif
        return Button {
            connect(host)
        } label: {
            VStack(spacing: 10) {
                ZStack {
                    Image(systemName: "play.display")
                        .font(.system(size: iconSize, weight: .light))
                        .foregroundStyle(.tint)
                        .opacity(isConnecting ? 0.3 : 1)
                    if isConnecting {
                        ProgressView()
                    }
                }
                .frame(height: iconBox)
                VStack(spacing: 2) {
                    Text(host.displayName)
                        .font(nameFont)
                        .lineLimit(1)
                    HStack(spacing: 4) {
                        if host.pinnedSHA256 != nil {
                            Image(systemName: "lock.fill")
                                .font(.system(size: 9))
                                .foregroundStyle(.secondary)
                        }
                        Text("\(host.address):\(String(host.port))")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    if let last = host.lastConnected {
                        Text("Connected \(last, format: .relative(presentation: .named))")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                    }
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, cardPadding)
            .padding(.horizontal, 12)
            #if !os(tvOS)
            // tvOS: the .card button style owns platter + focus motion — extra chrome
            // inside it mutes the grow/tilt. Material + accent ring are for pointer UIs.
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
            .overlay {
                if host.id == mostRecentHostID {
                    RoundedRectangle(cornerRadius: 14)
                        .strokeBorder(Color.accentColor.opacity(0.35), lineWidth: 1.5)
                }
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(model.isBusy)
        .contextMenu {
            Button("Pair with PIN…") {
                guard !model.isBusy else { return }
                pairingTarget = host
            }
            if host.pinnedSHA256 != nil {
                Button("Forget Identity") { store.forgetIdentity(host) }
            }
            Button("Remove", role: .destructive) { store.remove(host) }
        }
    }

    /// First run on iOS: default the stream mode to this device's native screen so the
    /// video fills the display instead of letterboxing 1920×1080 onto a 4:3 iPad. (The
    /// compiled-in AppStorage defaults only apply until any value is saved; macOS keeps
    /// 1080p — a desktop window is not the screen.)
    private func seedDefaultModeIfNeeded() {
        #if !os(macOS)
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: "punktfunk.width") == nil else { return }
        let bounds = UIScreen.main.nativeBounds // portrait-oriented pixels
        defaults.set(Int(max(bounds.width, bounds.height)), forKey: "punktfunk.width")
        defaults.set(Int(min(bounds.width, bounds.height)), forKey: "punktfunk.height")
        defaults.set(UIScreen.main.maximumFramesPerSecond, forKey: "punktfunk.hz")
        #endif
    }

    /// The host of the most recent session — its card carries the accent ring.
    private var mostRecentHostID: UUID? {
        store.hosts
            .compactMap { host in host.lastConnected.map { (host.id, $0) } }
            .max { $0.1 < $1.1 }?.0
    }

    private func connect(_ host: StoredHost) {
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
                    rawValue: UInt32(clamping: gamepadType)) ?? .auto))
    }

    // MARK: - Trust on first use

    private func trustCard(_ fingerprint: Data) -> some View {
        VStack(spacing: 14) {
            Image(systemName: "lock.shield")
                .font(.system(size: 36, weight: .light))
                .foregroundStyle(.tint)
            Text("Verify \(model.activeHost?.displayName ?? "host")")
                .font(.title3.weight(.semibold))
            Text("First connection. Compare this fingerprint with the one "
                + "punktfunk-host logged at startup (\u{201C}clients pin this "
                + "fingerprint\u{201D}):")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Text(Self.format(fingerprint: fingerprint))
                .font(.system(.callout, design: .monospaced))
                #if !os(tvOS)
                .textSelection(.enabled)
                #endif
                .padding(10)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            HStack(spacing: 12) {
                Button("Cancel", role: .cancel) { model.rejectTrust() }
                    #if !os(tvOS)
                    .keyboardShortcut(.cancelAction)
                    #endif
                Button("Trust & Connect") {
                    if let fp = model.confirmTrust(), let host = model.activeHost {
                        store.pin(host.id, fingerprint: fp)
                    }
                }
                .buttonStyle(.borderedProminent)
                #if !os(tvOS)
                .keyboardShortcut(.defaultAction)
                #endif
            }
            #if os(iOS)
            .controlSize(.large)
            #endif
            // The verified alternative to eyeballing hex: drop this session (the host
            // serves one connection at a time) and run the SPAKE2 PIN ceremony instead.
            Button("Pair with PIN instead…") {
                let host = model.activeHost
                model.rejectTrust()
                pairingTarget = host
            }
            #if os(macOS)
            .buttonStyle(.link)
            #else
            .buttonStyle(.borderless)
            #endif
            .font(.callout)
        }
        .padding(28)
        .frame(maxWidth: 440)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 18))
    }

    /// 64 hex chars → four groups per line, two lines — easy to eyeball against the log.
    private static func format(fingerprint: Data) -> String {
        let hex = fingerprint.map { String(format: "%02x", $0) }.joined()
        let groups = stride(from: 0, to: hex.count, by: 8).map { i -> String in
            let start = hex.index(hex.startIndex, offsetBy: i)
            let end = hex.index(start, offsetBy: min(8, hex.count - i))
            return String(hex[start..<end])
        }
        return groups.chunks(of: 4).map { $0.joined(separator: " ") }.joined(separator: "\n")
    }

    // MARK: - Stream

    private func stream(captureEnabled: Bool) -> some View {
        Group {
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
                    }
                )
                .overlay(alignment: .topTrailing) {
                    if captureEnabled { hud(conn) }
                }
            }
        }
    }

    private func hud(_ conn: PunktfunkConnection) -> some View {
        VStack(alignment: .trailing, spacing: 4) {
            HStack(spacing: 6) {
                Circle()
                    .fill(Color.accentColor)
                    .frame(width: 7, height: 7)
                Text("\(conn.width)×\(conn.height)@\(conn.refreshHz)  \(model.fps) fps  \(model.mbps, specifier: "%.1f") Mb/s")
                    .font(.system(.caption, design: .monospaced))
            }
            if model.latencyValid {
                // Capture→client-receipt (skew-corrected); excludes the layer's decode+present —
                // see LatencyMeter. "(same-host)" when the host didn't answer the skew handshake.
                Text("capture→client \(model.latencyP50Ms, specifier: "%.1f")/\(model.latencyP95Ms, specifier: "%.1f") ms p50/p95"
                    + (model.latencySkewCorrected ? "" : " (same-host)"))
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            // While captured the cursor is hidden+frozen, so the button is keyboard-only
            // (⌘⎋ or Cmd+Tab release the cursor; released, it's clickable again).
            #if os(macOS)
            Text(model.mouseCaptured
                ? "⌘⎋ releases the mouse"
                : "Click the stream to capture input")
                .font(.caption2)
                .foregroundStyle(.secondary)
            #elseif os(iOS)
            // Touch always plays directly; ⌘⎋ (hardware keyboard) toggles kb/mouse.
            Text(model.mouseCaptured
                ? "⌘⎋ releases keyboard & mouse"
                : "⌘⎋ captures keyboard & mouse")
                .font(.caption2)
                .foregroundStyle(.secondary)
            #endif
            Button("Disconnect (⌘D)") { model.disconnect() }
                .font(.caption)
                #if !os(tvOS)
                .keyboardShortcut("d", modifiers: .command)
                #endif
        }
        .padding(10)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        .padding(10)
    }

    // MARK: - Dev hook

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
        model.connect(
            to: host,
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz),
            compositor: pref,
            gamepad: pad,
            autoTrust: true)
    }
}

private extension Array {
    func chunks(of size: Int) -> [[Element]] {
        stride(from: 0, to: count, by: size).map { Array(self[$0..<Swift.min($0 + size, count)]) }
    }
}
