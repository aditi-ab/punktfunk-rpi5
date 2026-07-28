// The settings ONE session runs on — the global defaults with the session's profile overlaid,
// resolved once at connect and read from there on (design/client-settings-profiles.md §4.2/§4.4):
//
//     effective = overlay(profile).apply(globals)
//     profile   = one-off pick (Connect with ▸)  ??  host.profileID  ??  none
//
// Before this existed, ~10 sites scattered across the app AND the kit read `UserDefaults` directly
// mid-session — a per-host profile would have applied to some of them and not others, which is
// worse than not shipping the feature. They now read `SessionSettings.current`: the live session's
// resolution while one is up, the plain globals otherwise (byte-for-byte today's behaviour when no
// profile is involved).
//
// Only SESSION-CONSUMED values live here. Pure app-level preferences — the library toggle, the
// gamepad-UI switch, HUD placement, auto-wake, background keep-alive — stay plain `@AppStorage`
// where they are read: they are about the app, not about a stream.

import Foundation

public struct EffectiveSettings: Equatable, Sendable {
    // Tier P — profileable (design §3).
    public var width = 1920
    public var height = 1080
    public var refreshHz = 60
    public var matchWindow = false
    public var bitrateKbps = 0
    public var renderScale = 1.0
    public var codec = "auto"
    public var hdrEnabled = true
    public var compositor = 0
    public var audioChannels = 2
    public var micEnabled = true
    public var touchMode = "trackpad"
    public var mouseMode = "capture"
    public var invertScroll = false
    public var gamepadType = 0
    /// A `StatsVerbosity` raw value; the enum lives in PunktfunkKit, which this module can't see.
    public var statsVerbosity = "normal"
    public var fullscreenWhileStreaming = true
    public var enable444 = false
    public var presentPriority = "latency"
    public var smoothBuffer = 0
    public var vsync = false
    public var allowVRR = true
    public var windowedSafePresent = true
    public var modifierLayout = "mac"
    // Tier G — this device's endpoints and hardware. Session-consumed, so they ride along, but
    // never profileable: a profile is about how a host is streamed, not about which speaker this
    // Mac uses.
    public var speakerUID = ""
    public var micUID = ""
    public var micChannel = 0
    public var pointerCapture = true
    /// The profile this resolution came from, when one applied — the HUD names it so "which
    /// profile am I on?" is answerable mid-session, and the one-off/binding distinction never has
    /// to be guessed from the settings themselves.
    public var profileID: String?
    public var profileName: String?

    public init() {}

    /// The global defaults, as every `@AppStorage` in the settings surface sees them. `.standard`
    /// by default — settings are per-device, unlike the App-Group-shared host store.
    public init(defaults: UserDefaults) {
        func int(_ key: String, _ fallback: Int) -> Int {
            defaults.object(forKey: key) as? Int ?? fallback
        }
        func dbl(_ key: String, _ fallback: Double) -> Double {
            defaults.object(forKey: key) as? Double ?? fallback
        }
        func bool(_ key: String, _ fallback: Bool) -> Bool {
            defaults.object(forKey: key) as? Bool ?? fallback
        }
        func str(_ key: String, _ fallback: String) -> String {
            defaults.string(forKey: key) ?? fallback
        }
        width = int(DefaultsKey.streamWidth, width)
        height = int(DefaultsKey.streamHeight, height)
        refreshHz = int(DefaultsKey.streamHz, refreshHz)
        matchWindow = bool(DefaultsKey.matchWindow, matchWindow)
        bitrateKbps = int(DefaultsKey.bitrateKbps, bitrateKbps)
        renderScale = dbl(DefaultsKey.renderScale, renderScale)
        codec = str(DefaultsKey.codec, codec)
        hdrEnabled = bool(DefaultsKey.hdrEnabled, hdrEnabled)
        compositor = int(DefaultsKey.compositor, compositor)
        audioChannels = int(DefaultsKey.audioChannels, audioChannels)
        micEnabled = bool(DefaultsKey.micEnabled, micEnabled)
        touchMode = str(DefaultsKey.touchMode, touchMode)
        mouseMode = str(DefaultsKey.mouseMode, mouseMode)
        invertScroll = bool(DefaultsKey.invertScroll, invertScroll)
        gamepadType = int(DefaultsKey.gamepadType, gamepadType)
        statsVerbosity = Self.storedStatsVerbosity(defaults)
        fullscreenWhileStreaming = bool(
            DefaultsKey.fullscreenWhileStreaming, fullscreenWhileStreaming)
        enable444 = bool(DefaultsKey.enable444, enable444)
        presentPriority = str(DefaultsKey.presentPriority, presentPriority)
        smoothBuffer = int(DefaultsKey.smoothBuffer, smoothBuffer)
        vsync = bool(DefaultsKey.vsync, vsync)
        allowVRR = bool(DefaultsKey.allowVRR, allowVRR)
        windowedSafePresent = bool(DefaultsKey.windowedSafePresent, windowedSafePresent)
        modifierLayout = str(DefaultsKey.modifierLayout, modifierLayout)
        speakerUID = str(DefaultsKey.speakerUID, speakerUID)
        micUID = str(DefaultsKey.micUID, micUID)
        micChannel = int(DefaultsKey.micChannel, micChannel)
        pointerCapture = bool(DefaultsKey.pointerCapture, pointerCapture)
    }

    /// The stats tier as stored, with the pre-tier `hudEnabled` migration `StatsVerbosity.current`
    /// performs — duplicated in one line here so this module needn't reach into PunktfunkKit.
    private static func storedStatsVerbosity(_ defaults: UserDefaults) -> String {
        if let raw = defaults.string(forKey: DefaultsKey.statsVerbosity) { return raw }
        if let legacy = defaults.object(forKey: DefaultsKey.hudEnabled) as? Bool, !legacy {
            return "off"
        }
        return "normal"
    }

    /// The one resolution seam: this overlay on top of these settings. Pure — no store reads, no
    /// clock — so it is testable field by field. A `.some` that happens to equal the base is a
    /// legitimate PIN: it keeps its value when the global later moves.
    public func applying(_ overlay: SettingsOverlay) -> EffectiveSettings {
        var s = self
        if let v = overlay.width { s.width = v }
        if let v = overlay.height { s.height = v }
        if let v = overlay.refreshHz { s.refreshHz = v }
        if let v = overlay.matchWindow { s.matchWindow = v }
        if let v = overlay.bitrateKbps { s.bitrateKbps = v }
        if let v = overlay.renderScale { s.renderScale = v }
        if let v = overlay.codec { s.codec = v }
        if let v = overlay.hdrEnabled { s.hdrEnabled = v }
        if let v = overlay.compositor { s.compositor = v }
        if let v = overlay.audioChannels { s.audioChannels = v }
        if let v = overlay.micEnabled { s.micEnabled = v }
        if let v = overlay.touchMode { s.touchMode = v }
        if let v = overlay.mouseMode { s.mouseMode = v }
        if let v = overlay.invertScroll { s.invertScroll = v }
        if let v = overlay.gamepadType { s.gamepadType = v }
        if let v = overlay.statsVerbosity { s.statsVerbosity = v }
        if let v = overlay.fullscreenWhileStreaming { s.fullscreenWhileStreaming = v }
        if let v = overlay.enable444 { s.enable444 = v }
        if let v = overlay.presentPriority { s.presentPriority = v }
        if let v = overlay.smoothBuffer { s.smoothBuffer = v }
        if let v = overlay.vsync { s.vsync = v }
        if let v = overlay.allowVRR { s.allowVRR = v }
        if let v = overlay.windowedSafePresent { s.windowedSafePresent = v }
        if let v = overlay.modifierLayout { s.modifierLayout = v }
        return s
    }

    /// The whole resolution, in one call, in the precedence every client shares:
    /// **one-off pick ?? host binding ?? none**. A dangling id — a profile deleted out from under
    /// a binding, a link naming one that no longer exists — resolves as none: never an error,
    /// never a blocked connect (§4.4).
    public static func resolve(
        host: StoredHost?, selection: ProfileSelection = .inherit,
        catalog: ProfileCatalog? = nil, defaults: UserDefaults = .standard
    ) -> EffectiveSettings {
        let base = EffectiveSettings(defaults: defaults)
        let profile: StreamProfile? = {
            switch selection {
            case .defaults:
                return nil
            case .profile(let id):
                return (catalog ?? ProfileCatalog.load()).profile(id: id)
            case .inherit:
                guard let host else { return nil }
                return (catalog ?? ProfileCatalog.load()).binding(for: host)
            }
        }()
        guard let profile else { return base }
        var out = base.applying(profile.overrides)
        out.profileID = profile.id
        out.profileName = profile.name
        return out
    }
}

/// What a single connect was told to use, before any store is consulted.
///
/// The third case is why this is an enum rather than an `Optional<StreamProfile>`: "Connect with ▸
/// Default settings" on a BOUND host has to force the globals, and "no pick at all" has to fall
/// through to the binding. Collapsing the two would make the menu item that says "Default
/// settings" silently connect with the host's profile. It is the same distinction the session
/// binary's `--profile ""` reserves on the desktop clients.
public enum ProfileSelection: Equatable, Sendable {
    /// No pick — the host's default binding applies (a plain click/tap).
    case inherit
    /// Force the global defaults for this one connect, whatever the host is bound to.
    case defaults
    /// This profile, for this one connect. NEVER rebinds the host (§5.2).
    case profile(String)

    public init(profileID: String?) {
        self = profileID.map(ProfileSelection.profile) ?? .inherit
    }
}

// MARK: - The live session's resolution

/// What a session-scoped reader should read instead of `UserDefaults`.
///
/// A plain `static var` would be the obvious shape, but these are read from the decode pump and
/// the presenter's display-link callback as well as the main actor, so the value sits behind a
/// lock — the `FrameMeter` pattern used elsewhere in the client.
public enum SessionSettings {
    private final class Box: @unchecked Sendable {
        private let lock = NSLock()
        private var value: EffectiveSettings?

        var active: EffectiveSettings? {
            get { lock.withLock { value } }
            set { lock.withLock { value = newValue } }
        }
    }

    private static let box = Box()

    /// The live session's resolution, or nil while idle.
    public static var active: EffectiveSettings? { box.active }

    /// What every session-scoped reader uses: the live session's resolution while one is up, the
    /// plain globals otherwise. Reading it off-session is exactly what those sites did before.
    public static var current: EffectiveSettings {
        box.active ?? EffectiveSettings(defaults: .standard)
    }

    /// Latch the settings a starting session resolved. Called once per connect, before the
    /// connection exists, so nothing reads a half-applied mix.
    public static func begin(_ settings: EffectiveSettings) {
        box.active = settings
    }

    /// Release the latch when the session ends — later reads fall back to the globals.
    public static func end() {
        box.active = nil
    }

    /// The one value that legitimately moves mid-session: the stats tier, which every client
    /// cycles live (⌃⌥⇧S, the three-finger tap). The cycle writes the global as before AND moves
    /// the session's own value, so cycling works in a profile-driven session too.
    public static func setStatsVerbosity(_ raw: String) {
        guard var s = box.active else { return }
        s.statsVerbosity = raw
        box.active = s
    }
}

private extension NSLock {
    func withLock<T>(_ body: () -> T) -> T {
        lock()
        defer { unlock() }
        return body()
    }
}
