//! Local key/button codes → the punktfunk input wire contract.
//!
//! The wire carries Windows Virtual-Key codes (the GameStream convention; the host maps them
//! back with `inject::vk_to_evdev`). On Windows the VK is the *native* source — but winit
//! hands us a layout-independent **physical** `KeyCode` (`KeyA` is always the QWERTY-A
//! position), which is exactly what a game wants: positional keys map positionally regardless
//! of the user's keyboard layout. This table is that physical-position → VK mapping, the
//! direct analogue of the Linux client's evdev table.

use winit::keyboard::KeyCode;

/// Map a winit physical `KeyCode` to the Windows VK code the host expects. `None` = a key the
/// wire contract doesn't cover (media keys etc.) — drop it rather than guess.
pub fn keycode_to_vk(code: KeyCode) -> Option<u8> {
    use KeyCode::*;
    Some(match code {
        // --- Navigation / editing / whitespace ---
        Backspace => 0x08,
        Tab => 0x09,
        Enter => 0x0D,
        Pause => 0x13,
        CapsLock => 0x14,
        Escape => 0x1B,
        Space => 0x20,
        PageUp => 0x21,
        PageDown => 0x22,
        End => 0x23,
        Home => 0x24,
        ArrowLeft => 0x25,
        ArrowUp => 0x26,
        ArrowRight => 0x27,
        ArrowDown => 0x28,
        PrintScreen => 0x2C,
        Insert => 0x2D,
        Delete => 0x2E,

        // --- Digit row ---
        Digit0 => 0x30,
        Digit1 => 0x31,
        Digit2 => 0x32,
        Digit3 => 0x33,
        Digit4 => 0x34,
        Digit5 => 0x35,
        Digit6 => 0x36,
        Digit7 => 0x37,
        Digit8 => 0x38,
        Digit9 => 0x39,

        // --- Letters ---
        KeyA => 0x41,
        KeyB => 0x42,
        KeyC => 0x43,
        KeyD => 0x44,
        KeyE => 0x45,
        KeyF => 0x46,
        KeyG => 0x47,
        KeyH => 0x48,
        KeyI => 0x49,
        KeyJ => 0x4A,
        KeyK => 0x4B,
        KeyL => 0x4C,
        KeyM => 0x4D,
        KeyN => 0x4E,
        KeyO => 0x4F,
        KeyP => 0x50,
        KeyQ => 0x51,
        KeyR => 0x52,
        KeyS => 0x53,
        KeyT => 0x54,
        KeyU => 0x55,
        KeyV => 0x56,
        KeyW => 0x57,
        KeyX => 0x58,
        KeyY => 0x59,
        KeyZ => 0x5A,

        // --- Meta / context-menu ---
        SuperLeft => 0x5B,  // VK_LWIN
        SuperRight => 0x5C, // VK_RWIN
        ContextMenu => 0x5D,

        // --- Numpad ---
        Numpad0 => 0x60,
        Numpad1 => 0x61,
        Numpad2 => 0x62,
        Numpad3 => 0x63,
        Numpad4 => 0x64,
        Numpad5 => 0x65,
        Numpad6 => 0x66,
        Numpad7 => 0x67,
        Numpad8 => 0x68,
        Numpad9 => 0x69,
        NumpadMultiply => 0x6A,
        NumpadAdd => 0x6B,
        NumpadEnter => 0x6C, // VK_SEPARATOR (matches the Linux client's KP_ENTER mapping)
        NumpadSubtract => 0x6D,
        NumpadDecimal => 0x6E,
        NumpadDivide => 0x6F,

        // --- Function keys ---
        F1 => 0x70,
        F2 => 0x71,
        F3 => 0x72,
        F4 => 0x73,
        F5 => 0x74,
        F6 => 0x75,
        F7 => 0x76,
        F8 => 0x77,
        F9 => 0x78,
        F10 => 0x79,
        F11 => 0x7A,
        F12 => 0x7B,

        // --- Locks ---
        NumLock => 0x90,
        ScrollLock => 0x91,

        // --- Left/right modifiers ---
        ShiftLeft => 0xA0,
        ShiftRight => 0xA1,
        ControlLeft => 0xA2,
        ControlRight => 0xA3,
        AltLeft => 0xA4,
        AltRight => 0xA5,

        // --- OEM punctuation (US-layout positions) ---
        Semicolon => 0xBA,     // VK_OEM_1
        Equal => 0xBB,         // VK_OEM_PLUS
        Comma => 0xBC,         // VK_OEM_COMMA
        Minus => 0xBD,         // VK_OEM_MINUS
        Period => 0xBE,        // VK_OEM_PERIOD
        Slash => 0xBF,         // VK_OEM_2
        Backquote => 0xC0,     // VK_OEM_3
        BracketLeft => 0xDB,   // VK_OEM_4
        Backslash => 0xDC,     // VK_OEM_5
        BracketRight => 0xDD,  // VK_OEM_6
        Quote => 0xDE,         // VK_OEM_7
        IntlBackslash => 0xE2, // VK_OEM_102 (the 102nd key)

        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-check positions against the Windows VK constants the host's `vk_to_evdev` knows.
    #[test]
    fn maps_known_positions() {
        assert_eq!(keycode_to_vk(KeyCode::KeyA), Some(0x41));
        assert_eq!(keycode_to_vk(KeyCode::KeyZ), Some(0x5A));
        assert_eq!(keycode_to_vk(KeyCode::Digit0), Some(0x30));
        assert_eq!(keycode_to_vk(KeyCode::Escape), Some(0x1B));
        assert_eq!(keycode_to_vk(KeyCode::F11), Some(0x7A));
        assert_eq!(keycode_to_vk(KeyCode::ShiftLeft), Some(0xA0));
        assert_eq!(keycode_to_vk(KeyCode::IntlBackslash), Some(0xE2));
        assert_eq!(keycode_to_vk(KeyCode::Numpad9), Some(0x69));
        // A key outside the wire contract is dropped, not guessed.
        assert_eq!(keycode_to_vk(KeyCode::AudioVolumeMute), None);
    }
}
