// The in-stream quick-action ring's configuration — the `overlay_actions` setting, one JSON blob
// (schema v2, design/touch-client-overlay.md §3.2). The Swift twin of pf-client-core's
// `overlay_actions.rs`; the Rust tests are the contract, `OverlayActionsTests` ports them.
//
// Parsing never fails: fewer than six slots pad with empty, more are truncated, an unknown id or
// a dangling `shortcut:` reference is an empty slot, an absent field takes its default, and an
// unparseable blob is the platform default — profiles sync between client versions.

import Foundation

/// What a ring slot does. `host` carries a host-advertised action id; `shortcut` refers into
/// `OverlayConfig.shortcuts` by id.
public enum SlotId: Equatable, Sendable {
    case endStream, disconnectLinger, touchMode, keyboard, stats, mic, pad, sendText
    case host(String)
    case shortcut(String)

    /// The wire id, the inverse of `parse`.
    public var id: String {
        switch self {
        case .endStream: return "end_stream"
        case .disconnectLinger: return "disconnect_linger"
        case .touchMode: return "touch_mode"
        case .keyboard: return "keyboard"
        case .stats: return "stats"
        case .mic: return "mic"
        case .pad: return "pad"
        case .sendText: return "send_text"
        case .host(let id): return "host:\(id)"
        case .shortcut(let id): return "shortcut:\(id)"
        }
    }

    /// An id from the blob; `nil` for one this build does not know (an empty slot).
    public static func parse(_ s: String) -> SlotId? {
        switch s {
        case "end_stream": return .endStream
        case "disconnect_linger": return .disconnectLinger
        case "touch_mode": return .touchMode
        case "keyboard": return .keyboard
        case "stats": return .stats
        case "mic": return .mic
        case "pad": return .pad
        case "send_text": return .sendText
        default:
            if s.hasPrefix("host:"), s.count > 5 { return .host(String(s.dropFirst(5))) }
            if s.hasPrefix("shortcut:"), s.count > 9 { return .shortcut(String(s.dropFirst(9))) }
            return nil
        }
    }
}

/// A custom key chord. `keys` are names from the shared keymap tables, never raw key codes.
public struct OverlayShortcut: Equatable, Sendable {
    public var id: String
    public var label: String
    public var keys: [String]

    public init(id: String, label: String = "", keys: [String] = []) {
        self.id = id
        self.label = label
        self.keys = keys
    }
}

/// The virtual controller's preset: `layout` is `full`, `sticks` or `dpad`.
public struct PadConfig: Equatable, Sendable {
    public var layout = "full"
    public var opacity: Float = 0.45
    public var scale: Float = 1

    public init(layout: String = "full", opacity: Float = 0.45, scale: Float = 1) {
        self.layout = layout
        self.opacity = opacity
        self.scale = scale
    }
}

/// Which platform default ring applies: iOS and iPadOS are `touch`; macOS is `desktop`.
public enum RingPlatform: Sendable {
    case touch, desktop
}

public struct OverlayConfig: Equatable, Sendable {
    public static let ringSlots = 6
    public static let schemaVersion = 2

    /// Exactly `ringSlots` entries, clockwise from 12 o'clock; `nil` is an empty slot.
    public var ring: [SlotId?]
    public var shortcuts: [OverlayShortcut]
    public var pad: PadConfig

    public init(ring: [SlotId?], shortcuts: [OverlayShortcut] = [], pad: PadConfig = PadConfig()) {
        self.ring = ring
        self.shortcuts = shortcuts
        self.pad = pad
    }

    public func shortcut(_ id: String) -> OverlayShortcut? {
        shortcuts.first { $0.id == id }
    }

    public static func platformDefault(_ platform: RingPlatform = .touch) -> OverlayConfig {
        switch platform {
        case .touch:
            return OverlayConfig(ring: [.endStream, .keyboard, .touchMode, .stats, .mic, .pad])
        case .desktop:
            return OverlayConfig(ring: [.endStream, .disconnectLinger, .touchMode, .stats, .mic, .sendText])
        }
    }

    /// Parse the setting; an empty or unparseable blob is the platform default.
    public static func parse(_ json: String?, platform: RingPlatform = .touch) -> OverlayConfig {
        guard let json, !json.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              let data = json.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return platformDefault(platform) }
        let shortcuts: [OverlayShortcut] = ((obj["shortcuts"] as? [[String: Any]]) ?? []).compactMap { s in
            guard let id = s["id"] as? String, !id.isEmpty else { return nil }
            return OverlayShortcut(
                id: id, label: s["label"] as? String ?? "", keys: s["keys"] as? [String] ?? [])
        }
        let ringIn = obj["ring"] as? [Any] ?? []
        let ring: [SlotId?] = (0..<ringSlots).map { i in
            guard i < ringIn.count, let s = ringIn[i] as? String, let slot = SlotId.parse(s) else {
                return nil
            }
            if case .shortcut(let id) = slot, !shortcuts.contains(where: { $0.id == id }) { return nil }
            return slot
        }
        let padIn = obj["pad"] as? [String: Any] ?? [:]
        var pad = PadConfig()
        if let l = padIn["layout"] as? String, !l.isEmpty { pad.layout = l }
        if let o = padIn["opacity"] as? Double { pad.opacity = Float(o) }
        if let s = padIn["scale"] as? Double { pad.scale = Float(s) }
        return OverlayConfig(ring: ring, shortcuts: shortcuts, pad: pad)
    }

    /// The blob to store — always the current schema version.
    public func toJSON() -> String {
        let obj: [String: Any] = [
            "v": Self.schemaVersion,
            "ring": ring.map { $0.map { $0.id as Any } ?? NSNull() },
            "shortcuts": shortcuts.map { ["id": $0.id, "label": $0.label, "keys": $0.keys] },
            "pad": ["layout": pad.layout, "opacity": Double(pad.opacity), "scale": Double(pad.scale)],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys]),
              let text = String(data: data, encoding: .utf8)
        else { return "" } // plain data always serialises
        return text
    }
}
