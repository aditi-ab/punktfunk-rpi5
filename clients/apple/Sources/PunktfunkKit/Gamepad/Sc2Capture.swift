// One captured Steam Controller 2 — the glue between the BLE transport (`Sc2BleLink`) and the
// punktfunk wire, modeled on Android's `Sc2Capture.kt` with Apple idioms per `GamepadCapture`:
//
// - **Raw plane (the point):** every input report is forwarded byte-for-byte
//   (`PunktfunkConnection.sendHidReport` → the host's as-is virtual 28DE:1302 pad, which Steam
//   Input drives like the physical controller) — with ONE exception: `Sc2ImuGate` zeroes a
//   frozen (gyro-off) IMU block out of state reports, so a stale resting sample can't drive
//   Steam's desktop gyro-mouse (the cursor-fly the bench debugged 2026-06-08).
// - **Typed mirror:** buttons/sticks/triggers are ALSO diffed onto the ordinary per-transition
//   plane, so the emergency exit chord works, and a host that degraded the kind still gets a
//   playable controller. No rich Motion/Touchpad is ever sent for an SC2 — its IMU rides inside
//   the opaque raw report; the capture never uses the motion plane.
// - **Raw return:** the host's hidraw writes (Steam's 0x80 rumble outputs, lizard/IMU feature
//   settings) arrive via `GamepadFeedback`'s hidRaw sink → `onHidRaw` → the link, landing on the
//   real controller's motors/firmware.
//
// The wire slot is claimed LAZILY on the first parsed state report (`GamepadArrival` pref 9 —
// `GamepadPref::SteamController2` — before any input; an idle radio stays invisible to the
// host) and released on link drop / suspend / stop, so pad indices never leak. The index comes
// from `GamepadManager.reserveExternalPadIndex()` — the SAME lowest-free allocator the
// GameController slots use, so an SC2 and a GC pad can never collide.
//
// No global "SC2 is active" suppression flag exists here (the obvious such design mutes ALL
// pads' normal feed — a known trap): on punktfunk there is no double feed by
// construction — GameController never surfaces the raw Valve device on Apple, and the BLE
// lizard-mode kb/mouse never produces gamepad events — so no suppression wiring exists here.
// If GameController ever does surface it, the designed-in idioms are the per-plane source-drop
// (the DeviceGyro precedent) and GamepadCapture's single computed `wire` nil-gate; wire one of
// those rather than resurrecting a global flag.
//
// Threading: BLE reports arrive on the link's serial queue; the host's hidRaw replay arrives on
// the feedback drain thread; start/stop/suspend run on the main actor. All mutable state sits
// behind one lock (the Android port's slot-table contract); the pad-index reservation hops to
// the main actor, and reports are dropped until the claim lands (a few frames at ~66 Hz —
// idempotent state, nothing missed).

#if os(iOS) || os(macOS)

#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Foundation

private let log = ClientLog(category: "gamepad")

public final class Sc2Capture {
    private let connection: PunktfunkConnection
    private let manager: GamepadManager
    /// The BLE delegate/report queue — USER_INTERACTIVE (SDL's warning: BLE packets are
    /// silently dropped if the consumer stalls).
    private let queue = DispatchQueue(
        label: "io.unom.punktfunk.sc2-ble",
        qos: .userInteractive)
    private var link: Sc2BleLink!
    private var observers: [NSObjectProtocol] = []

    /// Guards every field below (see the threading note in the header).
    private let lock = NSLock()
    private var padIndex: UInt8?
    private var claimPending = false
    private var stopped = false
    /// App inactive → BLE is released and the slot freed; resume re-acquires (the recommended
    /// backgrounding behavior for a CoreBluetooth central).
    private var suspended = false
    // Typed-mirror diff state (wire units).
    private var wireButtons: UInt32 = 0
    private var lastAxis = [Int32](repeating: Int32.min, count: 6)
    /// Zeroes a frozen (gyro-off) IMU block out of forwarded state reports — see `Sc2ImuGate`.
    private let imuGate = Sc2ImuGate()
    /// Reusable up-path buffer: the gated report is copied in and sent from here, so the raw
    /// plane costs no per-report allocation beyond the link's own framing.
    private var rawBuf = [UInt8](repeating: 0, count: 64)
    /// Armed while the escape chord is held (fires `onDisconnectRequest` on main).
    private var chordWork: DispatchWorkItem?

    /// The cross-client controller escape chord, read off this capture's own typed mirror —
    /// MUST stay equal to `GamepadCapture.escapeChord` (pinned by `Sc2EscapeChordMirrorTests`;
    /// re-declared here because the original is main-actor-isolated and this class reads the
    /// mask on the BLE queue). Held `disconnectHold` it ends the session, so a captured SC2 —
    /// whose raw feed bypasses GamepadCapture entirely — can still exit the stream.
    static let escapeChord: UInt32 =
        GamepadWire.leftShoulder | GamepadWire.rightShoulder | GamepadWire.start | GamepadWire.back
    /// pf-client-core's `DISCONNECT_HOLD` — the same 1.5 s on every client (and the same value
    /// as GamepadCapture's private `disconnectHold`; the mirror test pins it).
    static let disconnectHold: TimeInterval = 1.5

    /// Fired ON MAIN once the escape chord has been held `disconnectHold` — the session owner
    /// disconnects (same contract as `GamepadCapture.onDisconnectRequest`).
    public var onDisconnectRequest: (() -> Void)?

    /// The capture's claim/release edges, for the stream surface. `captured` fires when the
    /// wire slot lands (the host is building its virtual SC2 — at stream start, or whenever
    /// the pad powers on mid-session); `released` on every teardown of a CLAIMED slot
    /// (power-off, background, stop) and never while unclaimed. Delivered ON MAIN, like
    /// `onDisconnectRequest` — the capture otherwise leaves no UI trace at all, since the
    /// device never enters the GameController world the Controllers page lists.
    public enum Phase: Equatable {
        case captured(pad: UInt8)
        case released
    }
    /// Fired ON MAIN on `Phase` edges — the session owner surfaces the passthrough badge.
    public var onPhaseChange: ((Phase) -> Void)?

    public init(connection: PunktfunkConnection, manager: GamepadManager) {
        self.connection = connection
        self.manager = manager
        link = Sc2BleLink(
            queue: queue,
            onReport: { [weak self] report in self?.handleReport(report) },
            onClosed: { [weak self] in
                // Controller powered off / out of range. Release the slot (the punktfunk
                // analogue of "Steam sees a REAL disconnect": the host tears down /
                // neutralizes its virtual pad) — the link keeps re-acquiring on its own 2 s
                // poll, and the next connection re-claims + re-proves its IMU live.
                self?.releaseSlot(reason: "link closed")
            })
    }

    /// Begin acquisition (main actor: it registers the app-lifecycle observers). The wire slot
    /// is claimed later, on the first state report.
    @MainActor
    public func start() {
        lock.lock()
        stopped = false
        suspended = false
        lock.unlock()
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
            guard let self else { return }
            self.lock.lock()
            self.suspended = true
            self.lock.unlock()
            // Release BLE while backgrounded (and the slot with it — a host pad frozen on the
            // last raw state would otherwise hold its buttons for the whole background stay).
            self.releaseSlot(reason: "app inactive")
            self.link.stop()
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: activate, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            self.lock.lock()
            self.suspended = false
            let dead = self.stopped
            self.lock.unlock()
            if !dead { self.link.start() } // reacquire; the first report re-claims a slot
        })
        link.start()
    }

    /// Tear everything down: link stopped (unsubscribe, cancel, stop scanning), slot released,
    /// typed state cleared. Idempotent (main actor, like `start`).
    @MainActor
    public func stop() {
        lock.lock()
        let wasStopped = stopped
        stopped = true
        lock.unlock()
        guard !wasStopped else { return }
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
        releaseSlot(reason: "stop")
        link.stop()
    }

    /// Replay one host raw write on the physical pad — wire this to `GamepadFeedback`'s hidRaw
    /// sink. Called on the feedback drain thread; `kind` is `PUNKTFUNK_HID_RAW_OUTPUT` (0) /
    /// `PUNKTFUNK_HID_RAW_FEATURE` (1) and `data` the id-first frame. NO main-actor hop — the
    /// rumble replay runs at Steam's 25–40 ms resend cadence and the GATT write happens on the
    /// BLE queue anyway.
    public func onHidRaw(pad: UInt8, kind: UInt8, data: [UInt8]) {
        lock.lock()
        let claimed = padIndex
        lock.unlock()
        guard claimed == pad else { return } // addressed to some other controller
        link.writeRaw(kind: kind, frame: data)
    }

    // MARK: - Report path (BLE queue)

    private func handleReport(_ framed: [UInt8]) {
        guard let id = framed.first else { return }
        // Wireless status is authoritative only through a Puck dongle (USB — out of scope on
        // Apple); a BLE pad emits it too, truthfully saying "no radio link", and acting on it
        // tore the slot down 255 ms after creation on Android's first on-glass run. Swallow.
        if id == Sc2Device.idWireless || id == Sc2Device.idWirelessX { return }
        var state = Sc2Device.State()
        var report = framed
        let isState = Sc2Device.parseState(report, into: &state)
        lock.lock()
        if stopped || suspended {
            lock.unlock()
            return
        }
        guard let pad = padIndex else {
            // Lazy slot claim on the FIRST parsed state report, BEFORE any input. The claim
            // hops to the main actor; reports (including this one) drop until it lands —
            // idempotent snapshots at ~66 Hz, nothing is missed. `claimPending` also clears on
            // a full table (all 16 indices taken), so a later report simply retries.
            let shouldClaim = isState && !claimPending
            if shouldClaim { claimPending = true }
            lock.unlock()
            if shouldClaim { claimSlot() }
            return
        }
        if !isState {
            // Battery/status and future report types still belong to the as-is stream.
            forwardRawLocked(&report, pad: pad)
            lock.unlock()
            return
        }
        forwardRawLocked(&report, pad: pad)
        mirrorTypedLocked(state, pad: pad)
        lock.unlock()
    }

    /// Forward one id-first report on the raw plane: IMU-gate in place, copy into the reusable
    /// buffer, send. Caller holds `lock`.
    private func forwardRawLocked(_ report: inout [UInt8], pad: UInt8) {
        imuGate.apply(&report)
        let n = min(report.count, rawBuf.count)
        rawBuf.replaceSubrange(0 ..< n, with: report[0 ..< n])
        rawBuf.withUnsafeBytes { buf in
            connection.sendHidReport(pad: pad, UnsafeRawBufferPointer(rebasing: buf[0 ..< n]))
        }
    }

    /// Diff the parsed state onto the per-transition plane (buttons + axes, on change only) and
    /// feed the escape chord. Caller holds `lock`.
    private func mirrorTypedLocked(_ state: Sc2Device.State, pad: UInt8) {
        let wired = Sc2Device.wireButtons(state.buttons)
        var changed = wired ^ wireButtons
        while changed != 0 {
            let bit = changed & (~changed &+ 1) // lowest changed bit
            connection.send(.gamepadButton(bit, down: wired & bit != 0, pad: UInt32(pad)))
            changed &= ~bit
        }
        wireButtons = wired
        axisLocked(GamepadWire.axisLSX, state.lsX, pad: pad)
        axisLocked(GamepadWire.axisLSY, state.lsY, pad: pad)
        axisLocked(GamepadWire.axisRSX, state.rsX, pad: pad)
        axisLocked(GamepadWire.axisRSY, state.rsY, pad: pad)
        axisLocked(GamepadWire.axisLT, state.lt, pad: pad)
        axisLocked(GamepadWire.axisRT, state.rt, pad: pad)
        updateChordLocked()
    }

    private func axisLocked(_ id: UInt32, _ value: Int32, pad: UInt8) {
        let i = Int(id)
        guard lastAxis[i] != value else { return }
        lastAxis[i] = value
        connection.send(.gamepadAxis(id, value: value, pad: UInt32(pad)))
    }

    /// Arm the disconnect timer while the full chord is held on the typed mirror, disarm on any
    /// release — GamepadCapture's rule, off this capture's own state. Caller holds `lock`.
    private func updateChordLocked() {
        let held = wireButtons & Self.escapeChord == Self.escapeChord
        if held, chordWork == nil {
            let work = DispatchWorkItem { [weak self] in
                guard let self else { return }
                MainActor.assumeIsolated { self.onDisconnectRequest?() }
            }
            chordWork = work
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.disconnectHold, execute: work)
        } else if !held, let work = chordWork {
            work.cancel()
            chordWork = nil
        }
    }

    // MARK: - Slot lifecycle

    /// Reserve a wire index on the main actor and finish the claim under the lock. On success
    /// sends `gamepadArrival(pref 9)` — the declaration the host builds the virtual SC2 from —
    /// before any input can flow on that index.
    private func claimSlot() {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            MainActor.assumeIsolated {
                let index = self.manager.reserveExternalPadIndex()
                self.lock.lock()
                guard self.claimPending else {
                    // The link closed between the report that scheduled this claim and the
                    // hop landing: `releaseSlot` cleared the pending token (it is set nowhere
                    // else while a claim is in flight). Completing now would arm a virtual
                    // pad — and announce a badge — for a dead link, with no `.released` ever
                    // coming. Hand the index straight back instead; a live link's next state
                    // report simply schedules a fresh claim.
                    self.lock.unlock()
                    if let index { self.manager.releaseExternalPadIndex(index) }
                    return
                }
                self.claimPending = false
                if self.stopped || self.suspended {
                    self.lock.unlock()
                    if let index { self.manager.releaseExternalPadIndex(index) }
                    return
                }
                guard let index else {
                    // All 16 wire indices taken — drop reports until one frees (a later
                    // report retries).
                    self.lock.unlock()
                    return
                }
                self.padIndex = index
                self.lock.unlock()
                self.connection.send(.gamepadArrival(
                    pref: PunktfunkConnection.GamepadType.steamController2.rawValue,
                    pad: UInt32(index)))
                log.info("SC2 captured → wire pad \(index) (BLE passthrough, pref 9)")
                self.onPhaseChange?(.captured(pad: index))
            }
        }
    }

    /// Free the wire slot: `gamepadRemove` (the host tears its virtual pad down — no stuck last
    /// frame), typed-diff state cleared, IMU gate re-armed so whatever connects next re-proves
    /// its IMU live, index handed back to the shared allocator. Every teardown funnels through
    /// here (stop, link drop, suspend). Safe from any thread; no-op while unclaimed.
    private func releaseSlot(reason: String) {
        lock.lock()
        let index = padIndex
        padIndex = nil
        claimPending = false
        wireButtons = 0
        for i in lastAxis.indices { lastAxis[i] = Int32.min }
        imuGate.reset()
        let chord = chordWork
        chordWork = nil
        lock.unlock()
        chord?.cancel()
        guard let index else { return }
        connection.send(.gamepadRemove(pad: UInt32(index)))
        DispatchQueue.main.async { [weak self, manager] in
            MainActor.assumeIsolated {
                manager.releaseExternalPadIndex(index)
                self?.onPhaseChange?(.released)
            }
        }
        log.info("SC2: wire pad \(index) released (\(reason))")
    }
}

#endif
