// The host grid's cards: a saved host (tap to connect, context menu) and an mDNS-discovered
// host (tap to save + connect). Both share the same platform-tuned sizing.

import PunktfunkKit
import SwiftUI

/// Shared host-card sizing — touch-first on iOS, compact on macOS/tvOS.
private struct CardMetrics {
    let iconSize: CGFloat
    let iconBox: CGFloat
    let cardPadding: CGFloat
    let nameFont: Font

    static var current: CardMetrics {
        #if os(iOS)
        CardMetrics(iconSize: 56, iconBox: 76, cardPadding: 28, nameFont: .title3.weight(.semibold))
        #else
        CardMetrics(iconSize: 42, iconBox: 56, cardPadding: 18, nameFont: .headline)
        #endif
    }
}

/// A saved host. The accent ring marks the most-recently-connected one; the context menu
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

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            VStack(spacing: 10) {
                ZStack {
                    Image(systemName: "play.display")
                        .font(.system(size: m.iconSize, weight: .light))
                        .foregroundStyle(.tint)
                        .opacity(isConnecting ? 0.3 : 1)
                    if isConnecting {
                        ProgressView()
                    }
                }
                .frame(height: m.iconBox)
                VStack(spacing: 2) {
                    HStack(spacing: 6) {
                        // Presence dot: green = advertising on the LAN now; grey = not seen.
                        Circle()
                            .fill(isOnline ? Color.green : Color.secondary.opacity(0.35))
                            .frame(width: 7, height: 7)
                            .accessibilityLabel(isOnline ? "Online" : "Offline")
                        Text(host.displayName)
                            .font(m.nameFont)
                            .lineLimit(1)
                    }
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
            .padding(.vertical, m.cardPadding)
            .padding(.horizontal, 12)
            #if !os(tvOS)
            // tvOS: the .card button style owns platter + focus motion — extra chrome
            // inside it mutes the grow/tilt. Material + accent ring are for pointer UIs.
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
            .overlay {
                if isMostRecent {
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
        .disabled(isBusy)
        .contextMenu {
            Button("Pair with PIN…", action: onPair)
            Button("Test Network Speed…", action: onSpeedTest)
            if let onBrowseLibrary {
                Button("Browse Library…", action: onBrowseLibrary)
            }
            if host.pinnedSHA256 != nil {
                Button("Forget Identity", action: onForget)
            }
            Button("Remove", role: .destructive, action: onRemove)
        }
    }
}

/// A host found on the LAN but not yet saved. A dashed ring distinguishes it from saved cards;
/// tapping saves it and connects (or pairs, if the host requires it).
struct DiscoveredCardView: View {
    let discovered: DiscoveredHost
    let isBusy: Bool
    let onConnect: () -> Void

    var body: some View {
        let m = CardMetrics.current
        return Button(action: onConnect) {
            VStack(spacing: 10) {
                Image(systemName: "play.display")
                    .font(.system(size: m.iconSize, weight: .light))
                    .foregroundStyle(.tint)
                    .frame(height: m.iconBox)
                VStack(spacing: 2) {
                    Text(discovered.name)
                        .font(m.nameFont)
                        .lineLimit(1)
                    HStack(spacing: 4) {
                        Image(systemName: discovered.requiresPairing ? "lock.fill" : "wifi")
                            .font(.system(size: 9))
                            .foregroundStyle(.secondary)
                        Text("\(discovered.host):\(String(discovered.port))")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    Text(discovered.requiresPairing ? "Pairing required" : "Discovered")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, m.cardPadding)
            .padding(.horizontal, 12)
            #if !os(tvOS)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
            .overlay {
                RoundedRectangle(cornerRadius: 14)
                    .strokeBorder(
                        Color.secondary.opacity(0.25),
                        style: StrokeStyle(lineWidth: 1, dash: [4, 3]))
            }
            #endif
        }
        #if os(tvOS)
        .buttonStyle(.card)
        #else
        .buttonStyle(.plain)
        #endif
        .disabled(isBusy)
    }
}
