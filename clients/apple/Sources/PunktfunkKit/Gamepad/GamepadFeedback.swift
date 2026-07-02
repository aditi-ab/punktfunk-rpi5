// Host→client gamepad feedback rendering: one drain thread polls the rumble (0xCA) and
// HID-output (0xCD) planes and replays them on the active physical controller —
//
//   rumble        → CHHapticEngine players (per-handle localities when the pad has them,
//                   one combined engine otherwise),
//   lightbar      → GCDeviceLight,
//   player LEDs   → GCController.playerIndex (the DS bit patterns map to player 1–4),
//   trigger FX    → DualSenseTriggerEffect.parse → GCDualSenseAdaptiveTrigger.
//
// Only pad 0 is rendered (exactly one controller is forwarded). HID-output traffic exists
// only on PlayStation-pad sessions (a DualSense, or a DualShock 4 = lightbar only) — the
// drain always polls both planes with short timeouts and never spins, so an Xbox session
// just renders rumble. GameController profile mutation
// happens on main; CHHapticEngine work on its own serial queue; the drain thread itself
// touches neither. When GamepadManager switches the active controller mid-session, the
// old pad is reset (triggers off, player index unset) and the last known feedback state
// is replayed onto the new one.

import Combine
import Foundation
import GameController

public final class GamepadFeedback {
    private let connection: PunktfunkConnection
    private let flag = StopFlag()
    private let drainDone = DispatchSemaphore(value: 0)
    private var drainStarted = false
    private let rumble = RumbleRenderer(policy: .session)
    private var activeSub: AnyCancellable?

    // Last applied feedback (main-actor) — replayed when the active controller changes.
    @MainActor private var target: GCController?
    @MainActor private var lastLight: (r: UInt8, g: UInt8, b: UInt8)?
    @MainActor private var lastPlayerBits: UInt8?
    @MainActor private var lastTrigger: [DualSenseTriggerEffect?] = [nil, nil]

    public init(connection: PunktfunkConnection, manager: GamepadManager) {
        self.connection = connection
        // Capture self weakly in the hop too, so the inner sink's weak capture isn't shadowing
        // an implicit strong one — and the subscription (stored on self) never retain-cycles.
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.activeSub = manager.$active.sink { [weak self] dc in
                MainActor.assumeIsolated { self?.retarget(dc?.controller) }
            }
        }
    }

    /// Safety net: the drain thread captures `connection` strongly and only `self` weakly, so if
    /// this is dropped without `stop()` (an abrupt teardown) the thread would poll forever and
    /// leak the connection — signal it to exit. (`stop()` is the normal path and also joins it.)
    deinit { flag.stop() }

    /// Map the DualSense player-LED bit patterns (5 LEDs, hid-playstation's player
    /// conventions) onto GCControllerPlayerIndex. Unknown patterns fall back to the lit
    /// count, clamped to the four indices GC offers.
    public static func playerIndex(forBits bits: UInt8) -> GCControllerPlayerIndex {
        switch bits & 0x1F {
        case 0: return .indexUnset
        case 0b00100: return .index1
        case 0b01010: return .index2
        case 0b10101: return .index3
        case 0b11011: return .index4
        default:
            let lit = (bits & 0x1F).nonzeroBitCount
            return GCControllerPlayerIndex(rawValue: min(lit, 4) - 1) ?? .index1
        }
    }

    public func start() {
        guard !drainStarted else { return }
        drainStarted = true
        // Hidout traffic (lightbar / player LEDs / triggers) only exists on a PlayStation-pad
        // session — a DualSense or a DualShock 4 (lightbar only). Block briefly on it there and
        // let rumble own the wait elsewhere; on an Xbox session it stays nonblocking.
        let thread = Thread { [connection, flag, drainDone, weak self] in
            while !flag.isStopped {
                do {
                    // Poll the feedback planes NON-BLOCKING. A blocking poll (timeoutMs > 0) holds
                    // the connection's shared feedback lock for its whole wait; the video pump drains
                    // HDR mastering metadata (nextHdrMeta) on the SAME lock every frame, so a blocking
                    // poll here starved it and throttled HDR to ~1 fps (SDR, which never drains HDR
                    // meta, was unaffected). Pacing with a short sleep OUTSIDE the lock (below) keeps
                    // rumble/HID latency low while leaving the lock free between polls.
                    //
                    // Rumble is idempotent state, so drain the plane DRY and apply only the newest
                    // level. The old one-datagram-per-cycle shape let a burst outpace the ~125 Hz
                    // drain: levels rendered up to ~130 ms late through the core's 16-deep queue,
                    // and its drop-newest overflow could shed a stop while stale nonzero states
                    // queued ahead of it — buzzing until the host's next 500 ms refresh.
                    var newest: (low: UInt16, high: UInt16)?
                    var rumbleBurst = 0
                    while rumbleBurst < 64, !flag.isStopped,
                          let r = try connection.nextRumble(timeoutMs: 0) {
                        if r.pad == 0 { newest = (r.low, r.high) }
                        rumbleBurst += 1
                    }
                    if let n = newest {
                        self?.rumble.apply(low: n.low, high: n.high)
                    }
                    // Drain a BOUNDED burst of hidout events so sustained 0xCD traffic (a game writing
                    // per-frame LED/trigger reports) can't spin here or block stop() past one cycle.
                    var burst = 0
                    while burst < 64, !flag.isStopped,
                          let ev = try connection.nextHidOutput(timeoutMs: 0) {
                        self?.render(ev)
                        burst += 1
                    }
                } catch {
                    break // .closed (or fatal) — the session is over
                }
                // ~8 ms poll cadence (≈125 Hz), slept OUTSIDE the feedback lock — low rumble/HID
                // latency without holding the lock the HDR-meta drain needs.
                if !flag.isStopped { Thread.sleep(forTimeInterval: 0.008) }
            }
            drainDone.signal()
        }
        thread.name = "punktfunk-feedback"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// Stop the drain and silence the motors. Blocks until the drain thread exits (≤ one
    /// poll cycle) — call off the main actor, before `connection.close()`.
    public func stop() {
        flag.stop()
        if drainStarted {
            drainDone.wait()
            drainStarted = false
        }
        rumble.stop()
        // Drop the retarget subscription and the dead session's cached feedback — a
        // controller change after teardown must not replay this session's triggers/LEDs.
        Task { @MainActor in
            self.activeSub = nil
            self.lastLight = nil
            self.lastPlayerBits = nil
            self.lastTrigger = [nil, nil]
            self.reset(self.target)
            self.target = nil
        }
    }

    private func render(_ ev: PunktfunkConnection.HidOutputEvent) {
        DispatchQueue.main.async {
            MainActor.assumeIsolated { self.apply(ev) }
        }
    }

    @MainActor
    private func apply(_ ev: PunktfunkConnection.HidOutputEvent) {
        switch ev {
        case let .led(pad, r, g, b):
            guard pad == 0 else { return }
            lastLight = (r, g, b)
            target?.light?.color = GCColor(
                red: Float(r) / 255, green: Float(g) / 255, blue: Float(b) / 255)
        case let .playerLEDs(pad, bits):
            guard pad == 0 else { return }
            lastPlayerBits = bits
            target?.playerIndex = Self.playerIndex(forBits: bits)
        case let .triggerEffect(pad, which, effect):
            guard pad == 0, which < 2 else { return }
            let parsed = DualSenseTriggerEffect.parse(effect)
            lastTrigger[Int(which)] = parsed
            if let trigger = adaptiveTrigger(which) {
                parsed.apply(to: trigger)
            }
        }
    }

    @MainActor
    private func retarget(_ controller: GCController?) {
        guard controller !== target else { return }
        reset(target)
        target = controller
        rumble.retarget(controller)
        // Replay the session's feedback state so a swapped-in controller looks the same.
        if let (r, g, b) = lastLight {
            controller?.light?.color = GCColor(
                red: Float(r) / 255, green: Float(g) / 255, blue: Float(b) / 255)
        }
        if let bits = lastPlayerBits {
            controller?.playerIndex = Self.playerIndex(forBits: bits)
        }
        for which in 0..<2 {
            if let effect = lastTrigger[which], let trigger = adaptiveTrigger(UInt8(which)) {
                effect.apply(to: trigger)
            }
        }
    }

    @MainActor
    private func reset(_ controller: GCController?) {
        guard let c = controller else { return }
        c.playerIndex = .indexUnset
        if let ds = c.extendedGamepad as? GCDualSenseGamepad {
            ds.leftTrigger.setModeOff()
            ds.rightTrigger.setModeOff()
        }
    }

    @MainActor
    private func adaptiveTrigger(_ which: UInt8) -> GCDualSenseAdaptiveTrigger? {
        guard let ds = target?.extendedGamepad as? GCDualSenseGamepad else { return nil }
        return which == 0 ? ds.leftTrigger : ds.rightTrigger
    }
}
