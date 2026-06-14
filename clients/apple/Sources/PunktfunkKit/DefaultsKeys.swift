// One source of truth for the client's UserDefaults / @AppStorage keys. A magic-string key
// duplicated across a setting's writer (a Settings @AppStorage) and reader (e.g. a stream view
// reading UserDefaults) splits silently on a typo — the setting just stops taking effect. These
// live in PunktfunkKit because both the app and the kit's views read them.

import Foundation

/// Persisted-setting keys. The string VALUES are stable on disk — rename the symbol freely, but
/// never the string (it would orphan everyone's saved value).
public enum DefaultsKey {
    public static let streamWidth = "punktfunk.width"
    public static let streamHeight = "punktfunk.height"
    public static let streamHz = "punktfunk.hz"
    public static let compositor = "punktfunk.compositor"
    public static let gamepadType = "punktfunk.gamepadType"
    public static let gamepadID = "punktfunk.gamepadID"
    public static let bitrateKbps = "punktfunk.bitrateKbps"
    public static let micEnabled = "punktfunk.micEnabled"
    public static let speakerUID = "punktfunk.speakerUID"
    public static let micUID = "punktfunk.micUID"
    public static let presenter = "punktfunk.presenter"
    public static let hosts = "punktfunk.hosts"
    /// Client-side cursor mode: "auto" (shown only in gamescope sessions), "always", "never".
    public static let cursorMode = "punktfunk.cursorMode"
    /// Experimental: show the host's game library (browsed over the management API). Off by default.
    public static let libraryEnabled = "punktfunk.libraryEnabled"
    /// macOS: take the window fullscreen while streaming and restore it on the host list. On by default.
    public static let fullscreenWhileStreaming = "punktfunk.fullscreenWhileStreaming"
}
