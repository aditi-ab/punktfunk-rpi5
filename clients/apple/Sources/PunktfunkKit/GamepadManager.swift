// Controller discovery + selection, app-lifetime. One GamepadManager (`.shared`) watches
// GCController connect/disconnect from launch, so the Settings page shows live controller
// state without a session, and the session components (GamepadCapture / GamepadFeedback)
// follow `active` — exactly ONE physical controller is forwarded to the host, as pad 0.
//
// Selection: the user can pin a controller in Settings (persisted under
// "punktfunk.gamepadID"); with no pin — or the pinned one absent — the most recently
// connected extended gamepad wins. GCController has no stable hardware serial, so the pin
// is a fingerprint of vendorName|productCategory (+ a connect-order suffix for twins);
// identical twin controllers may swap a pin across reconnects, which the Settings footer
// documents.
//
// A singleton (not a SwiftUI environment object) because macOS shows Settings in its own
// `Settings{}` scene — there is no common ancestor view to inject from.

import Combine
import Foundation
import GameController

@MainActor
public final class GamepadManager: ObservableObject {
    public static let shared = GamepadManager()

    /// One detected controller, decorated for the Settings UI.
    public struct DiscoveredController: Identifiable, Equatable {
        /// Stable-ish fingerprint: `vendorName|productCategory` (+ `#n` for twins).
        public let id: String
        /// User-facing name (the vendor string, e.g. "DualSense Wireless Controller").
        public let name: String
        public let productCategory: String
        /// The full extended profile exists — only these are forwardable.
        public let isExtended: Bool
        public let isDualSense: Bool
        public let hasLight: Bool
        public let hasHaptics: Bool
        public let hasMotion: Bool
        public let hasAdaptiveTriggers: Bool
        /// 0...1, nil when the controller doesn't report a battery (e.g. wired).
        public let batteryLevel: Float?
        public let isCharging: Bool
        public let controller: GCController

        public static func == (l: DiscoveredController, r: DiscoveredController) -> Bool {
            l.id == r.id && l.controller === r.controller
                && l.batteryLevel == r.batteryLevel && l.isCharging == r.isCharging
        }
    }

    /// Every detected controller, in connect order (Settings lists these).
    @Published public private(set) var controllers: [DiscoveredController] = []

    /// The one controller forwarded to the host (pad 0); nil when none qualifies.
    @Published public private(set) var active: DiscoveredController?

    /// The user's pinned controller fingerprint ("" = automatic). Persisted; updating it
    /// reselects immediately, so a Settings Picker can bind straight to this.
    @Published public var preferredID: String {
        didSet {
            UserDefaults.standard.set(preferredID, forKey: Self.preferredKey)
            reselect()
        }
    }

    private static let preferredKey = "punktfunk.gamepadID"
    /// Connect order (identity-keyed) — drives both twin de-dup suffixes and auto-pick.
    private var connectOrder: [ObjectIdentifier] = []
    private var observers: [NSObjectProtocol] = []

    private init() {
        preferredID = UserDefaults.standard.string(forKey: Self.preferredKey) ?? ""
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCControllerDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            MainActor.assumeIsolated {
                guard let self, let c = n.object as? GCController else { return }
                self.noteConnected(c)
            }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCControllerDidDisconnect, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.rebuild() }
        })
        for c in GCController.controllers() { connectOrder.append(ObjectIdentifier(c)) }
        rebuild()
    }

    /// Re-read battery levels etc. (the notifications only fire on connect/disconnect) —
    /// Settings calls this on appear.
    public func refresh() {
        rebuild()
    }

    /// Scan for nearby wireless controllers while the Settings page is visible.
    public func startDiscovery() {
        GCController.startWirelessControllerDiscovery()
    }

    public func stopDiscovery() {
        GCController.stopWirelessControllerDiscovery()
    }

    /// Connect-time resolution of the user's controller-type setting: an explicit choice
    /// wins; `.auto` matches the virtual pad to the active physical controller (DualSense →
    /// DualSense, anything else → Xbox 360); no controller at all defers to the host.
    public func resolveType(
        setting: PunktfunkConnection.GamepadType
    ) -> PunktfunkConnection.GamepadType {
        guard setting == .auto else { return setting }
        guard let active else { return .auto }
        return active.isDualSense ? .dualSense : .xbox360
    }

    private func noteConnected(_ c: GCController) {
        let key = ObjectIdentifier(c)
        connectOrder.removeAll { $0 == key }
        connectOrder.append(key)
        rebuild()
    }

    private func rebuild() {
        let present = GCController.controllers()
        connectOrder.removeAll { key in !present.contains { ObjectIdentifier($0) == key } }
        for c in present where !connectOrder.contains(ObjectIdentifier(c)) {
            connectOrder.append(ObjectIdentifier(c))
        }
        // In connect order, fingerprinting twins by their position among same-named pads.
        let ordered = connectOrder.compactMap { key in
            present.first { ObjectIdentifier($0) == key }
        }
        var seen: [String: Int] = [:]
        controllers = ordered.map { c in
            let base = "\(c.vendorName ?? "Controller")|\(c.productCategory)"
            let n = (seen[base] ?? 0) + 1
            seen[base] = n
            return Self.describe(c, id: n == 1 ? base : "\(base)#\(n)")
        }
        reselect()
    }

    private func reselect() {
        let candidates = controllers.filter(\.isExtended)
        // The pin wins when present; otherwise the most recently connected extended pad
        // (list is in connect order). A stale pin falls back to automatic.
        active = candidates.last { $0.id == preferredID } ?? candidates.last
    }

    private static func describe(_ c: GCController, id: String) -> DiscoveredController {
        let extended = c.extendedGamepad
        let ds = extended as? GCDualSenseGamepad
        return DiscoveredController(
            id: id,
            name: c.vendorName ?? c.productCategory,
            productCategory: c.productCategory,
            isExtended: extended != nil,
            isDualSense: ds != nil,
            hasLight: c.light != nil,
            hasHaptics: c.haptics != nil,
            hasMotion: c.motion != nil,
            // GCDualSenseGamepad's triggers are GCDualSenseAdaptiveTrigger by declaration.
            hasAdaptiveTriggers: ds != nil,
            batteryLevel: c.battery.flatMap { $0.batteryLevel >= 0 ? $0.batteryLevel : nil },
            isCharging: c.battery?.batteryState == .charging,
            controller: c)
    }
}
