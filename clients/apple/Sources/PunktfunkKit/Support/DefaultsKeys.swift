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
    /// Preferred video codec: `"auto"` (host decides), `"hevc"`, or `"h264"`. A soft preference —
    /// the host emits it when it can, else falls back. Drives the decoder via `Welcome.codec`.
    public static let codec = "punktfunk.codec"
    public static let micEnabled = "punktfunk.micEnabled"
    public static let speakerUID = "punktfunk.speakerUID"
    public static let micUID = "punktfunk.micUID"
    /// macOS: which input channel of the chosen mic device feeds the host. 0 = "Auto" (sum every
    /// channel to mono — a mic on a single input of a multi-channel interface passes at full
    /// level); n≥1 pins 1-based input channel n. Multi-channel interfaces expose the mic on ONE
    /// discrete channel, and the default N→stereo downmix grabs channels 0/1 (silence when the mic
    /// is higher up), so we fold to mono ourselves. Only meaningful for multi-channel devices.
    public static let micChannel = "punktfunk.micChannel"
    public static let presenter = "punktfunk.presenter"
    /// macOS: V-Sync the stream's presents — each decoded frame flips on the next display vsync
    /// (evenly paced, no tearing under direct scanout) instead of as soon as the GPU finishes
    /// (lowest latency — the default, OFF). Resolved once per session;
    /// PUNKTFUNK_PRESENT_MODE=immediate|vsync overrides it for A/B. See Stage2Pipeline's header.
    public static let vsync = "punktfunk.vsync"
    /// Allow variable refresh rate: hand the display link a wide frame-rate RANGE (low floor,
    /// preferred = stream rate) so a ProMotion / adaptive-sync display can vary its physical
    /// refresh to match the stream. On by default; a no-op on fixed-refresh displays. When off,
    /// macOS lets the link free-run at the display's native rate and iOS keeps its proven 30 Hz
    /// floor. Read per session/reconfigure by `SessionPresenter.syncFrameRate`.
    public static let allowVRR = "punktfunk.allowVRR"
    /// Request a 10-bit BT.2020 PQ (HDR10) stream. On by default; only takes effect when the host
    /// has HDR content AND this display supports HDR — otherwise the stream stays 8-bit SDR.
    public static let hdrEnabled = "punktfunk.hdrEnabled"
    /// Request a full-chroma 4:4:4 stream when this device can HARDWARE-decode it (`Stage444Probe`).
    /// On by default; only takes effect when the host also opted in to 4:4:4 (otherwise the stream
    /// stays 4:2:0). Sharper text/UI at the cost of more bandwidth.
    public static let enable444 = "punktfunk.enable444"
    public static let hosts = "punktfunk.hosts"
    /// Client-side cursor mode: "auto" (shown only in gamescope sessions), "always", "never".
    public static let cursorMode = "punktfunk.cursorMode"
    /// iPad: capture the mouse/trackpad pointer (pointer lock → relative movement) for games,
    /// rather than forwarding an absolute cursor position. On by default. Only meaningful on iPad
    /// with a hardware mouse/trackpad; the system grants the lock only to a full-screen, frontmost
    /// scene and silently falls back to the absolute pointer when it can't (Stage Manager / Slide
    /// Over). Read by `StreamViewController.prefersPointerLocked`.
    public static let pointerCapture = "punktfunk.pointerCapture"
    /// iPhone/iPad: how touchscreen fingers drive the host — a `TouchInputMode` raw value:
    /// "trackpad" (default: relative cursor with tap-click / two-finger-scroll gestures),
    /// "pointer" (the cursor jumps to the finger), or "touch" (real multi-touch passthrough).
    /// Read live per gesture by `StreamLayerUIView`.
    public static let touchMode = "punktfunk.touchMode"
    /// Experimental: show the host's game library (browsed over the management API). Off by default.
    public static let libraryEnabled = "punktfunk.libraryEnabled"
    /// macOS: take the window fullscreen while streaming and restore it on the host list. On by default.
    public static let fullscreenWhileStreaming = "punktfunk.fullscreenWhileStreaming"
    /// Show the streaming statistics overlay (mode/fps/throughput/latency). On by default; toggle
    /// while streaming with ⌃⌥⇧S (the cross-client Ctrl+Alt+Shift+S; macOS / hardware keyboard).
    public static let hudEnabled = "punktfunk.hudEnabled"
    /// Which corner the statistics overlay sits in — a `HUDPlacement` raw value
    /// ("topLeading"/"topTrailing"/"bottomLeading"/"bottomTrailing"). Default top-trailing.
    public static let hudPlacement = "punktfunk.hudPlacement"
    /// iOS/iPadOS/macOS: switch the host list, settings and game library to a controller-friendly
    /// layout (the console launcher, gamepad-navigable settings, a coverflow-style library)
    /// whenever a gamepad is connected. On by default; see `GamepadUIEnvironment.isActive`.
    public static let gamepadUIEnabled = "punktfunk.gamepadUIEnabled"
}

extension Notification.Name {
    /// Posted by the app's Stream menu ("Release Mouse", ⌃⌥⇧Q): the key window's stream view
    /// releases input capture if it holds it. Only reachable while NOT captured (a captured
    /// session swallows the combo in InputCapture's monitor and the frozen cursor can't click
    /// menus) — it exists so the menu item is honest whenever it CAN fire, and as the shortcut's
    /// discoverable menu-bar surface.
    public static let punktfunkReleaseCapture = Notification.Name("io.unom.punktfunk.release-capture")
}
