// Hosts grid ⇄ trust prompt ⇄ live stream.
//
// Home is a grid of saved hosts (click to connect); "+" in the toolbar adds one; the
// stream mode lives in Settings (⌘,). First connect to a host shows its certificate
// fingerprint over the live-but-blurred stream for explicit trust-on-first-use; once
// pinned, reconnects are silent and a changed host identity refuses to connect.

import AppKit
import PunktfunkKit
import SwiftUI

struct ContentView: View {
    @StateObject private var model = SessionModel()
    @StateObject private var store = HostStore()
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @State private var showAddHost = false

    var body: some View {
        Group {
            switch model.phase {
            case .idle, .connecting:
                home
            case .awaitingTrust(let fingerprint):
                trustPrompt(fingerprint)
            case .streaming:
                stream(capturesCursor: true)
            }
        }
        .onAppear { autoConnectIfAsked() }
        .onDisappear { model.disconnect() } // window closed mid-session (Cmd+N spawns more)
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
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Button {
                        showAddHost = true
                    } label: {
                        Label("Add Host", systemImage: "plus")
                    }
                    .help("Add a host")
                }
                ToolbarItem {
                    SettingsLink {
                        Label("Settings", systemImage: "gearshape")
                    }
                    .help("Stream mode and settings")
                }
            }
        }
        .frame(minWidth: 480, minHeight: 360)
        .sheet(isPresented: $showAddHost) {
            AddHostSheet { store.add($0) }
        }
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

    private var emptyState: some View {
        ContentUnavailableView {
            Label("No Hosts", systemImage: "rectangle.connected.to.line.below")
        } description: {
            Text("Add your punktfunk host with the + button.")
        } actions: {
            Button("Add Host") { showAddHost = true }
                .buttonStyle(.borderedProminent)
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
            hz: UInt32(clamping: hz))
    }

    // MARK: - Trust on first use

    private func trustPrompt(_ fingerprint: Data) -> some View {
        ZStack {
            // Keep the stream pumping (the opening IDR must be consumed) but blurred and
            // cursor-free until the host is verified.
            stream(capturesCursor: false)
                .blur(radius: 32)
                .overlay(.black.opacity(0.45))
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
            }
            .padding(28)
            .frame(maxWidth: 440)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 18))
        }
        .frame(minWidth: 640, minHeight: 360)
        .background(Color.black)
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

    private func stream(capturesCursor: Bool) -> some View {
        Group {
            if let conn = model.connection {
                StreamView(
                    connection: conn,
                    capturesCursor: capturesCursor,
                    onFrame: { [meter = model.meter] au in meter.note(byteCount: au.data.count) },
                    onSessionEnd: { [weak model] in
                        Task { @MainActor in model?.sessionEnded() }
                    }
                )
                .overlay(alignment: .topTrailing) {
                    if capturesCursor { hud(conn) }
                }
                .frame(minWidth: 640, minHeight: 360)
                .background(Color.black)
            }
        }
    }

    private func hud(_ conn: PunktfunkConnection) -> some View {
        VStack(alignment: .trailing, spacing: 4) {
            Text("\(conn.width)×\(conn.height)@\(conn.refreshHz)  \(model.fps) fps  \(model.mbps, specifier: "%.1f") Mb/s")
                .font(.system(.caption, design: .monospaced))
            // ⌘D because the local cursor is hidden+frozen while streaming — the button
            // can't be clicked. (Cmd+Tab away also frees the cursor.)
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
    /// touching the saved host list. (IPv4/hostname only.)
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
        model.connect(
            to: host,
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz),
            autoTrust: true)
    }
}

private extension Array {
    func chunks(of size: Int) -> [[Element]] {
        stride(from: 0, to: count, by: size).map { Array(self[$0..<Swift.min($0 + size, count)]) }
    }
}
