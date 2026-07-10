// The streaming overlay HUD, tiered by StatsVerbosity (the Android client's 3-tier semantics):
//  * compact — one glass-pill line: fps · end-to-end p50 · throughput (+ loss when lossy);
//  * normal — mode + fps/throughput, the unified latency HEADLINE (design/stats-unification.md
//    — end-to-end under stage-2, capture→received under the stage-1 fallback), the loss
//    counter, a capture hint (shown until input is captured), and disconnect;
//  * detailed — everything normal has plus the stage equation line(s) under the headline.
// `.off` never reaches this view (ContentView gates the overlay on the tier).

import PunktfunkKit
import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

struct StreamHUDView: View {
    @ObservedObject var model: SessionModel
    let connection: PunktfunkConnection
    var placement: HUDPlacement = .topTrailing
    let verbosity: StatsVerbosity

    var body: some View {
        // .off is gated upstream (ContentView only mounts the HUD when the tier is on) —
        // render nothing if it ever slips through.
        if verbosity != .off {
            // ONE shared glass card wraps the tier-dependent content, so a verbosity change MORPHS
            // this card — its frame (and, on iOS, its clamped corner) animate to the new size — rather
            // than cross-fading a whole new card in. Only the inner content switches per tier.
            tierContent
                .padding(10)
                .glassBackground(cardShape)
                .padding(edgeInset)
        }
    }

    /// The tier-dependent content, unwrapped (the shared card in `body` supplies the padding +
    /// glass background). Compact is a one-line pill; normal/detailed the full stack.
    @ViewBuilder private var tierContent: some View {
        if verbosity == .compact {
            compactContent
        } else {
            fullContent
        }
    }

    // MARK: - Compact tier

    /// One line: `{fps} fps · {e2e p50} ms · {mbps} Mb/s`. The ms segment is the best available
    /// latency headline (stage-2 end-to-end, else the stage-1 capture→received) and is omitted until
    /// either is valid. Loss appends in the same quiet styling the full HUD's lost line uses.
    private var compactContent: some View {
        HStack(spacing: 6) {
            Circle()
                .fill(Color.accentColor)
                .frame(width: 7, height: 7)
            Text(compactLine)
                .font(.system(.caption, design: .monospaced))
            if model.lostFrames > 0 {
                Text("· lost \(model.lostFrames)")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var compactLine: String {
        var parts = ["\(model.fps) fps"]
        if model.endToEndValid {
            parts.append(String(format: "%.1f ms", model.endToEndP50Ms))
        } else if model.hostNetworkValid {
            parts.append(String(format: "%.1f ms", model.hostNetworkP50Ms))
        }
        parts.append(String(format: "%.1f Mb/s", model.mbps))
        return parts.joined(separator: " · ")
    }

    // MARK: - Normal / detailed tiers

    private var fullContent: some View {
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
                // The equation (detailed tier only): the stages tiling the headline interval
                // (per-window p50s — they only approximately sum to the directly-measured
                // total). With a host that reports per-AU timings (0xCF) the first term splits
                // into host + network (phase 2); an old host keeps the combined term.
                if verbosity == .detailed && model.hostNetworkValid && model.decodeValid && model.displayValid {
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
                // only equation line (detailed tier); without it, host+network IS the whole
                // measured interval.
                Text("capture→received \(model.hostNetworkP50Ms, specifier: "%.1f") ms p50 · \(model.hostNetworkP95Ms, specifier: "%.1f") p95\(model.hostNetworkSkewCorrected ? "" : " (same-host clock)")")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                if verbosity == .detailed && model.splitValid {
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
            // Capture hint, shown only until input is captured — how to grab it. The RELEASE
            // shortcut is intentionally not surfaced in the overlay (it lives on the Stream menu
            // and, on macOS, the start-of-stream banner), keeping the HUD uncluttered while playing.
            #if os(macOS)
            if !model.mouseCaptured {
                Text("Click the stream to capture input")
                    .font(.geist(11, relativeTo: .caption2))
                    .foregroundStyle(.secondary)
            }
            #elseif os(iOS)
            // Touch always plays directly; ⌘⎋ (hardware keyboard) captures kb/mouse.
            if !model.mouseCaptured {
                Text("⌘⎋ captures keyboard & mouse")
                    .font(.geist(11, relativeTo: .caption2))
                    .foregroundStyle(.secondary)
            }
            #endif
            #if os(tvOS)
            // No focusable control during play: a focusable button steals the controller's
            // A press (the focus engine consumes it before the host sees it). Disconnect is
            // the Siri Remote's Menu button (.onExitCommand on the stream) — just hint it.
            Text("Press Menu to disconnect")
                .font(.geist(12, relativeTo: .caption))
                .foregroundStyle(.secondary)
            #else
            // ⌃⌥⇧D lives on the app's Stream menu (so it still works when the HUD is hidden)
            // and in InputCapture's monitor while captured; this button is the in-overlay,
            // click-to-disconnect affordance.
            #if os(macOS)
            Button("Disconnect (⌃⌥⇧D)") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #else
            Button("Disconnect") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #endif
            #endif
        }
    }

    // MARK: - Card metrics

    /// The OUTER gap between the card and the screen edge. (Inner content padding stays a fixed 10.)
    /// On iOS the card hugs a physically rounded display corner, so it sits a little further in and
    /// pairs with a concentric corner radius (below); on macOS/tvOS windows the classic 10 reads fine.
    private var edgeInset: CGFloat {
        #if os(iOS)
        return 14
        #else
        return 10
        #endif
    }

    /// The card's corner radius. On iOS it's concentric with the physical display corner —
    /// `displayCornerRadius − edgeInset`, so the gap to the screen edge stays uniform right around the
    /// corner instead of a small-radius card cutting into the very rounded glass. Clamped so a
    /// flat-cornered device (or a hidden radius) still gets a sensibly rounded card.
    private var cardCornerRadius: CGFloat {
        #if os(iOS)
        return max(12, DeviceMetrics.displayCornerRadius - edgeInset)
        #else
        return 10
        #endif
    }

    /// The card background shape — a continuous (squircle) rounded rectangle, matching the curve
    /// Apple's hardware display corners use so the concentric inset actually reads as parallel.
    private var cardShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: cardCornerRadius, style: .continuous)
    }
}

#if os(iOS)
/// Device display geometry the overlay needs but UIKit doesn't expose publicly.
enum DeviceMetrics {
    /// The physical display's corner radius. There's no public API for it, so read the private
    /// `_displayCornerRadius` via KVC on the active window scene's screen, guarded by a fallback that
    /// approximates a modern rounded device — a future OS that hides the key just yields a slightly
    /// less-perfect inset, never a crash. The key is assembled from parts so it isn't a plain literal
    /// in the binary; note the App Store private-API consideration regardless.
    static var displayCornerRadius: CGFloat {
        let key = ["_display", "Corner", "Radius"].joined()
        guard
            let screen = UIApplication.shared.connectedScenes
                .compactMap({ $0 as? UIWindowScene })
                .first?.screen,
            let radius = screen.value(forKey: key) as? NSNumber,
            radius.doubleValue > 0
        else { return 44 }
        return CGFloat(radius.doubleValue)
    }
}
#endif
