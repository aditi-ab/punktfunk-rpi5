import CoreHaptics
import Foundation
import GameController
import os

private let log = Logger(subsystem: "io.unom.punktfunk", category: "gamepad")

/// Tuning constants + the pure scheduling decisions of the rumble renderer, split out so the
/// policy is unit-testable without a `CHHapticEngine` or a physical pad.
enum RumbleTuning {
    /// Haptic segment length. **No event is ever infinite**: a player the renderer loses track
    /// of (a stop dropped inside CoreHaptics, an engine race) self-silences when its segment
    /// expires, so this is the hard ceiling on how long the actuator can diverge from the
    /// target state.
    static let segmentSeconds: TimeInterval = 4.0
    /// Re-arm the successor segment once the current one has less than this left. Generous
    /// against the ticker period so a steady rumble can never miss the boundary and gap.
    static let rearmHeadroom: TimeInterval = 1.0
    /// Renderer ticker period while anything is (or should be) audible. Silence runs no timer.
    static let tickSeconds: TimeInterval = 0.05
    /// Minimum spacing between player rebuilds for nonzero→nonzero level changes — a game
    /// ramping rumble per frame would otherwise stop/start players at 60+ Hz, which is exactly
    /// the churn that lost stops inside CoreHaptics. Newest level wins when the window opens;
    /// zero is never throttled.
    static let minRebakeSeconds: TimeInterval = 0.025
    /// Levels closer than this (≈0.4 % of full scale) are the same level — an identical host
    /// refresh must never rebuild a player.
    static let levelEpsilon: Float = 1.0 / 256.0
    /// macOS DualSense raw-HID path: re-write an unchanged nonzero level this often so the
    /// pad's firmware never times the rumble out mid-effect (Bluetooth pads watchdog output
    /// reports), and a dropped report heals.
    static let hidKeepaliveSeconds: TimeInterval = 0.9

    /// `CHHapticEvent` sharpness = actuator frequency. A DualSense's voice-coil motors need a
    /// defined frequency to move at all (an intensity-only event left them silent) while a
    /// classic Xbox ERM rotor ignores it. On split-handle pads the wire's two motors render at
    /// distinct frequencies mirroring the real hardware they emulate — low/left ≈ the heavy
    /// low-frequency rotor, high/right ≈ the light buzzer; a single combined actuator keeps the
    /// proven mid value.
    static let sharpnessLow: Float = 0.3
    static let sharpnessHigh: Float = 0.7
    static let sharpnessCombined: Float = 0.5

    /// Wire amplitude (0...0xFFFF) → CoreHaptics intensity (0...1).
    static func amplitude(_ wire: UInt16) -> Float { Float(wire) / 65535 }
    /// Wire amplitude → DualSense HID motor byte.
    static func hidByte(_ wire: UInt16) -> UInt8 { UInt8(wire >> 8) }
    /// Single-actuator pads render whichever motor is stronger.
    static func combined(low: UInt16, high: UInt16) -> UInt16 { max(low, high) }
    /// Are two baked levels the same (skip the rebuild)?
    static func sameLevel(_ a: Float, _ b: Float) -> Bool { abs(a - b) <= levelEpsilon }
    /// Time for a segment handoff to act (engine timeline).
    static func shouldRearm(endsAt: TimeInterval, now: TimeInterval) -> Bool {
        endsAt - now <= rearmHeadroom
    }
    /// When the successor segment starts: exactly as the current one expires — unless that
    /// already passed (the gap already happened; start now).
    static func handoffStart(endsAt: TimeInterval, now: TimeInterval) -> TimeInterval {
        max(endsAt, now)
    }
}

/// Rumble → the active physical controller (CoreHaptics; a DualSense on macOS goes over raw HID
/// instead, see `DualSenseHID`), built around one principle: **rumble is idempotent state on a
/// lossy channel, and the actuator's divergence from that state must be bounded** — not
/// best-effort. The previous renderer drove infinite-duration players torn down and rebuilt per
/// wire update; one asynchronous `stop` dropped inside CoreHaptics left an unstoppable player
/// buzzing with its handle discarded, which no later (0,0) could reach — the "walked into the
/// menu and the rumble never stopped" bug.
///
/// The invariants that bound divergence now:
///  1. **No infinite events.** A motor plays finite `segmentSeconds` segments; while the level
///     holds, the successor is scheduled ON the engine timeline to start exactly when the
///     current segment expires (seamless — no stop/start race in steady state). A leaked player
///     therefore self-silences in ≤ `segmentSeconds`.
///  2. **Idempotent targets.** An update equal to the current target (the host re-sends rumble
///     state every 500 ms as its loss heal) is a liveness stamp, never a player rebuild.
///  3. **Zero is immediate, ramps are throttled.** (0,0) stops players the moment it lands;
///     nonzero→nonzero changes rebuild at most every `minRebakeSeconds` per motor (the ticker
///     lands the newest value once the window opens).
///  4. **Escalating stop.** A throwing `player.stop` means the engine's state is unknown — the
///     whole engine is stopped (silencing every player it hosts) and lazily rebuilt behind the
///     exponential backoff.
///  5. **Staleness watchdog** (`Policy.session`): audible with no wire command for
///     `sessionStaleSeconds` → force silence. A lost stop can outlive the host's 500 ms heal
///     only if the channel itself died, and then the pad must not buzz forever. `Policy.manual`
///     (the settings test panel) instead holds a level until it is changed.
///
/// Engines are created lazily on the first nonzero amplitude and torn down on retarget;
/// failures (pads without haptics, engine resets) downgrade to silence — rumble is best-effort
/// by design, but *staying silent* when told to stop is not.
///
/// `@unchecked Sendable` is sound because every property is read and written only inside
/// `queue` closures — the serial queue is the synchronization.
final class RumbleRenderer: @unchecked Sendable {
    /// Who ends an un-refreshed nonzero target. Session mode applies the core policy engine's
    /// commands verbatim — the engine (punktfunk-core `client/rumble.rs`) owns every lease,
    /// staleness, and close decision and emits explicit zeros, so the renderer keeps NO
    /// staleness policy of its own anymore. The controller test panel (`manual`) holds a slider
    /// level indefinitely; both are identical renderer-side today, the distinction is kept for
    /// the call sites' intent.
    struct Policy {
        static let session = Policy()
        static let manual = Policy()
    }

    /// Which physical actuator this renderer drives: the forwarded controller's haptics engine
    /// (the default), or THIS device's own Taptic Engine (`CHHapticEngine()`) — the opt-in
    /// "rumble on this device" mirror for phone-clip pads that ship without rumble motors.
    /// Device mode ignores `retarget`'s controller and always renders one combined motor
    /// (a phone body has a single actuator).
    enum Actuator {
        case controller
        case device
    }

    private let queue = DispatchQueue(label: "io.unom.punktfunk.haptics", qos: .userInteractive)
    private let policy: Policy
    private let actuator: Actuator

    /// One finite haptic play on a motor: the player plus when (engine timeline) it expires.
    /// A PLAIN pattern player on purpose: the controller haptics server (gamecontrollerd)
    /// advertises `adv players: 0`, and as of iOS 27 beta 2 an advanced-player sequence load
    /// doesn't degrade gracefully there — the daemon faults decoding the XPC message and drops
    /// it (CoreHaptics -4811/4097, rumble dead). We only need `start(atTime:)`/`stop(atTime:)`,
    /// which the plain protocol has.
    private struct Segment {
        let player: CHHapticPatternPlayer
        let endsAt: TimeInterval
    }

    /// One actuator's started engine and the segment(s) realizing `level` on it. `retiring` is
    /// the predecessor across a segment handoff — left to expire naturally (its successor
    /// starts the instant it ends), but the reference is held so a level change or stop can
    /// still force-stop it.
    private struct Motor {
        let engine: CHHapticEngine
        let sharpness: Float
        var level: Float = 0
        var current: Segment?
        var retiring: Segment?
        var lastRebake = DispatchTime(uptimeNanoseconds: 0)
    }

    private var controller: GCController?
    private var low: Motor?
    private var high: Motor?
    /// Wire-truth target (raw wire units) — the engine command's level, applied verbatim; the
    /// core policy engine owns when it ends (explicit zero commands), so no deadline lives here.
    private var target: (low: UInt16, high: UInt16) = (0, 0)
    /// Runs while anything is (or should be) audible: staleness watchdog, segment re-arm,
    /// throttled-level catch-up, engine rebuild after a reset, HID keepalive. Nil while silent,
    /// so an idle controller costs no timer wakeups and no radio traffic.
    private var ticker: DispatchSourceTimer?

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
    // the moment a player is actually running (or the controller changes).
    private var retryAfter = DispatchTime(uptimeNanoseconds: 0)
    private var consecutiveFailures = 0
    /// Downgrade after split-handle engines fail: retry with ONE combined `.default` engine —
    /// the configuration virtually every iOS game (and this app's own menu haptics) uses — before
    /// treating the service as unreachable. A haptics daemon that mishandles per-handle
    /// localities for a particular pad can still serve the combined engine. One-way per
    /// controller; retarget resets it.
    private var preferCombined = false
    /// Health reporting for the debug test panel: a human-readable problem while rumble cannot
    /// work (nil = healthy). Without this, a wedged system haptics service (gamecontrollerd
    /// refusing every XPC connection — CoreHaptics -4811/4097, which no in-app retry can fix)
    /// reads as "the app's rumble is broken" when actually no app on the device can rumble.
    private var healthSink: ((String?) -> Void)?
    private var lastHealth: String?

    #if os(macOS)
    /// Set when the active pad is a DualSense: its motors are driven over raw HID (CoreHaptics
    /// does not reach them on macOS — adaptive triggers/lightbar work, rumble is silent). nil for
    /// every other controller, which keeps the CoreHaptics path.
    private var dualSenseHID: DualSenseHID?
    private var lastHidWrite: (levels: (UInt8, UInt8), at: DispatchTime) =
        ((0, 0), DispatchTime(uptimeNanoseconds: 0))
    #endif

    init(policy: Policy = .session, actuator: Actuator = .controller) {
        self.policy = policy
        self.actuator = actuator
    }

    /// `onBackend`, if given, is invoked (on the internal queue) with a human-readable name of the
    /// rumble backend now in use; `onHealth` with a problem description whenever rumble transitions
    /// between working and structurally failing (nil = healthy) — both for the debug test panel.
    func retarget(
        _ c: GCController?, onBackend: ((String) -> Void)? = nil,
        onHealth: ((String?) -> Void)? = nil
    ) {
        queue.async {
            self.teardown()
            self.closeHID()
            self.controller = c
            self.broken = false
            self.preferCombined = false
            self.consecutiveFailures = 0
            self.retryAfter = DispatchTime(uptimeNanoseconds: 0)
            if let onHealth { self.healthSink = onHealth }
            self.lastHealth = nil
            self.healthSink?(nil)
            _ = self.openHIDIfDualSense(c)
            onBackend?(self.backendNote(for: c))
            // The target survives the swap: render replays the current level onto the new pad
            // right away (a mid-rumble controller change keeps rumbling, like moving a real pad
            // between hands mid-effect).
            self.render()
        }
    }

    /// Set the wire-truth target. Called with every 0xCA state the host sends — level changes AND
    /// renewals (v2) / 500 ms refreshes (legacy); both stamp liveness and, for v2, refresh the
    /// self-termination deadline. `ttlMs` is the envelope lease in ms, or [`RumbleTuning.noTTL`]
    /// against a legacy host (no lease → the staleness watchdog is the backstop). Renewals at an
    /// unchanged level extend the deadline before the idempotence guard, so a held rumble never
    /// lapses mid-effect.
    func apply(low lowAmp: UInt16, high highAmp: UInt16) {
        queue.async {
            let active = lowAmp != 0 || highAmp != 0
            if active != self.wasActive {
                self.wasActive = active
                log.debug(
                    "rumble: \(active ? "active" : "stop", privacy: .public) low=\(lowAmp, privacy: .public) high=\(highAmp, privacy: .public)")
            }
            guard (lowAmp, highAmp) != self.target else { return }
            self.target = (lowAmp, highAmp)
            self.render()
        }
    }

    /// Silence the motors and drop the engines. Blocks until done — call off the main actor.
    func stop() {
        queue.sync {
            self.ticker?.cancel()
            self.ticker = nil
            self.target = (0, 0)
            self.wasActive = false
            self.teardown()
            self.closeHID()
        }
    }

    // MARK: - Reconciliation (all on `queue`)

    /// Drive the actuators toward `target`. Idempotent — safe to call from every wire update,
    /// tick, and retarget; when everything already matches it does nothing.
    private func render() {
        defer { updateTicker() }
        if renderHID() { return }
        guard !broken else { return }
        let audible = target.low != 0 || target.high != 0
        if audible, low == nil, high == nil, DispatchTime.now() >= retryAfter {
            setup()
        }
        // Reconcile BOTH motors (no short-circuit skipping the second on a first-motor error),
        // and tear down OUTSIDE the `inout` accesses so teardown() never mutates a motor a
        // reconcile call still holds an exclusive reference to.
        let ok: Bool
        if high != nil {
            // Per-handle: low = left/heavy motor, high = right/light — the XInput convention
            // the wire carries.
            let okLow = reconcile(&low, to: RumbleTuning.amplitude(target.low))
            let okHigh = reconcile(&high, to: RumbleTuning.amplitude(target.high))
            ok = okLow && okHigh
        } else {
            let mixed = RumbleTuning.combined(low: target.low, high: target.high)
            ok = reconcile(&low, to: RumbleTuning.amplitude(mixed))
        }
        if !ok {
            let wasSplit = high != nil
            teardown()
            scheduleRetryBackoff()
            if wasSplit, !preferCombined {
                preferCombined = true
                log.info("rumble: split-handle engines failing — will retry with one combined engine")
            }
        } else if low?.current != nil || high?.current != nil {
            // A player is actually running — the path has recovered; clear the backoff.
            consecutiveFailures = 0
            retryAfter = DispatchTime(uptimeNanoseconds: 0)
            reportHealth(nil)
        }
    }

    /// Publish a health transition to the test panel (deduped — transitions only).
    private func reportHealth(_ problem: String?) {
        guard problem != lastHealth else { return }
        lastHealth = problem
        healthSink?(problem)
    }

    /// Housekeeping heartbeat while audible: segment re-arm, HID keepalive, backoff retries.
    /// Every liveness decision (lease expiry, legacy-host staleness, session close) lives in the
    /// core policy engine now — it emits explicit zero commands, so the renderer never guesses
    /// when a level should end.
    private func tick() {
        render()
    }

    /// Drive one motor toward `desired`, per the invariants above. Returns false when the
    /// engine errored — the caller then tears everything down (outside this `inout` access) for
    /// a lazy, backoff-gated rebuild.
    private func reconcile(_ slot: inout Motor?, to desired: Float) -> Bool {
        guard var m = slot else { return true }
        defer { slot = m }
        // Release a handed-off predecessor once it has expired on its own.
        if let r = m.retiring, m.engine.currentTime >= r.endsAt + 0.25 {
            m.retiring = nil
        }
        if desired <= RumbleTuning.levelEpsilon {
            guard m.level > 0 || m.current != nil || m.retiring != nil else { return true }
            m.level = 0
            return stopSegments(&m)
        }
        if RumbleTuning.sameLevel(desired, m.level), m.current != nil {
            return rearmIfNeeded(&m)
        }
        // Nonzero level change. Throttled: the ticker re-runs render() and lands the newest
        // value once the window opens (zero above is never throttled).
        if m.current != nil, seconds(since: m.lastRebake) < RumbleTuning.minRebakeSeconds {
            return true
        }
        guard stopSegments(&m) else { return false }
        do {
            m.current = try makeSegment(
                m.engine, sharpness: m.sharpness, amplitude: desired, at: CHHapticTimeImmediate)
            m.level = desired
            m.lastRebake = .now()
            return true
        } catch {
            // A transient failure (the engine stopped/reset between its handler firing and now).
            // Signal a rebuild — do NOT latch rumble off for the session.
            log.warning("rumble: haptic start failed — rebuilding: \(error, privacy: .public)")
            return false
        }
    }

    /// Keep a steady level seamless across the finite-segment boundary: when the current
    /// segment nears its end, start the successor ON the engine timeline exactly as it expires
    /// — no stop call, no race, no gap. The old segment is kept as `retiring` until it dies
    /// naturally, so a level change can still force-stop it.
    private func rearmIfNeeded(_ m: inout Motor) -> Bool {
        guard let cur = m.current else { return true }
        let now = m.engine.currentTime
        guard RumbleTuning.shouldRearm(endsAt: cur.endsAt, now: now) else { return true }
        // A predecessor still held this deep into the segment already expired; drop it.
        m.retiring = nil
        do {
            let next = try makeSegment(
                m.engine, sharpness: m.sharpness, amplitude: m.level,
                at: RumbleTuning.handoffStart(endsAt: cur.endsAt, now: now))
            m.retiring = m.current
            m.current = next
            return true
        } catch {
            log.warning("rumble: segment re-arm failed — rebuilding: \(error, privacy: .public)")
            return false
        }
    }

    /// Stop every segment on the motor NOW. False = a stop threw, so the engine's real state is
    /// unknown (a player may still run with its handle gone) — the caller must escalate to a
    /// full engine teardown, whose `engine.stop()` silences every player the engine hosts.
    private func stopSegments(_ m: inout Motor) -> Bool {
        var ok = true
        for seg in [m.current, m.retiring].compactMap({ $0 }) {
            do {
                try seg.player.stop(atTime: CHHapticTimeImmediate)
            } catch {
                log.warning(
                    "rumble: player stop failed — escalating to engine stop: \(error, privacy: .public)")
                ok = false
            }
        }
        m.current = nil
        m.retiring = nil
        return ok
    }

    /// Build + start one finite continuous event at `amplitude`. `at` is `CHHapticTimeImmediate`
    /// or an absolute engine-timeline instant (a scheduled handoff). The intensity is BAKED into
    /// the event: a fixed event scaled by a dynamic `.hapticIntensityControl` parameter drives
    /// the iPhone Taptic Engine but is silent on a controller's haptic engine.
    private func makeSegment(
        _ engine: CHHapticEngine, sharpness: Float, amplitude: Float, at start: TimeInterval
    ) throws -> Segment {
        let event = CHHapticEvent(
            eventType: .hapticContinuous,
            parameters: [
                CHHapticEventParameter(parameterID: .hapticIntensity, value: amplitude),
                CHHapticEventParameter(parameterID: .hapticSharpness, value: sharpness),
            ],
            relativeTime: 0,
            duration: RumbleTuning.segmentSeconds)
        let player = try engine.makePlayer(
            with: CHHapticPattern(events: [event], parameters: []))
        try player.start(atTime: start)
        let begins = start == CHHapticTimeImmediate ? engine.currentTime : start
        return Segment(player: player, endsAt: begins + RumbleTuning.segmentSeconds)
    }

    /// The ticker runs only while something needs tending — any nonzero target (watchdog,
    /// throttle catch-up, HID keepalive, post-reset engine rebuild) or segments still alive.
    private func updateTicker() {
        let needed = target != (0, 0)
            || low?.current != nil || low?.retiring != nil
            || high?.current != nil || high?.retiring != nil
        if needed, ticker == nil {
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(
                deadline: .now() + RumbleTuning.tickSeconds, repeating: RumbleTuning.tickSeconds)
            t.setEventHandler { [weak self] in self?.tick() }
            t.resume()
            ticker = t
        } else if !needed, let t = ticker {
            t.cancel()
            ticker = nil
        }
    }

    // MARK: - Engine lifecycle

    /// Engines per handle when the pad distinguishes them (low = left/heavy motor,
    /// high = right/light — the Xbox/XInput convention the wire carries); one combined
    /// engine otherwise, driven by whichever amplitude is stronger.
    private func setup() {
        if actuator == .device {
            setupDevice()
            return
        }
        guard let haptics = controller?.haptics else {
            // No haptics engine at all — an Xbox controller on an OS/firmware that doesn't expose
            // rumble through GameController (works on Android via the standard Vibrator path, but
            // Apple's support is controller/OS-dependent), or a Siri Remote. Nothing to retry until
            // the controller changes; latch off (retarget clears it) and say so once.
            log.info("rumble: active controller exposes no haptics engine — rumble unavailable")
            broken = true
            reportHealth("This controller exposes no rumble engine to apps on this OS.")
            return
        }
        let localities = haptics.supportedLocalities
        let split =
            !preferCombined && localities.contains(.leftHandle)
            && localities.contains(.rightHandle)
        if split {
            low = makeMotor(haptics, .leftHandle, sharpness: RumbleTuning.sharpnessLow)
            high = makeMotor(haptics, .rightHandle, sharpness: RumbleTuning.sharpnessHigh)
        } else {
            low = makeMotor(haptics, .default, sharpness: RumbleTuning.sharpnessCombined)
        }
        if low == nil, high == nil {
            // Haptics present but no engine could be built right now (server busy / XPC broken). Do
            // NOT latch broken — back off and a later render past the cooldown retries.
            log.warning("rumble: haptics present but engine setup failed — backing off, will retry")
            scheduleRetryBackoff()
            if split {
                preferCombined = true
                log.info("rumble: split-handle engines failing — will retry with one combined engine")
            }
        }
    }

    /// Push the next engine-build attempt out after a failure (capped exponential backoff), so a
    /// broken `gamecontrollerd.haptics` connection gets time to relaunch instead of being re-hit on
    /// every rumble update.
    private func scheduleRetryBackoff() {
        consecutiveFailures += 1
        let shift = min(consecutiveFailures - 1, 4)
        retryAfter = .now() + min(0.5 * Double(1 << shift), 4)
        if consecutiveFailures >= 2 {
            // One failure is a hiccup; repeated ones are the wedged-service signature (every
            // XPC connection to gamecontrollerd.haptics breaks — no app on the device can
            // rumble until it relaunches). Say so instead of failing silently.
            reportHealth(
                "The system haptics service is refusing connections — no app can rumble a "
                    + "controller right now. Rebooting the device usually clears it.")
        }
    }

    /// Device-actuator mode: one combined motor on this device's own Taptic Engine. Only an
    /// iPhone has one — everything else (iPad, Mac, TV) reports no haptic hardware and latches
    /// off (nothing to retry; the settings toggle is hidden there anyway, this is the backstop).
    private func setupDevice() {
        #if os(iOS)
        guard CHHapticEngine.capabilitiesForHardware().supportsHaptics else {
            log.info("rumble: this device has no haptic actuator — device rumble unavailable")
            broken = true
            reportHealth("This device has no haptic actuator.")
            return
        }
        do {
            low = startMotor(try CHHapticEngine(), sharpness: RumbleTuning.sharpnessCombined)
        } catch {
            log.warning("rumble: device haptic engine creation failed: \(error, privacy: .public)")
        }
        if low == nil {
            // Same shape as the controller path: haptics exist but the engine couldn't be built
            // right now — back off and retry, don't latch off.
            scheduleRetryBackoff()
        }
        #else
        broken = true
        #endif
    }

    private func makeMotor(
        _ haptics: GCDeviceHaptics, _ locality: GCHapticsLocality, sharpness: Float
    ) -> Motor? {
        guard let engine = haptics.createEngine(withLocality: locality) else { return nil }
        return startMotor(engine, sharpness: sharpness)
    }

    /// Configure + start an engine (controller-locality or the device's own) into a [`Motor`].
    private func startMotor(_ engine: CHHapticEngine, sharpness: Float) -> Motor? {
        // A controller's motors carry no audio, so keep this engine OUT of the app's audio session
        // (the default is to join it). Streaming keeps an AVAudioSession active the whole time;
        // letting a haptics-only engine join it is a needless coupling that can get its
        // gamecontrollerd XPC connection interrupted (the repeated -4811 server-connection breaks).
        engine.playsHapticsOnly = true
        // The haptic server can stop or reset the engine out from under us — app backgrounding, an
        // audio-session interruption (a call, Siri, another audio app), or a server crash. Left
        // unhandled the players go dead and every later rumble throws, latching rumble off for the
        // rest of the session (the "rumble worked, then went spotty" failure). Tear down on the
        // serial queue; the ticker (or the next wire update) lazily rebuilds the engine and
        // re-renders the still-current target.
        engine.stoppedHandler = { [weak self] reason in
            log.info("rumble: haptic engine stopped (reason \(reason.rawValue, privacy: .public)) — will rebuild")
            self?.queue.async { self?.teardown() }
        }
        engine.resetHandler = { [weak self] in
            log.info("rumble: haptic engine reset — will rebuild")
            self?.queue.async { self?.teardown() }
        }
        do {
            // Start the engine now; the players that actually move the motor are the finite
            // segments `reconcile` bakes per level.
            try engine.start()
            return Motor(engine: engine, sharpness: sharpness)
        } catch {
            log.warning("haptic engine setup failed: \(error, privacy: .public)")
            return nil
        }
    }

    private func teardown() {
        for m in [low, high].compactMap({ $0 }) {
            // Disarm the handlers before stopping so stop() can't re-enter teardown via them.
            // (Both properties are non-optional closures on this SDK, so assign no-ops, not nil.)
            m.engine.stoppedHandler = { _ in }
            m.engine.resetHandler = {}
            for seg in [m.current, m.retiring].compactMap({ $0 }) {
                try? seg.player.stop(atTime: CHHapticTimeImmediate)
            }
            // The authoritative silencer: a stopped engine plays nothing, including any player
            // whose individual stop was dropped.
            m.engine.stop()
        }
        low = nil
        high = nil
    }

    private func seconds(since t: DispatchTime) -> TimeInterval {
        TimeInterval(DispatchTime.now().uptimeNanoseconds - t.uptimeNanoseconds) / 1_000_000_000
    }

    // MARK: - DualSense raw-HID rumble (macOS)
    //
    // On macOS the DualSense's motors aren't reachable through CHHapticEngine, so for a DualSense
    // we drive them over raw HID (see `DualSenseHID`); every other pad keeps the CoreHaptics path.
    // Runs on the serial `queue`, like the rest of the renderer state.

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

    /// Write the target to the DualSense over HID if that's the active backend; false → not a
    /// HID pad, so the caller renders via CoreHaptics. Deduped on the pad's 0...255 resolution,
    /// with a periodic keepalive re-write while nonzero (the ticker calls back in here).
    private func renderHID() -> Bool {
        #if os(macOS)
        guard let hid = dualSenseHID else { return false }
        let levels = (RumbleTuning.hidByte(target.low), RumbleTuning.hidByte(target.high))
        let keepalive = levels != (0, 0)
            && seconds(since: lastHidWrite.at) > RumbleTuning.hidKeepaliveSeconds
        if levels != lastHidWrite.levels || keepalive {
            hid.rumble(low: levels.0, high: levels.1)
            lastHidWrite = (levels, .now())
        }
        return true
        #else
        return false
        #endif
    }

    private func closeHID() {
        #if os(macOS)
        dualSenseHID?.close() // writes (0,0) before releasing
        dualSenseHID = nil
        lastHidWrite = ((0, 0), DispatchTime(uptimeNanoseconds: 0))
        #endif
    }

    private func backendNote(for c: GCController?) -> String {
        #if os(macOS)
        if let hid = dualSenseHID { return "DualSense HID · \(hid.transport)" }
        #endif
        return c == nil ? "—" : "CoreHaptics"
    }
}
