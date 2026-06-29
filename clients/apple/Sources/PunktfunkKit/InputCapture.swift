// Input capture → punktfunk/1 datagrams.
//
// Mouse MOTION and BUTTONS take different paths per platform. On macOS GCMouse's
// mouseMovedHandler/pressedChangedHandler proved unreliable in the field (delivered
// nothing on a live Mac while GCKeyboard worked — a documented GameController quirk), so
// macOS drives motion + buttons from NSEvent under cursor disassociation instead (fed by
// StreamLayerView, the same channel that already carries scroll), and the GCMouse motion/
// button handlers are not installed there. NSEvent deltas under
// CGAssociateMouseAndMouseCursorPosition(false) are the relative motion — OS-acceleration-
// applied (not raw HID), which is exactly what Moonlight's macOS client ships and is fine.
// iOS keeps the GCMouse path (raw deltas under pointer lock). GCKeyboard (both platforms)
// gives HID keycodes which we map to the Windows VK space the host's vk_to_evdev table
// consumes (same space Moonlight uses). Gamepads (GCController) come later — the host's
// uinput pads already speak the GamepadButton/GamepadAxis event kinds, but m3's injector
// path doesn't route them yet.
//
// The wire carries integer deltas; GC hands us Floats. We accumulate the fractional
// remainder per axis so slow, sub-pixel motion isn't truncated away.
//
// GC only delivers while the app is active, so anything held when focus leaves would
// stick down on the host forever — we track pressed keys/buttons and release them all on
// didResignActive and on stop(). All GC handlers and notifications fire on the main
// queue (the framework default), so the mutable state here needs no locking.
//
// Forwarding is gated by `forwarding` (driven by StreamLayerView's capture state): the
// handlers stay attached for the whole session, but while the user has released capture
// (⌘⎋, focus loss) nothing reaches the host and key events travel the responder chain
// normally. Everything held is flushed host-side on each transition to released.
//
// GCMouse.current/GCKeyboard.coalesced are process-global singletons with one handler
// slot each: only one InputCapture can be live per process. `activeCapture` tracks
// ownership so a stale capture's stop() can't clobber a newer one's handlers.

#if os(macOS)
import AppKit
#endif
#if canImport(UIKit)
import UIKit
#endif
import Foundation
import GameController
import PunktfunkCore
import os

/// Diagnostic logging for the input path. Off by default (input is high-rate); set
/// PUNKTFUNK_INPUT_DEBUG=1 in the environment to surface whether relative motion + buttons
/// are actually being SENT to the host without needing host-side logs. Motion is throttled
/// to once per second (see `motionDebugTick`); buttons log every transition.
private let inputLog = Logger(subsystem: "io.unom.punktfunk", category: "input")
private let inputDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

public final class InputCapture {
    private static weak var activeCapture: InputCapture?

    private let connection: PunktfunkConnection
    private var observers: [NSObjectProtocol] = []
    private var mice: [GCMouse] = []
    private var keyboards: [GCKeyboard] = []
    #if os(macOS)
    private var keyEventMonitor: Any?
    #endif

    // Main-queue-only state (see header comment).
    private var residualX: Float = 0
    private var residualY: Float = 0
    private var residualScrollX: Float = 0
    private var residualScrollY: Float = 0
    private var pressedVKs: Set<UInt32> = []
    private var pressedButtons: Set<UInt32> = []
    /// One-shot: the left click that engaged capture belongs to the local UI — GC sees
    /// it at the HID layer regardless, so its press AND release are dropped here.
    private var suppressedButton: UInt32?

    /// Throttle for the PUNKTFUNK_INPUT_DEBUG motion counter (motion is high-rate — we log
    /// a rolling count + the last delta once per second, never per event). Main-queue only.
    private var motionDebugCount = 0
    private var motionDebugTick = Date.distantPast

    /// One-shot twin of `suppressedButton` for the ⌘⎋ toggle: the physical Esc also
    /// reaches GCKeyboard, racing the NSEvent monitor — latched here so it can't type
    /// an Escape into the host in either toggle direction.
    private var suppressedVK: UInt32?
    /// Physical ⌘ keys currently held (tracked even while released — the ⌘⎋ toggle and
    /// its Esc suppression need it in both states).
    private var cmdKeysDown: Set<UInt32> = []

    #if os(macOS)
    /// Previous raw `NSEvent.modifierFlags.rawValue` (LOW 16 bits intact — those carry the
    /// device-dependent L/R bits). Modifier keys never fire keyDown/keyUp on macOS; they
    /// arrive as flagsChanged, which doesn't carry down-vs-up — we recover that by diffing
    /// this snapshot. Resynced (not diffed) while forwarding is off so a modifier held
    /// across a capture toggle can't produce a phantom transition on re-engage.
    private var prevModFlags: UInt = 0
    #endif

    /// While true, mouse/keyboard flow to the host and key NSEvents are swallowed
    /// locally; while false the user is interacting with the local UI (dragging the
    /// window, clicking the HUD) and nothing is forwarded. Main-queue only.
    public private(set) var forwarding = false

    /// iPad pointer routing (the StreamViewController mirrors the scene's live pointer-lock
    /// state into this). GCMouse only delivers relative deltas + buttons while the scene is
    /// LOCKED, so this is true then and the GCMouse handlers forward. When the scene can't
    /// lock (Stage Manager, not frontmost, iPhone) the iPad routes the mouse through UIKit's
    /// pointer path as ABSOLUTE moves (`sendMouseAbs`) instead — so this goes false, gating
    /// GCMouse off and enabling the absolute path, the two never double-sending. Moot on
    /// macOS (no GCMouse handlers installed; `sendMouseAbs` is never called there). Main-queue.
    public var gcMouseForwarding = false

    #if os(iOS)
    /// Whether any device is attached as a `GCMouse` right now. The Magic Keyboard TRACKPAD does
    /// not always register as a GCMouse on iPadOS (only a standalone mouse does) — when no GCMouse
    /// is present the relative GCMouse path can't carry pointer motion. Main-queue.
    public var hasGCMouse: Bool { !mice.isEmpty }

    /// Diagnostic: a one-line description of every attached GCMouse (count + GCDevice identity), so
    /// PUNKTFUNK_INPUT_DEBUG can reveal whether the trackpad showed up as a mouse at all.
    public var attachedMiceSummary: String {
        guard !mice.isEmpty else { return "0 mice" }
        let parts = mice.map { mouse -> String in
            "\(mouse.productCategory)/\(mouse.vendorName ?? "?")"
        }
        return "\(mice.count) mice: \(parts.joined(separator: ", "))"
    }
    #endif

    /// Fired on ⌘⎋ (the capture toggle — detected here so it works in both states; the
    /// event itself is swallowed). Main queue.
    public var onToggleCapture: (() -> Void)?

    /// Fired on ⌘⇧C (the client-side-cursor toggle — flips between the captured/disassociated
    /// relative path and the visible-cursor absolute path; detected here, like ⌘⎋, so it works
    /// regardless of the current capture state and the event itself is swallowed). macOS only;
    /// the absolute-vs-relative forwarding lives entirely in StreamLayerView. Main queue.
    public var onToggleCursor: (() -> Void)?

    /// Fired when a newer InputCapture takes the process-global GC handler slots (the
    /// singletons hold ONE handler each): the preempted owner must drop its capture
    /// state — its handlers are gone, so it would otherwise sit "captured" with dead
    /// input. Main queue.
    public var onPreempted: (() -> Void)?

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Gate the forwarding without detaching the GC handlers. `suppressClick` marks the
    /// transition as click-driven: that click's press/release are not forwarded. Every
    /// transition to false flushes held keys/buttons host-side.
    public func setForwarding(_ on: Bool, suppressClick: Bool = false) {
        if on {
            forwarding = true
            suppressedButton = suppressClick ? 1 : nil
        } else if forwarding {
            releaseAll()
            forwarding = false
            suppressedButton = nil
        }
    }

    /// The engage click is over (its NSEvent mouseUp processed) — stop suppressing.
    /// Backstop for the GC-vs-NSEvent ordering where both halves of the click landed
    /// before mouseDown armed the latch, which would otherwise eat the next real click.
    public func endClickSuppression() {
        suppressedButton = nil
    }

    /// Begin forwarding the current (and future) mouse/keyboard to the host. Steals the
    /// global GC handler slots from any previous capture (one live capture per process),
    /// notifying it via `onPreempted` so its owner releases its capture state.
    public func start() {
        if let previous = Self.activeCapture, previous !== self {
            // Drop the previous owner's device lists first: its stop() must not be able
            // to nil out the handler slots this capture is about to claim.
            previous.mice.removeAll()
            previous.keyboards.removeAll()
            previous.onPreempted?()
        }
        Self.activeCapture = self
        if let mouse = GCMouse.current { attach(mouse: mouse) }
        if let keyboard = GCKeyboard.coalesced { attach(keyboard: keyboard) }
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCMouseDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            if let m = n.object as? GCMouse { self?.attach(mouse: m) }
        })
        #if os(iOS)
        // The mouse can become the *current* one after it connected (and after our start()
        // already ran) — re-attach on that too so a launch-time race doesn't leave the iOS
        // GCMouse path without handlers. attach() is idempotent (dedupes by identity).
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCMouseDidBecomeCurrent, object: nil, queue: .main
        ) { [weak self] n in
            if let m = n.object as? GCMouse { self?.attach(mouse: m) }
        })
        #endif
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            if let k = n.object as? GCKeyboard { self?.attach(keyboard: k) }
        })
        // Focus loss: GC stops delivering, so release everything still held host-side.
        #if os(macOS)
        let resignActive = NSApplication.didResignActiveNotification
        #else
        let resignActive = UIApplication.willResignActiveNotification
        #endif
        observers.append(NotificationCenter.default.addObserver(
            forName: resignActive, object: nil, queue: .main
        ) { [weak self] _ in
            self?.releaseAll()
        })
        // ⌘⎋ — the capture toggle — is detected here so it works in both states. ONLY
        // that one combo is intercepted: swallowing keys wholesale at the monitor level
        // risks starving GC's own delivery, so the no-beep behavior lives in
        // StreamLayerView (first responder consumes keyDown/keyUp while captured).
        // (On iOS there is no NSEvent monitor — the GC key handler detects the combo.)
        #if os(macOS)
        keyEventMonitor = NSEvent.addLocalMonitorForEvents(
            matching: [.keyDown]
        ) { [weak self] event in
            guard let self else { return event }
            let flags = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            if event.keyCode == 53 /* Esc */, flags == .command {
                self.suppressedVK = 0x1B // the same physical Esc is en route via GC
                self.onToggleCapture?()
                return nil
            }
            // ⌘⇧C toggles the client-side cursor (visible-cursor absolute path vs the
            // captured relative path). keyCode 8 = kVK_ANSI_C; layout-independent so it
            // fires the same on any keyboard. Suppress the C (latched like ⌘⎋'s Esc) so it
            // doesn't type into the host, and swallow the event so it doesn't beep.
            if event.keyCode == 8 /* C */, flags == [.command, .shift] {
                self.suppressedVK = 0x43 // VK_C — the same physical C is en route via GC
                self.onToggleCursor?()
                return nil
            }
            return event
        }
        #endif
    }

    public func stop() {
        releaseAll()
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        #if os(macOS)
        if let monitor = keyEventMonitor {
            NSEvent.removeMonitor(monitor)
            keyEventMonitor = nil
        }
        #endif
        // Don't clobber the handlers if a newer capture has taken the global devices.
        if Self.activeCapture === self || Self.activeCapture == nil {
            for mouse in mice {
                guard let input = mouse.mouseInput else { continue }
                input.mouseMovedHandler = nil
                input.leftButton.pressedChangedHandler = nil
                input.rightButton?.pressedChangedHandler = nil
                input.middleButton?.pressedChangedHandler = nil
                input.auxiliaryButtons?.forEach { $0.pressedChangedHandler = nil }
                input.scroll.valueChangedHandler = nil
            }
            for keyboard in keyboards {
                keyboard.keyboardInput?.keyChangedHandler = nil
            }
            Self.activeCapture = nil
        }
        mice.removeAll()
        keyboards.removeAll()
    }

    deinit { stop() }

    /// Send release events for everything currently held, and drop the motion residuals
    /// and modifier/latch tracking (GC delivers nothing while inactive, so a ⌘ released
    /// in another app would otherwise stay "held" here forever — hijacking Esc).
    private func releaseAll() {
        cmdKeysDown.removeAll()
        suppressedVK = nil
        for vk in pressedVKs {
            connection.send(.key(vk, down: false))
        }
        for button in pressedButtons {
            connection.send(.mouseButton(button, down: false))
        }
        pressedVKs.removeAll()
        pressedButtons.removeAll()
        residualX = 0
        residualY = 0
        residualScrollX = 0
        residualScrollY = 0
        #if os(macOS)
        // Drop the modifier snapshot too: a flagsChanged transition can be missed if focus
        // leaves mid-chord, and the next handleFlagsChanged resyncs from a clean slate (it
        // resyncs while released anyway, but this keeps stuck state from outliving a blur).
        prevModFlags = 0
        #endif
    }

    /// Release any held MOUSE buttons host-side, leaving keyboard state untouched. Used when
    /// the iPad pointer lock drops while a GCMouse button is held: by then the GCMouse release
    /// handler is gated off (`gcMouseForwarding` is false), so it can't deliver the release
    /// itself and the button would otherwise stick until the next `releaseAll` (blur / stop).
    public func releaseMouseButtons() {
        for button in pressedButtons {
            connection.send(.mouseButton(button, down: false))
        }
        pressedButtons.removeAll()
    }

    private func sendButton(_ button: UInt32, pressed: Bool) {
        guard forwarding else { return }
        if button == suppressedButton {
            if !pressed { suppressedButton = nil } // capture click over — stop suppressing
            if inputDebug {
                inputLog.debug(
                    "button \(button, privacy: .public) \(pressed ? "down" : "up", privacy: .public) SUPPRESSED (engage click)")
            }
            return
        }
        if pressed {
            pressedButtons.insert(button)
        } else {
            pressedButtons.remove(button)
        }
        if inputDebug {
            inputLog.debug(
                "button \(button, privacy: .public) \(pressed ? "down" : "up", privacy: .public) sent")
        }
        connection.send(.mouseButton(button, down: pressed))
    }

    /// NSEvent button path (macOS): StreamLayerView's local mouse monitor routes physical
    /// button transitions here so they go through the same `suppressedButton` engage-click
    /// latch and `pressedButtons` release-on-blur set as the (iOS) GCMouse path. Wire ids:
    /// 1=left 2=middle 3=right 4=X1 5=X2.
    public func sendMouseButton(_ button: UInt32, pressed: Bool) {
        sendButton(button, pressed: pressed)
    }

    #if os(macOS)
    /// NSEvent key path (macOS): StreamLayerView's keyDown/keyUp/flagsChanged route Windows
    /// VKs here while captured. Mirrors `sendButton` — gated by `forwarding`, honours the
    /// ⌘⎋ toggle's `suppressedVK` latch, and tracks into `pressedVKs` so releaseAll()/blur
    /// flushes anything still held (a flagsChanged up can be missed on focus change). macOS
    /// has no GCKeyboard send (that path is iOS-only now), so this is the single key source.
    public func sendKey(_ vk: UInt32, down: Bool) {
        guard forwarding else { return }
        // The ⌘⎋ toggle's Esc is latched here (see the keyDown monitor) so it never types
        // an Escape into the host — clear the latch on its release, in front of the send.
        if vk == suppressedVK {
            if !down { suppressedVK = nil }
            if inputDebug {
                inputLog.debug(
                    "key \(vk, privacy: .public) \(down ? "down" : "up", privacy: .public) SUPPRESSED (⌘⎋ toggle)")
            }
            return
        }
        if down {
            pressedVKs.insert(vk)
        } else {
            pressedVKs.remove(vk)
        }
        if inputDebug {
            inputLog.debug(
                "key \(vk, privacy: .public) \(down ? "down" : "up", privacy: .public) sent")
        }
        connection.send(.key(vk, down: down))
    }

    /// NSEvent modifier path (macOS): modifier keys never fire keyDown/keyUp — they arrive
    /// as flagsChanged, which carries no down-vs-up. We diff the raw flags against the prior
    /// snapshot to recover each transition, and the changed key's L/R identity from the
    /// device-dependent bits in the LOW 16 bits (the .deviceIndependentFlagsMask the ⌘⎋
    /// monitor uses deliberately strips exactly these — do NOT pre-mask here). Each side maps
    /// to the same L/R modifier VK `hidToVK` already emits, so the host needs no change.
    /// Fed `UInt(event.modifierFlags.rawValue)`.
    public func handleFlagsChanged(_ rawFlags: UInt) {
        // While released we only resync the snapshot, so a modifier held across a capture
        // toggle doesn't show up as a spurious transition the moment forwarding re-engages.
        guard forwarding else {
            prevModFlags = rawFlags
            return
        }
        // (device-dependent mask, VK). LOW-16-bit masks from IOLLEvent.h (NX_DEVICE*MASK):
        // Lshift 0x2 Rshift 0x4 | Lctrl 0x1 Rctrl 0x2000 | Lalt 0x20 Ralt 0x40 | Lcmd 0x8 Rcmd 0x10.
        let table: [(UInt, UInt32)] = [
            (0x2, 0xA0), (0x4, 0xA1), // VK_LSHIFT / VK_RSHIFT
            (0x1, 0xA2), (0x2000, 0xA3), // VK_LCONTROL / VK_RCONTROL
            (0x20, 0xA4), (0x40, 0xA5), // VK_LMENU / VK_RMENU (left/right alt-option)
            (0x8, 0x5B), (0x10, 0x5C), // VK_LWIN / VK_RWIN (left/right command)
        ]
        for (mask, vk) in table {
            let now = (rawFlags & mask) != 0
            let was = (prevModFlags & mask) != 0
            guard now != was else { continue }
            // Keep cmdKeysDown in step (the ⌘⎋ toggle + Esc suppression read it); sendKey
            // adds the VK to pressedVKs so releaseAll/blur flushes a held modifier cleanly.
            if vk == 0x5B || vk == 0x5C {
                if now { cmdKeysDown.insert(vk) } else { cmdKeysDown.remove(vk) }
            }
            sendKey(vk, down: now)
        }
        prevModFlags = rawFlags
    }
    #endif

    private func attach(mouse: GCMouse) {
        guard let input = mouse.mouseInput,
              !mice.contains(where: { $0 === mouse }) // re-delivered on wake — attach once
        else { return }
        mice.append(mouse)
        #if os(iOS)
        if inputDebug {
            inputLog.debug(
                "GCMouse attached: \(mouse.productCategory, privacy: .public)/\(mouse.vendorName ?? "?", privacy: .public) — now \(self.attachedMiceSummary, privacy: .public)")
        }
        #endif
        // macOS drives motion + buttons from NSEvent (StreamLayerView's local monitor →
        // sendMotion/sendMouseButton) because GCMouse's handlers proved unreliable there;
        // installing them too would double-send. iOS keeps GCMouse (raw deltas under
        // pointer lock). See the file header.
        #if !os(macOS)
        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self, self.forwarding, self.gcMouseForwarding else { return }
            // GC gives +y up; the host expects screen-space (+y down).
            let fx = dx + self.residualX
            let fy = -dy + self.residualY
            let ix = fx.rounded(.towardZero)
            let iy = fy.rounded(.towardZero)
            self.residualX = fx - ix
            self.residualY = fy - iy
            if ix != 0 || iy != 0 {
                self.connection.send(.mouseMove(dx: Int32(ix), dy: Int32(iy)))
                if inputDebug {
                    self.motionDebugCount += 1
                    let now = Date()
                    if now.timeIntervalSince(self.motionDebugTick) >= 1 {
                        inputLog.debug(
                            "motion forwarded: \(self.motionDebugCount, privacy: .public) events, last dx \(Int(ix), privacy: .public) dy \(Int(iy), privacy: .public)")
                        self.motionDebugCount = 0
                        self.motionDebugTick = now
                    }
                }
            }
        }
        // Buttons take the GCMouse path only while the scene is pointer-locked; when it
        // isn't, the UIKit indirect-pointer path carries them (gcMouseForwarding gates here
        // so the two can't double-send).
        input.leftButton.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self, self.gcMouseForwarding else { return }
            self.sendButton(1, pressed: pressed)
        }
        input.rightButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self, self.gcMouseForwarding else { return }
            self.sendButton(3, pressed: pressed)
        }
        input.middleButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            guard let self, self.gcMouseForwarding else { return }
            self.sendButton(2, pressed: pressed)
        }
        // First two side buttons → GameStream X1/X2.
        if let aux = input.auxiliaryButtons {
            for (i, button) in aux.prefix(2).enumerated() {
                button.pressedChangedHandler = { [weak self] _, _, pressed in
                    guard let self, self.gcMouseForwarding else { return }
                    self.sendButton(UInt32(4 + i), pressed: pressed)
                }
            }
        }
        // Scroll WHEEL (plain HID mice) while pointer-locked: GCMouse's scroll dpad reports
        // wheel deltas here, +y up / +x right — already the host's WHEEL convention, one unit
        // per notch → ×120 (WHEEL_DELTA), residual-accumulated by sendScroll. (Trackpad
        // two-finger scrolling is gesture-based and does NOT reach GameController — that
        // arrives via the stream view's scroll pan recognizer; on macOS, via scrollWheel.)
        input.scroll.valueChangedHandler = { [weak self] _, dx, dy in
            guard let self, self.forwarding, self.gcMouseForwarding else { return }
            self.sendScroll(dx: dx * 120, dy: dy * 120)
        }
        #endif
    }

    /// Forward relative mouse motion (macOS). Fed by StreamLayerView's NSEvent monitor —
    /// while captured the cursor is disassociated (CGAssociateMouseAndMouseCursorPosition
    /// (false)), so mouseMoved/dragged deltaX/deltaY ARE the relative motion, the same
    /// channel sendScroll already uses. Unlike the (iOS) GCMouse path this is NOT y-negated:
    /// NSEvent deltaY is already screen-space (+y down), which is what the host expects.
    /// Fractional remainders accumulate so slow, sub-pixel motion isn't truncated away.
    public func sendMotion(dx: Float, dy: Float) {
        guard forwarding else { return }
        let fx = dx + residualX
        let fy = dy + residualY
        let ix = fx.rounded(.towardZero)
        let iy = fy.rounded(.towardZero)
        residualX = fx - ix
        residualY = fy - iy
        guard ix != 0 || iy != 0 else { return }
        connection.send(.mouseMove(dx: Int32(ix), dy: Int32(iy)))
        if inputDebug {
            // High-rate — log a rolling count + the last delta once per second, not per event.
            motionDebugCount += 1
            let now = Date()
            if now.timeIntervalSince(motionDebugTick) >= 1 {
                inputLog.debug(
                    "motion forwarded: \(self.motionDebugCount, privacy: .public) events, last dx \(Int(ix), privacy: .public) dy \(Int(iy), privacy: .public)")
                motionDebugCount = 0
                motionDebugTick = now
            }
        }
    }

    /// Forward an ABSOLUTE cursor position (iPad pointer fallback). Fed by the iOS stream
    /// view's hover / indirect-pointer path when the scene can't pointer-lock: the host
    /// places its cursor at this client-surface pixel — the same letterbox mapping the touch
    /// path uses. Gated by `forwarding` AND `!gcMouseForwarding` (the relative GCMouse path
    /// owns motion while locked), so absolute and relative motion never both fire. No residual
    /// accumulation — the value is absolute, not a delta.
    public func sendMouseAbs(x: Int32, y: Int32, surfaceWidth: UInt32, surfaceHeight: UInt32) {
        guard forwarding, !gcMouseForwarding else { return }
        connection.send(.mouseMoveAbs(
            x: x, y: y, surfaceWidth: surfaceWidth, surfaceHeight: surfaceHeight))
    }

    /// Forward a scroll gesture, WHEEL_DELTA(120)-scaled (positive = up / right,
    /// Moonlight's convention). Fed by StreamLayerView.scrollWheel — the only delivery
    /// path that covers trackpad/Magic Mouse gestures (GCMouse never reports them).
    /// Fractional remainders accumulate so slow two-finger scrolling isn't truncated away.
    public func sendScroll(dx: Float, dy: Float) {
        guard forwarding else { return }
        let fy = dy + residualScrollY
        let fx = dx + residualScrollX
        let iy = fy.rounded(.towardZero)
        let ix = fx.rounded(.towardZero)
        residualScrollY = fy - iy
        residualScrollX = fx - ix
        if iy != 0 { connection.send(.scroll(Int32(iy))) }
        if ix != 0 { connection.send(.scroll(Int32(ix), horizontal: true)) }
    }

    private func attach(keyboard: GCKeyboard) {
        guard !keyboards.contains(where: { $0 === keyboard }) else { return }
        keyboards.append(keyboard)
        // macOS sends keys from NSEvent (StreamLayerView's keyDown/keyUp/flagsChanged →
        // sendKey/handleFlagsChanged) because GCKeyboard delivery proved unreliable there —
        // the same GameController quirk that killed GCMouse motion (fixed in e414ec0).
        // Installing this handler too would double-send, so on macOS we leave the keyboard
        // tracked (for stop()'s cleanup) but attach no send handler: NSEvent is the only
        // key path. iOS keeps the GCKeyboard path (and detects the ⌘⎋ toggle from the HID
        // stream, since there's no NSEvent monitor there). See the file header.
        #if !os(macOS)
        keyboard.keyboardInput?.keyChangedHandler = { [weak self] _, _, keyCode, pressed in
            guard let self, let vk = Self.hidToVK[keyCode.rawValue] else { return }
            if vk == 0x5B || vk == 0x5C { // physical ⌘ state, tracked in both states
                if pressed {
                    self.cmdKeysDown.insert(vk)
                } else {
                    self.cmdKeysDown.remove(vk)
                }
            }
            // The ⌘⎋ toggle's Esc — checked before the forwarding gate, because in the
            // engage direction forwarding is already true when this fires.
            if vk == self.suppressedVK {
                if !pressed { self.suppressedVK = nil }
                return
            }
            #if os(iOS)
            // No NSEvent monitor here — the toggle combo is detected from the HID
            // stream itself.
            if pressed, vk == 0x1B, !self.cmdKeysDown.isEmpty {
                self.suppressedVK = 0x1B
                self.onToggleCapture?()
                return
            }
            #endif
            guard self.forwarding else { return }
            // Release direction of the toggle: GC's Esc-down can beat the NSEvent
            // monitor — never type Esc into the host while ⌘ is held (⌘⎋ is reserved).
            if vk == 0x1B, !self.cmdKeysDown.isEmpty {
                return
            }
            if pressed {
                self.pressedVKs.insert(vk)
            } else {
                self.pressedVKs.remove(vk)
            }
            self.connection.send(.key(vk, down: pressed))
        }
        #endif
    }

    /// HID usage (GCKeyCode raw) → Windows VK (the host maps VK → evdev; every VK emitted
    /// here exists in punktfunk-host/src/inject.rs::vk_to_evdev — extend the two together).
    static let hidToVK: [Int: UInt32] = {
        var m: [Int: UInt32] = [:]
        // a–z: HID 0x04..0x1D → VK 'A'..'Z'.
        for i in 0..<26 { m[0x04 + i] = UInt32(0x41 + i) }
        // 1–9: HID 0x1E..0x26 → VK '1'..'9'; then 0: HID 0x27 → VK '0' (set separately —
        // the '0' key sits AFTER '9' in HID but its VK 0x30 sits BEFORE '1' (0x31)).
        for i in 0..<9 { m[0x1E + i] = UInt32(0x31 + i) }
        m[0x27] = 0x30
        m[0x28] = 0x0D // return
        m[0x29] = 0x1B // escape
        m[0x2A] = 0x08 // backspace
        m[0x2B] = 0x09 // tab
        m[0x2C] = 0x20 // space
        m[0x2D] = 0xBD; m[0x2E] = 0xBB // - =
        m[0x2F] = 0xDB; m[0x30] = 0xDD; m[0x31] = 0xDC // [ ] backslash
        m[0x33] = 0xBA; m[0x34] = 0xDE; m[0x35] = 0xC0 // ; ' `
        m[0x36] = 0xBC; m[0x37] = 0xBE; m[0x38] = 0xBF // , . /
        m[0x39] = 0x14 // caps lock
        // F1..F12: HID 0x3A..0x45 → VK 0x70..0x7B.
        for i in 0..<12 { m[0x3A + i] = UInt32(0x70 + i) }
        m[0x46] = 0x2C; m[0x47] = 0x91; m[0x48] = 0x13 // printscreen scrolllock pause
        m[0x4F] = 0x27; m[0x50] = 0x25; m[0x51] = 0x28; m[0x52] = 0x26 // arrows R L D U
        m[0x49] = 0x2D; m[0x4A] = 0x24; m[0x4B] = 0x21 // insert home pageup
        m[0x4C] = 0x2E; m[0x4D] = 0x23; m[0x4E] = 0x22 // delete end pagedown
        // Keypad: NumLock, / * - +, Enter, 1..9, 0, decimal. KP Enter goes as
        // VK_SEPARATOR (0x6C) — this host maps it to KEY_KPENTER (Windows itself would
        // send VK_RETURN+extended, which vk_to_evdev can't distinguish).
        m[0x53] = 0x90
        m[0x54] = 0x6F; m[0x55] = 0x6A; m[0x56] = 0x6D; m[0x57] = 0x6B
        m[0x58] = 0x6C
        for i in 0..<9 { m[0x59 + i] = UInt32(0x61 + i) }
        m[0x62] = 0x60; m[0x63] = 0x6E
        m[0x64] = 0xE2 // ISO 102nd key (<> next to left shift on ISO layouts)
        m[0x65] = 0x5D // menu/application
        m[0xE0] = 0xA2; m[0xE1] = 0xA0; m[0xE2] = 0xA4; m[0xE3] = 0x5B // Lctrl Lshift Lalt Lcmd
        m[0xE4] = 0xA3; m[0xE5] = 0xA1; m[0xE6] = 0xA5; m[0xE7] = 0x5C // Rctrl Rshift Ralt Rcmd
        return m
    }()

    #if os(macOS)
    /// NSEvent.keyCode (Carbon virtual keycode, kVK_*) → Windows VK. The macOS NSEvent key
    /// path is keyed by keyCode (a layout-independent hardware position), NOT by HID usage,
    /// so it needs its own table — but it emits the EXACT SAME Windows VK integers `hidToVK`
    /// already produces for each physical key (A→0x41, Return→0x0D, KeypadEnter→0x6C, …), so
    /// the host's vk_to_evdev (inject.rs) accepts both with zero change. Modifier keys come
    /// via flagsChanged (handleFlagsChanged), not keyDown, so they're absent here. Keys with
    /// no host evdev arm (F13–F20, KeypadEquals, the Fn key) are omitted → nil → swallowed.
    static let keyCodeToVK: [UInt16: UInt32] = {
        var m: [UInt16: UInt32] = [:]
        // Letters — kVK_ANSI_A..Z (scattered keycodes) → VK 'A'..'Z'.
        m[0x00] = 0x41; m[0x01] = 0x53; m[0x02] = 0x44; m[0x03] = 0x46 // A S D F
        m[0x04] = 0x48; m[0x05] = 0x47; m[0x06] = 0x5A; m[0x07] = 0x58 // H G Z X
        m[0x08] = 0x43; m[0x09] = 0x56; m[0x0B] = 0x42; m[0x0C] = 0x51 // C V B Q
        m[0x0D] = 0x57; m[0x0E] = 0x45; m[0x0F] = 0x52; m[0x10] = 0x59 // W E R Y
        m[0x11] = 0x54; m[0x1F] = 0x4F; m[0x20] = 0x55; m[0x22] = 0x49 // T O U I
        m[0x23] = 0x50; m[0x25] = 0x4C; m[0x26] = 0x4A; m[0x28] = 0x4B // P L J K
        m[0x2D] = 0x4E; m[0x2E] = 0x4D // N M
        // Digit row — kVK_ANSI_1..0 (scattered) → VK '1'..'9','0'.
        m[0x12] = 0x31; m[0x13] = 0x32; m[0x14] = 0x33; m[0x15] = 0x34 // 1 2 3 4
        m[0x16] = 0x36; m[0x17] = 0x35; m[0x19] = 0x39; m[0x1A] = 0x37 // 6 5 9 7
        m[0x1C] = 0x38; m[0x1D] = 0x30 // 8 0
        // Whitespace / control.
        m[0x24] = 0x0D // return
        m[0x30] = 0x09 // tab
        m[0x31] = 0x20 // space
        m[0x33] = 0x08 // delete (backspace)
        m[0x35] = 0x1B // escape
        m[0x75] = 0x2E // forward delete (VK_DELETE)
        m[0x39] = 0x14 // caps lock
        // Punctuation (US ANSI) + ISO 102nd key.
        m[0x1B] = 0xBD; m[0x18] = 0xBB // - = (OEM_MINUS OEM_PLUS)
        m[0x21] = 0xDB; m[0x1E] = 0xDD; m[0x2A] = 0xDC // [ ] backslash (OEM_4 6 5)
        m[0x29] = 0xBA; m[0x27] = 0xDE; m[0x32] = 0xC0 // ; ' ` (OEM_1 7 3)
        m[0x2B] = 0xBC; m[0x2F] = 0xBE; m[0x2C] = 0xBF // , . / (OEM_COMMA PERIOD 2)
        m[0x0A] = 0xE2 // ISO 102nd key (<> next to left shift; OEM_102)
        // Function keys F1..F12 (scattered) → VK 0x70..0x7B. F13+ omitted (no host arm).
        m[0x7A] = 0x70; m[0x78] = 0x71; m[0x63] = 0x72; m[0x76] = 0x73 // F1 F2 F3 F4
        m[0x60] = 0x74; m[0x61] = 0x75; m[0x62] = 0x76; m[0x64] = 0x77 // F5 F6 F7 F8
        m[0x65] = 0x78; m[0x6D] = 0x79; m[0x67] = 0x7A; m[0x6F] = 0x7B // F9 F10 F11 F12
        // Arrows.
        m[0x7B] = 0x25; m[0x7C] = 0x27; m[0x7D] = 0x28; m[0x7E] = 0x26 // left right down up
        // Nav cluster (Apple keycodes; Help sits where Insert is).
        m[0x72] = 0x2D; m[0x73] = 0x24; m[0x74] = 0x21 // insert home pageup
        m[0x77] = 0x23; m[0x79] = 0x22 // end pagedown (forward-delete handled above)
        // Keypad — kVK_ANSI_Keypad0..9 (scattered) → VK_NUMPAD0..9, plus the operators.
        m[0x52] = 0x60; m[0x53] = 0x61; m[0x54] = 0x62; m[0x55] = 0x63 // KP0 KP1 KP2 KP3
        m[0x56] = 0x64; m[0x57] = 0x65; m[0x58] = 0x66; m[0x59] = 0x67 // KP4 KP5 KP6 KP7
        m[0x5B] = 0x68; m[0x5C] = 0x69 // KP8 KP9
        m[0x41] = 0x6E; m[0x43] = 0x6A; m[0x45] = 0x6B // KP decimal multiply plus
        m[0x4E] = 0x6D; m[0x4B] = 0x6F // KP minus divide
        m[0x4C] = 0x6C // KP enter → VK_SEPARATOR (host maps to KEY_KPENTER, matching hidToVK)
        m[0x47] = 0x90 // KP clear sits where NumLock is → VK_NUMLOCK. (KP equals 0x51 dropped.)
        return m
    }()
    #endif
}
