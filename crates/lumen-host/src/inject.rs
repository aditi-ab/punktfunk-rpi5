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
        other => anyhow::bail!("injection backend {other:?} not implemented; use WlrVirtual"),
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
mod wlr {
    use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
    use anyhow::{bail, Context, Result};
    use lumen_core::input::InputKind;
    use std::io::Write;
    use std::os::fd::{AsFd, FromRawFd};
    use std::time::Instant;
    use wayland_client::protocol::{wl_output::WlOutput, wl_pointer, wl_registry, wl_seat::WlSeat};
    use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
        zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
        zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    };
    use wayland_protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    };
    use xkbcommon::xkb;

    /// `code` value marking a horizontal scroll event (mirrors `gamestream::input`).
    const SCROLL_HORIZONTAL: u32 = 1;

    /// Globals bound from the registry (the Wayland dispatch state).
    #[derive(Default)]
    struct Globals {
        pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
        keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
        seat: Option<WlSeat>,
        output: Option<WlOutput>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for Globals {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                match interface.as_str() {
                    "zwlr_virtual_pointer_manager_v1" => {
                        state.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()));
                    }
                    "zwp_virtual_keyboard_manager_v1" => {
                        state.keyboard_mgr = Some(registry.bind(name, version.min(1), qh, ()));
                    }
                    "wl_seat" => {
                        state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                    }
                    "wl_output" if state.output.is_none() => {
                        state.output = Some(registry.bind(name, version.min(3), qh, ()));
                    }
                    _ => {}
                }
            }
        }
    }

    // The managers, the two virtual devices, the seat and the output emit no events we use.
    macro_rules! ignore_events {
        ($($t:ty),* $(,)?) => {$(
            impl Dispatch<$t, ()> for Globals {
                fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            }
        )*};
    }
    ignore_events!(
        WlSeat,
        WlOutput,
        ZwlrVirtualPointerManagerV1,
        ZwlrVirtualPointerV1,
        ZwpVirtualKeyboardManagerV1,
        ZwpVirtualKeyboardV1,
    );

    pub struct WlrootsInjector {
        conn: Connection,
        queue: EventQueue<Globals>,
        globals: Globals,
        pointer: ZwlrVirtualPointerV1,
        keyboard: ZwpVirtualKeyboardV1,
        xkb_state: xkb::State,
        _keymap_file: std::fs::File, // keep the memfd alive for the compositor's mmap
        start: Instant,
    }

    impl WlrootsInjector {
        pub fn open() -> Result<Self> {
            let conn = Connection::connect_to_env().context(
                "connect to Wayland (is Sway up + WAYLAND_DISPLAY/XDG_RUNTIME_DIR set?)",
            )?;
            let mut queue = conn.new_event_queue();
            let qh = queue.handle();
            let _registry = conn.display().get_registry(&qh, ());
            let mut globals = Globals::default();
            queue
                .roundtrip(&mut globals)
                .context("Wayland registry roundtrip")?;

            let pointer_mgr = globals
                .pointer_mgr
                .clone()
                .context("compositor lacks zwlr_virtual_pointer_manager_v1")?;
            let keyboard_mgr = globals
                .keyboard_mgr
                .clone()
                .context("compositor lacks zwp_virtual_keyboard_manager_v1")?;
            let seat = globals
                .seat
                .clone()
                .context("compositor advertised no wl_seat")?;

            let pointer = pointer_mgr.create_virtual_pointer_with_output(
                Some(&seat),
                globals.output.as_ref(),
                &qh,
                (),
            );
            let keyboard = keyboard_mgr.create_virtual_keyboard(&seat, &qh, ());

            // A standard evdev/US keymap so raw evdev keycodes resolve to the right keysyms.
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let keymap = xkb::Keymap::new_from_names(
                &ctx,
                "evdev",
                "pc105",
                "us",
                "",
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .context("compile xkb keymap")?;
            let keymap_str = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
            let xkb_state = xkb::State::new(&keymap);

            let file = memfd_with(&keymap_str)?;
            let size = keymap_str.len() as u32 + 1; // include the trailing NUL
            keyboard.keymap(1 /* XKB_V1 */, file.as_fd(), size);
            queue
                .roundtrip(&mut globals)
                .context("keymap upload roundtrip")?;
            conn.flush().ok();

            tracing::info!(
                output = globals.output.is_some(),
                "wlroots virtual input ready (pointer + keyboard)"
            );
            Ok(Self {
                conn,
                queue,
                globals,
                pointer,
                keyboard,
                xkb_state,
                _keymap_file: file,
                start: Instant::now(),
            })
        }

        fn now_ms(&self) -> u32 {
            self.start.elapsed().as_millis() as u32
        }

        /// Update xkb state for a key and tell the compositor the resulting modifier mask.
        fn send_modifiers(&mut self, evdev: u16, down: bool) {
            let kc = xkb::Keycode::new(evdev as u32 + 8); // evdev -> xkb keycode
            let dir = if down {
                xkb::KeyDirection::Down
            } else {
                xkb::KeyDirection::Up
            };
            self.xkb_state.update_key(kc, dir);
            let depressed = self.xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
            let latched = self.xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED);
            let locked = self.xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED);
            let group = self.xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
            self.keyboard.modifiers(depressed, latched, locked, group);
        }
    }

    impl InputInjector for WlrootsInjector {
        fn inject(&mut self, event: &InputEvent) -> Result<()> {
            let t = self.now_ms();
            match event.kind {
                InputKind::MouseMove => {
                    self.pointer.motion(t, event.x as f64, event.y as f64);
                    self.pointer.frame();
                }
                InputKind::MouseMoveAbs => {
                    let w = (event.flags >> 16) & 0xffff;
                    let h = event.flags & 0xffff;
                    if w > 0 && h > 0 {
                        let x = event.x.clamp(0, w as i32) as u32;
                        let y = event.y.clamp(0, h as i32) as u32;
                        self.pointer.motion_absolute(t, x, y, w, h);
                        self.pointer.frame();
                    }
                }
                InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                    if let Some(btn) = gs_button_to_evdev(event.code) {
                        let st = if event.kind == InputKind::MouseButtonDown {
                            wl_pointer::ButtonState::Pressed
                        } else {
                            wl_pointer::ButtonState::Released
                        };
                        self.pointer.button(t, btn, st);
                        self.pointer.frame();
                    }
                }
                InputKind::MouseScroll => {
                    let axis = if event.code == SCROLL_HORIZONTAL {
                        wl_pointer::Axis::HorizontalScroll
                    } else {
                        wl_pointer::Axis::VerticalScroll
                    };
                    // GameStream sends WHEEL_DELTA(120)-scaled units; a notch ≈ 15px. Positive
                    // GameStream = scroll up, which is negative on the Wayland axis.
                    let notches = event.x as f64 / 120.0;
                    self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                    self.pointer.axis(t, axis, -notches * 15.0);
                    self.pointer.frame();
                }
                InputKind::KeyDown | InputKind::KeyUp => {
                    let down = event.kind == InputKind::KeyDown;
                    if let Some(evdev) = vk_to_evdev(event.code as u8) {
                        self.keyboard.key(t, evdev as u32, if down { 1 } else { 0 });
                        self.send_modifiers(evdev, down);
                    } else {
                        tracing::debug!(vk = event.code, "unmapped VK keycode — dropped");
                    }
                }
                InputKind::GamepadButton | InputKind::GamepadAxis => {} // not yet injected
            }
            // Surface protocol errors / disconnects, then push the batch to the compositor.
            self.queue
                .dispatch_pending(&mut self.globals)
                .context("wayland dispatch")?;
            self.conn.flush().context("wayland flush")?;
            Ok(())
        }
    }

    /// Create an anonymous in-memory file holding `s` + a trailing NUL (for the keymap fd).
    fn memfd_with(s: &str) -> Result<std::fs::File> {
        let name = b"lumen-keymap\0";
        let fd =
            unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
        if fd < 0 {
            bail!("memfd_create failed: {}", std::io::Error::last_os_error());
        }
        let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
        f.write_all(s.as_bytes()).context("write keymap")?;
        f.write_all(&[0]).context("write keymap NUL")?;
        Ok(f)
    }
}
