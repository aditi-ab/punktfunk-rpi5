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
    /// Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
    /// can capture; the resolved count drives the in-core decode + AVAudioEngine layout.
    public static let audioChannels = "punktfunk.audioChannels"
    public static let micEnabled = "punktfunk.micEnabled"
    public static let speakerUID = "punktfunk.speakerUID"
    public static let micUID = "punktfunk.micUID"
    public static let presenter = "punktfunk.presenter"
    /// Request a 10-bit BT.2020 PQ (HDR10) stream. On by default; only takes effect when the host
    /// has HDR content AND this display supports HDR — otherwise the stream stays 8-bit SDR.
    public static let hdrEnabled = "punktfunk.hdrEnabled"
    public static let hosts = "punktfunk.hosts"
    /// Client-side cursor mode: "auto" (shown only in gamescope sessions), "always", "never".
    public static let cursorMode = "punktfunk.cursorMode"
    /// Experimental: show the host's game library (browsed over the management API). Off by default.
    public static let libraryEnabled = "punktfunk.libraryEnabled"
    /// macOS: take the window fullscreen while streaming and restore it on the host list. On by default.
    public static let fullscreenWhileStreaming = "punktfunk.fullscreenWhileStreaming"
    /// Show the streaming statistics overlay (mode/fps/throughput/latency). On by default; toggle
    /// while streaming with ⌘⇧S (macOS / hardware keyboard).
    public static let hudEnabled = "punktfunk.hudEnabled"
    /// Which corner the statistics overlay sits in — a `HUDPlacement` raw value
    /// ("topLeading"/"topTrailing"/"bottomLeading"/"bottomTrailing"). Default top-trailing.
    public static let hudPlacement = "punktfunk.hudPlacement"
}
