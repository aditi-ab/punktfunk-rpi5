// The host grid's cards: a saved host (tap to connect, context menu) and an mDNS-discovered
// host (tap to save + connect). Both share the "monogram module" look — a squared brand-purple
// monogram tile + a left-aligned bold Geist name over monospaced technical metadata
// (address, status), framed by a hairline panel border. Industrial, not soft.

import PunktfunkKit
import SwiftUI

/// Shared host-card sizing — touch-first on iOS, compact on macOS, roomy on tvOS.
private struct CardMetrics {
    let tile: CGFloat       // monogram tile side
    let monogram: CGFloat   // monogram letter point size
    let name: CGFloat       // host-name point size
    let meta: CGFloat       // address (mono) point size
    let status: CGFloat     // status-label (mono) point size
    let padding: CGFloat
    let spacing: CGFloat    // tile ↔ text gap
    let radius: CGFloat

    static var current: CardMetrics {
        #if os(iOS)
        CardMetrics(tile: 54, monogram: 26, name: 19, meta: 13, status: 11,
                    padding: 16, spacing: 14, radius: 12)
        #elseif os(tvOS)
        CardMetrics(tile: 64, monogram: 32, name: 24, meta: 16, status: 14,
                    padding: 18, spacing: 18, radius: 14)
        #else
        CardMetrics(tile: 44, monogram: 21, name: 15, meta: 12, status: 10.5,
                    padding: 13, spacing: 12, radius: 10)
        #endif
    }
}

/// First letter of a host name, uppercased — the monogram glyph. Falls back to a bullet.
private func monogram(_ name: String) -> String {
    guard let first = name.trimmingCharacters(in: .whitespacesAndNewlines).first else { return "•" }
    return String(first).uppercased()
}

/// The squared monogram tile. `filled` = a solid brand-purple chip (saved hosts); otherwise a
/// tinted outline (discovered hosts). Shows a spinner in place of the glyph while connecting.
private func monogramTile(_ letter: String, m: CardMetrics, connecting: Bool, filled: Bool) -> some View {
    let shape = RoundedRectangle(cornerRadius: m.radius - 3, style: .continuous)
    return ZStack {
        shape.fill(filled
            ? AnyShapeStyle(LinearGradient(
                colors: [Color.brand, Color.brand.opacity(0.72)],
                startPoint: .top, endPoint: .bottom))
            : AnyShapeStyle(Color.brand.opacity(0.14)))
        if connecting {
            ProgressView().tint(filled ? .white : Color.brand)
        } else {
            // Fixed size (not Dynamic Type): the glyph is pinned inside a fixed tile, so it must
            // not scale up and spill out at large accessibility text sizes. minimumScaleFactor +
            // the clip below are belt-and-suspenders for an unusually wide glyph.
            Text(letter)
                .font(.geistFixed(m.monogram, .bold))
                .minimumScaleFactor(0.5)
                .lineLimit(1)
                .foregroundStyle(filled ? Color.white : Color.brand)
        }
    }
    .frame(width: m.tile, height: m.tile)
    .clipShape(shape)
    .overlay {
        if !filled {
            shape.strokeBorder(Color.brand.opacity(0.45), lineWidth: 1)
        }
    }
}

/// A saved host. A left accent bar marks the most-recently-connected one; the context menu
/// pairs / speed-tests / forgets / removes. Disabled while a session is busy.
struct HostCardView: View {
    let host: StoredHost
    /// Currently advertising on the LAN (matched against live mDNS discovery). False means
    /// "not seen on this network" — off, or a remote/cross-subnet host we can't observe.
    let isOnline: Bool
    let isConnecting: Bool
    let isMostRecent: Bool
    let isBusy: Bool
    let onConnect: () -> Void
    let onPair: () -> Void
    let onSpeedTest: () -> Void
    let onForget: () -> Void
    let onRemove: () -> Void
    /// Open the experimental library browser — nil (no menu item) unless the feature flag is on.
    var onBrowseLibrary: (() -> Void)? = nil
    /// Send a Wake-on-LAN magic packet. Shown only when the host is offline and we have a stored
    /// MAC to target (a tap-to-connect already auto-wakes; this is the explicit "just wake it").
    var onWake: (() -> Void)? = nil

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            HStack(spacing: m.spacing) {
                monogramTile(monogram(host.displayName), m: m, connecting: isConnecting, filled: true)
                VStack(alignment: .leading, spacing: 4) {
                    Text(host.displayName)
                        .font(.geist(m.name, .bold, relativeTo: .title3))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Text("\(host.address):\(String(host.port))")
                        .font(.geist(m.meta, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    statusRow(m)
                }
                Spacer(minLength: 0)
            }
            .padding(m.padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            #if !os(tvOS)
            // tvOS: the .card button style owns platter + focus motion; extra chrome mutes it.
            // Elsewhere: a flat material panel with a hairline border (industrial, not a soft blob),
            // and a brand accent bar down the leading edge for the most-recent host.
            .background(.regularMaterial)
            .overlay(alignment: .leading) {
                if isMostRecent {
                    Rectangle().fill(Color.brand).frame(width: 3)
                }
            }
            .clipShape(RoundedRectangle(cornerRadius: m.radius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: m.radius, style: .continuous)
                    .strokeBorder(.quaternary, lineWidth: 1)
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #elseif os(iOS)
        .buttonStyle(HostCardButtonStyle(cornerRadius: m.radius))
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(isBusy)
        .contextMenu {
            Button("Pair with PIN…", action: onPair)
            Button("Test Network Speed…", action: onSpeedTest)
            if let onBrowseLibrary {
                Button("Browse Library…", action: onBrowseLibrary)
            }
            if !isOnline, !host.wakeMacs.isEmpty, PunktfunkConnection.wakeOnLANAvailable, let onWake {
                Button("Wake Host", systemImage: "power", action: onWake)
            }
            if host.pinnedSHA256 != nil {
                // Dropping the pin does NOT downgrade to TOFU: the next connect must re-pair via
                // PIN (unless the host advertises pair=optional). Wording reflects that.
                Button("Forget Identity (re-pair to reconnect)", action: onForget)
            }
            Button("Remove", role: .destructive, action: onRemove)
        }
    }

    /// Technical status line: a square presence pip + monospaced ONLINE/OFFLINE, and PAIRED when a
    /// certificate is pinned (the lock state, spelled out).
    @ViewBuilder private func statusRow(_ m: CardMetrics) -> some View {
        HStack(spacing: 6) {
            RoundedRectangle(cornerRadius: 1.5)
                .fill(isOnline ? Color.green : Color.secondary.opacity(0.4))
                .frame(width: 6, height: 6)
                // The state is spelled out in the adjacent text, so the pip is decorative —
                // otherwise VoiceOver reads the status twice ("Online, ONLINE …").
                .accessibilityHidden(true)
            Text(isOnline ? "ONLINE" : "OFFLINE")
            if host.pinnedSHA256 != nil {
                Text("· PAIRED")
            }
        }
        .font(.geist(m.status, .medium, relativeTo: .caption2))
        .tracking(0.8)
        .foregroundStyle(.secondary)
        .lineLimit(1)
    }
}

/// A host found on the LAN but not yet saved. A tinted-outline monogram + dashed panel border
/// distinguish it from saved cards; tapping saves it and connects (or pairs, if required).
struct DiscoveredCardView: View {
    let discovered: DiscoveredHost
    let isBusy: Bool
    let onConnect: () -> Void

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            HStack(spacing: m.spacing) {
                monogramTile(monogram(discovered.name), m: m, connecting: false, filled: false)
                VStack(alignment: .leading, spacing: 4) {
                    Text(discovered.name)
                        .font(.geist(m.name, .bold, relativeTo: .title3))
                        .foregroundStyle(.primary)
                        .lineLimit(1)
                    Text("\(discovered.host):\(String(discovered.port))")
                        .font(.geist(m.meta, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    HStack(spacing: 6) {
                        Image(systemName: discovered.requiresPairing
                            ? "lock.fill" : "antenna.radiowaves.left.and.right")
                            .font(.system(size: m.status))
                            .accessibilityHidden(true) // decorative; the adjacent text says the state
                        Text(discovered.requiresPairing ? "PAIRING REQUIRED" : "DISCOVERED")
                    }
                    .font(.geist(m.status, .medium, relativeTo: .caption2))
                    .tracking(0.8)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                }
                Spacer(minLength: 0)
            }
            .padding(m.padding)
            .frame(maxWidth: .infinity, alignment: .leading)
            #if !os(tvOS)
            .background(.regularMaterial)
            .clipShape(RoundedRectangle(cornerRadius: m.radius, style: .continuous))
            .overlay {
                RoundedRectangle(cornerRadius: m.radius, style: .continuous)
                    .strokeBorder(
                        Color.secondary.opacity(0.3),
                        style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #elseif os(iOS)
        .buttonStyle(HostCardButtonStyle(cornerRadius: m.radius))
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(isBusy)
    }
}

#if os(iOS)
/// The iOS host-card press/hover treatment, one style for both idioms:
/// - iPhone: a subtle scale-down on press + a light impact haptic on press-down. (`hoverEffect` is
///   inert without a pointer.)
/// - iPad: the system pointer "magnet" — the cursor morphs into a highlight that conforms to the
///   card's rounded rect on hover. (`sensoryFeedback` is inert without a Taptic Engine, and the
///   press scale doubles as click feedback.)
struct HostCardButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .scaleEffect(configuration.isPressed ? 0.96 : 1)
            .animation(.spring(response: 0.3, dampingFraction: 0.65), value: configuration.isPressed)
            // Conform the pointer highlight to the card's rounded rect, not its square bounds.
            .contentShape(.hoverEffect, RoundedRectangle(cornerRadius: cornerRadius, style: .continuous))
            .hoverEffect(.highlight)
            // Light tap on press-down (nil on release so it fires once, on touch). No haptic
            // hardware on iPad → silently ignored there.
            .sensoryFeedback(trigger: configuration.isPressed) { _, pressed in
                pressed ? .impact(weight: .light) : nil
            }
    }
}
#endif
