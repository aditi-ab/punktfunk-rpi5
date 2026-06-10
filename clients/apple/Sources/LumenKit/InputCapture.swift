// Input capture → lumen/1 datagrams, via the GameController framework.
//
// GCMouse delivers RAW deltas (not the accelerated cursor) — exactly what the host-side
// injector expects for relative motion. GCKeyboard gives HID keycodes which we map to the
// Windows VK space the host's vk_to_evdev table consumes (same space Moonlight uses).
// Gamepads (GCController) come later — the host's uinput pads already speak the
// GamepadButton/GamepadAxis event kinds.
//
// SCAFFOLD: written on the Linux host, not yet compiled against Xcode. The VK map covers
// the common keys; extend alongside lumen-host/src/inject.rs::vk_to_evdev.

#if os(macOS)
import Foundation
import GameController
import LumenCore

public final class InputCapture {
    private let connection: LumenConnection
    private var observers: [NSObjectProtocol] = []

    public init(connection: LumenConnection) {
        self.connection = connection
    }

    /// Begin forwarding the current (and future) mouse/keyboard to the host.
    public func start() {
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
    }

    public func stop() {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
    }

    private func attach(mouse: GCMouse) {
        guard let input = mouse.mouseInput else { return }
        let conn = connection
        input.mouseMovedHandler = { _, dx, dy in
            // GC gives +y up; the host expects screen-space (+y down).
            conn.send(.mouseMove(dx: Int32(dx), dy: Int32(-dy)))
        }
        input.leftButton.pressedChangedHandler = { _, _, pressed in
            conn.send(.mouseButton(1, down: pressed))
        }
        input.rightButton?.pressedChangedHandler = { _, _, pressed in
            conn.send(.mouseButton(3, down: pressed))
        }
        input.middleButton?.pressedChangedHandler = { _, _, pressed in
            conn.send(.mouseButton(2, down: pressed))
        }
        input.scroll.valueChangedHandler = { _, _, dy in
            if dy != 0 { conn.send(.scroll(Int32(dy * 120))) }
        }
    }

    private func attach(keyboard: GCKeyboard) {
        let conn = connection
        keyboard.keyboardInput?.keyChangedHandler = { _, _, keyCode, pressed in
            if let vk = Self.hidToVK[keyCode.rawValue] {
                conn.send(.key(vk, down: pressed))
            }
        }
    }

    /// HID usage (GCKeyCode raw) → Windows VK (the host maps VK → evdev).
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
        // F1..F12: HID 0x3A..0x45 → VK 0x70..0x7B.
        for i in 0..<12 { m[0x3A + i] = UInt32(0x70 + i) }
        m[0x4F] = 0x27; m[0x50] = 0x25; m[0x51] = 0x28; m[0x52] = 0x26 // arrows R L D U
        m[0x49] = 0x2D; m[0x4A] = 0x24; m[0x4B] = 0x21 // insert home pageup
        m[0x4C] = 0x2E; m[0x4D] = 0x23; m[0x4E] = 0x22 // delete end pagedown
        m[0xE0] = 0xA2; m[0xE1] = 0xA0; m[0xE2] = 0xA4; m[0xE3] = 0x5B // Lctrl Lshift Lalt Lcmd
        m[0xE4] = 0xA3; m[0xE5] = 0xA1; m[0xE6] = 0xA5; m[0xE7] = 0x5C // Rctrl Rshift Ralt Rcmd
        return m
    }()
}
#endif
