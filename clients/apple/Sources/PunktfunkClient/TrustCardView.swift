// Trust-on-first-use prompt: shown over the live-but-blurred stream when connecting to an
// unpinned host. The user compares the fingerprint with the one the host logged at startup,
// or drops this and runs the PIN pairing ceremony instead.

import Foundation
import PunktfunkKit
import SwiftUI

struct TrustCardView: View {
    let fingerprint: Data
    let hostName: String
    let onCancel: () -> Void
    let onTrust: () -> Void
    let onPairInstead: () -> Void

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "lock.shield")
                .font(.system(size: 36, weight: .light))
                .foregroundStyle(.tint)
            Text("Verify \(hostName)")
                .font(.geist(20, .semibold, relativeTo: .title3))
            Text("First connection. Compare this fingerprint with the one "
                + "punktfunk-host logged at startup (\u{201C}clients pin this "
                + "fingerprint\u{201D}):")
                .font(.geist(16, relativeTo: .callout))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Text(Self.format(fingerprint: fingerprint))
                .font(.system(.callout, design: .monospaced))
                #if !os(tvOS)
                .textSelection(.enabled)
                #endif
                .padding(10)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            HStack(spacing: 12) {
                Button("Cancel", role: .cancel, action: onCancel)
                    #if !os(tvOS)
                    .keyboardShortcut(.cancelAction)
                    #endif
                Button("Trust & Connect", action: onTrust)
                    // Opaque prominent, NOT glass: this card is itself a glass panel
                    // (.glassBackground below), and glass-on-glass loses contrast — a tinted
                    // bordered button reads cleanly over glass (HIG). The sheet primaries stay
                    // glass because the system manages the sheet's own glass layering.
                    .buttonStyle(.borderedProminent)
                    #if !os(tvOS)
                    .keyboardShortcut(.defaultAction)
                    #endif
            }
            #if os(iOS)
            .controlSize(.large)
            #endif
            // The verified alternative to eyeballing hex: drop this session (the host
            // serves one connection at a time) and run the SPAKE2 PIN ceremony instead.
            Button("Pair with PIN instead…", action: onPairInstead)
                #if os(macOS)
                .buttonStyle(.link)
                #else
                .buttonStyle(.borderless)
                #endif
                .font(.geist(16, relativeTo: .callout))
        }
        .padding(28)
        .frame(maxWidth: 440)
        // Floating trust card over the blurred stream — Liquid Glass on 26+, .regularMaterial
        // fallback below. The inner fingerprint box stays .quaternary (content, not glass).
        .glassBackground(RoundedRectangle(cornerRadius: 18))
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
}

private extension Array {
    func chunks(of size: Int) -> [[Element]] {
        stride(from: 0, to: count, by: size).map { Array(self[$0..<Swift.min($0 + size, count)]) }
    }
}
