// Network speed-test sheet (roadmap §9): connect to the host, ask it to burst probe
// filler over the real data plane (FEC-encoded UDP, video paused — the measurement IS the
// streaming path), poll the measurement, and recommend a bitrate (~70% of the measured
// goodput, headroom for encoder burstiness). "Use N Mbps" writes the bitrate setting; it
// applies from the next session.
//
// Runs only while idle (the host serves one session at a time, so it can't share the wire
// with a live stream — the host-card grid is the idle UI anyway). Trust: a pinned host is
// verified as usual; an unpinned one is probed trust-on-first-use WITHOUT persisting
// anything — a bandwidth number doesn't justify a trust decision.

import Foundation
import PunktfunkKit
import SwiftUI

/// Dismissal must abandon the in-flight probe: the connect/poll loop runs detached and
/// checks this flag, closing the connection itself. Only the flag is shared; it is safe
/// to read/write from the loop and the main actor (single Bool, torn reads harmless).
private final class ProbeToken: @unchecked Sendable {
    var cancelled = false
}

/// What the host is asked to burst: the host's full probe ceiling (it clamps to ≤ 3 Gbps),
/// so the measurement surfaces the link's real ceiling instead of an artificial cap —
/// bursting ABOVE what the link can carry is how the probe finds where delivery falls off.
/// Two seconds rides out scheduler jitter. File-scope so the detached probe task reads them
/// without crossing into the view's main actor.
private let probeTargetKbps: UInt32 = 3_000_000
private let probeDurationMs: UInt32 = 2_000

struct SpeedTestSheet: View {
    @Environment(\.dismiss) private var dismiss
    let host: StoredHost

    @AppStorage("punktfunk.width") private var width = 1920
    @AppStorage("punktfunk.height") private var height = 1080
    @AppStorage("punktfunk.hz") private var hz = 60
    @AppStorage("punktfunk.bitrateKbps") private var bitrateKbps = 0

    private enum Phase: Equatable {
        case connecting
        case probing(partial: PunktfunkConnection.ProbeResult?)
        case done(PunktfunkConnection.ProbeResult)
        case failed(String)
    }

    @State private var phase: Phase = .connecting
    @State private var token = ProbeToken()

    var body: some View {
        VStack(spacing: 20) {
            Label("Speed test — \(host.displayName)", systemImage: "gauge.with.needle")
                .font(.headline)
                .foregroundStyle(.tint)

            switch phase {
            case .connecting:
                ProgressView("Connecting…")
                    .padding(.vertical, 12)
            case .probing(let partial):
                VStack(spacing: 8) {
                    ProgressView("Measuring — the host is bursting probe data…")
                    if let partial, partial.throughputKbps > 0 {
                        Text("~\(Self.mbpsLabel(kbps: Int(partial.throughputKbps))) so far")
                            .font(.callout.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                }
                .padding(.vertical, 12)
            case .done(let result):
                resultView(result)
            case .failed(let message):
                Text(message)
                    .font(.callout)
                    .foregroundStyle(.red)
                    .multilineTextAlignment(.center)
            }

            HStack(spacing: 24) {
                Button(phaseIsFinal ? "Close" : "Cancel", role: .cancel) {
                    token.cancelled = true
                    dismiss()
                }
                #if !os(tvOS)
                .keyboardShortcut(.cancelAction)
                #endif
                if case .done(let result) = phase, let rec = Self.recommendedKbps(result) {
                    Button("Use \(Self.mbpsLabel(kbps: rec))") {
                        bitrateKbps = rec
                        dismiss()
                    }
                    .buttonStyle(.borderedProminent)
                    #if !os(tvOS)
                    .keyboardShortcut(.defaultAction)
                    #endif
                }
                if case .failed = phase {
                    Button("Retry") { run() }
                        .buttonStyle(.borderedProminent)
                }
            }
        }
        #if os(tvOS)
        .frame(maxWidth: 1000)
        .padding(60)
        #else
        .padding(24)
        #endif
        #if os(macOS)
        .frame(width: 420)
        .fixedSize(horizontal: false, vertical: true)
        #endif
        .onAppear { run() }
        .onDisappear { token.cancelled = true }
    }

    private var phaseIsFinal: Bool {
        switch phase {
        case .done, .failed: return true
        case .connecting, .probing: return false
        }
    }

    private func resultView(_ result: PunktfunkConnection.ProbeResult) -> some View {
        VStack(spacing: 10) {
            Text(Self.mbpsLabel(kbps: Int(result.throughputKbps)))
                .font(.system(.largeTitle, design: .rounded).weight(.semibold))
                .monospacedDigit()
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 4) {
                GridRow {
                    Text("Loss").foregroundStyle(.secondary)
                    Text(String(format: "%.1f %%", result.lossPct)).monospacedDigit()
                }
                GridRow {
                    Text("Received").foregroundStyle(.secondary)
                    Text("\(ByteCountFormatter.string(fromByteCount: Int64(result.recvBytes), countStyle: .binary)) in \(result.elapsedMs) ms")
                        .monospacedDigit()
                }
            }
            .font(.callout)
            if let rec = Self.recommendedKbps(result) {
                Text("Recommended bitrate: \(Self.mbpsLabel(kbps: rec)) "
                    + "(~70% of measured, headroom for encoder bursts).")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            } else {
                Text("Too little data made it through to recommend a bitrate — "
                    + "check the network and retry.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
            }
        }
    }

    /// ~70% of the measured goodput, whole Mbps, clamped to the host's session bitrate
    /// ceiling (2 Gbps — it clamps any session request above that, so recommending more is
    /// pointless). nil when the measurement carried too little signal to recommend anything.
    static func recommendedKbps(_ result: PunktfunkConnection.ProbeResult) -> Int? {
        guard result.throughputKbps >= 2_000 else { return nil }
        let raw = Int(result.throughputKbps) * 7 / 10
        let wholeMbps = max(raw / 1_000, 2)
        return min(wholeMbps, 2_000) * 1_000
    }

    static func mbpsLabel(kbps: Int) -> String {
        if kbps >= 1_000_000 {
            let gbps = Double(kbps) / 1_000_000
            return gbps == gbps.rounded()
                ? "\(Int(gbps)) Gbps"
                : String(format: "%.1f Gbps", gbps)
        }
        return kbps % 1_000 == 0
            ? "\(kbps / 1_000) Mbps"
            : String(format: "%.1f Mbps", Double(kbps) / 1_000)
    }

    private func run() {
        phase = .connecting
        let token = token
        let address = host.address
        let port = host.port
        let pin = host.pinnedSHA256
        let (w, h, fps) = (UInt32(clamping: width), UInt32(clamping: height), UInt32(clamping: hz))
        Task.detached(priority: .userInitiated) {
            // Connect (blocking) — same identity/trust as a session, but TOFU results are
            // NOT persisted from here.
            let identity = (try? ClientIdentityStore.shared.load())?.identity
            let conn: PunktfunkConnection
            do {
                conn = try PunktfunkConnection(
                    host: address, port: port, width: w, height: h, refreshHz: fps,
                    pinSHA256: pin, identity: identity)
            } catch {
                await MainActor.run {
                    guard !token.cancelled else { return }
                    phase = .failed(
                        "Could not connect to \(address):\(port) — is punktfunk-host "
                        + "running and not mid-session?")
                }
                return
            }
            defer { conn.close() }

            conn.startSpeedTest(targetKbps: probeTargetKbps, durationMs: probeDurationMs)
            await MainActor.run { if !token.cancelled { phase = .probing(partial: nil) } }

            // Poll until the host's end-of-burst report lands (or a generous deadline —
            // the host clamps the burst to ≤ 5 s).
            let deadline = Date().addingTimeInterval(Double(probeDurationMs) / 1000 + 8)
            var final: PunktfunkConnection.ProbeResult?
            while !token.cancelled, Date() < deadline {
                try? await Task.sleep(nanoseconds: 200_000_000)
                guard let r = conn.probeResult() else { break } // closed underneath us
                if r.done {
                    final = r
                    break
                }
                await MainActor.run {
                    if !token.cancelled { phase = .probing(partial: r) }
                }
            }
            let result = final
            await MainActor.run {
                guard !token.cancelled else { return }
                if let result {
                    phase = .done(result)
                } else {
                    phase = .failed(
                        "The measurement never completed — the connection may have "
                        + "dropped mid-probe. Retry?")
                }
            }
        }
    }
}
