// The streaming overlay HUD: mode + fps/throughput, the unified latency lines
// (design/stats-unification.md — end-to-end headline + the stage equation under stage-2, the
// capture→received headline under the stage-1 fallback), the loss counter, the platform input
// hint, and disconnect.

import PunktfunkKit
import SwiftUI

struct StreamHUDView: View {
    @ObservedObject var model: SessionModel
    let connection: PunktfunkConnection
    var placement: HUDPlacement = .topTrailing

    var body: some View {
        VStack(alignment: placement.isTrailing ? .trailing : .leading, spacing: 4) {
            HStack(spacing: 6) {
                Circle()
                    .fill(Color.accentColor)
                    .frame(width: 7, height: 7)
                Text("\(connection.width)×\(connection.height)@\(connection.refreshHz)  \(model.fps) fps  \(model.mbps, specifier: "%.1f") Mb/s")
                    .font(.system(.caption, design: .monospaced))
            }
            if model.endToEndValid {
                // Stage-2: the end-to-end headline (capture→on-glass, measured directly, skew-
                // corrected) — "(same-host clock)" when the host didn't answer the skew handshake.
                Text("end-to-end \(model.endToEndP50Ms, specifier: "%.1f") ms p50 · \(model.endToEndP95Ms, specifier: "%.1f") p95 · capture→on-glass\(model.endToEndSkewCorrected ? "" : " (same-host clock)")")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                // The equation: the stages tiling the headline interval (per-window p50s —
                // they only approximately sum to the directly-measured total). With a host
                // that reports per-AU timings (0xCF) the first term splits into host + network
                // (phase 2); an old host keeps the combined term.
                if model.hostNetworkValid && model.decodeValid && model.displayValid {
                    if model.splitValid {
                        Text("= host \(model.hostP50Ms, specifier: "%.1f") + network \(model.networkP50Ms, specifier: "%.1f") + decode \(model.decodeP50Ms, specifier: "%.1f") + display \(model.displayP50Ms, specifier: "%.1f")")
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                    } else {
                        Text("= host+network \(model.hostNetworkP50Ms, specifier: "%.1f") + decode \(model.decodeP50Ms, specifier: "%.1f") + display \(model.displayP50Ms, specifier: "%.1f")")
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                }
            } else if model.hostNetworkValid {
                // Stage-1 fallback presenter: the layer decodes + presents internally with no
                // per-frame stamp, so the honest headline ends at receipt. The host/network
                // split still applies there (receipt is presenter-independent) — it becomes the
                // only equation line; without it, host+network IS the whole measured interval.
                Text("capture→received \(model.hostNetworkP50Ms, specifier: "%.1f") ms p50 · \(model.hostNetworkP95Ms, specifier: "%.1f") p95\(model.hostNetworkSkewCorrected ? "" : " (same-host clock)")")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                if model.splitValid {
                    Text("= host \(model.hostP50Ms, specifier: "%.1f") + network \(model.networkP50Ms, specifier: "%.1f")")
                        .font(.system(.caption2, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
            }
            if model.lostFrames > 0 {
                // Unrecoverable network drops this window; hidden while the link is clean.
                // String(format:) rather than specifier interpolation: the literal % would
                // otherwise land in the LocalizedStringKey's format string as a bogus conversion.
                Text(String(format: "lost %d (%.1f%%)", model.lostFrames, model.lostPct))
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            // While captured the cursor is hidden+frozen, so the button is keyboard-only
            // (⌘⎋ or Cmd+Tab release the cursor; released, it's clickable again).
            #if os(macOS)
            Text(model.mouseCaptured
                ? "⌘⎋ releases the mouse"
                : "Click the stream to capture input")
                .font(.geist(11, relativeTo: .caption2))
                .foregroundStyle(.secondary)
            #elseif os(iOS)
            // Touch always plays directly; ⌘⎋ (hardware keyboard) toggles kb/mouse.
            Text(model.mouseCaptured
                ? "⌘⎋ releases keyboard & mouse"
                : "⌘⎋ captures keyboard & mouse")
                .font(.geist(11, relativeTo: .caption2))
                .foregroundStyle(.secondary)
            #endif
            #if os(tvOS)
            // No focusable control during play: a focusable button steals the controller's
            // A press (the focus engine consumes it before the host sees it). Disconnect is
            // the Siri Remote's Menu button (.onExitCommand on the stream) — just hint it.
            Text("Press Menu to disconnect")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
            #else
            // ⌘D lives on the app's Stream menu (so it still works when the HUD is hidden);
            // this button is the in-overlay, click-to-disconnect affordance.
            Button("Disconnect (⌘D)") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #endif
        }
        .padding(10)
        // Floating HUD over live video — the canonical Liquid-Glass overlay surface (26+);
        // falls back to .regularMaterial below 26 (see GlassStyle).
        .glassBackground(RoundedRectangle(cornerRadius: 10))
        .padding(10)
    }
}
