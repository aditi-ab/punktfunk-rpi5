// The streaming overlay HUD: mode + fps/throughput, the capture→client (and, under the stage-2
// presenter, capture→present) latency lines, the platform input hint, and disconnect.

import PunktfunkKit
import SwiftUI

struct StreamHUDView: View {
    @ObservedObject var model: SessionModel
    let connection: PunktfunkConnection

    var body: some View {
        VStack(alignment: .trailing, spacing: 4) {
            HStack(spacing: 6) {
                Circle()
                    .fill(Color.accentColor)
                    .frame(width: 7, height: 7)
                Text("\(connection.width)×\(connection.height)@\(connection.refreshHz)  \(model.fps) fps  \(model.mbps, specifier: "%.1f") Mb/s")
                    .font(.system(.caption, design: .monospaced))
            }
            if model.latencyValid {
                // Capture→client-receipt (skew-corrected); excludes the layer's decode+present —
                // see LatencyMeter. "(same-host)" when the host didn't answer the skew handshake.
                Text("capture→client \(model.latencyP50Ms, specifier: "%.1f")/\(model.latencyP95Ms, specifier: "%.1f") ms p50/p95\(model.latencySkewCorrected ? "" : " (same-host)")")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            if model.presentLatencyValid {
                // Capture→present (glass-to-glass, modulo host render→capture) — stage-2 presenter
                // only; stage-1's layer presents internally with no per-frame stamp.
                Text("capture→present \(model.presentLatencyP50Ms, specifier: "%.1f")/\(model.presentLatencyP95Ms, specifier: "%.1f") ms p50/p95\(model.presentLatencySkewCorrected ? "" : " (same-host)")")
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
            #if os(tvOS)
            // No focusable control during play: a focusable button steals the controller's
            // A press (the focus engine consumes it before the host sees it). Disconnect is
            // the Siri Remote's Menu button (.onExitCommand on the stream) — just hint it.
            Text("Press Menu to disconnect")
                .font(.caption)
                .foregroundStyle(.secondary)
            #else
            Button("Disconnect (⌘D)") { model.disconnect() }
                .font(.caption)
                .keyboardShortcut("d", modifiers: .command)
            #endif
        }
        .padding(10)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
        .padding(10)
    }
}
