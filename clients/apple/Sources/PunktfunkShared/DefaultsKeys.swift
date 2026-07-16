// One source of truth for the client's UserDefaults / @AppStorage keys. A magic-string key
// duplicated across a setting's writer (a Settings @AppStorage) and reader (e.g. a stream view
// reading UserDefaults) splits silently on a typo — the setting just stops taking effect. These
// live in the dependency-free PunktfunkShared module (re-exported by PunktfunkKit) because the app,
// the kit's views, AND the widget extension all read them — the widget needs `DefaultsKey.hosts`.

import Foundation

/// Persisted-setting keys. The string VALUES are stable on disk — rename the symbol freely, but
/// never the string (it would orphan everyone's saved value).
public enum DefaultsKey {
    public static let streamWidth = "punktfunk.width"
    public static let streamHeight = "punktfunk.height"
    public static let streamHz = "punktfunk.hz"
    /// Match-window resolution policy (design/midstream-resolution-resize.md D1/D2): when on, the
    /// stream mode FOLLOWS the session view — the connect asks for the view's pixel size and a
    /// mid-session resize (a windowed macOS window, an iPad Stage Manager / Split View scene)
    /// renegotiates the host's virtual display + encoder (`PunktfunkConnection.requestMode`), so a
    /// windowed session streams native-resolution pixels instead of scaling. Off (default): the
    /// explicit `streamWidth`/`streamHeight` are used and never auto-resized (a fullscreen session
    /// is native either way, so this degenerates to Auto-native there). Read per session by the
    /// stream views' `MatchWindowFollower`.
    public static let matchWindow = "punktfunk.matchWindow"
    public static let compositor = "punktfunk.compositor"
    public static let gamepadType = "punktfunk.gamepadType"
    public static let gamepadID = "punktfunk.gamepadID"
    public static let bitrateKbps = "punktfunk.bitrateKbps"
    /// Requested audio channel count: 2 (stereo), 6 (5.1) or 8 (7.1). The host clamps to what it
    /// can capture; the resolved count drives the in-core decode + AVAudioEngine layout.
    public static let audioChannels = "punktfunk.audioChannels"
    /// Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, `"av1"`, or
    /// `"pyrowave"` (the opt-in wired-LAN wavelet codec — picking it advertises AND prefers it,
    /// and forces the session SDR). A soft preference — the host emits it when it can, else
    /// falls back. Drives the decoder via `Welcome.codec`.
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
    /// Which presenter runs a session: "stage2" (default — explicit decode + Metal present on
    /// frame arrival), "stage3" (same pipeline, glass-gated present pacing — the experimental
    /// low-display-latency A/B; see Stage2Pipeline's PresentPacing), or "stage1" (DEBUG-only
    /// system-layer fallback). Resolved once per session by SessionPresenter;
    /// PUNKTFUNK_PRESENTER=stage1|stage2|stage3 overrides it for A/B.
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
    /// LEGACY (pre-tiered overlay): the old boolean stats-overlay toggle. Kept ONLY as the
    /// migration fallback `StatsVerbosity.current` reads when `statsVerbosity` was never
    /// written (absent-or-true → .normal, explicit false → .off). Never written anymore.
    public static let hudEnabled = "punktfunk.hudEnabled"
    /// The statistics overlay tier — a `StatsVerbosity` raw value ("off"/"compact"/"normal"/
    /// "detailed"). Absent → migrated from the legacy `hudEnabled` bool (see above). Cycle it
    /// while streaming with ⌃⌥⇧S (the cross-client Ctrl+Alt+Shift+S; macOS / hardware
    /// keyboard) or a three-finger tap (touch), matching the Android client.
    public static let statsVerbosity = "punktfunk.statsVerbosity"
    /// Which corner the statistics overlay sits in — a `HUDPlacement` raw value
    /// ("topLeading"/"topTrailing"/"bottomLeading"/"bottomTrailing"). Default top-trailing.
    public static let hudPlacement = "punktfunk.hudPlacement"
    /// iOS/iPadOS/macOS: switch the host list, settings and game library to a controller-friendly
    /// layout (the console launcher, gamepad-navigable settings, a coverflow-style library)
    /// whenever a gamepad is connected. On by default; see `GamepadUIEnvironment.isActive`.
    public static let gamepadUIEnabled = "punktfunk.gamepadUIEnabled"
    /// iPhone: ALSO play the rumble the host addresses to controller 1 (wire pad 0) on this
    /// device's own Taptic Engine — for phone-clip pads that ship without rumble motors, where
    /// the phone body is the only actuator in the player's hands. Off by default (opt-in); read
    /// once per session by `GamepadFeedback`. The toggle is shown only where the device actually
    /// has a haptic actuator (no iPad/Mac/TV).
    public static let rumbleOnDevice = "punktfunk.rumbleOnDevice"
    /// Auto-wake on connect: when connecting to a saved host that isn't advertising on mDNS, fire
    /// Wake-on-LAN and, if the dial fails, wait for it to come back before retrying (the "Waking…"
    /// overlay). On by default. Turn off if a host that's already on just isn't seen on mDNS (a
    /// routed/VPN host), so connects go straight through instead of waiting out the wake timeout.
    /// The explicit "Wake Host" action stays available regardless. Read by ContentView.startSession.
    public static let autoWake = "punktfunk.autoWake"
    /// iOS/iPadOS: keep a streaming session ALIVE when the app is backgrounded (audio background
    /// mode). Off by default (today's freeze-on-background is the default). When on, backgrounding a
    /// live session keeps audio playing and the QUIC/pump live while DROPPING video decode, and a
    /// bounded timer (`backgroundTimeoutMinutes`) auto-disconnects if the user doesn't return. Read
    /// by ContentView's scenePhase driver. Hidden on tvOS/macOS.
    public static let backgroundKeepAlive = "punktfunk.backgroundKeepAlive"
    /// iOS/iPadOS: minutes a backgrounded keep-alive session runs before auto-disconnecting (a
    /// battery/thermal/bandwidth backstop). Default 10; the UI offers 1/5/10/30. The auto-disconnect
    /// is non-deliberate (host linger kept), so a late return reconnects fast. Read on enterBackground.
    public static let backgroundTimeoutMinutes = "punktfunk.backgroundTimeoutMinutes"
}

extension Notification.Name {
    /// Posted by the app's Stream menu ("Release Mouse", ⌃⌥⇧Q): the key window's stream view
    /// releases input capture if it holds it. Only reachable while NOT captured (a captured
    /// session swallows the combo in InputCapture's monitor and the frozen cursor can't click
    /// menus) — it exists so the menu item is honest whenever it CAN fire, and as the shortcut's
    /// discoverable menu-bar surface.
    public static let punktfunkReleaseCapture = Notification.Name("io.unom.punktfunk.release-capture")

    /// Posted by the Live Activity's / Shortcuts' End-stream intent (`EndStreamIntent.perform`,
    /// which runs in the app's process): the app tears the active session down deliberately
    /// (quit-close the host). Same cross-process-signal pattern as `punktfunkReleaseCapture` —
    /// the intent lives in PunktfunkShared and can't reach the app's `SessionModel` directly.
    public static let punktfunkEndActiveSession = Notification.Name("io.unom.punktfunk.end-active-session")

    /// Posted by the Connect App Intent (Siri/Shortcuts) with a `punktfunk://` URL as `object`:
    /// the app routes it through the SAME `.onOpenURL` handler a widget tap uses (one router, one
    /// set of guards). The intent uses `openAppWhenRun`, so the app is foregrounded to receive it.
    public static let punktfunkOpenDeepLink = Notification.Name("io.unom.punktfunk.open-deep-link")
}
