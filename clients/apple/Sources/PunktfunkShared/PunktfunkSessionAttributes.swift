// The Live Activity's attributes — the ONE type that must be identical in the app (which starts
// and updates the Activity) and the widget extension (which renders it). Hence it lives in the
// dependency-free shared module.
//
// Gated on `os(iOS)`, NOT `canImport(ActivityKit)`: ActivityKit *imports* on macOS but its types
// are `@available(macOS, unavailable)`, so canImport would wrongly admit this on the macOS build.
// Live Activities are iPhone/iPad only (iPadOS reports os(iOS)).
//
// Naming/shape is a runtime contract: an Activity started by one build is decoded by the extension
// of the same build, so keep `ContentState` Codable-stable across releases the way `StoredHost` is.

#if os(iOS)
import ActivityKit
import Foundation

public struct PunktfunkSessionAttributes: ActivityAttributes {
    // Static for the Activity's whole life (set at request time).
    public let hostID: UUID
    public let hostName: String
    /// The title of the launched game, if the session started from the library; nil for a plain
    /// host connect (nothing tracks the live foreground app mid-session).
    public let launchTitle: String?

    public init(hostID: UUID, hostName: String, launchTitle: String?) {
        self.hostID = hostID
        self.hostName = hostName
        self.launchTitle = launchTitle
    }

    public struct ContentState: Codable, Hashable {
        public enum Stage: String, Codable, Hashable {
            case streaming     // foreground, live
            case background    // backgrounded keep-alive (countdown running)
            case reconnecting  // post-loss re-anchor hold
            case ending        // torn down — final state before dismissal
        }

        public var stage: Stage
        /// Session start — drives `Text(timerInterval:)` for a free client-side ticking clock (no
        /// per-second push needed).
        public var startedAt: Date
        /// e.g. "2560×1440 @120 · HEVC · HDR". Updated only when it actually changes.
        public var modeLine: String
        /// Coarse, updated sparsely (every ~30 s) — never the 1 Hz stats firehose.
        public var latencyMs: Int?
        public var mbps: Double?
        /// While backgrounded: when the keep-alive auto-disconnect fires — drives the countdown.
        public var backgroundDeadline: Date?

        public init(
            stage: Stage, startedAt: Date, modeLine: String,
            latencyMs: Int? = nil, mbps: Double? = nil, backgroundDeadline: Date? = nil
        ) {
            self.stage = stage
            self.startedAt = startedAt
            self.modeLine = modeLine
            self.latencyMs = latencyMs
            self.mbps = mbps
            self.backgroundDeadline = backgroundDeadline
        }
    }
}

/// Kind string for the Live Activity — kept next to the attributes so app + extension agree.
public enum PunktfunkActivity {
    public static let kind = "PunktfunkSession"
}
#endif
