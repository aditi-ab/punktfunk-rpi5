// Whether the iOS/iPadOS/macOS UI should be in its controller-friendly mode (the console-style
// host launcher, gamepad settings, and the coverflow library browser instead of the touch/desktop
// layouts). A pure function, not a singleton: the reactivity comes from callers already observing
// `GamepadManager.shared` and the `DefaultsKey.gamepadUIEnabled` @AppStorage themselves (the same
// local-read pattern SettingsView already uses for GamepadManager), so this stays the single place
// the two combine without adding a second ObservableObject or an environment key nobody else needs.

import Foundation
import PunktfunkShared

public enum GamepadUIEnvironment {
    /// `enabledSetting` is the user's Settings toggle (`DefaultsKey.gamepadUIEnabled`);
    /// `gamepadConnected` is `GamepadManager.shared.active != nil` — active only once a usable
    /// controller is actually attached (a non-extended-profile device leaves `active` nil, which
    /// keeps the touch UI). A `Bool` rather than the `DiscoveredController` itself: this function's
    /// whole job is the AND, so there's nothing else to inspect, and it keeps the helper testable
    /// without a real `GCController` (which XCTest can't construct).
    public static func isActive(gamepadConnected: Bool, enabledSetting: Bool) -> Bool {
        enabledSetting && (gamepadConnected || forced)
    }

    /// Dev-only escape hatch (like ContentView's `PUNKTFUNK_AUTOCONNECT`): pretend a controller is
    /// attached so the gamepad UI can be exercised/screenshotted without physical hardware —
    /// essential on a headless CI Mac and for `swift run` UI work. Never set in production.
    private static let forced =
        ProcessInfo.processInfo.environment["PUNKTFUNK_FORCE_GAMEPAD_UI"] == "1"
}
