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

    /// While true, mouse/keyboard flow to the host and key NSEvents are swallowed
    /// locally; while false the user is interacting with the local UI (dragging the
    /// window, clicking the HUD) and nothing is forwarded. Main-queue only.
    public private(set) var forwarding = false

    /// Fired on ⌘⎋ (the capture toggle — detected here so it works in both states; the
    /// event itself is swallowed). Main queue.
    public var onToggleCapture: (() -> Void)?

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

    private func attach(mouse: GCMouse) {
        guard let input = mouse.mouseInput,
              !mice.contains(where: { $0 === mouse }) // re-delivered on wake — attach once
        else { return }
        mice.append(mouse)
        // macOS drives motion + buttons from NSEvent (StreamLayerView's local monitor →
        // sendMotion/sendMouseButton) because GCMouse's handlers proved unreliable there;
        // installing them too would double-send. iOS keeps GCMouse (raw deltas under
        // pointer lock). See the file header.
        #if !os(macOS)
        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self, self.forwarding else { return }
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
        input.leftButton.pressedChangedHandler = { [weak self] _, _, pressed in
            self?.sendButton(1, pressed: pressed)
        }
        input.rightButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            self?.sendButton(3, pressed: pressed)
        }
        input.middleButton?.pressedChangedHandler = { [weak self] _, _, pressed in
            self?.sendButton(2, pressed: pressed)
        }
        // First two side buttons → GameStream X1/X2.
        if let aux = input.auxiliaryButtons {
            for (i, button) in aux.prefix(2).enumerated() {
                button.pressedChangedHandler = { [weak self] _, _, pressed in
                    self?.sendButton(UInt32(4 + i), pressed: pressed)
                }
            }
        }
        #endif
        // NOTE: no scroll handler here. GCMouse's scroll dpad only fires for plain HID
        // wheel deltas — trackpad/Magic Mouse scrolling is gesture-based and never
        // reaches GameController. Scroll arrives via the stream view's scrollWheel
        // override (NSEvent covers wheels too) → sendScroll().
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
    }

    /// HID usage (GCKeyCode raw) → Windows VK (the host maps VK → evdev; every VK emitted
    /// here exists in punktfunk-host/src/inject.rs::vk_to_evdev — extend the two together).
    static let hidToVK: [Int: UInt32] = {
        var m: [Int: UInt32] = [:]
        // a–z: HID 0x04..0x1D → VK 'A'..'Z'.
        for i in 0..<26 { m[0x04 + i] = UInt32(0x41 + i) }
        // 1–9, 0: HID 0x1E..0x27 → VK '1'..'9','0'.
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
}
