// A profile's chip colour (design/client-settings-profiles.md §5.1 "Change color"). The catalog
// stores `accent` as `#RRGGBB` — a plain string, so the Rust and Kotlin catalogs read the same
// value — and this is the palette the Apple client offers for it.
//
// A fixed palette rather than a full colour well, for two reasons. The chip is small, tinted text
// on a tinted capsule, so a colour picked freehand can land somewhere unreadable; and a profile's
// colour is an IDENTIFIER — "the orange one" — which works when there are eight of them and stops
// working when there are sixteen million. Any `#RRGGBB` a newer client (or another platform)
// writes still renders: the palette is what this client OFFERS, not what it accepts.

import SwiftUI

public struct ProfileAccent: Identifiable, Sendable, Hashable {
    /// `#RRGGBB`, lowercase — exactly what lands in the catalog.
    public let hex: String
    public let name: String

    public var id: String { hex }

    public var color: Color { Color(hex: hex) ?? .brand }

    /// Hues that stay legible as tinted text on their own tinted capsule, in both appearances,
    /// and that stay tellable apart from each other at chip size.
    /// No violet: the brand purple IS the default, and a swatch a shade off it would be a
    /// choice you can't see you made.
    public static let palette: [ProfileAccent] = [
        ProfileAccent(hex: "#3aa0ff", name: "Blue"),
        ProfileAccent(hex: "#2bb8a6", name: "Teal"),
        ProfileAccent(hex: "#35c759", name: "Green"),
        ProfileAccent(hex: "#e0a800", name: "Amber"),
        ProfileAccent(hex: "#ff8800", name: "Orange"),
        ProfileAccent(hex: "#ff4f6d", name: "Red"),
        ProfileAccent(hex: "#ff4f9a", name: "Pink"),
        ProfileAccent(hex: "#8e8e93", name: "Graphite"),
    ]

    public static func named(_ hex: String?) -> ProfileAccent? {
        guard let hex else { return nil }
        return palette.first { $0.hex.caseInsensitiveCompare(hex) == .orderedSame }
    }
}

public extension Color {
    /// Parse a `#RRGGBB` string. nil for anything else — a profile with an accent this build
    /// can't read falls back to the brand tint rather than rendering as black.
    init?(hex: String) {
        guard hex.hasPrefix("#"), hex.count == 7,
              let value = UInt32(hex.dropFirst(), radix: 16)
        else { return nil }
        self.init(
            red: Double((value >> 16) & 0xFF) / 255,
            green: Double((value >> 8) & 0xFF) / 255,
            blue: Double(value & 0xFF) / 255)
    }
}

public extension StreamProfile {
    /// This profile's colour, or the brand tint when it has none — every surface that shows a
    /// profile reads it through here, so "no colour chosen" looks deliberate everywhere.
    var accentColor: Color { Color(hex: accent ?? "") ?? .brand }
}
