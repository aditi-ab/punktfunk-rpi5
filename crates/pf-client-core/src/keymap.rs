//! Linux evdev / GDK button codes → GameStream Windows VK on the input wire.
//!
//! The host maps those VKs back with `inject::vk_to_evdev`. GTK's hardware keycode is
//! evdev + 8 on Wayland and X11, so this table is that host table inverted, keyed on
//! evdev. Unmapped keys (`None`) are dropped — media keys are not in the wire contract.

/// `None` is a key the wire contract does not cover (media keys). Drop it; do not guess.
pub fn evdev_to_vk(evdev: u16) -> Option<u8> {
    Some(match evdev {
        14 => 0x08,  // KEY_BACKSPACE -> VK_BACK
        15 => 0x09,  // KEY_TAB       -> VK_TAB
        28 => 0x0D,  // KEY_ENTER     -> VK_RETURN
        119 => 0x13, // KEY_PAUSE     -> VK_PAUSE
        58 => 0x14,  // KEY_CAPSLOCK  -> VK_CAPITAL
        1 => 0x1B,   // KEY_ESC       -> VK_ESCAPE
        57 => 0x20,  // KEY_SPACE     -> VK_SPACE
        104 => 0x21, // KEY_PAGEUP    -> VK_PRIOR
        109 => 0x22, // KEY_PAGEDOWN  -> VK_NEXT
        107 => 0x23, // KEY_END       -> VK_END
        102 => 0x24, // KEY_HOME      -> VK_HOME
        105 => 0x25, // KEY_LEFT      -> VK_LEFT
        103 => 0x26, // KEY_UP        -> VK_UP
        106 => 0x27, // KEY_RIGHT     -> VK_RIGHT
        108 => 0x28, // KEY_DOWN      -> VK_DOWN
        99 => 0x2C,  // KEY_SYSRQ     -> VK_SNAPSHOT
        110 => 0x2D, // KEY_INSERT    -> VK_INSERT
        111 => 0x2E, // KEY_DELETE    -> VK_DELETE

        // KEY_1..KEY_9 are 2..10; KEY_0 is 11.
        11 => 0x30,
        2 => 0x31,
        3 => 0x32,
        4 => 0x33,
        5 => 0x34,
        6 => 0x35,
        7 => 0x36,
        8 => 0x37,
        9 => 0x38,
        10 => 0x39,

        // Evdev letters are QWERTY-row order; arms are VK order (A = 0x41).
        30 => 0x41, // A
        48 => 0x42, // B
        46 => 0x43, // C
        32 => 0x44, // D
        18 => 0x45, // E
        33 => 0x46, // F
        34 => 0x47, // G
        35 => 0x48, // H
        23 => 0x49, // I
        36 => 0x4A, // J
        37 => 0x4B, // K
        38 => 0x4C, // L
        50 => 0x4D, // M
        49 => 0x4E, // N
        24 => 0x4F, // O
        25 => 0x50, // P
        16 => 0x51, // Q
        19 => 0x52, // R
        31 => 0x53, // S
        20 => 0x54, // T
        22 => 0x55, // U
        47 => 0x56, // V
        17 => 0x57, // W
        45 => 0x58, // X
        21 => 0x59, // Y
        44 => 0x5A, // Z

        125 => 0x5B, // KEY_LEFTMETA  -> VK_LWIN
        126 => 0x5C, // KEY_RIGHTMETA -> VK_RWIN
        127 => 0x5D, // KEY_COMPOSE   -> VK_APPS

        82 => 0x60, // KP0
        79 => 0x61,
        80 => 0x62,
        81 => 0x63,
        75 => 0x64,
        76 => 0x65,
        77 => 0x66,
        71 => 0x67,
        72 => 0x68,
        73 => 0x69, // KP9
        55 => 0x6A, // KEY_KPASTERISK -> VK_MULTIPLY
        78 => 0x6B, // KEY_KPPLUS     -> VK_ADD
        96 => 0x6C, // KEY_KPENTER    -> VK_SEPARATOR
        74 => 0x6D, // KEY_KPMINUS    -> VK_SUBTRACT
        83 => 0x6E, // KEY_KPDOT      -> VK_DECIMAL
        98 => 0x6F, // KEY_KPSLASH    -> VK_DIVIDE

        59 => 0x70, // F1
        60 => 0x71,
        61 => 0x72,
        62 => 0x73,
        63 => 0x74,
        64 => 0x75,
        65 => 0x76,
        66 => 0x77,
        67 => 0x78,
        68 => 0x79, // F10
        87 => 0x7A, // F11
        88 => 0x7B, // F12

        69 => 0x90, // KEY_NUMLOCK    -> VK_NUMLOCK
        70 => 0x91, // KEY_SCROLLLOCK -> VK_SCROLL

        // Specific L/R VKs. The host maps generic VK_SHIFT/CONTROL/MENU onto these too.
        42 => 0xA0,  // KEY_LEFTSHIFT  -> VK_LSHIFT
        54 => 0xA1,  // KEY_RIGHTSHIFT -> VK_RSHIFT
        29 => 0xA2,  // KEY_LEFTCTRL   -> VK_LCONTROL
        97 => 0xA3,  // KEY_RIGHTCTRL  -> VK_RCONTROL
        56 => 0xA4,  // KEY_LEFTALT    -> VK_LMENU
        100 => 0xA5, // KEY_RIGHTALT   -> VK_RMENU

        // OEM punctuation at US-layout positions.
        39 => 0xBA, // KEY_SEMICOLON  -> VK_OEM_1
        13 => 0xBB, // KEY_EQUAL      -> VK_OEM_PLUS
        51 => 0xBC, // KEY_COMMA      -> VK_OEM_COMMA
        12 => 0xBD, // KEY_MINUS      -> VK_OEM_MINUS
        52 => 0xBE, // KEY_DOT        -> VK_OEM_PERIOD
        53 => 0xBF, // KEY_SLASH      -> VK_OEM_2
        41 => 0xC0, // KEY_GRAVE      -> VK_OEM_3
        26 => 0xDB, // KEY_LEFTBRACE  -> VK_OEM_4
        43 => 0xDC, // KEY_BACKSLASH  -> VK_OEM_5
        27 => 0xDD, // KEY_RIGHTBRACE -> VK_OEM_6
        40 => 0xDE, // KEY_APOSTROPHE -> VK_OEM_7
        86 => 0xE2, // KEY_102ND      -> VK_OEM_102

        _ => return None,
    })
}

/// GDK back/forward are 8/9; GameStream wants X1/X2 as 4/5. Other buttons are 1:1.
pub fn gdk_button_to_gs(button: u32) -> Option<u32> {
    Some(match button {
        1 => 1,
        2 => 2,
        3 => 3,
        8 => 4,
        9 => 5,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inverse of host `inject::vk_to_evdev`. Generic-modifier VKs collapse onto the
    /// left-hand evdev codes and are omitted here.
    #[test]
    fn roundtrips_through_the_host_table() {
        let host_pairs: &[(u8, u16)] = &[
            (0x08, 14),
            (0x09, 15),
            (0x0D, 28),
            (0x13, 119),
            (0x14, 58),
            (0x1B, 1),
            (0x20, 57),
            (0x21, 104),
            (0x22, 109),
            (0x23, 107),
            (0x24, 102),
            (0x25, 105),
            (0x26, 103),
            (0x27, 106),
            (0x28, 108),
            (0x2C, 99),
            (0x2D, 110),
            (0x2E, 111),
            (0x30, 11),
            (0x31, 2),
            (0x39, 10),
            (0x41, 30),
            (0x5A, 44),
            (0x5B, 125),
            (0x60, 82),
            (0x69, 73),
            (0x70, 59),
            (0x7B, 88),
            (0x90, 69),
            (0xA0, 42),
            (0xA5, 100),
            (0xBA, 39),
            (0xE2, 86),
        ];
        for &(vk, evdev) in host_pairs {
            assert_eq!(evdev_to_vk(evdev), Some(vk), "evdev {evdev}");
        }
        assert_eq!(evdev_to_vk(113), None); // KEY_MUTE — not in the wire contract
    }
}
