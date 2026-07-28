// The stats overlay's verbosity tier — a port of the Android client's 3-tier overlay semantics
// so the two clients feel identical: Off → Compact (one-line pill) → Normal (headline stats) →
// Detailed (plus the latency stage equation). Persisted under `DefaultsKey.statsVerbosity`;
// the in-stream cycle surfaces (⌃⌥⇧S, the three-finger tap) advance it with
// `store(current.next())`, and every UI reader observes the same default via @AppStorage.
//
// Lives in PunktfunkKit (not the app) because the kit's input paths (TouchMouse's three-finger
// tap, InputCapture's captured-state ⌃⌥⇧S) cycle it directly.

import Foundation
import PunktfunkShared

/// How much of the streaming statistics overlay to show. The raw values are stable on disk —
/// rename the cases freely, never the strings.
public enum StatsVerbosity: String, CaseIterable, Sendable {
    case off, compact, normal, detailed

    /// User-facing tier label (Settings pickers, the gamepad settings row).
    public var label: String {
        switch self {
        case .off: return "Off"
        case .compact: return "Compact"
        case .normal: return "Normal"
        case .detailed: return "Detailed"
        }
    }

    /// The next tier in the cycle: off → compact → normal → detailed → off (wrapping) —
    /// the ⌃⌥⇧S / three-finger-tap order, same as Android.
    public func next() -> StatsVerbosity {
        switch self {
        case .off: return .compact
        case .compact: return .normal
        case .normal: return .detailed
        case .detailed: return .off
        }
    }

    /// The persisted tier. When `statsVerbosity` has never been written, migrates from the
    /// legacy `hudEnabled` bool the pre-tiered clients stored: absent-or-true → `.normal`
    /// (the old "on" look minus the equation lines), explicit false → `.off`.
    public static var current: StatsVerbosity {
        let defaults = UserDefaults.standard
        if let raw = defaults.string(forKey: DefaultsKey.statsVerbosity),
           let tier = StatsVerbosity(rawValue: raw) {
            return tier
        }
        if let legacy = defaults.object(forKey: DefaultsKey.hudEnabled) as? Bool, !legacy {
            return .off
        }
        return .normal
    }

    /// Persist a tier (the cycle surfaces write through here; the Settings pickers write the
    /// same key via @AppStorage).
    public static func store(_ tier: StatsVerbosity) {
        UserDefaults.standard.set(tier.rawValue, forKey: DefaultsKey.statsVerbosity)
    }

    /// The tier the LIVE session is showing — its profile's, if one overrode it — falling back to
    /// the persisted global while idle. What the in-stream cycle advances FROM: cycling in a
    /// session a profile put on Detailed must go to Off, not to whatever the global happens to be.
    public static var session: StatsVerbosity {
        StatsVerbosity(rawValue: SessionSettings.current.statsVerbosity) ?? .normal
    }

    /// Advance the in-stream overlay one tier (⌃⌥⇧S, the three-finger tap, the Stream menu).
    ///
    /// It writes the GLOBAL, as every client's cycle always has, and the app pushes that back into
    /// the live session — so from the moment the user cycles, the session follows the global
    /// rather than the profile's start-of-session tier. That is the honest reading of an explicit
    /// live override, and it keeps one observable source (`@AppStorage`) driving the overlay.
    public static func cycle() {
        store(session.next())
    }
}
