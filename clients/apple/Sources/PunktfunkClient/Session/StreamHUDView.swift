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
                .padding(cardPadding)
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
            // Floor-shaved (design/apple-presentation-rebuild.md): the OS present pipeline's
            // fixed depth is excluded, so the headline describes Punktfunk's own latency.
            parts.append(String(format: "%.1f ms", model.endToEndAdjP50Ms))
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
                // Which settings profile this session resolved to, if any (design §5.2). Near-zero
                // cost, and it answers "which profile am I on?" without leaving the stream —
                // otherwise the only evidence is the settings themselves, which is a guessing game.
                if let profile = model.settings.profileName {
                    Text("· \(profile)")
                        .font(.system(.caption, design: .monospaced))
                        .foregroundStyle(
                            Color(hex: model.settings.profileAccent ?? "") ?? Color.accentColor)
                        .lineLimit(1)
                }
            }
            if model.endToEndValid {
                // Stage-2: the end-to-end headline (capture→on-glass, measured directly, skew-
                // corrected) — "(same-host clock)" when the host didn't answer the skew
                // handshake. FLOOR-SHAVED (design/apple-presentation-rebuild.md): the OS present
                // pipeline's fixed depth is excluded so the number describes Punktfunk's own
                // latency; the detailed tier shows the excluded floor as its own line, and the
                // stats log keeps the raw values.
                Text("end-to-end \(model.endToEndAdjP50Ms, specifier: "%.1f") ms p50 · \(model.endToEndAdjP95Ms, specifier: "%.1f") p95 · capture→on-glass\(model.endToEndSkewCorrected ? "" : " (same-host clock)")")
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.secondary)
                // The equation (detailed tier only): the stages tiling the headline interval
                // (per-window p50s — they only approximately sum to the directly-measured
                // total). With a host that reports per-AU timings (0xCF) the first term splits
                // into host + network (phase 2); an old host keeps the combined term. The
                // display term is floor-shaved like the headline, so the equation still sums.
                if verbosity == .detailed && model.hostNetworkValid && model.decodeValid && model.displayValid {
                    if model.splitValid {
                        Text("= host \(model.hostP50Ms, specifier: "%.1f") + network \(model.networkP50Ms, specifier: "%.1f") + decode \(model.decodeP50Ms, specifier: "%.1f") + display \(model.displayAdjP50Ms, specifier: "%.1f")")
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                    } else {
                        Text("= host+network \(model.hostNetworkP50Ms, specifier: "%.1f") + decode \(model.decodeP50Ms, specifier: "%.1f") + display \(model.displayAdjP50Ms, specifier: "%.1f")")
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.secondary)
                    }
                    if model.osFloorValid {
                        // The excluded OS term, kept visible for honesty: display-pipeline
                        // minimum no client can pace under (~2 refresh intervals composited).
                        Text("os present +\(model.osFloorP50Ms, specifier: "%.1f") excluded (display pipeline minimum)")
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.tertiary)
                    }
                    // Client-queue wait (reassembly receipt → decode pull, ABI v9 split): ~0 on
                    // a healthy stream and hidden as noise; shown from 2 ms — a persistent value
                    // is a client-side standing backlog that pre-split builds displayed as
                    // "network" (the 2026-07 two-pair plateau). The core's standing-latency
                    // bleed logs alongside when it acts on the same state.
                    if model.clientQueueValid && model.clientQueueP50Ms >= 2 {
                        Text("client queue +\(model.clientQueueP50Ms, specifier: "%.1f") (receive backlog — standing if it persists)")
                            .font(.system(.caption2, design: .monospaced))
                            .foregroundStyle(.tertiary)
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
            // The AUDIO plane's own latency (detailed tier). Deliberately OUTSIDE the video branch
            // above: it is not a term of that equation — audio is steered to MEET the video total,
            // never summed into it — and the depth is exactly as worth seeing under the stage-1
            // fallback presenter, which measures no end-to-end at all.
            //
            // `buffer` is how much decoded audio is queued ahead of the speaker; `a/v` is where
            // that puts it relative to the picture (+ = audio behind). Both, not just the depth: a
            // deep ring on a jittery link is the adaptive floor doing its job, and only the offset
            // distinguishes that from a ring holding audio late. Neither number was renderable
            // anywhere before — they lived in a periodic log line — which is how a report of "the
            // audio delay seems way too high" got triaged to a conclusion with no instrument.
            if verbosity == .detailed && model.audioValid && model.audioBufferMs > 0 {
                // String(format:) for the signed offset: `%+d` has no specifier-interpolation
                // equivalent, and Swift's Int is 64-bit (%lld, never the 32-bit %d).
                Text(model.audioAvOffsetMs == 0
                    ? "audio buffer \(model.audioBufferMs) ms"
                    : String(
                        format: "audio buffer %lld ms · a/v %+lld ms",
                        model.audioBufferMs, model.audioAvOffsetMs))
                    .font(.system(.caption2, design: .monospaced))
                    .foregroundStyle(.tertiary)
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
            // Mic mute — the in-stream toggle, on the same card as the other in-overlay action.
            // Absent (not greyed) when the session sends no microphone: the HUD is a status card,
            // and a dead control on it would read as "there is a mic, and it is on". The muted
            // STATE is not this button's job — the badge over the stream says that at every tier
            // and with the overlay off entirely. tvOS gets no control: no microphone, and a
            // focusable one would steal the controller's A press from the host.
            #if !os(tvOS)
            if model.micAvailable {
                Button(micButtonTitle) { model.toggleMicMute() }
                    .font(.geist(12, relativeTo: .caption))
            }
            #endif
            // ⌃⌥⇧D lives on the app's Stream menu (so it still works when the HUD is hidden)
            // and in InputCapture's monitor while captured; this button is the in-overlay,
            // click-to-disconnect affordance. tvOS deliberately gets NEITHER a button (a
            // focusable control would steal the controller's A press from the host) NOR a hint
            // line: the exits are the hold gestures the start-of-stream banner teaches (hold
            // the remote's Back; hold L1+R1+Start+Select on a pad).
            #if os(macOS)
            Button("Disconnect (⌃⌥⇧D)") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #elseif os(iOS)
            Button("Disconnect") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #endif
        }
    }

    #if !os(tvOS)
    /// The mute button's wording. macOS names the chord, exactly as its Disconnect button does;
    /// iOS/iPadOS spells the action out (the HUD's buttons there carry no shortcuts, even where a
    /// hardware keyboard could fire one — the Stream menu is that keyboard's surface).
    private var micButtonTitle: String {
        #if os(macOS)
        return model.micMuted ? "Unmute Mic (⌃⌥⇧A)" : "Mute Mic (⌃⌥⇧A)"
        #else
        return model.micMuted ? "Unmute Microphone" : "Mute Microphone"
        #endif
    }
    #endif

    // MARK: - Card metrics

    /// The card's inner content padding. Roomier on tvOS — the stat text auto-scales for the
    /// couch (relative system styles), so the card's chrome must keep pace or it reads cramped.
    ///
    /// On iOS it also has to CLEAR THE CORNER. A rounded corner of radius `r` pulls the card's
    /// edge inward by `r − √(r² − (r−y)²)` at a distance `y` below the top, so the first and last
    /// lines of a padded stack sit inside the arc unless the padding keeps pace with the radius.
    /// At `0.45 · r` that intrusion stays well inside the padding across the whole range this
    /// card can wear (≈4.6 pt of arc against 12.6 pt of padding at the 28 pt cap), so no line
    /// ever runs into the curve.
    private var cardPadding: CGFloat {
        #if os(tvOS)
        return 16
        #elseif os(iOS)
        return max(10, cardCornerRadius * 0.45)
        #else
        return 10
        #endif
    }

    /// The OUTER gap between the card and the screen edge. On iOS the card hugs a physically
    /// rounded display corner, so it sits a little further in and pairs with a concentric corner
    /// radius (below); tvOS floats it well clear of the TV's overscan-ish edge; macOS windows
    /// keep the classic 10.
    private var edgeInset: CGFloat {
        #if os(iOS)
        return 14
        #elseif os(tvOS)
        return 24
        #else
        return 10
        #endif
    }

    /// The card's corner radius. On iOS it aims to be concentric with the physical display
    /// corner — `displayCornerRadius − edgeInset`, so the gap to the screen edge stays uniform
    /// right around the corner instead of a small-radius card cutting into the very rounded
    /// glass — but that aim is BOUNDED by what a card this small can actually carry.
    ///
    /// Unbounded, a modern phone (~62 pt of display radius) asked for a 48 pt corner on a card
    /// whose lines sit 10 pt from the edge: the arc reaches ~19 pt inward at the first line, so
    /// the top and bottom lines rendered INSIDE the curve. Concentricity is only a virtue while
    /// the radius is small next to the card; past that it is just a blob eating its own text.
    /// 28 pt is the most this card's stack can wear (with `cardPadding` scaling alongside), and
    /// devices whose display radius asks for less than that still get a truly concentric corner.
    private var cardCornerRadius: CGFloat {
        #if os(iOS)
        return min(28, max(12, DeviceMetrics.displayCornerRadius - edgeInset))
        #elseif os(tvOS)
        return 16 // scales with the roomier padding
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

/// "This pad's gyro can't reach the game" — shown briefly when a forwarded controller with motion
/// meets a session whose virtual controller has no motion plane (an X-Box class pad has no gyro in
/// its HID contract, so every sample would be decoded and dropped).
///
/// Not a control, unlike `MicMutedBadge`: the fix is the Controller type setting, which is not
/// reachable mid-stream on every platform, and changing it applies from the next session anyway.
/// So this states the fact and names the setting, in the HUD's glass language, and gets out of the
/// way — the alternative is what shipped before, which was a gyro that silently did nothing with
/// no way to tell that from a broken sensor.
///
/// Every platform: a DualSense on an Apple TV is an ordinary way to play, and it is exactly the
/// pad this can happen to.
struct MotionUnreachableBadge: View {
    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "gyroscope")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.yellow)
            Text("Motion won't reach this session — set Controller type to DualSense")
                .font(.geist(12, .medium, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.9))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .glassBackground(Capsule())
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "This controller's motion will not reach the game. "
                + "Set Controller type to DualSense to enable it.")
    }
}

#if !os(tvOS)
/// The muted-microphone badge — the mute STATE, as opposed to the buttons that flip it. It rides
/// over the stream whenever the mic is muted, INDEPENDENT of the stats overlay (which the user
/// may have cycled off, and which the compact tier reduces to a stat line): "am I muted?" is not a
/// statistic, and a mute you can't see is how people talk to nobody for a minute. Same glass
/// language as the HUD, sized like the start-of-stream banner it shares the bottom edge with.
///
/// It is also a control: tapping it unmutes. That is the guaranteed way back for a touch user who
/// muted with the overlay off, and it costs the badge nothing (it is on screen either way).
struct MicMutedBadge: View {
    let onUnmute: () -> Void

    var body: some View {
        Button(action: onUnmute) {
            HStack(spacing: 7) {
                Image(systemName: "mic.slash.fill")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(.red)
                Text("Microphone muted")
                    .font(.geist(12, .medium, relativeTo: .caption))
                    .foregroundStyle(.white.opacity(0.9))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            // interactive: the badge IS the tap target, so the glass reacts to press.
            .glassBackground(Capsule(), interactive: true)
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityLabel("Microphone muted")
        .accessibilityHint("Unmutes the microphone")
    }
}
#endif

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
