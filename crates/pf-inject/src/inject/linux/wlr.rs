//! Virtual pointer and keyboard on wlroots (`zwlr_virtual_pointer_manager_v1`,
//! `zwp_virtual_keyboard_manager_v1`).
//!
//! Absolute motion is mapped onto the `wl_output` the pointer was created
//! with; there is no re-aim request. The streamed head is published by name
//! in [`crate::stream_output`] and rebound by [`WlrootsInjector::retarget`].
//! A miss binds no output (whole-layout mapping). First-advertised is the
//! operator's physical display, never the session head.

use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
use anyhow::{bail, Context, Result};
use punktfunk_core::input::InputKind;
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd};
use std::time::Instant;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_output::{self, WlOutput},
    wl_pointer, wl_registry,
    wl_seat::WlSeat,
};
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

const SCROLL_HORIZONTAL: u32 = 1;

/// v4 is the first `wl_output` with `name`; bind that high to match the streamed head.
const WL_OUTPUT_MAX: u32 = 4;

struct Output {
    /// Registry global name: `GlobalRemove` key and `WlOutput` user data.
    global: u32,
    proxy: WlOutput,
    /// `wl_output.name` (v4). Same string every client sees; `None` below v4.
    name: Option<String>,
}

#[derive(Default)]
struct Globals {
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<WlSeat>,
    /// Every advertised head, advertisement order. The first is the operator's
    /// physical display; the streamed head is added later and is never first.
    outputs: Vec<Output>,
}

/// Index of `want` in advertisement order. No fallback: a miss is `None`
/// (whole-layout mapping). First-advertised is the operator's physical head.
///
/// Split from [`Globals::output_named`] so the rule is testable without a live connection.
fn index_named<'a>(
    names: impl IntoIterator<Item = Option<&'a str>>,
    want: Option<&str>,
) -> Option<usize> {
    let want = want?;
    names.into_iter().position(|n| n == Some(want))
}

impl Globals {
    fn output_named(&self, want: &str) -> Option<WlOutput> {
        index_named(self.outputs.iter().map(|o| o.name.as_deref()), Some(want))
            .map(|i| self.outputs[i].proxy.clone())
    }
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_mgr = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                "wl_output" => {
                    // User data is the registry global name so later events hit this entry.
                    let proxy = registry.bind(name, version.min(WL_OUTPUT_MAX), qh, name);
                    state.outputs.push(Output {
                        global: name,
                        proxy,
                        name: None,
                    });
                }
                _ => {}
            },
            // Drop the gone head; the pointer may still be bound to it until the next `retarget`.
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| o.global != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for Globals {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global) {
                o.name = Some(name);
            }
        }
    }
}

// These proxies emit no events we handle; Dispatch is still required to bind them.
macro_rules! ignore_events {
    ($($t:ty),* $(,)?) => {$(
        impl Dispatch<$t, ()> for Globals {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    WlSeat,
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
    /// Output `pointer` is bound to; `None` maps absolute motion over the whole layout.
    bound_output: Option<String>,
    /// Buttons held on `pointer`. Released before destroy; the compositor will not.
    pressed: Vec<u32>,
    keyboard: ZwpVirtualKeyboardV1,
    xkb_state: xkb::State,
    _keymap_file: std::fs::File, // compositor mmaps this memfd; drop would unmap it
    text: Option<TextKeyboard>,
    start: Instant,
}

fn resolve_target(globals: &Globals) -> (Option<WlOutput>, Option<String>) {
    let Some(want) = crate::stream_output() else {
        return (None, None);
    };
    match globals.output_named(&want) {
        Some(proxy) => (Some(proxy), Some(want)),
        None => (None, None),
    }
}

/// Distinct chars before the text keymap restarts. Keycodes start at 9; xkb max is 255.
const TEXT_KEYMAP_MAX: usize = 200;

/// Separate `zwp_virtual_keyboard` for [`InputKind::TextInput`]. Keymap re-uploads
/// must not touch the main device's layout or modifier state.
struct TextKeyboard {
    keyboard: ZwpVirtualKeyboardV1,
    /// `chars[i]` types on wire keycode `i + 1` (xkb `i + 9`).
    chars: Vec<char>,
    _keymap_file: Option<std::fs::File>, // compositor mmaps this memfd; drop would unmap it
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

        // First roundtrip bound the outputs; `name` events land on this one. Resolve before create.
        queue
            .roundtrip(&mut globals)
            .context("Wayland output-name roundtrip")?;

        let (target, bound_output) = resolve_target(&globals);
        let pointer =
            pointer_mgr.create_virtual_pointer_with_output(Some(&seat), target.as_ref(), &qh, ());
        let keyboard = keyboard_mgr.create_virtual_keyboard(&seat, &qh, ());

        // Wire keys are US-positional; this keymap is the host layout or ISO keys
        // type as US neighbours. `crate::layout`, not empty names: those fall
        // through to `XKB_DEFAULT_*`, which a Wayland session does not export.
        let resolved = pf_host_config::layout::system_layout();
        let (rules, model, layout, variant, options) = resolved.names.as_args();
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            rules,
            model,
            layout,
            variant,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .with_context(|| {
            format!(
                "compile xkb keymap {} (from {})",
                resolved.names.describe(),
                resolved.source
            )
        })?;
        tracing::info!(
            layout = %resolved.names.describe(),
            source = %resolved.source,
            "virtual keyboard keymap compiled"
        );
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
            outputs = globals.outputs.len(),
            want = ?crate::stream_output(),
            bound = ?bound_output,
            "wlroots virtual input ready (pointer + keyboard)"
        );
        Ok(Self {
            conn,
            queue,
            globals,
            pointer,
            bound_output,
            pressed: Vec::new(),
            keyboard,
            xkb_state,
            _keymap_file: file,
            text: None,
            start: Instant::now(),
        })
    }

    /// Recreate the pointer when the streamed head changes. The protocol maps
    /// `motion_absolute` onto the output given at create and has no re-aim.
    /// Call immediately before the motion so the new device's first position
    /// is this sample, not the compositor's default.
    ///
    /// Match by name, never by size: `MouseMoveAbs` extent is the client's
    /// letterboxed content rect, not the streamed mode.
    fn retarget(&mut self) {
        let (target, want) = resolve_target(&self.globals);
        if want == self.bound_output {
            return;
        }
        let (Some(mgr), Some(seat)) = (self.globals.pointer_mgr.clone(), self.globals.seat.clone())
        else {
            return;
        };
        // Release first; destroy mid-press leaves a stuck host button.
        if !self.pressed.is_empty() {
            let t = self.now_ms();
            for btn in std::mem::take(&mut self.pressed) {
                self.pointer
                    .button(t, btn, wl_pointer::ButtonState::Released);
            }
            self.pointer.frame();
        }
        self.pointer.destroy();
        self.pointer = mgr.create_virtual_pointer_with_output(
            Some(&seat),
            target.as_ref(),
            &self.queue.handle(),
            (),
        );
        tracing::info!(
            from = ?self.bound_output,
            to = ?want,
            "wlroots virtual pointer re-aimed (absolute input now maps into this output)"
        );
        self.bound_output = want;
    }

    /// Read the socket, dispatch, flush. `dispatch_pending` does not read, so
    /// without `read()` a `wl_output` created after `open` is never seen and
    /// protocol errors sit unread. `WouldBlock` is the idle case, not an error.
    fn pump(&mut self) -> Result<()> {
        // `prepare_read` refuses a guard while events are queued; dispatch first.
        self.queue
            .dispatch_pending(&mut self.globals)
            .context("wayland dispatch")?;
        if let Some(guard) = self.conn.prepare_read() {
            match guard.read() {
                Ok(_) => {
                    self.queue
                        .dispatch_pending(&mut self.globals)
                        .context("wayland dispatch (post-read)")?;
                }
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e).context("wayland read"),
            }
        }
        self.conn.flush().context("wayland flush")?;
        Ok(())
    }

    fn now_ms(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// One Unicode scalar on the text device. Controls are dropped; Enter/Backspace/Tab
    /// ride the VK path.
    fn type_text(&mut self, cp: u32) -> Result<()> {
        let Some(ch) = char::from_u32(cp) else {
            return Ok(());
        };
        if ch.is_control() {
            return Ok(());
        }
        if self.text.is_none() {
            let (Some(mgr), Some(seat)) =
                (self.globals.keyboard_mgr.clone(), self.globals.seat.clone())
            else {
                return Ok(());
            };
            let kb = mgr.create_virtual_keyboard(&seat, &self.queue.handle(), ());
            self.text = Some(TextKeyboard {
                keyboard: kb,
                chars: Vec::new(),
                _keymap_file: None,
            });
        }
        let t = self.now_ms();
        let text = self.text.as_mut().expect("created above");
        let code = match text.chars.iter().position(|&c| c == ch) {
            Some(i) => (i + 1) as u32,
            None => {
                if text.chars.len() >= TEXT_KEYMAP_MAX {
                    text.chars.clear();
                }
                text.chars.push(ch);
                let keymap_str = text_keymap(&text.chars);
                let file = memfd_with(&keymap_str)?;
                text.keyboard.keymap(
                    1, /* XKB_V1 */
                    file.as_fd(),
                    keymap_str.len() as u32 + 1,
                );
                text._keymap_file = Some(file);
                text.chars.len() as u32
            }
        };
        text.keyboard.key(t, code, 1);
        text.keyboard.key(t, code, 0);
        Ok(())
    }

    fn send_modifiers(&mut self, evdev: u16, down: bool) {
        let kc = xkb::Keycode::new(evdev as u32 + 8); // xkb keycodes are evdev + 8
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
                    // Absolute motion maps onto the bound output; only this arm depends on it.
                    self.retarget();
                    let t = self.now_ms(); // `retarget` may have consumed time releasing buttons
                    let x = event.x.clamp(0, w as i32) as u32;
                    let y = event.y.clamp(0, h as i32) as u32;
                    self.pointer.motion_absolute(t, x, y, w, h);
                    self.pointer.frame();
                }
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                if let Some(btn) = gs_button_to_evdev(event.code) {
                    let st = if event.kind == InputKind::MouseButtonDown {
                        if !self.pressed.contains(&btn) {
                            self.pressed.push(btn);
                        }
                        wl_pointer::ButtonState::Pressed
                    } else {
                        self.pressed.retain(|&b| b != btn);
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
                // GameStream is WHEEL_DELTA (120) per notch; a notch ≈ 15 px. Vertical
                // up is positive on GameStream and negative on Wayland; horizontal
                // right is already positive (moonlight-qt/Sunshine pass it unnegated).
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
            InputKind::TextInput => {
                self.type_text(event.code)?;
            }
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => {}
            // No virtual-touch protocol here; touch is libei only.
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {}
        }
        self.pump()
    }
}

/// Keycode `i + 9` (wire `i + 1`) types `chars[i]` as Unicode keysym `U<hex>`.
/// Types/compat `include "complete"` is the `wtype` shape; system XKB data is
/// already required by `open`.
fn text_keymap(chars: &[char]) -> String {
    use std::fmt::Write as _;
    let mut keycodes = String::new();
    let mut symbols = String::new();
    for (i, ch) in chars.iter().enumerate() {
        let _ = writeln!(keycodes, "        <T{i}> = {};", i + 9);
        let _ = writeln!(symbols, "        key <T{i}> {{ [ U{:04X} ] }};", *ch as u32);
    }
    format!(
        "xkb_keymap {{\n\
             xkb_keycodes \"punktfunk-text\" {{\n\
                 minimum = 8;\n\
                 maximum = {};\n\
         {keycodes}\
             }};\n\
             xkb_types \"punktfunk-text\" {{ include \"complete\" }};\n\
             xkb_compatibility \"punktfunk-text\" {{ include \"complete\" }};\n\
             xkb_symbols \"punktfunk-text\" {{\n{symbols}    }};\n\
         }};\n",
        chars.len() + 9,
    )
}

/// Anonymous file of `s` plus a trailing NUL; the compositor's keymap mmap needs the NUL.
fn memfd_with(s: &str) -> Result<std::fs::File> {
    let name = b"punktfunk-keymap\0";
    // SAFETY: `name` is a byte-string literal with an explicit trailing NUL, so `name.as_ptr()` is a
    // valid NUL-terminated C string; `memfd_create` only reads that name (copying it) and creates an
    // anonymous file, returning a fresh fd (or -1). `MFD_CLOEXEC` is a valid flag. The 'static literal
    // outlives the synchronous call and nothing aliases it. The result is checked `< 0` below.
    let fd = unsafe { libc::memfd_create(name.as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC) };
    if fd < 0 {
        bail!("memfd_create failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is the fresh memfd `memfd_create` just returned and checked `>= 0`; it is a unique
    // open fd nothing else owns, so `File` takes sole ownership and closes it exactly once on drop —
    // no alias, no double-close.
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    f.write_all(s.as_bytes()).context("write keymap")?;
    f.write_all(&[0]).context("write keymap NUL")?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Physical head first, session head later — advertisement order on a real box.
    const HYPRLAND_BOX: [Option<&str>; 2] = [Some("HDMI-A-1"), Some("PF-87756-3")];

    #[test]
    fn binds_the_streamed_head_not_the_first_advertised_one() {
        assert_eq!(index_named(HYPRLAND_BOX, Some("PF-87756-3")), Some(1));
        assert_eq!(index_named(HYPRLAND_BOX, Some("HDMI-A-1")), Some(0));
        let sway = [Some("HEADLESS-1"), Some("DP-2"), Some("HEADLESS-2")];
        assert_eq!(index_named(sway, Some("HEADLESS-2")), Some(2));
        assert_eq!(index_named(sway, Some("DP-2")), Some(1));
    }

    #[test]
    fn an_unknown_target_binds_nothing_rather_than_falling_back() {
        // Name not in the advertised set (injector can open before the head exists).
        assert_eq!(index_named(HYPRLAND_BOX, Some("PF-87756-9")), None);
        assert_eq!(index_named(HYPRLAND_BOX, None), None);
        // v3 compositor: globals exist but never get a `name`.
        assert_eq!(index_named([None, None], Some("PF-87756-3")), None);
        let headless: [Option<&str>; 0] = [];
        assert_eq!(index_named(headless, Some("PF-87756-3")), None);
    }
}
