// Gamepad capture → punktfunk/1 datagrams. Forwards EVERY controller GamepadManager selected —
// each on its own stable wire pad index (pf-client-core's slot model) — for the lifetime of a
// streaming session. One physical controller with no pin is player 0 (byte-identical to the old
// single-pad path); a pin forwards only that one, also as pad 0.
//
// Each forwarded controller gets a `Slot`: its open GC handlers plus the wire state (buttons,
// axes, touchpad fingers, motion throttle) for its pad index — isolated per device so two
// controllers never clobber each other. On connect a slot opens (GamepadArrival declares its
// kind, then input flows); on disconnect / pin change / stop it closes (held state flushed to
// rest on the wire, then GamepadRemove tells the host to tear the pad's virtual device down).
//
// The wire is incremental (one button/axis transition per 18-byte event, accumulated host-side
// into the virtual pad — see punktfunk_core::input::gamepad), so we snapshot the full
// GCExtendedGamepad state on every valueChanged and diff against the previous snapshot. Sticks
// are ±32767 with +y = up (GC already matches, no flip), triggers 0...255. The core folds these
// per-pad transitions into idempotent, sequence-numbered snapshots keyed on the same pad index,
// so all this layer must get right is the index — one controller per slot, one slot per index.
//
// PlayStation-pad extras ride the rich-input plane (0xCC): touchpad contacts normalized
// 0...65535 (origin top-left, +y down — GC's ±1/+y-up is converted here) and motion samples in
// raw DualSense sensor units (gyro 20 LSB per deg/s, accel 10000 LSB per g — derived from the
// host's fixed calibration blob; the conversion lives in ONE place, `Wire`, so a live sign/scale
// correction is a one-line change). The host ignores both unless a pad's virtual device is a
// DualSense or DualShock 4 — both carry a touchpad and motion, so the capture below covers either
// (`GCDualShockGamepad` exposes the same `touchpad*` surface as `GCDualSenseGamepad`).
//
// Unlike mouse/keyboard capture, gamepad forwarding is NOT gated on the mouse-capture toggle — a
// controller can't click local UI, so it always drives the host while the app is active. On
// deactivation, controller switch, or stop, every held control is released on the wire (the host
// pad would otherwise stay stuck on the last state).

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
    private var forwardedSub: AnyCancellable?
    private var observers: [NSObjectProtocol] = []
    /// App inactive → GC stops delivering; everything is released and stays silent.
    private var suspended = false

    /// One forwarded controller: the open device plus the last wire state for its pad index (the
    /// diff base — also what `flush` unwinds). Held per Slot so two controllers never clobber each
    /// other's held buttons/axes/fingers. Mirrors pf-client-core's `Slot`.
    private final class Slot {
        let controller: GCController
        /// Wire pad index (GamepadManager's stable lowest-free assignment), threaded onto every
        /// event this controller sends — the low byte of `flags`.
        let pad: UInt32
        /// The controller KIND declared to the host (GamepadArrival) when the slot opened — the
        /// user's explicit "Controller type" setting when they picked one, else the detected
        /// kind (`GamepadManager.declaredKind(for:)`). NOT the physical pad's kind: local feedback
        /// keys off the live `GCController` subclass instead, so whatever the host DOES send is
        /// applied natively to the pad in the user's hands. What the host sends is bounded by the
        /// emulated type, though — a virtual DualShock 4 has no adaptive-trigger reports in its
        /// protocol, so emulating one gives those up by construction (rumble + lightbar remain).
        let pref: PunktfunkConnection.GamepadType
        var buttons: UInt32 = 0
        var axes: [Int32] = [0, 0, 0, 0, 0, 0]
        var fingerActive: [Bool] = [false, false]
        var lastMotionNs: UInt64 = 0
        // Hold-Select→guide gesture state (pf-client-core's `SelectGesture`, adapted to
        // this class's mask-diff model): a Select pressed ALONE is held out of the mask
        // until it resolves into a tap (delivered on release) or — past `guideHold` — a
        // synthetic guide, down until release.
        var selectPending = false
        var selectAsGuide = false
        /// A delivered tap's release is owed (`tapTimer` scheduled) — its down went out
        /// outside `buttons`, so `flush` must know to lift it.
        var tapReleaseOwed = false
        var gestureTimer: Timer?
        var tapTimer: Timer?
        init(controller: GCController, pad: UInt32, pref: PunktfunkConnection.GamepadType) {
            self.controller = controller
            self.pad = pad
            self.pref = pref
        }
    }

    /// Open forwarded controllers, one Slot per physical pad on its own wire index. Reconciled
    /// against `manager.forwarded` (empty until a session's `start`, cleared by `stop`).
    private var slots: [Slot] = []

    /// Motion forwarding floor: ≥ 4 ms between samples (≈ 250 Hz, the DualSense's own rate).
    private static let motionIntervalNs: UInt64 = 4_000_000

    /// The cross-client controller escape chord (pf-client-core's `ESCAPE_CHORD`):
    /// L1+R1+Start+Select held together — four simultaneous buttons no game uses, so normal
    /// play can't trip it. Held for `disconnectHold` it ends the session via
    /// `onDisconnectRequest`; the chord keeps forwarding to the host meanwhile (the user is
    /// leaving anyway). The desktop clients' quick-press step (leave fullscreen / release
    /// capture) has no Apple equivalent worth wiring — macOS has ⌃⌥⇧Q/D, touch has the HUD.
    /// Internal rather than private only so `GamepadEscapeChordTests` can pin it against
    /// `escapeChordElements` below — the two must not drift.
    static let escapeChord: UInt32 =
        GamepadWire.leftShoulder | GamepadWire.rightShoulder | GamepadWire.start | GamepadWire.back
    /// `escapeChord`'s four elements by GameController alias — the ONLY system gestures claimed
    /// while forwarding is off (see `openSlot`). Kept beside the mask it mirrors: change one and
    /// change the other, or the chord silently stops reaching us on tvOS. A test asserts the two
    /// agree, because the failure is invisible until someone is stuck in a stream on an Apple TV.
    static let escapeChordElements = [
        GCInputLeftShoulder, GCInputRightShoulder, GCInputButtonMenu, GCInputButtonOptions,
    ]
    /// pf-client-core's `DISCONNECT_HOLD` — the same 1.5 s on every client.
    private static let disconnectHold: TimeInterval = 1.5
    /// pf-client-core's `GUIDE_HOLD`: hold Select alone this long → the HOST's guide goes
    /// down (until release, so a long hold is the host's long-press — a Gaming-Mode
    /// host's QAM). The gesture exists because iOS reserves the physical Home press (the
    /// Game Overlay; sanctioned opt-out only via the user's iOS 27+ Home-button setting)
    /// and tvOS never delivers it at all.
    private static let guideHold: TimeInterval = 0.35
    /// pf-client-core's `TAP_PRESS`: a held-back Select tap is delivered as a press with
    /// its release this far behind — back-to-back transitions can fold into nothing in
    /// the host's per-pad input fold.
    private static let tapPress: TimeInterval = 0.05
    private var chordTimer: Timer?
    /// Fired ON MAIN once the escape chord has been held `disconnectHold` — the session owner
    /// disconnects. On tvOS this (plus the Siri Remote's hold-Back) is the ONLY way out of a
    /// stream with a controller: B/Menu presses are deliberately swallowed during a session so
    /// gameplay can't end it (see ContentView's tvOS session branch).
    public var onDisconnectRequest: (() -> Void)?

    /// Forward this device's controllers to the host at all (`Settings.gamepadForwarding`,
    /// default true). Off is for a couch whose controller reaches the host another way — USB
    /// passthrough such as VirtualHere, or a pad plugged into the host itself — where
    /// forwarding as well would give the host two pads for one pair of hands.
    ///
    /// Off still opens slots and tracks button state; it just sends nothing (see `wire`). That
    /// is deliberate, not laziness: the escape chord is read off the same slots, and on tvOS it
    /// is the ONLY controller way out of a stream — a session that silently lost its exit
    /// because a forwarding preference was off would be a worse bug than the one this fixes.
    /// Unlike pf-client-core's slots, GameController claims nothing exclusive, so holding one
    /// open costs the host nothing and blocks no passthrough tool.
    public let forwarding: Bool

    /// The connection, or nil while forwarding is off — every wire send goes through this, so
    /// "don't forward" is one fact in one place rather than a condition at twelve call sites.
    private var wire: PunktfunkConnection? { forwarding ? connection : nil }

    /// Forward the raw guide + share/QAM presses (`EffectiveSettings.systemButtonsForward`,
    /// default true on Apple — where the OS shows its own overlay for them, that's the OS's
    /// business; local mode exists for profile parity with the Gaming-Mode clients).
    public let systemForward: Bool
    /// The hold-Select guide gesture (`EffectiveSettings.guideGestureEnabled` — auto = on
    /// everywhere but macOS). See `guideHold`.
    public let guideGesture: Bool

    public init(
        connection: PunktfunkConnection, manager: GamepadManager, forwarding: Bool = true,
        systemForward: Bool = true, guideGesture: Bool = false
    ) {
        self.connection = connection
        self.manager = manager
        self.forwarding = forwarding
        self.systemForward = systemForward
        self.guideGesture = guideGesture
    }

    public func start() {
        // Session-scoped index assignment: a controller pinned before the session forwards as
        // pad 0 (pf-client-core assigns indices at slot-open time, not app-launch time).
        manager.resetForwardingAssignment()
        // Fires immediately with the current forwarded set, then on every change — a connect,
        // disconnect, or pin change reconciles the open slots against it (opening/closing devices
        // and flushing wire state so nothing sticks down).
        forwardedSub = manager.$forwarded.sink { [weak self] list in
            MainActor.assumeIsolated { self?.reconcile(list) }
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
                // Re-send every open pad's current state (GC delivered nothing while inactive).
                for slot in self.slots {
                    if let ext = slot.controller.extendedGamepad { self.sync(slot, ext) }
                }
            }
        })
    }

    public func stop() {
        closeAllSlots()
        forwardedSub = nil
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
    }

    /// Bring `slots` in line with the forwarded set: close any slot no longer wanted (flushing its
    /// held wire state and sending GamepadRemove first) and open any newly-forwarded controller into
    /// its assigned wire index. A controller that stays forwarded keeps its slot untouched, so a
    /// second pad connecting never disturbs the first. Mirrors pf-client-core's `reconcile_slots`.
    private func reconcile(_ forwarded: [GamepadManager.DiscoveredController]) {
        let wantIDs = Set(forwarded.map { ObjectIdentifier($0.controller) })
        for slot in slots where !wantIDs.contains(ObjectIdentifier(slot.controller)) {
            closeSlot(slot)
        }
        for dc in forwarded where !slots.contains(where: { $0.controller === dc.controller }) {
            openSlot(dc)
        }
        // A chord-holding pad may have just unplugged — re-evaluate so a stale hold disarms.
        updateEscapeChord()
    }

    /// Open one forwarded controller on its assigned wire index: attach GC handlers, claim its
    /// system gestures, declare its kind (GamepadArrival — before any input), then wake the host
    /// pad and send its initial state. Skipped when the pad has no wire index (every slot taken)
    /// or exposes no extended profile.
    private func openSlot(_ dc: GamepadManager.DiscoveredController) {
        guard let pad = manager.padIndex(for: dc), let ext = dc.controller.extendedGamepad else { return }
        let c = dc.controller
        let slot = Slot(controller: c, pad: UInt32(pad), pref: manager.declaredKind(for: dc))
        slots.append(slot)

        ext.valueChangedHandler = { [weak self, weak slot] g, _ in
            MainActor.assumeIsolated { if let self, let slot { self.sync(slot, g) } }
        }
        // Claim EVERY element's system gesture while this pad drives a stream. The OS attaches
        // gestures to several controller buttons — share/create → local screenshot/recording,
        // Home → Game Center overlay (iOS) / Launchpad's Games folder (macOS) — and with a
        // gesture attached the press is the system's, not the game's. During capture the remote
        // session IS the game: the share button must reach the host (e.g. Steam screenshots),
        // the PS button must open the host's Steam overlay. Restored to .enabled on close.
        //
        // With forwarding OFF none of that applies — no press reaches the host, so taking the
        // user's screenshot gesture away buys nothing. NARROWED, not skipped: the escape chord
        // is still read off this slot, and on tvOS it is the only controller way out of a
        // stream, so the chord's own four elements keep their claim. (Menu especially: leave
        // its gesture attached on tvOS and the press is the system's — the chord would never
        // complete and the session would have no controller exit at all.)
        let claimed = forwarding
            ? Array(c.physicalInputProfile.elements.values)
            : Self.escapeChordElements.compactMap { c.physicalInputProfile.elements[$0] }
        for element in claimed {
            element.preferredSystemGestureState = .disabled
        }
        // The Home/PS button (→ guide; the host maps it to the DualSense PS / Xbox guide bit,
        // BTN_MODE on the virtual xpad — the Steam-overlay button). On iOS 26 the OS opens its
        // Game Overlay for this press regardless of the gesture claim below (the app is
        // LSApplicationCategoryType=games, which enrolls it); the sanctioned per-controller
        // opt-out is the USER's iOS 27+ Home-button setting. TODO(iOS 27 SDK): read
        // `GCControllerHomeButtonSettingsManager` and surface a one-time
        // `openControllerHomeButtonSettings(for:)` deep-link so users can hand the button to
        // the stream — the class is Swift-only and 27.0+, so it needs the Xcode 27 SDK to
        // even compile. Until then hold-Select is the reliable route. Driven DIRECTLY from this
        // handler's pressed value (not via buttonMask), because the legacy
        // `extendedGamepad.buttonHome` is unreliable/often nil even when the physical element
        // exists. On tvOS the element is absent (reserved) → nil, the whole block no-ops.
        if let home = c.physicalInputProfile.buttons[GCInputButtonHome] {
            home.pressedChangedHandler = { [weak self, weak slot] _, _, pressed in
                MainActor.assumeIsolated { if let self, let slot { self.sendGuide(slot, down: pressed) } }
            }
        }
        // Declare this pad's controller KIND before any of its input, so the host builds a
        // matching virtual device — the user's chosen type when they picked one, else per-pad
        // detection (mixed types — pad 0 a DualSense, pad 1 an Xbox pad). This declaration is
        // what the host actually builds from, so it MUST carry an explicit setting; the
        // handshake's session default is only the fallback for a pad that never declares. The
        // core re-sends it a few times against datagram loss; an older host ignores it and uses
        // the session-default kind. Then wake the host pad (pads are created lazily from the first
        // event; a DualSense's UHID handshake + initial lightbar write only start then).
        wire?.send(.gamepadArrival(pref: slot.pref.rawValue, pad: slot.pad))
        wire?.send(.gamepadAxis(GamepadWire.axisLSX, value: 0, pad: slot.pad))
        sync(slot, ext)

        if let tp = Self.touchpad(ext) {
            tp.primary.valueChangedHandler = { [weak self, weak slot] _, x, y in
                MainActor.assumeIsolated { if let self, let slot { self.touch(slot, finger: 0, x: x, y: y) } }
            }
            tp.secondary.valueChangedHandler = { [weak self, weak slot] _, x, y in
                MainActor.assumeIsolated { if let self, let slot { self.touch(slot, finger: 1, x: x, y: y) } }
            }
        }
        // Motion is wire-only — `forwardMotion` has nothing to do with forwarding off, and no
        // local feature reads it. Powering the IMU anyway costs the pad real battery (it streams
        // gyro + accel continuously over Bluetooth, which is why `closeSlot` is careful to power
        // it back down), so with nothing to forward we simply never turn it on.
        if forwarding, let motion = c.motion {
            if motion.sensorsRequireManualActivation { motion.sensorsActive = true }
            motion.valueChangedHandler = { [weak self, weak slot] m in
                MainActor.assumeIsolated { if let self, let slot { self.forwardMotion(slot, m) } }
            }
        }
    }

    /// Flush a slot's held wire state (so nothing sticks down host-side) and signal the host to tear
    /// its virtual device down (GamepadRemove), then detach GC handlers, hand the system gestures
    /// back, and power the sensors down. Wire-only until the GC cleanup, so it is safe even when the
    /// device already physically unplugged. Mirrors pf-client-core's `close_slot_at`.
    private func closeSlot(_ slot: Slot) {
        flush(slot)
        // Sent after the flush so the core stamps it with a seq past the zeroing snapshots; the host
        // seq-gates it, so a reordered snapshot can't resurrect the removed pad.
        wire?.send(.gamepadRemove(pad: slot.pad))
        let c = slot.controller
        if let ext = c.extendedGamepad {
            ext.valueChangedHandler = nil
            let tp = Self.touchpad(ext)
            tp?.primary.valueChangedHandler = nil
            tp?.secondary.valueChangedHandler = nil
        }
        c.physicalInputProfile.buttons[GCInputButtonHome]?.pressedChangedHandler = nil
        // Hand the system gestures back to the OS before letting the pad go — outside a stream the
        // share button's screenshot and the Home overlay are the user's, not ours.
        for element in c.physicalInputProfile.elements.values {
            element.preferredSystemGestureState = .enabled
        }
        if let motion = c.motion {
            motion.valueChangedHandler = nil
            // Power the sensors back down — left active they keep the pad streaming gyro/accel
            // over Bluetooth (battery drain) long after the session.
            if motion.sensorsRequireManualActivation { motion.sensorsActive = false }
        }
        slots.removeAll { $0 === slot }
    }

    private func closeAllSlots() {
        while let slot = slots.first { closeSlot(slot) }
        chordTimer?.invalidate()
        chordTimer = nil
    }

    /// Snapshot the profile into a slot's wire state and send every transition since the last one,
    /// tagged with the slot's wire pad index.
    private func sync(_ slot: Slot, _ g: GCExtendedGamepad) {
        guard !suspended else { return }
        // guide is driven separately (`sendGuide`, off the Home handler) and deliberately kept out
        // of `buttonMask`. Preserve its current held state here so the XOR diff below never sees it
        // as "changed" — otherwise the first stick/button move after a guide press would emit a
        // spurious guide-UP while the button is still physically held (and drop the bit from
        // `slot.buttons`, swallowing the real release too). `flush`/`allButtons` still release it.
        var raw = Self.buttonMask(g)
        // Raw system buttons stay local when passthrough is off: misc1 (share/QAM) is
        // masked here, guide is gated at its own handler.
        if !systemForward { raw &= ~GamepadWire.misc1 }
        // The hold-Select gesture rewrites the mask: a Select pressed alone is held out
        // until it resolves (tap on release / synthetic guide past the threshold).
        if guideGesture { raw = gestureFiltered(slot, raw) }
        let newButtons = raw | (slot.buttons & GamepadWire.guide)
        let changed = newButtons ^ slot.buttons
        if changed != 0 {
            for bit in GamepadWire.allButtons where changed & bit != 0 {
                wire?.send(.gamepadButton(bit, down: newButtons & bit != 0, pad: slot.pad))
            }
            slot.buttons = newButtons
        }
        let newAxes: [Int32] = [
            Int32(g.leftThumbstick.xAxis.value * 32767),
            Int32(g.leftThumbstick.yAxis.value * 32767),
            Int32(g.rightThumbstick.xAxis.value * 32767),
            Int32(g.rightThumbstick.yAxis.value * 32767),
            Int32(g.leftTrigger.value * 255),
            Int32(g.rightTrigger.value * 255),
        ]
        for (i, v) in newAxes.enumerated() where v != slot.axes[i] {
            wire?.send(.gamepadAxis(UInt32(i), value: v, pad: slot.pad))
            slot.axes[i] = v
        }
        updateEscapeChord()
    }

    /// The hold-Select→guide state machine over one sync's raw mask (pf-client-core's
    /// `SelectGesture` rules): Select pressed ALONE is suppressed while pending; another
    /// button joining makes it real (unsuppressed — the diff sends its down); released
    /// inside `guideHold` it's a tap, delivered out-of-band on release with the release
    /// `tapPress` behind; past the threshold `gestureHoldFired` turned it into a synthetic
    /// guide, lifted here when Select physically releases.
    ///
    /// One deliberate divergence from the Rust worker: while transformed into a guide the
    /// Select stays OUT of `slot.buttons`, so the escape chord doesn't complete on top of
    /// an in-flight guide-hold — release Select and press the chord plainly instead (the
    /// chord's four-at-once press never lingers in pending long enough to be affected).
    private func gestureFiltered(_ slot: Slot, _ raw: UInt32) -> UInt32 {
        let back = GamepadWire.back
        let backDown = raw & back != 0
        let othersDown = raw & ~back != 0
        if slot.selectAsGuide {
            if backDown { return raw & ~back }
            slot.selectAsGuide = false
            sendGuide(slot, down: false, raw: false)
            return raw
        }
        if slot.selectPending {
            if !backDown {
                endPending(slot)
                deliverTap(slot)
                return raw
            }
            if othersDown {
                // A combo after all — Select unsuppresses and the diff sends its down.
                endPending(slot)
                return raw
            }
            return raw & ~back
        }
        if backDown, !othersDown, slot.buttons & back == 0 {
            // Newly pressed, alone: hold it back. An owed tap release goes out first so
            // the host never sees two downs in a row.
            if slot.tapReleaseOwed { finishTap(slot) }
            slot.selectPending = true
            let timer = Timer(timeInterval: Self.guideHold, repeats: false) { [weak self, weak slot] _ in
                Task { @MainActor in
                    if let self, let slot { self.gestureHoldFired(slot) }
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            slot.gestureTimer?.invalidate()
            slot.gestureTimer = timer
            return raw & ~back
        }
        return raw
    }

    /// The hold threshold passed with Select still pending → it IS the guide now, down
    /// until the physical release (`gestureFiltered`'s `selectAsGuide` branch lifts it).
    private func gestureHoldFired(_ slot: Slot) {
        guard slot.selectPending else { return }
        slot.selectPending = false
        slot.gestureTimer = nil
        slot.selectAsGuide = true
        sendGuide(slot, down: true, raw: false)
    }

    private func endPending(_ slot: Slot) {
        slot.selectPending = false
        slot.gestureTimer?.invalidate()
        slot.gestureTimer = nil
    }

    /// Deliver a held-back Select tap: the press now, its release `tapPress` behind. Both
    /// sends bypass `slot.buttons` (the raw mask no longer carries Select, so the diff
    /// stays consistent); `tapReleaseOwed` is what `flush` checks so the press can't
    /// outlive the slot.
    private func deliverTap(_ slot: Slot) {
        wire?.send(.gamepadButton(GamepadWire.back, down: true, pad: slot.pad))
        slot.tapReleaseOwed = true
        let timer = Timer(timeInterval: Self.tapPress, repeats: false) { [weak self, weak slot] _ in
            Task { @MainActor in
                if let self, let slot { self.finishTap(slot) }
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        slot.tapTimer?.invalidate()
        slot.tapTimer = timer
    }

    private func finishTap(_ slot: Slot) {
        guard slot.tapReleaseOwed else { return }
        slot.tapReleaseOwed = false
        slot.tapTimer?.invalidate()
        slot.tapTimer = nil
        wire?.send(.gamepadButton(GamepadWire.back, down: false, pad: slot.pad))
    }

    /// Forward the guide (Home/PS) transition directly — it's kept out of `buttonMask` (the legacy
    /// `buttonHome` element is unreliable). Folds into the slot's `buttons` so a held PS button is
    /// released by `flush` on focus loss / close just like the others. `raw: true` marks the
    /// physical Home handler's calls, which the system-buttons policy can keep local; the
    /// gesture's synthetic transitions pass `raw: false` and always go out.
    private func sendGuide(_ slot: Slot, down: Bool, raw: Bool = true) {
        if raw, !systemForward { return }
        guard !suspended else { return }
        let bit = GamepadWire.guide
        let now = down ? (slot.buttons | bit) : (slot.buttons & ~bit)
        guard now != slot.buttons else { return }
        wire?.send(.gamepadButton(bit, down: down, pad: slot.pad))
        slot.buttons = now
    }

    private static func buttonMask(_ g: GCExtendedGamepad) -> UInt32 {
        var b: UInt32 = 0
        if g.dpad.up.isPressed { b |= GamepadWire.dpadUp }
        if g.dpad.down.isPressed { b |= GamepadWire.dpadDown }
        if g.dpad.left.isPressed { b |= GamepadWire.dpadLeft }
        if g.dpad.right.isPressed { b |= GamepadWire.dpadRight }
        if g.buttonMenu.isPressed { b |= GamepadWire.start }
        if g.buttonOptions?.isPressed == true { b |= GamepadWire.back }
        // The dedicated share/create/capture element (Xbox-Series Share, DualSense Create, a clone
        // pad's screenshot button — e.g. the GameSir G8's, below its d-pad) → the wire's capture
        // bit, matching the Rust client's `Button::Misc1 => wire::BTN_MISC1`. On an Xbox-Series pad
        // this is a button physically DISTINCT from View (buttonOptions, above), so it must not
        // collapse onto back — the host reads MISC1 as its own control (DualSense mute / Steam
        // quick-access). Caveat: a pad that surfaces ONE physical button as both buttonOptions and
        // this share element now emits back+misc1 for it — harmless on a plain xpad session (no
        // misc button) and rare otherwise. NOTE: on-glass verify on a real Xbox-Series pad.
        if g.buttons[GCInputButtonShare]?.isPressed == true { b |= GamepadWire.misc1 }
        if g.leftThumbstickButton?.isPressed == true { b |= GamepadWire.leftStickClick }
        if g.rightThumbstickButton?.isPressed == true { b |= GamepadWire.rightStickClick }
        if g.leftShoulder.isPressed { b |= GamepadWire.leftShoulder }
        if g.rightShoulder.isPressed { b |= GamepadWire.rightShoulder }
        // guide (Home/PS) is NOT read here — it's forwarded directly by the Home button's
        // pressedChangedHandler (the legacy `buttonHome` element is unreliable). See `openSlot`.
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

    /// One touchpad finger moved on a slot's pad. GC reports ±1 positions and snaps to exactly
    /// (0, 0) on lift — treated as the lift signal (a real finger landing on the precise center
    /// momentarily reads as a lift; harmless for a 1-in-65k coincidence).
    private func touch(_ slot: Slot, finger: Int, x: Float, y: Float) {
        guard !suspended else { return }
        let lifted = x == 0 && y == 0
        if lifted {
            if slot.fingerActive[finger] {
                slot.fingerActive[finger] = false
                wire?.sendTouchpad(pad: UInt8(slot.pad), finger: UInt8(finger), active: false, x: 0, y: 0)
            }
            return
        }
        slot.fingerActive[finger] = true
        let w = GamepadWire.touchpad(x: x, y: y)
        wire?.sendTouchpad(pad: UInt8(slot.pad), finger: UInt8(finger), active: true, x: w.x, y: w.y)
    }

    private func forwardMotion(_ slot: Slot, _ m: GCMotion) {
        guard !suspended else { return }
        let now = DispatchTime.now().uptimeNanoseconds
        guard now &- slot.lastMotionNs >= Self.motionIntervalNs else { return }
        slot.lastMotionNs = now
        // Total acceleration in g: gravity + user when split, else the raw vector — then NEGATED
        // into the wire's convention.
        //
        // Apple reports acceleration as the gravity VECTOR: a device lying flat face-up reads
        // z = −1, because gravity points down. An accelerometer physically measures proper
        // acceleration, which at rest is the +1 g normal force pushing UP, and that is what a
        // DualSense's report — the wire's convention — carries. The two are exact negatives, so
        // every sample we sent was upside down, on both branches (`m.acceleration` follows the
        // same Apple convention as the gravity/user split).
        //
        // Measured on glass 2026-08-07 (G16): a DualSense flat and face-up, streamed from an
        // iPhone to a Linux host, arrived at hid-playstation as z = −0.99 g where +1.00 was owed.
        // Magnitude was 1.006 g, so the SCALE was already right — this is purely direction.
        // `rotationRate` is a true angular rate and needs no flip; the same session confirmed yaw
        // came through with the correct sign.
        let ax: Float
        let ay: Float
        let az: Float
        if m.hasGravityAndUserAcceleration {
            ax = -Float(m.gravity.x + m.userAcceleration.x)
            ay = -Float(m.gravity.y + m.userAcceleration.y)
            az = -Float(m.gravity.z + m.userAcceleration.z)
        } else {
            ax = -Float(m.acceleration.x)
            ay = -Float(m.acceleration.y)
            az = -Float(m.acceleration.z)
        }
        let gs = GamepadWire.gyroLSBPerRadS
        let as_ = GamepadWire.accelLSBPerG
        wire?.sendMotion(
            pad: UInt8(slot.pad),
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

    /// Arm the disconnect timer when ANY forwarded pad holds the full escape chord, disarm the
    /// moment none do — a release, or the holding pad unplugged (pf-client-core's `chord_held` is
    /// likewise any-slot). GC events only arrive on state CHANGES, so a held chord needs the timer:
    /// the handler won't fire again until something moves.
    private func updateEscapeChord() {
        let held = slots.contains { $0.buttons & Self.escapeChord == Self.escapeChord }
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

    /// Unwind everything a slot holds on the wire: button-ups, neutral axes, lifted fingers. The
    /// host's virtual pad returns to rest instead of running with the last state. Wire events only
    /// (no GC calls) — safe against an already-removed device. Does NOT close the slot or send
    /// GamepadRemove (that's `closeSlot`).
    private func flush(_ slot: Slot) {
        // Gesture first: a pending (never-sent) Select just drops, an owed tap release
        // goes out, and a transformed guide's bit — folded into `buttons` by `sendGuide`
        // — is lifted by the loop below like any held button.
        endPending(slot)
        slot.selectAsGuide = false
        if slot.tapReleaseOwed { finishTap(slot) }
        for bit in GamepadWire.allButtons where slot.buttons & bit != 0 {
            wire?.send(.gamepadButton(bit, down: false, pad: slot.pad))
        }
        slot.buttons = 0
        for (i, v) in slot.axes.enumerated() where v != 0 {
            wire?.send(.gamepadAxis(UInt32(i), value: 0, pad: slot.pad))
            slot.axes[i] = 0
        }
        for (f, active) in slot.fingerActive.enumerated() where active {
            wire?.sendTouchpad(pad: UInt8(slot.pad), finger: UInt8(f), active: false, x: 0, y: 0)
            slot.fingerActive[f] = false
        }
    }

    /// Flush every open slot's held state (app deactivation) — keeps the slots open (GC just stops
    /// delivering; resume re-syncs), disarms the escape chord. Distinct from `closeAllSlots`, which
    /// also sends GamepadRemove and detaches handlers.
    private func releaseAll() {
        chordTimer?.invalidate()
        chordTimer = nil
        for slot in slots { flush(slot) }
    }
}
