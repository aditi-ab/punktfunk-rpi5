// The option lists every settings surface renders from — one source of truth shared by the
// touch/desktop SettingsView (Pickers), the tvOS pushed selection rows, and the gamepad settings
// screen (GamepadSettingsView's left/right cycling). Pure data + small pure helpers; anything that
// reads live view state (e.g. the bitrate slider mapping) stays on SettingsView.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

enum SettingsOptions {
    /// Compositor choices — the `tag` is the wire value (`PunktfunkConnection.Compositor` raw).
    static let compositors: [(label: String, tag: Int)] = [
        ("Automatic", 0),
        ("KWin (KDE Plasma)", 1),
        ("wlroots (Sway / Hyprland)", 2),
        ("Mutter (GNOME)", 3),
        ("gamescope", 4),
    ]

    static let audioChannels: [(label: String, tag: Int)] = [
        ("Stereo", 2),
        ("5.1 Surround", 6),
        ("7.1 Surround", 8),
    ]

    /// Virtual-pad types — the `tag` is the wire value (`PunktfunkConnection.GamepadType` raw).
    static let padTypes: [(label: String, tag: Int)] = [
        ("Automatic", 0),
        ("Xbox 360", 1),
        ("Xbox One", 3),
        ("DualSense", 2),
        ("DualShock 4", 4),
    ]

    static let hudPlacements: [(label: String, tag: String)] =
        HUDPlacement.allCases.map { ($0.label, $0.rawValue) }

    /// Video-codec preference (`DefaultsKey.codec`) — a soft preference the host falls back from.
    /// AV1 appears only on devices with an AV1 hardware decoder (the same
    /// `AV1.hardwareDecodeSupported` gate SessionModel advertises by) — elsewhere it would be a
    /// dead setting the host could never honor. Ordered by the host's resolve precedence
    /// (HEVC > AV1 > H.264).
    static let codecs: [(label: String, tag: String)] = {
        var options: [(label: String, tag: String)] = [
            ("Automatic", "auto"),
            ("HEVC (H.265)", "hevc"),
            ("H.264 (AVC)", "h264"),
        ]
        if AV1.hardwareDecodeSupported {
            options.insert(("AV1", "av1"), at: 2)
        }
        return options
    }()

    // MARK: - Bitrate

    /// Discrete bitrate steps for the surfaces with no Slider (tvOS pushed pickers, the gamepad
    /// settings' left/right cycling), up to the same 3 Gbps ceiling the slider has.
    static let bitratePresets: [(label: String, tag: Int)] = [
        ("Automatic", 0),
        ("10 Mbps", 10_000),
        ("20 Mbps", 20_000),
        ("40 Mbps", 40_000),
        ("80 Mbps", 80_000),
        ("150 Mbps", 150_000),
        ("300 Mbps", 300_000),
        ("500 Mbps", 500_000),
        ("1 Gbps", 1_000_000),
        ("1.5 Gbps", 1_500_000),
        ("2 Gbps", 2_000_000),
        ("3 Gbps", 3_000_000),
    ]

    /// The presets plus the currently stored value when it isn't one of them (set via the touch
    /// slider or a synced device) — so the current choice stays visible/selectable.
    static func bitrateOptions(current: Int) -> [(label: String, tag: Int)] {
        var options = bitratePresets
        if !options.contains(where: { $0.tag == current }) {
            options.insert(
                (SpeedTestSheet.mbpsLabel(kbps: current) + " (custom)", current), at: 1)
        }
        return options
    }

    // MARK: - Controllers

    /// "Use controller" choices: Automatic, every forwardable controller, and — so a stale pin
    /// stays visible instead of leaving the selection tag-less — any pinned id that is NOT among
    /// the selectable (extended) entries, present-but-unusable included.
    @MainActor
    static func controllerOptions(_ gamepads: GamepadManager) -> [(label: String, tag: String)] {
        let selectable = gamepads.controllers.filter(\.isExtended)
        var options: [(label: String, tag: String)] = [("Automatic", "")]
        options += selectable.map { ($0.name, $0.id) }
        if !gamepads.preferredID.isEmpty,
           !selectable.contains(where: { $0.id == gamepads.preferredID }) {
            options.append(("Unavailable controller", gamepads.preferredID))
        }
        return options
    }

    #if os(iOS) || os(macOS)
    // MARK: - Stream mode (iOS + macOS pickers; tvOS builds its own preset list)

    /// 16:9 then ultrawide presets; the device's native mode is prepended by `resolutionModes`.
    static let resolutionPresets: [(name: String, w: Int, h: Int)] = [
        ("720p", 1280, 720),
        ("1080p", 1920, 1080),
        ("1440p", 2560, 1440),
        ("4K", 3840, 2160),
        ("Ultrawide 1080p", 2560, 1080),
        ("Ultrawide 1440p", 3440, 1440),
        ("Super ultrawide", 5120, 1440),
    ]

    /// This device's native mode first, then the presets, deduped by dimensions (native wins a
    /// tie).
    @MainActor
    static func resolutionModes() -> [(name: String, w: Int, h: Int)] {
        var native: [(name: String, w: Int, h: Int)] = []
        #if os(iOS)
        let bounds = UIScreen.main.nativeBounds // portrait-oriented pixels
        native = [("This device",
                   Int(max(bounds.width, bounds.height)),
                   Int(min(bounds.width, bounds.height)))]
        #else
        if let screen = NSScreen.main {
            let scale = screen.backingScaleFactor
            native = [("This display",
                       Int(screen.frame.width * scale),
                       Int(screen.frame.height * scale))]
        }
        #endif
        var seen = Set<String>()
        return (native + resolutionPresets).filter { seen.insert("\($0.w)x\($0.h)").inserted }
    }

    /// Refresh rates the device can actually display (no point asking the host to render frames
    /// the screen can't show), plus any stored custom value so it stays selectable.
    @MainActor
    static func refreshRates(including current: Int) -> [Int] {
        #if os(iOS)
        let maxHz = UIScreen.main.maximumFramesPerSecond
        #else
        let maxHz = NSScreen.main?.maximumFramesPerSecond ?? 60
        #endif
        var rates = [60, 120, 240].filter { $0 <= maxHz }
        if rates.isEmpty { rates = [maxHz] }
        if !rates.contains(current) { rates.append(current) }
        return rates.sorted()
    }
    #endif
}
