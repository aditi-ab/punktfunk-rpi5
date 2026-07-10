// Gamepad capture → punktfunk/1 datagrams. Forwards exactly ONE controller — whatever
// GamepadManager selected — as pad 0, for the lifetime of a streaming session.
//
// The wire is incremental (one button/axis transition per 18-byte event, accumulated
// host-side into the virtual pad — see punktfunk_core::input::gamepad), so we snapshot the
// full GCExtendedGamepad state on every valueChanged and diff against the previous
// snapshot. Sticks are ±32767 with +y = up (GC already matches, no flip), triggers 0...255.
//
// PlayStation-pad extras ride the rich-input plane (0xCC): touchpad contacts normalized
// 0...65535 (origin top-left, +y down — GC's ±1/+y-up is converted here) and motion
// samples in raw DualSense sensor units (gyro 20 LSB per deg/s, accel 10000 LSB per g —
// derived from the host's fixed calibration blob; the conversion lives in ONE place,
// `Wire`, so a live sign/scale correction is a one-line change). The host ignores both
// unless the session's virtual pad is a DualSense or DualShock 4 — both carry a touchpad
// and motion, so the capture below covers either (`GCDualShockGamepad` exposes the same
// `touchpad*` surface as `GCDualSenseGamepad`).
//
// Unlike mouse/keyboard capture, gamepad forwarding is NOT gated on the mouse-capture
// toggle — a controller can't click local UI, so it always drives the host while the app
// is active. On deactivation, controller switch, or stop, every held control is released
// on the wire (the host pad would otherwise stay stuck on the last state).

#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Combine
import Foundation
import GameController

@MainActor
public final class GamepadCapture {
    private let connection: PunktfunkConnection
    private let manager: GamepadManager
    private var activeSub: AnyCancellable?
    private var observers: [NSObjectProtocol] = []
    private var bound: GCController?
    /// App inactive → GC stops delivering; everything is released and stays silent.
    private var suspended = false

    // Last wire state (the diff base — also what releaseAll() unwinds).
    private var buttons: UInt32 = 0
    private var axes: [Int32] = [0, 0, 0, 0, 0, 0]
    private var fingerActive: [Bool] = [false, false]
    private var lastMotionNs: UInt64 = 0

    /// Motion forwarding floor: ≥ 4 ms between samples (≈ 250 Hz, the DualSense's own rate).
    private static let motionIntervalNs: UInt64 = 4_000_000

    /// The cross-client controller escape chord (pf-client-core's `ESCAPE_CHORD`):
    /// L1+R1+Start+Select held together — four simultaneous buttons no game uses, so normal
    /// play can't trip it. Held for `disconnectHold` it ends the session via
    /// `onDisconnectRequest`; the chord keeps forwarding to the host meanwhile (the user is
    /// leaving anyway). The desktop clients' quick-press step (leave fullscreen / release
    /// capture) has no Apple equivalent worth wiring — macOS has ⌃⌥⇧Q/D, touch has the HUD.
    private static let escapeChord: UInt32 =
        GamepadWire.leftShoulder | GamepadWire.rightShoulder | GamepadWire.start | GamepadWire.back
    /// pf-client-core's `DISCONNECT_HOLD` — the same 1.5 s on every client.
    private static let disconnectHold: TimeInterval = 1.5
    private var chordTimer: Timer?
    /// Fired ON MAIN once the escape chord has been held `disconnectHold` — the session owner
    /// disconnects. On tvOS this (plus the Siri Remote's hold-Back) is the ONLY way out of a
    /// stream with a controller: B/Menu presses are deliberately swallowed during a session so
    /// gameplay can't end it (see ContentView's tvOS session branch).
    public var onDisconnectRequest: (() -> Void)?

    public init(connection: PunktfunkConnection, manager: GamepadManager) {
        self.connection = connection
        self.manager = manager
    }

    public func start() {
        // Fires immediately with the current selection, then on every change — a switch
        // releases the old controller's wire state before the new one takes over.
        activeSub = manager.$active.sink { [weak self] dc in
            MainActor.assumeIsolated { self?.rebind(to: dc?.controller) }
        }
        #if os(macOS)
        let resign = NSApplication.willResignActiveNotification
        let activate = NSApplication.didBecomeActiveNotification
        #else
        let resign = UIApplication.willResignActiveNotification
        let activate = UIApplication.didBecomeActiveNotification
        #endif
        observers.append(NotificationCenter.default.addObserver(
            forName: resign, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.suspended = true
                self?.releaseAll()
            }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: activate, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.suspended = false
                if let ext = self.bound?.extendedGamepad { self.sync(ext) }
            }
        })
    }

    public func stop() {
        releaseAll()
        rebind(to: nil)
        activeSub = nil
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
    }

    private func rebind(to controller: GCController?) {
        guard controller !== bound else { return }
        releaseAll()
        if let ext = bound?.extendedGamepad {
            ext.valueChangedHandler = nil
            let tp = Self.touchpad(ext)
            tp?.primary.valueChangedHandler = nil
            tp?.secondary.valueChangedHandler = nil
        }
        // Hand the system gestures back to the OS before letting the old pad go — outside a
        // stream the share button's screenshot and the Home overlay are the user's, not ours.
        if let old = bound {
            for element in old.physicalInputProfile.elements.values {
                element.preferredSystemGestureState = .enabled
            }
        }
        if let motion = bound?.motion {
            motion.valueChangedHandler = nil
            // Power the sensors back down — left active they keep the pad streaming
            // gyro/accel over Bluetooth (battery drain) long after the session.
            if motion.sensorsRequireManualActivation { motion.sensorsActive = false }
        }
        bound = controller
        guard let c = controller, let ext = c.extendedGamepad else { return }

        ext.valueChangedHandler = { [weak self] g, _ in
            MainActor.assumeIsolated { self?.sync(g) }
        }
        // Claim EVERY element's system gesture while this pad drives a stream. The OS attaches
        // gestures to several controller buttons — share/create → local screenshot/recording,
        // Home → Game Center overlay (iOS) / Launchpad's Games folder (macOS) — and with a
        // gesture attached the press is the system's, not the game's. During capture the remote
        // session IS the game: the share button must reach the host (e.g. Steam screenshots),
        // the PS button must open the host's Steam overlay. Restored to .enabled on unbind.
        for element in c.physicalInputProfile.elements.values {
            element.preferredSystemGestureState = .disabled
        }
        // The Home/PS button (→ guide; the host maps it to the DualSense PS / Xbox guide bit,
        // BTN_MODE on the virtual xpad — the Steam-overlay button). Driven DIRECTLY from this
        // handler's pressed value (not via buttonMask), because the legacy
        // `extendedGamepad.buttonHome` is unreliable/often nil even when the physical element
        // exists. On tvOS the element is absent (reserved) → nil, the whole block no-ops.
        if let home = c.physicalInputProfile.buttons[GCInputButtonHome] {
            home.pressedChangedHandler = { [weak self] _, _, pressed in
                MainActor.assumeIsolated { self?.sendGuide(down: pressed) }
            }
        }
        // Wake the host pad immediately (pads are created lazily from the first event;
        // a DualSense's UHID handshake + initial lightbar write only start then).
        connection.send(.gamepadAxis(GamepadWire.axisLSX, value: 0, pad: 0))
        sync(ext)

        if let tp = Self.touchpad(ext) {
            tp.primary.valueChangedHandler = { [weak self] _, x, y in
                MainActor.assumeIsolated { self?.touch(finger: 0, x: x, y: y) }
            }
            tp.secondary.valueChangedHandler = { [weak self] _, x, y in
                MainActor.assumeIsolated { self?.touch(finger: 1, x: x, y: y) }
            }
        }
        if let motion = c.motion {
            if motion.sensorsRequireManualActivation { motion.sensorsActive = true }
            motion.valueChangedHandler = { [weak self] m in
                MainActor.assumeIsolated { self?.forwardMotion(m) }
            }
        }
    }

    /// Snapshot the profile into wire state and send every transition since the last one.
    private func sync(_ g: GCExtendedGamepad) {
        guard !suspended else { return }
        let newButtons = Self.buttonMask(g)
        updateEscapeChord(newButtons)
        let changed = newButtons ^ buttons
        if changed != 0 {
            for bit in GamepadWire.allButtons where changed & bit != 0 {
                connection.send(.gamepadButton(bit, down: newButtons & bit != 0, pad: 0))
            }
            buttons = newButtons
        }
        let newAxes: [Int32] = [
            Int32((g.leftThumbstick.xAxis.value * 32767).rounded()),
            Int32((g.leftThumbstick.yAxis.value * 32767).rounded()),
            Int32((g.rightThumbstick.xAxis.value * 32767).rounded()),
            Int32((g.rightThumbstick.yAxis.value * 32767).rounded()),
            Int32((g.leftTrigger.value * 255).rounded()),
            Int32((g.rightTrigger.value * 255).rounded()),
        ]
        for (i, v) in newAxes.enumerated() where v != axes[i] {
            connection.send(.gamepadAxis(UInt32(i), value: v, pad: 0))
            axes[i] = v
        }
    }

    /// Forward the guide (Home/PS) transition directly — it's kept out of `buttonMask` (the legacy
    /// `buttonHome` element is unreliable). Folds into `buttons` so a held PS button is released by
    /// `releaseAll` on focus loss just like the others.
    private func sendGuide(down: Bool) {
        guard !suspended else { return }
        let bit = GamepadWire.guide
        let now = down ? (buttons | bit) : (buttons & ~bit)
        guard now != buttons else { return }
        connection.send(.gamepadButton(bit, down: down, pad: 0))
        buttons = now
    }

    private static func buttonMask(_ g: GCExtendedGamepad) -> UInt32 {
        var b: UInt32 = 0
        if g.dpad.up.isPressed { b |= GamepadWire.dpadUp }
        if g.dpad.down.isPressed { b |= GamepadWire.dpadDown }
        if g.dpad.left.isPressed { b |= GamepadWire.dpadLeft }
        if g.dpad.right.isPressed { b |= GamepadWire.dpadRight }
        if g.buttonMenu.isPressed { b |= GamepadWire.start }
        if g.buttonOptions?.isPressed == true { b |= GamepadWire.back }
        // The share/create/capture element (Xbox Series share, a clone pad's screenshot button —
        // e.g. the GameSir G8's, below its d-pad) folds into back/select too. On pads that expose
        // the create button BOTH as buttonOptions and as the share element this OR is harmless —
        // same wire bit.
        if g.buttons[GCInputButtonShare]?.isPressed == true { b |= GamepadWire.back }
        if g.leftThumbstickButton?.isPressed == true { b |= GamepadWire.leftStickClick }
        if g.rightThumbstickButton?.isPressed == true { b |= GamepadWire.rightStickClick }
        if g.leftShoulder.isPressed { b |= GamepadWire.leftShoulder }
        if g.rightShoulder.isPressed { b |= GamepadWire.rightShoulder }
        // guide (Home/PS) is NOT read here — it's forwarded directly by the Home button's
        // pressedChangedHandler (the legacy `buttonHome` element is unreliable). See `rebind`.
        if g.buttonA.isPressed { b |= GamepadWire.a }
        if g.buttonB.isPressed { b |= GamepadWire.b }
        if g.buttonX.isPressed { b |= GamepadWire.x }
        if g.buttonY.isPressed { b |= GamepadWire.y }
        if Self.touchpad(g)?.button.isPressed == true {
            b |= GamepadWire.touchpadClick
        }
        return b
    }

    /// The touchpad surface of a PlayStation pad — present on both `GCDualSenseGamepad` and
    /// `GCDualShockGamepad` (DualShock 4), which don't share a common touchpad type, so we
    /// downcast either and project the identical `touchpad*` properties. `nil` for any other
    /// controller (Xbox, MFi).
    private static func touchpad(
        _ g: GCExtendedGamepad
    ) -> (primary: GCControllerDirectionPad, secondary: GCControllerDirectionPad,
          button: GCControllerButtonInput)? {
        if let ds = g as? GCDualSenseGamepad {
            return (ds.touchpadPrimary, ds.touchpadSecondary, ds.touchpadButton)
        }
        if let ds4 = g as? GCDualShockGamepad {
            return (ds4.touchpadPrimary, ds4.touchpadSecondary, ds4.touchpadButton)
        }
        return nil
    }

    /// One touchpad finger moved. GC reports ±1 positions and snaps to exactly (0, 0) on
    /// lift — treated as the lift signal (a real finger landing on the precise center
    /// momentarily reads as a lift; harmless for a 1-in-65k coincidence).
    private func touch(finger: Int, x: Float, y: Float) {
        guard !suspended else { return }
        let lifted = x == 0 && y == 0
        if lifted {
            if fingerActive[finger] {
                fingerActive[finger] = false
                connection.sendTouchpad(finger: UInt8(finger), active: false, x: 0, y: 0)
            }
            return
        }
        fingerActive[finger] = true
        let w = GamepadWire.touchpad(x: x, y: y)
        connection.sendTouchpad(finger: UInt8(finger), active: true, x: w.x, y: w.y)
    }

    private func forwardMotion(_ m: GCMotion) {
        guard !suspended else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        guard now &- lastMotionNs >= Self.motionIntervalNs else { return }
        lastMotionNs = now
        // Total acceleration in g: gravity + user when split, else the raw vector.
        let ax: Float
        let ay: Float
        let az: Float
        if m.hasGravityAndUserAcceleration {
            ax = Float(m.gravity.x + m.userAcceleration.x)
            ay = Float(m.gravity.y + m.userAcceleration.y)
            az = Float(m.gravity.z + m.userAcceleration.z)
        } else {
            ax = Float(m.acceleration.x)
            ay = Float(m.acceleration.y)
            az = Float(m.acceleration.z)
        }
        let gs = GamepadWire.gyroLSBPerRadS
        let as_ = GamepadWire.accelLSBPerG
        connection.sendMotion(
            gyro: (
                GamepadWire.motionRaw(Float(m.rotationRate.x), scale: gs),
                GamepadWire.motionRaw(Float(m.rotationRate.y), scale: gs),
                GamepadWire.motionRaw(Float(m.rotationRate.z), scale: gs)
            ),
            accel: (
                GamepadWire.motionRaw(ax, scale: as_),
                GamepadWire.motionRaw(ay, scale: as_),
                GamepadWire.motionRaw(az, scale: as_)
            ))
    }

    /// Unwind everything held on the wire: button-ups, neutral axes, lifted fingers. The
    /// host's virtual pad returns to rest instead of running with the last state.
    /// Arm the disconnect timer when the full chord lands, disarm the moment any of the four
    /// releases. Events only arrive on state CHANGES, so a held chord needs the timer — the
    /// handler won't fire again until something moves.
    private func updateEscapeChord(_ newButtons: UInt32) {
        let held = newButtons & Self.escapeChord == Self.escapeChord
        if held, chordTimer == nil {
            let timer = Timer(timeInterval: Self.disconnectHold, repeats: false) { [weak self] _ in
                Task { @MainActor in self?.onDisconnectRequest?() }
            }
            RunLoop.main.add(timer, forMode: .common)
            chordTimer = timer
        } else if !held, chordTimer != nil {
            chordTimer?.invalidate()
            chordTimer = nil
        }
    }

    private func releaseAll() {
        chordTimer?.invalidate()
        chordTimer = nil
        for bit in GamepadWire.allButtons where buttons & bit != 0 {
            connection.send(.gamepadButton(bit, down: false, pad: 0))
        }
        buttons = 0
        for (i, v) in axes.enumerated() where v != 0 {
            connection.send(.gamepadAxis(UInt32(i), value: 0, pad: 0))
            axes[i] = 0
        }
        for (f, active) in fingerActive.enumerated() where active {
            connection.sendTouchpad(finger: UInt8(f), active: false, x: 0, y: 0)
            fingerActive[f] = false
        }
    }
}
