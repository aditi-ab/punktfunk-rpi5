//! Input injection (plan §4): turn client [`lumen_core::input::InputEvent`]s into host input.
//!
//! The headless Sway compositor runs with `WLR_LIBINPUT_NO_DEVICES=1`, so kernel `uinput`
//! devices are never picked up. Instead we inject through the wlroots virtual-input Wayland
//! protocols — `zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1` — which
//! Sway always advertises. We connect as an ordinary Wayland client (the host process
//! inherits Sway's `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`), bind the two managers, and translate
//! events into virtual pointer/keyboard requests. Keyboard codes are Linux evdev; we upload a
//! standard evdev/US xkb keymap and track modifier state so the compositor resolves shifted
//! keysyms correctly.

use anyhow::Result;
use lumen_core::input::InputEvent;

/// Injects input events into the host session. Not `Send`: an injector owns compositor
/// resources (a Wayland connection, an xkb state) and lives entirely on the control thread
/// that creates it.
pub trait InputInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()>;
}

/// Preferred injection backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// wlroots virtual pointer + keyboard Wayland protocols — the headless-Sway path.
    WlrVirtual,
    /// libei via `reis` — Wayland-native (RemoteDesktop portal). Not yet implemented.
    Libei,
    /// `/dev/uinput` — universal fallback (but invisible to `WLR_LIBINPUT_NO_DEVICES=1`).
    Uinput,
}

pub fn open(backend: Backend) -> Result<Box<dyn InputInjector>> {
    match backend {
        Backend::WlrVirtual => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(wlr::WlrootsInjector::open()?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("wlroots virtual input requires Linux + a Wayland compositor")
            }
        }
        Backend::Libei => {
            #[cfg(target_os = "linux")]
            {
                Ok(Box::new(libei::LibeiInjector::open()?))
            }
            #[cfg(not(target_os = "linux"))]
            {
                anyhow::bail!("libei input requires Linux + a RemoteDesktop portal")
            }
        }
        other => anyhow::bail!("injection backend {other:?} not implemented"),
    }
}

/// Pick the injection backend for the current session. wlroots/Sway only implements the
/// ScreenCast portal (no RemoteDesktop), so libei can't run there — use the wlr virtual-input
/// protocols. KWin and GNOME implement RemoteDesktop but not the wlr protocols, so use libei.
/// `LUMEN_INPUT_BACKEND=wlr|libei` overrides the auto-detection.
pub fn default_backend() -> Backend {
    if let Ok(v) = std::env::var("LUMEN_INPUT_BACKEND") {
        match v.trim().to_ascii_lowercase().as_str() {
            "wlr" | "wlroots" | "wlrvirtual" => return Backend::WlrVirtual,
            "libei" | "ei" | "portal" => return Backend::Libei,
            "uinput" => return Backend::Uinput,
            other => tracing::warn!(
                value = other,
                "unknown LUMEN_INPUT_BACKEND — auto-detecting"
            ),
        }
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let d = desktop.to_ascii_uppercase();
    if d.contains("KDE") || d.contains("GNOME") {
        Backend::Libei
    } else {
        Backend::WlrVirtual
    }
}

/// Map a Windows Virtual-Key code (as sent by Moonlight/GameStream) to a Linux evdev key code.
pub fn vk_to_evdev(vk: u8) -> Option<u16> {
    match vk {
        // --- Navigation / editing / whitespace ---
        0x08 => Some(14),  // VK_BACK     -> KEY_BACKSPACE
        0x09 => Some(15),  // VK_TAB      -> KEY_TAB
        0x0D => Some(28),  // VK_RETURN   -> KEY_ENTER
        0x13 => Some(119), // VK_PAUSE    -> KEY_PAUSE
        0x14 => Some(58),  // VK_CAPITAL  -> KEY_CAPSLOCK
        0x1B => Some(1),   // VK_ESCAPE   -> KEY_ESC
        0x20 => Some(57),  // VK_SPACE    -> KEY_SPACE
        0x21 => Some(104), // VK_PRIOR    -> KEY_PAGEUP
        0x22 => Some(109), // VK_NEXT     -> KEY_PAGEDOWN
        0x23 => Some(107), // VK_END      -> KEY_END
        0x24 => Some(102), // VK_HOME     -> KEY_HOME
        0x25 => Some(105), // VK_LEFT     -> KEY_LEFT
        0x26 => Some(103), // VK_UP       -> KEY_UP
        0x27 => Some(106), // VK_RIGHT    -> KEY_RIGHT
        0x28 => Some(108), // VK_DOWN     -> KEY_DOWN
        0x2C => Some(99),  // VK_SNAPSHOT -> KEY_SYSRQ
        0x2D => Some(110), // VK_INSERT   -> KEY_INSERT
        0x2E => Some(111), // VK_DELETE   -> KEY_DELETE

        // --- Generic modifiers ---
        0x10 => Some(42), // VK_SHIFT   -> KEY_LEFTSHIFT
        0x11 => Some(29), // VK_CONTROL -> KEY_LEFTCTRL
        0x12 => Some(56), // VK_MENU    -> KEY_LEFTALT

        // --- Digit row (KEY_0 is 11, KEY_1..KEY_9 are 2..10) ---
        0x30 => Some(11), // VK_0
        0x31 => Some(2),  // VK_1
        0x32 => Some(3),  // VK_2
        0x33 => Some(4),  // VK_3
        0x34 => Some(5),  // VK_4
        0x35 => Some(6),  // VK_5
        0x36 => Some(7),  // VK_6
        0x37 => Some(8),  // VK_7
        0x38 => Some(9),  // VK_8
        0x39 => Some(10), // VK_9

        // --- Letters A-Z (NOT sequential in evdev) ---
        0x41 => Some(30), // A
        0x42 => Some(48), // B
        0x43 => Some(46), // C
        0x44 => Some(32), // D
        0x45 => Some(18), // E
        0x46 => Some(33), // F
        0x47 => Some(34), // G
        0x48 => Some(35), // H
        0x49 => Some(23), // I
        0x4A => Some(36), // J
        0x4B => Some(37), // K
        0x4C => Some(38), // L
        0x4D => Some(50), // M
        0x4E => Some(49), // N
        0x4F => Some(24), // O
        0x50 => Some(25), // P
        0x51 => Some(16), // Q
        0x52 => Some(19), // R
        0x53 => Some(31), // S
        0x54 => Some(20), // T
        0x55 => Some(22), // U
        0x56 => Some(47), // V
        0x57 => Some(17), // W
        0x58 => Some(45), // X
        0x59 => Some(21), // Y
        0x5A => Some(44), // Z

        // --- Meta / context-menu ---
        0x5B => Some(125), // VK_LWIN -> KEY_LEFTMETA
        0x5C => Some(126), // VK_RWIN -> KEY_RIGHTMETA
        0x5D => Some(127), // VK_APPS -> KEY_COMPOSE

        // --- Numpad ---
        0x60 => Some(82), // KP0
        0x61 => Some(79), // KP1
        0x62 => Some(80), // KP2
        0x63 => Some(81), // KP3
        0x64 => Some(75), // KP4
        0x65 => Some(76), // KP5
        0x66 => Some(77), // KP6
        0x67 => Some(71), // KP7
        0x68 => Some(72), // KP8
        0x69 => Some(73), // KP9
        0x6A => Some(55), // VK_MULTIPLY  -> KEY_KPASTERISK
        0x6B => Some(78), // VK_ADD       -> KEY_KPPLUS
        0x6C => Some(96), // VK_SEPARATOR -> KEY_KPENTER
        0x6D => Some(74), // VK_SUBTRACT  -> KEY_KPMINUS
        0x6E => Some(83), // VK_DECIMAL   -> KEY_KPDOT
        0x6F => Some(98), // VK_DIVIDE    -> KEY_KPSLASH

        // --- Function keys (F1..F10 = 59..68, F11/F12 = 87/88) ---
        0x70 => Some(59),
        0x71 => Some(60),
        0x72 => Some(61),
        0x73 => Some(62),
        0x74 => Some(63),
        0x75 => Some(64),
        0x76 => Some(65),
        0x77 => Some(66),
        0x78 => Some(67),
        0x79 => Some(68),
        0x7A => Some(87),
        0x7B => Some(88),

        // --- Locks ---
        0x90 => Some(69), // VK_NUMLOCK -> KEY_NUMLOCK
        0x91 => Some(70), // VK_SCROLL  -> KEY_SCROLLLOCK

        // --- Left/right modifiers ---
        0xA0 => Some(42),  // VK_LSHIFT   -> KEY_LEFTSHIFT
        0xA1 => Some(54),  // VK_RSHIFT   -> KEY_RIGHTSHIFT
        0xA2 => Some(29),  // VK_LCONTROL -> KEY_LEFTCTRL
        0xA3 => Some(97),  // VK_RCONTROL -> KEY_RIGHTCTRL
        0xA4 => Some(56),  // VK_LMENU    -> KEY_LEFTALT
        0xA5 => Some(100), // VK_RMENU    -> KEY_RIGHTALT

        // --- OEM punctuation (US layout) ---
        0xBA => Some(39), // VK_OEM_1      -> KEY_SEMICOLON
        0xBB => Some(13), // VK_OEM_PLUS   -> KEY_EQUAL
        0xBC => Some(51), // VK_OEM_COMMA  -> KEY_COMMA
        0xBD => Some(12), // VK_OEM_MINUS  -> KEY_MINUS
        0xBE => Some(52), // VK_OEM_PERIOD -> KEY_DOT
        0xBF => Some(53), // VK_OEM_2      -> KEY_SLASH
        0xC0 => Some(41), // VK_OEM_3      -> KEY_GRAVE
        0xDB => Some(26), // VK_OEM_4      -> KEY_LEFTBRACE
        0xDC => Some(43), // VK_OEM_5      -> KEY_BACKSLASH
        0xDD => Some(27), // VK_OEM_6      -> KEY_RIGHTBRACE
        0xDE => Some(40), // VK_OEM_7      -> KEY_APOSTROPHE
        0xE2 => Some(86), // VK_OEM_102    -> KEY_102ND

        _ => None,
    }
}

/// Map a GameStream mouse button id (1=left … 5=X2) to a Linux evdev `BTN_*` code.
#[cfg(target_os = "linux")]
fn gs_button_to_evdev(b: u32) -> Option<u32> {
    Some(match b {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        4 => 0x113, // BTN_SIDE  (X1)
        5 => 0x114, // BTN_EXTRA (X2)
        _ => return None,
    })
}

#[cfg(target_os = "linux")]
mod libei;
#[cfg(target_os = "linux")]
mod wlr;
