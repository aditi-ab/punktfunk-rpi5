//! Input injection through the wlroots virtual-input Wayland protocols
//! (`zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1`) — the headless-Sway
//! path. We connect as an ordinary Wayland client (the host inherits Sway's
//! `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`), bind the two managers, upload a standard evdev/US xkb
//! keymap, and translate events into virtual pointer/keyboard requests, tracking modifier state
//! so the compositor resolves shifted keysyms correctly.

use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
use anyhow::{bail, Context, Result};
use punktfunk_core::input::InputKind;
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
        let conn = Connection::connect_to_env()
            .context("connect to Wayland (is Sway up + WAYLAND_DISPLAY/XDG_RUNTIME_DIR set?)")?;
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
                // GameStream = up (vertical), negative on the Wayland axis; but = RIGHT
                // (horizontal), already positive there (moonlight-qt/Sunshine pass
                // horizontal through unnegated) — only the vertical axis flips.
                let notches = event.x as f64 / 120.0;
                let sign = if event.code == SCROLL_HORIZONTAL {
                    1.0
                } else {
                    -1.0
                };
                self.pointer.axis_source(wl_pointer::AxisSource::Wheel);
                self.pointer.axis(t, axis, sign * notches * 15.0);
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
            // wlroots has no virtual-touch protocol wired here; touch is the libei path only.
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {}
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
    let name = b"punktfunk-keymap\0";
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(s.as_bytes()).context("write keymap")?;
    f.write_all(&[0]).context("write keymap NUL")?;
    Ok(f)
}
