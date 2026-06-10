// Input capture → lumen/1 datagrams, via the GameController framework.
//
// GCMouse delivers RAW deltas (not the accelerated cursor) — exactly what the host-side
// injector expects for relative motion. GCKeyboard gives HID keycodes which we map to the
// Windows VK space the host's vk_to_evdev table consumes (same space Moonlight uses).
// Gamepads (GCController) come later — the host's uinput pads already speak the
// GamepadButton/GamepadAxis event kinds, but m3's injector path doesn't route them yet.
//
// The wire carries integer deltas; GC hands us Floats. We accumulate the fractional
// remainder per axis so slow, sub-pixel motion isn't truncated away.
//
// GC only delivers while the app is active, so anything held when focus leaves would
// stick down on the host forever — we track pressed keys/buttons and release them all on
// didResignActive and on stop(). All GC handlers and notifications fire on the main
// queue (the framework default), so the mutable state here needs no locking.
//
// GCMouse.current/GCKeyboard.coalesced are process-global singletons with one handler
// slot each: only one InputCapture can be live per process. `activeCapture` tracks
// ownership so a stale capture's stop() can't clobber a newer one's handlers.

#if os(macOS)
import AppKit
import Foundation
import GameController
import LumenCore

public final class InputCapture {
    private static weak var activeCapture: InputCapture?

    private let connection: LumenConnection
    private var observers: [NSObjectProtocol] = []
    private var mice: [GCMouse] = []
    private var keyboards: [GCKeyboard] = []

    // Main-queue-only state (see header comment).
    private var residualX: Float = 0
    private var residualY: Float = 0
    private var residualScrollX: Float = 0
    private var residualScrollY: Float = 0
    private var pressedVKs: Set<UInt32> = []
    private var pressedButtons: Set<UInt32> = []

    public init(connection: LumenConnection) {
        self.connection = connection
    }

    /// Begin forwarding the current (and future) mouse/keyboard to the host. Steals the
    /// global GC handler slots from any previous capture (one live capture per process).
    public func start() {
        Self.activeCapture = self
        if let mouse = GCMouse.current { attach(mouse: mouse) }
        if let keyboard = GCKeyboard.coalesced { attach(keyboard: keyboard) }
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCMouseDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            if let m = n.object as? GCMouse { self?.attach(mouse: m) }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: .GCKeyboardDidConnect, object: nil, queue: .main
        ) { [weak self] n in
            if let k = n.object as? GCKeyboard { self?.attach(keyboard: k) }
        })
        // Focus loss: GC stops delivering, so release everything still held host-side.
        observers.append(NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.releaseAll()
        })
    }

    public func stop() {
        releaseAll()
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
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

    /// Send release events for everything currently held, and drop the motion residuals.
    private func releaseAll() {
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
        if pressed {
            pressedButtons.insert(button)
        } else {
            pressedButtons.remove(button)
        }
        connection.send(.mouseButton(button, down: pressed))
    }

    private func attach(mouse: GCMouse) {
        guard let input = mouse.mouseInput,
              !mice.contains(where: { $0 === mouse }) // re-delivered on wake — attach once
        else { return }
        mice.append(mouse)
        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self else { return }
            // GC gives +y up; the host expects screen-space (+y down).
            let fx = dx + self.residualX
            let fy = -dy + self.residualY
            let ix = fx.rounded(.towardZero)
            let iy = fy.rounded(.towardZero)
            self.residualX = fx - ix
            self.residualY = fy - iy
            if ix != 0 || iy != 0 {
                self.connection.send(.mouseMove(dx: Int32(ix), dy: Int32(iy)))
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
        input.scroll.valueChangedHandler = { [weak self] _, x, y in
            guard let self else { return }
            // WHEEL_DELTA(120) per notch; positive = up / right (Moonlight's convention).
            let fy = y * 120 + self.residualScrollY
            let fx = x * 120 + self.residualScrollX
            let iy = fy.rounded(.towardZero)
            let ix = fx.rounded(.towardZero)
            self.residualScrollY = fy - iy
            self.residualScrollX = fx - ix
            if iy != 0 { self.connection.send(.scroll(Int32(iy))) }
            if ix != 0 { self.connection.send(.scroll(Int32(ix), horizontal: true)) }
        }
    }

    private func attach(keyboard: GCKeyboard) {
        guard !keyboards.contains(where: { $0 === keyboard }) else { return }
        keyboards.append(keyboard)
        keyboard.keyboardInput?.keyChangedHandler = { [weak self] _, _, keyCode, pressed in
            guard let self, let vk = Self.hidToVK[keyCode.rawValue] else { return }
            if pressed {
                self.pressedVKs.insert(vk)
            } else {
                self.pressedVKs.remove(vk)
            }
            self.connection.send(.key(vk, down: pressed))
        }
    }

    /// HID usage (GCKeyCode raw) → Windows VK (the host maps VK → evdev; every VK emitted
    /// here exists in lumen-host/src/inject.rs::vk_to_evdev — extend the two together).
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
#endif
