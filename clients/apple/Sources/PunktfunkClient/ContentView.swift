// Connect form ⇄ live stream. Stage-1 UX: pick host + mode, see frames, type/aim.

import AppKit
import PunktfunkKit
import SwiftUI

struct ContentView: View {
    @StateObject private var model = SessionModel()
    @AppStorage("punktfunk.host") private var host = "192.168.1.70"
    @AppStorage("punktfunk.port") private var port = 9777
    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60

    var body: some View {
        Group {
            if let conn = model.connection {
                stream(conn)
            } else {
                connectForm
            }
        }
        .onAppear { autoConnectIfAsked() }
        .onDisappear { model.disconnect() } // window closed mid-session (Cmd+N spawns more)
    }

    /// Development hook: PUNKTFUNK_AUTOCONNECT=host[:port] connects immediately at the saved
    /// (or PUNKTFUNK_MODE=WxHxHz) mode — lets scripts drive first-light runs. (IPv4/hostname
    /// only; an IPv6 literal would need bracket parsing.)
    private func autoConnectIfAsked() {
        guard let target = ProcessInfo.processInfo.environment["PUNKTFUNK_AUTOCONNECT"],
              !target.isEmpty, model.connection == nil, !model.connecting
        else { return }
        let parts = target.split(separator: ":")
        host = String(parts[0])
        if parts.count == 2, let p = Int(parts[1]) { port = p }
        if let mode = ProcessInfo.processInfo.environment["PUNKTFUNK_MODE"] {
            let dims = mode.split(separator: "x").compactMap { Int($0) }
            if dims.count == 3 {
                width = dims[0]
                height = dims[1]
                hz = dims[2]
            }
        }
        model.connect(
            host: host, port: UInt16(clamping: port),
            width: UInt32(clamping: width), height: UInt32(clamping: height),
            hz: UInt32(clamping: hz))
    }

    private func stream(_ conn: PunktfunkConnection) -> some View {
        StreamView(
            connection: conn,
            onFrame: { [meter = model.meter] au in meter.note(byteCount: au.data.count) },
            onSessionEnd: { [weak model] in
                Task { @MainActor in model?.sessionEnded() }
            }
        )
        .overlay(alignment: .topTrailing) { hud(conn) }
        .frame(minWidth: 640, minHeight: 360)
        .background(Color.black)
    }

    private func hud(_ conn: PunktfunkConnection) -> some View {
        VStack(alignment: .trailing, spacing: 4) {
            Text("\(conn.width)×\(conn.height)@\(conn.refreshHz)  \(model.fps) fps  \(model.mbps, specifier: "%.1f") Mb/s")
                .font(.system(.caption, design: .monospaced))
            Button("Disconnect") { model.disconnect() }
                .font(.caption)
        }
        .padding(8)
        .background(.black.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
        .foregroundStyle(.white)
        .padding(10)
    }

    private var connectForm: some View {
        VStack(spacing: 14) {
            Text("punktfunk").font(.largeTitle.weight(.semibold))
            Form {
                TextField("Host", text: $host)
                TextField("Port", value: $port, format: .number.grouping(.never))
                HStack {
                    TextField("Width", value: $width, format: .number.grouping(.never))
                    Text("×")
                    TextField("Height", value: $height, format: .number.grouping(.never))
                    Text("@")
                    TextField("Hz", value: $hz, format: .number.grouping(.never))
                }
                Button("Use this display's mode") { fillFromMainScreen() }
                    .buttonStyle(.link)
            }
            .frame(width: 340)

            if let error = model.errorMessage {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .frame(width: 340)
            }

            Button(model.connecting ? "Connecting…" : "Connect") {
                model.connect(
                    host: host, port: UInt16(clamping: port),
                    width: UInt32(clamping: width), height: UInt32(clamping: height),
                    hz: UInt32(clamping: hz))
            }
            .keyboardShortcut(.defaultAction)
            .disabled(model.connecting || host.isEmpty)
        }
        .padding(28)
        .frame(minWidth: 420, minHeight: 320)
    }

    private func fillFromMainScreen() {
        guard let screen = NSScreen.main else { return }
        let scale = screen.backingScaleFactor
        width = Int(screen.frame.width * scale)
        height = Int(screen.frame.height * scale)
        hz = screen.maximumFramesPerSecond
    }
}
