import CoreHaptics
import Foundation
import GameController
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "gamepad")

/// Rumble → CoreHaptics, isolated on one serial queue (CHHapticEngine is not main-bound,
/// but it isn't a free-for-all either). Engines are created lazily on the first nonzero
/// amplitude and torn down on retarget; players run only while their motor is on, so an
/// idle controller costs no radio traffic. Failures (pads without haptics, engine resets)
/// downgrade to silence — rumble is best-effort by design.
///
/// `@unchecked Sendable` is sound because every property (`controller`/`low`/`high`/`broken`) is
/// read and written only inside `queue` closures — the serial queue is the synchronization.
final class RumbleRenderer: @unchecked Sendable {
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
    // Backoff after an engine failure. A broken `gamecontrollerd.haptics` XPC connection (CoreHaptics
    // -4811 "server connection broke") fails EVERY rebuild until the service relaunches — and that
    // break fires neither stoppedHandler nor resetHandler, so without a cooldown the next rumble
    // update immediately rebuilds into the same dead connection, flooding the log and never
    // recovering. Delay the next setup() — growing 0.5→1→2→4 s on repeated failure — and clear it
    // the moment a player runs cleanly (or the controller changes).
    private var retryAfter = Date.distantPast
    private var consecutiveFailures = 0

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
            self.consecutiveFailures = 0
            self.retryAfter = .distantPast
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
            if active, self.low == nil, self.high == nil, Date() >= self.retryAfter {
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
            // still holds an exclusive reference to. Back off so a broken XPC isn't re-hit every
            // update; once a player is actually running the path has recovered, so clear the backoff.
            if !ok {
                self.teardown()
                self.scheduleRetryBackoff()
            } else if self.low?.player != nil || self.high?.player != nil {
                self.consecutiveFailures = 0
                self.retryAfter = .distantPast
            }
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
            // Haptics present but no engine could be built right now (server busy / XPC broken). Do
            // NOT latch broken — back off and the next nonzero amplitude past the cooldown retries.
            log.warning("rumble: haptics present but engine setup failed — backing off, will retry")
            scheduleRetryBackoff()
        }
    }

    /// Push the next engine-build attempt out after a failure (capped exponential backoff), so a
    /// broken `gamecontrollerd.haptics` connection gets time to relaunch instead of being re-hit on
    /// every rumble update.
    private func scheduleRetryBackoff() {
        consecutiveFailures += 1
        let shift = min(consecutiveFailures - 1, 4)
        retryAfter = Date().addingTimeInterval(min(0.5 * Double(1 << shift), 4))
    }

    private func makeMotor(_ haptics: GCDeviceHaptics, _ locality: GCHapticsLocality) -> Motor? {
        guard let engine = haptics.createEngine(withLocality: locality) else { return nil }
        // A controller's motors carry no audio, so keep this engine OUT of the app's audio session
        // (the default is to join it). Streaming keeps an AVAudioSession active the whole time;
        // letting a haptics-only engine join it is a needless coupling that can get its
        // gamecontrollerd XPC connection interrupted (the repeated -4811 server-connection breaks).
        engine.playsHapticsOnly = true
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
