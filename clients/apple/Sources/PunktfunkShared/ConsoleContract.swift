// The console UI's cross-client contract, as NUMBERS — the parts of `clients/shared/
// console-vectors.json` this client implements that are not the palette table: the screen
// push/pop choreography (`motion_spring`, version 2) and the settings tab list (`tabs`).
//
// In PunktfunkShared, not beside the views that use them, for the reason `GamepadPalette` gives
// in its header: `PunktfunkClient` is an executable target with no test target of its own, so
// anything that must be PINNED against the vectors file has to live where `ConsoleVectorsTests`
// can reach it. `GamepadShellMotion` (the app) turns these into SwiftUI animations; the settings
// screen turns the tab list into pills. Foundation only — the widget extension links this module.

import Foundation

/// The console screen transition — the desktop console's spring push/pop (`shell.rs`), pinned by
/// the vectors' `motion_spring` block. Springs are integrator-dependent, so the contract pins
/// PARAMETERS, not sampled positions: two implementations that both honour response/damping agree
/// to the eye and disagree in the third decimal.
public enum ConsoleMotion {
    /// SwiftUI's `spring(response:dampingFraction:)` vocabulary, which is also the desktop's
    /// (`k = (2π/response)²`, `c = 2ζ√k`).
    public static let response: Double = 0.42
    public static let damping: Double = 0.88
    /// The incoming screen slides up out of a fade by this much (design units = points).
    public static let pushSlideDp: Double = 36
    /// …growing from this scale, while the screen beneath recedes to `exitScale`.
    public static let enterScale: Double = 0.985
    public static let exitScale: Double = 0.96
    /// A pop re-reveals the screen beneath from this alpha (the desktop's `NAV_REVEAL_ALPHA`;
    /// SwiftUI's opacity transition animates from 0 — a known, accepted deviation, see
    /// `AnyTransition.gamepadScreen`).
    public static let revealAlpha: Double = 0.4
    /// Back pressed mid-push turns the entering screen around; input other than Back stays
    /// dropped until the spring has passed 0.85 of its travel.
    public static let interruptible = true
    /// When a fresh push starts accepting input other than Back: the time this spring takes to
    /// pass 0.85 of its travel (≈ 0.2 s for response 0.42, damping 0.88 — the analytic step
    /// response, `1 − e^{−ζω₀t}(cos ω_d t + ζ/√(1−ζ²)·sin ω_d t)`, crosses 0.85 at t ≈ 0.20).
    public static let inputOpensAfter: TimeInterval = 0.20
    /// Under Reduce Motion the transition is a plain crossfade on this spring — no slide, no
    /// scale (the desktop's `REDUCED_NAV`).
    public static let reducedResponse: Double = 0.22
    public static let reducedDamping: Double = 1.0
}

/// The gamepad settings screen's sections, in strip order. The names are the cross-client tab
/// vocabulary (`tabs` in the vectors) — a setting is found under the same word on every client.
/// `about` is this client's own trailing section: it is built from something other than the
/// settings store and changes nothing, so it ends the strip and is not in the shared list.
public enum GpSettingsTab: String, CaseIterable, Hashable, Sendable {
    case stream = "Stream"
    case video = "Video"
    case audio = "Audio"
    case controller = "Controller"
    case interface = "Interface"
    case profiles = "Profiles"
    /// Trailing, like Profiles: both are built from something other than the settings store, and
    /// About is where the strip ends because it is the one section that changes nothing.
    case about = "About"

    /// The tabs this client shares with the vectors' list — everything but its own About.
    public static var shared: [GpSettingsTab] { allCases.filter { $0 != .about } }
}
