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

struct ContentView: View {
    @StateObject private var model = SessionModel()
    @StateObject private var store = HostStore()
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @AppStorage("punktfunk.compositor") private var compositor = 0
    @State private var showAddHost = false
    @State private var pairingTarget: StoredHost?
    #if os(iOS)
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
        .onAppear { autoConnectIfAsked() }
        .onDisappear { model.disconnect() } // window closed mid-session (Cmd+N spawns more)
        // On the outer Group so the sheet survives the trust-prompt → home transition
        // (the "Pair with PIN instead" path disconnects first — the host's accept loop
        // is sequential, a pairing connection would queue behind the live session).
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
        #endif
        .background(Color.black)
    }

    // MARK: - Home (hosts grid)

    private var home: some View {
        NavigationStack {
            Group {
                if store.hosts.isEmpty {
                    emptyState
                } else {
                    ScrollView {
                        LazyVGrid(
                            columns: [GridItem(.adaptive(minimum: 180, maximum: 240), spacing: 16)],
                            spacing: 16
                        ) {
                            ForEach(store.hosts) { host in
                                hostCard(host)
                            }
                        }
                        .padding(20)
                    }
                }
            }
            .navigationTitle("punktfunk")
            #if os(iOS)
            // Liquid-glass header: the large title shares the bar row with the action
            // circles instead of stacking under them.
            .toolbarTitleDisplayMode(.inlineLarge)
            #endif
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
        }
        #if os(macOS)
        .frame(minWidth: 480, minHeight: 360)
        #endif
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

    private var addHostButton: some View {
        Button {
            showAddHost = true
        } label: {
            Label("Add Host", systemImage: "plus")
        }
    }

    #if os(iOS)
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
        }
    }

    private func hostCard(_ host: StoredHost) -> some View {
        let isConnecting = model.phase == .connecting && model.activeHost?.id == host.id
        return Button {
            connect(host)
        } label: {
            VStack(spacing: 10) {
                ZStack {
                    Image(systemName: "play.display")
                        .font(.system(size: 42, weight: .light))
                        .foregroundStyle(.tint)
                        .opacity(isConnecting ? 0.3 : 1)
                    if isConnecting {
                        ProgressView()
                    }
                }
                .frame(height: 56)
                VStack(spacing: 2) {
                    Text(host.displayName)
                        .font(.headline)
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
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 18)
            .padding(.horizontal, 12)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
        }
        .buttonStyle(.plain)
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

    private func connect(_ host: StoredHost) {
        model.connect(
            to: host,
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz),
            compositor: PunktfunkConnection.Compositor(
                rawValue: UInt32(clamping: compositor)) ?? .auto)
    }

    // MARK: - Trust on first use

    private func trustCard(_ fingerprint: Data) -> some View {
        VStack(spacing: 14) {
            Image(systemName: "lock.shield")
                .font(.system(size: 36, weight: .light))
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
                .textSelection(.enabled)
                .padding(10)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            HStack(spacing: 12) {
                Button("Cancel", role: .cancel) { model.rejectTrust() }
                    .keyboardShortcut(.cancelAction)
                Button("Trust & Connect") {
                    if let fp = model.confirmTrust(), let host = model.activeHost {
                        store.pin(host.id, fingerprint: fp)
                    }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
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
                    onFrame: { [meter = model.meter] au in meter.note(byteCount: au.data.count) },
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
            Text("\(conn.width)×\(conn.height)@\(conn.refreshHz)  \(model.fps) fps  \(model.mbps, specifier: "%.1f") Mb/s")
                .font(.system(.caption, design: .monospaced))
            // While captured the cursor is hidden+frozen, so the button is keyboard-only
            // (⌘⎋ or Cmd+Tab release the cursor; released, it's clickable again).
            #if os(macOS)
            Text(model.mouseCaptured
                ? "⌘⎋ releases the mouse"
                : "Click the stream to capture input")
                .font(.caption2)
                .opacity(0.8)
            #else
            // Touch always plays directly; ⌘⎋ (hardware keyboard) toggles kb/mouse.
            Text(model.mouseCaptured
                ? "⌘⎋ releases keyboard & mouse"
                : "⌘⎋ captures keyboard & mouse")
                .font(.caption2)
                .opacity(0.8)
            #endif
            Button("Disconnect (⌘D)") { model.disconnect() }
                .font(.caption)
                .keyboardShortcut("d", modifiers: .command)
        }
        .padding(8)
        .background(.black.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
        .foregroundStyle(.white)
        .padding(10)
    }

    // MARK: - Dev hook

    /// PUNKTFUNK_AUTOCONNECT=host[:port] connects immediately (trust-on-first-use,
    /// auto-confirmed — dev only) at the saved or PUNKTFUNK_MODE=WxHxHz mode, without
    /// touching the saved host list. PUNKTFUNK_COMPOSITOR=kwin|gamescope|… overrides the
    /// compositor preference (same names as the host env knob). (IPv4/hostname only.)
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
        model.connect(
            to: host,
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz),
            compositor: pref,
            autoTrust: true)
    }
}

private extension Array {
    func chunks(of size: Int) -> [[Element]] {
        stride(from: 0, to: count, by: size).map { Array(self[$0..<Swift.min($0 + size, count)]) }
    }
}
