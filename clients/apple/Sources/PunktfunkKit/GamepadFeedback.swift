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

    /// One actuator's started engine plus the player currently driving it (nil = idle). The
    /// player is rebuilt per level change — `drive` bakes the target intensity into a fresh
    /// continuous event rather than scaling a long-lived one with a dynamic parameter.
    private struct Motor {
        let engine: CHHapticEngine
        var player: CHHapticAdvancedPatternPlayer?
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

    /// CHHapticEvent sharpness = actuator frequency. A DualSense's voice-coil motors need a
    /// defined frequency to move at all — an intensity-only event (no sharpness) left them
    /// silent, while a classic Xbox rotor (which ignores sharpness) rumbled fine. 0.5 is the mid
    /// value the known-working macOS DualSense rumble implementations use. (Used only on the
    /// CoreHaptics path — a DualSense on macOS is driven over raw HID instead, see below.)
    private static let sharpness: Float = 0.5

    #if os(macOS)
    /// Set when the active pad is a DualSense: its motors are driven over raw HID (CoreHaptics
    /// does not reach them on macOS — adaptive triggers/lightbar work, rumble is silent). nil for
    /// every other controller, which keeps the CoreHaptics path.
    private var dualSenseHID: DualSenseHID?
    #endif

    /// `onBackend`, if given, is invoked (on the internal queue) with a human-readable name of the
    /// rumble backend now in use — for the debug controller-test panel.
    func retarget(_ c: GCController?, onBackend: ((String) -> Void)? = nil) {
        queue.async {
            self.teardown()
            self.closeHID()
            self.controller = c
            self.broken = false
            _ = self.openHIDIfDualSense(c)
            onBackend?(self.backendNote(for: c))
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
            // A DualSense on macOS is driven over raw HID; CoreHaptics is the path for every
            // other pad (and for a DualSense whose HID device could not be opened).
            if self.hidRumble(low: lowAmp, high: highAmp) { return }
            guard !self.broken else { return }
            if active, self.low == nil, self.high == nil {
                self.setup()
            }
            let ok: Bool
            if self.high != nil {
                // Per-handle: low = left/heavy motor, high = right/light — the XInput convention
                // the wire carries.
                let okLow = self.drive(&self.low, Float(lowAmp) / 65535)
                let okHigh = self.drive(&self.high, Float(highAmp) / 65535)
                ok = okLow && okHigh
            } else {
                // Combined engine: whichever motor is stronger wins.
                ok = self.drive(&self.low, Float(max(lowAmp, highAmp)) / 65535)
            }
            // Rebuild on the next nonzero amplitude if an engine errored — and tear down OUTSIDE
            // the `inout` accesses above, so teardown() never mutates a motor that a `drive` call
            // still holds an exclusive reference to.
            if !ok { self.teardown() }
        }
    }

    func stop() {
        queue.sync {
            self.teardown()
            self.closeHID()
        }
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
            // Start the engine now; the player that actually moves the motor is built per level
            // change in `drive` (a fresh event baked at the target intensity).
            try engine.start()
            return Motor(engine: engine, player: nil)
        } catch {
            log.warning("haptic engine setup failed (\(locality.rawValue, privacy: .public)): \(error, privacy: .public)")
            return nil
        }
    }

    /// Drive one motor at `amplitude` (0...1) by (re)building a continuous player whose intensity
    /// is BAKED into the event. On a DualSense this is what actually moves the actuators: a
    /// fixed-intensity event scaled by a dynamic `.hapticIntensityControl` parameter (the old
    /// path) drives the iPhone Taptic Engine but is silent on a controller's haptic engine. The
    /// event carries an explicit sharpness (frequency) so the voice coils respond, and an infinite
    /// duration so a single host update — the host sends rumble only when the level changes —
    /// sustains until the next one. Returns false if the engine errored; the caller tears down for
    /// a rebuild (done outside this `inout` access to avoid an exclusivity violation).
    private func drive(_ motor: inout Motor?, _ amplitude: Float) -> Bool {
        guard var m = motor else { return true }
        // Replace any running player: stop the old, and for a zero level leave the motor idle.
        try? m.player?.stop(atTime: CHHapticTimeImmediate)
        m.player = nil
        guard amplitude > 0 else { motor = m; return true }
        do {
            let event = CHHapticEvent(
                eventType: .hapticContinuous,
                parameters: [
                    CHHapticEventParameter(parameterID: .hapticIntensity, value: amplitude),
                    CHHapticEventParameter(parameterID: .hapticSharpness, value: Self.sharpness),
                ],
                relativeTime: 0,
                duration: TimeInterval(GCHapticDurationInfinite))
            let player = try m.engine.makeAdvancedPlayer(
                with: CHHapticPattern(events: [event], parameters: []))
            try player.start(atTime: CHHapticTimeImmediate)
            m.player = player
            motor = m
            return true
        } catch {
            // A transient failure (the engine stopped/reset between its handler firing and now).
            // Signal a rebuild — do NOT latch rumble off for the session (the old "spotty" bug).
            log.warning("rumble: haptic update failed — rebuilding: \(error, privacy: .public)")
            motor = m
            return false
        }
    }

    private func teardown() {
        for m in [low, high].compactMap({ $0 }) {
            // Disarm the handlers before stopping so stop() can't re-enter teardown via them.
            // (Both properties are non-optional closures on this SDK, so assign no-ops, not nil.)
            m.engine.stoppedHandler = { _ in }
            m.engine.resetHandler = {}
            try? m.player?.stop(atTime: CHHapticTimeImmediate)
            m.engine.stop()
        }
        low = nil
        high = nil
    }

    // MARK: - DualSense raw-HID rumble (macOS)
    //
    // On macOS the DualSense's motors aren't reachable through CHHapticEngine, so for a DualSense
    // we drive them over raw HID (see `DualSenseHID`); every other pad keeps the CoreHaptics path.
    // All three run on the serial `queue`, like the rest of the renderer state.

    private func openHIDIfDualSense(_ c: GCController?) -> Bool {
        #if os(macOS)
        guard let c, c.extendedGamepad is GCDualSenseGamepad else { return false }
        let hid = DualSenseHID()
        guard hid.open() else { return false }
        dualSenseHID = hid
        return true
        #else
        return false
        #endif
    }

    /// Drive the DualSense's motors over HID if that's the active backend; false → not a HID pad,
    /// so the caller uses CoreHaptics. The wire's 0...0xFFFF amplitudes scale to the pad's 0...255.
    private func hidRumble(low: UInt16, high: UInt16) -> Bool {
        #if os(macOS)
        guard let hid = dualSenseHID else { return false }
        hid.rumble(low: UInt8(low >> 8), high: UInt8(high >> 8))
        return true
        #else
        return false
        #endif
    }

    private func closeHID() {
        #if os(macOS)
        dualSenseHID?.close()
        dualSenseHID = nil
        #endif
    }

    private func backendNote(for c: GCController?) -> String {
        #if os(macOS)
        if let hid = dualSenseHID { return "DualSense HID · \(hid.transport)" }
        #endif
        return c == nil ? "—" : "CoreHaptics"
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
        // Hidout traffic (lightbar / player LEDs / triggers) only exists on a PlayStation-pad
        // session — a DualSense or a DualShock 4 (lightbar only). Block briefly on it there and
        // let rumble own the wait elsewhere; on an Xbox session it stays nonblocking.
        let hasHidout = connection.resolvedGamepad == .dualSense
            || connection.resolvedGamepad == .dualShock4
        let hidTimeout: UInt32 = hasHidout ? 10 : 0
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

#if DEBUG
/// Local feedback driver for the Settings → Controllers "Test Controller" panel (DEBUG builds
/// only). It drives the SAME CoreHaptics rumble renderer and `DualSenseTriggerEffect` path a
/// live session uses — just aimed at the physically-connected controller instead of the
/// host→client feedback planes — so rumble, the adaptive triggers, the lightbar and the player
/// LEDs can be confirmed on-device without a host. Reusing the real renderers is the point:
/// a passing test exercises the exact code a session runs.
@MainActor
public final class ControllerTester: ObservableObject {
    private let renderer = RumbleRenderer()
    private weak var controller: GCController?

    /// The rumble backend now in use — "DualSense HID · USB/Bluetooth", "CoreHaptics", or "—" —
    /// for the test panel to display so it's obvious which path a given pad takes.
    @Published public private(set) var rumbleBackend = "—"

    public init() {}

    /// Aim the feedback at a controller (nil releases it). Idempotent — safe to call on every
    /// active-controller change.
    public func target(_ c: GCController?) {
        guard c !== controller else { return }
        controller = c
        renderer.retarget(c) { [weak self] note in
            Task { @MainActor in self?.rumbleBackend = note }
        }
    }

    /// Drive both motors at 0...1 amplitudes — low = left/heavy, high = right/light — mapped to
    /// the 0...0xFFFF wire range the session carries, through the real `RumbleRenderer`.
    public func rumble(low: Float, high: Float) {
        func u16(_ v: Float) -> UInt16 { UInt16((min(max(v, 0), 1) * 65535).rounded()) }
        renderer.apply(low: u16(low), high: u16(high))
    }

    public func stopRumble() { renderer.apply(low: 0, high: 0) }

    /// Replay an adaptive-trigger effect on a DualSense via the real `DualSenseTriggerEffect`
    /// renderer. `right == false` → L2, `true` → R2. No-op on a non-DualSense pad.
    public func applyTrigger(_ effect: DualSenseTriggerEffect, right: Bool) {
        guard let ds = controller?.extendedGamepad as? GCDualSenseGamepad else { return }
        effect.apply(to: right ? ds.rightTrigger : ds.leftTrigger)
    }

    public func resetTriggers() {
        guard let ds = controller?.extendedGamepad as? GCDualSenseGamepad else { return }
        ds.leftTrigger.setModeOff()
        ds.rightTrigger.setModeOff()
    }

    /// Lightbar colour (DualSense / DualShock 4); nil turns it off. No-op without a light.
    public func setLight(_ color: GCColor?) {
        controller?.light?.color = color ?? GCColor(red: 0, green: 0, blue: 0)
    }

    /// Player-indicator LEDs (`.index1`...`.index4`, or `.indexUnset` to clear).
    public func setPlayerIndex(_ index: GCControllerPlayerIndex) {
        controller?.playerIndex = index
    }

    /// Silence every channel and release the controller — call on the panel's disappear.
    public func stop() {
        resetTriggers()
        setPlayerIndex(.indexUnset)
        setLight(nil)
        renderer.retarget(nil) // async teardown: stops the motors + drops the controller ref
        controller = nil
    }
}
#endif
