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
// only on DualSense sessions — the drain always polls both planes with short timeouts and
// never spins, so an Xbox session just renders rumble. GameController profile mutation
// happens on main; CHHapticEngine work on its own serial queue; the drain thread itself
// touches neither. When GamepadManager switches the active controller mid-session, the
// old pad is reset (triggers off, player index unset) and the last known feedback state
// is replayed onto the new one.

import Combine
import CoreHaptics
import Foundation
import GameController
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "gamepad")

private final class FeedbackStopFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var stopped = false
    var isStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }
    func stop() {
        lock.lock()
        stopped = true
        lock.unlock()
    }
}

/// Rumble → CoreHaptics, isolated on one serial queue (CHHapticEngine is not main-bound,
/// but it isn't a free-for-all either). Engines are created lazily on the first nonzero
/// amplitude and torn down on retarget; players run only while their motor is on, so an
/// idle controller costs no radio traffic. Failures (pads without haptics, engine resets)
/// downgrade to silence — rumble is best-effort by design.
///
/// `@unchecked Sendable` is sound because every property (`controller`/`low`/`high`/`broken`) is
/// read and written only inside `queue` closures — the serial queue is the synchronization.
private final class RumbleRenderer: @unchecked Sendable {
    private let queue = DispatchQueue(label: "io.unom.punktfunk.haptics", qos: .userInteractive)

    private struct Motor {
        let engine: CHHapticEngine
        let player: CHHapticAdvancedPatternPlayer
        var playing = false
    }

    private var controller: GCController?
    private var low: Motor?
    private var high: Motor?
    // `broken` latches OFF only for a controller that genuinely has no haptics engine (an Xbox pad
    // on an OS that doesn't expose rumble through GameController, a Siri Remote) — nothing to retry
    // until the controller changes. A transient engine failure does NOT latch it; it tears down for
    // a lazy rebuild instead, so a single hiccup can't kill rumble for the whole session.
    private var broken = false
    /// Last logged active/silent state — for a one-line transition log, not per-event spam.
    private var wasActive = false

    func retarget(_ c: GCController?) {
        queue.async {
            self.teardown()
            self.controller = c
            self.broken = false
        }
    }

    func apply(low lowAmp: UInt16, high highAmp: UInt16) {
        queue.async {
            let active = lowAmp != 0 || highAmp != 0
            if active != self.wasActive {
                self.wasActive = active
                log.debug(
                    "rumble: \(active ? "active" : "stop", privacy: .public) low=\(lowAmp, privacy: .public) high=\(highAmp, privacy: .public)")
            }
            guard !self.broken else { return }
            if active, self.low == nil, self.high == nil {
                self.setup()
            }
            if self.high != nil {
                self.drive(&self.low, Float(lowAmp) / 65535)
                self.drive(&self.high, Float(highAmp) / 65535)
            } else {
                // Combined engine: whichever motor is stronger wins.
                self.drive(&self.low, Float(max(lowAmp, highAmp)) / 65535)
            }
        }
    }

    func stop() {
        queue.sync { self.teardown() }
    }

    /// Engines per handle when the pad distinguishes them (low = left/heavy motor,
    /// high = right/light — the Xbox/XInput convention the wire carries); one combined
    /// engine otherwise, driven by whichever amplitude is stronger.
    private func setup() {
        guard let haptics = controller?.haptics else {
            // No haptics engine at all — an Xbox controller on an OS/firmware that doesn't expose
            // rumble through GameController (works on Android via the standard Vibrator path, but
            // Apple's support is controller/OS-dependent), or a Siri Remote. Nothing to retry until
            // the controller changes; latch off (retarget clears it) and say so once.
            log.info("rumble: active controller exposes no haptics engine — rumble unavailable")
            broken = true
            return
        }
        let localities = haptics.supportedLocalities
        if localities.contains(.leftHandle), localities.contains(.rightHandle) {
            low = makeMotor(haptics, .leftHandle)
            high = makeMotor(haptics, .rightHandle)
        } else {
            low = makeMotor(haptics, .default)
        }
        if low == nil, high == nil {
            // Haptics present but no engine could be built right now (server busy / a transient
            // error). Do NOT latch broken — the next nonzero amplitude retries setup().
            log.warning("rumble: haptics present but engine setup failed — will retry on next rumble")
        }
    }

    private func makeMotor(_ haptics: GCDeviceHaptics, _ locality: GCHapticsLocality) -> Motor? {
        guard let engine = haptics.createEngine(withLocality: locality) else { return nil }
        // The haptic server can stop or reset the engine out from under us — app backgrounding, an
        // audio-session interruption (a call, Siri, another audio app), or a server crash. Left
        // unhandled the players go dead and every later rumble throws, latching rumble off for the
        // rest of the session (the "rumble worked, then went spotty" failure). Tear down on the
        // serial queue so the next nonzero amplitude lazily rebuilds the engine, instead.
        engine.stoppedHandler = { [weak self] reason in
            log.info("rumble: haptic engine stopped (reason \(reason.rawValue, privacy: .public)) — will rebuild")
            self?.queue.async { self?.teardown() }
        }
        engine.resetHandler = { [weak self] in
            log.info("rumble: haptic engine reset — will rebuild")
            self?.queue.async { self?.teardown() }
        }
        do {
            try engine.start()
            let event = CHHapticEvent(
                eventType: .hapticContinuous,
                parameters: [CHHapticEventParameter(parameterID: .hapticIntensity, value: 1)],
                relativeTime: 0,
                duration: TimeInterval(GCHapticDurationInfinite))
            let player = try engine.makeAdvancedPlayer(with: CHHapticPattern(events: [event], parameters: []))
            return Motor(engine: engine, player: player)
        } catch {
            log.warning("haptic engine setup failed (\(locality.rawValue, privacy: .public)): \(error, privacy: .public)")
            return nil
        }
    }

    private func drive(_ motor: inout Motor?, _ amplitude: Float) {
        guard var m = motor else { return }
        do {
            if amplitude > 0 {
                if !m.playing {
                    try m.player.start(atTime: CHHapticTimeImmediate)
                    m.playing = true
                }
                try m.player.sendParameters(
                    [CHHapticDynamicParameter(
                        parameterID: .hapticIntensityControl,
                        value: amplitude, relativeTime: 0)],
                    atTime: CHHapticTimeImmediate)
            } else if m.playing {
                try m.player.stop(atTime: CHHapticTimeImmediate)
                m.playing = false
            }
            motor = m
        } catch {
            // A transient failure (the engine stopped/reset between its handler firing and now).
            // Tear down so the next nonzero amplitude rebuilds — do NOT latch rumble off for the
            // session (that was the old "spotty" behaviour).
            log.warning("rumble: haptic update failed — rebuilding: \(error, privacy: .public)")
            teardown()
        }
    }

    private func teardown() {
        for m in [low, high].compactMap({ $0 }) {
            // Disarm the handlers before stopping so stop() can't re-enter teardown via them.
            // (Both properties are non-optional closures on this SDK, so assign no-ops, not nil.)
            m.engine.stoppedHandler = { _ in }
            m.engine.resetHandler = {}
            try? m.player.stop(atTime: CHHapticTimeImmediate)
            m.engine.stop()
        }
        low = nil
        high = nil
    }
}

public final class GamepadFeedback {
    private let connection: PunktfunkConnection
    private let flag = FeedbackStopFlag()
    private let drainDone = DispatchSemaphore(value: 0)
    private var drainStarted = false
    private let rumble = RumbleRenderer()
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
        // No hidout traffic can exist on a non-DualSense session — poll that plane
        // nonblocking there and let rumble own the wait.
        let hidTimeout: UInt32 = connection.resolvedGamepad == .dualSense ? 10 : 0
        let thread = Thread { [connection, flag, drainDone, weak self] in
            while !flag.isStopped {
                do {
                    if let r = try connection.nextRumble(timeoutMs: 10), r.pad == 0 {
                        self?.rumble.apply(low: r.low, high: r.high)
                    }
                    // Drain a BOUNDED burst of hidout events: only the first poll waits,
                    // and the cap + stop check keep sustained 0xCD traffic (a game writing
                    // per-frame LED/trigger reports) from starving the rumble poll above
                    // or blocking stop() past one cycle.
                    var burst = 0
                    while burst < 64, !flag.isStopped,
                          let ev = try connection.nextHidOutput(
                            timeoutMs: burst == 0 ? hidTimeout : 0) {
                        self?.render(ev)
                        burst += 1
                    }
                } catch {
                    break // .closed (or fatal) — the session is over
                }
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
