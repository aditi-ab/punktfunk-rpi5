//! Input injection through the wlroots virtual-input Wayland protocols
//! (`zwlr_virtual_pointer_manager_v1` + `zwp_virtual_keyboard_manager_v1`) — the headless-Sway
//! path. We connect as an ordinary Wayland client (the host inherits Sway's
//! `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`), bind the two managers, upload an xkb keymap for the
//! virtual keyboard (the host's layout via the standard `XKB_DEFAULT_LAYOUT` et al., defaulting
//! to evdev/US), and translate events into virtual pointer/keyboard requests, tracking modifier
//! state so the compositor resolves shifted keysyms correctly.
//!
//! **Absolute** motion is mapped by the compositor onto the `wl_output` the virtual pointer was
//! CREATED with, so which output that is decides where every absolute sample lands. We aim it at
//! the head the session is actually streaming — published by name in [`crate::stream_output`] and
//! re-resolved (re-creating the pointer) whenever it changes; see [`WlrootsInjector::retarget`].

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

/// `code` value marking a horizontal scroll event (mirrors `gamestream::input`).
const SCROLL_HORIZONTAL: u32 = 1;

/// `wl_output.name` — the connector name we match the streamed head on — arrived in v4. Nothing
/// else we ask of an output needs more than v1, so a lower advert only costs us the names (and
/// with them the ability to aim absolute input; see [`index_named`]). Same constant, same reason,
/// as `pf_vdisplay`'s `kwin_dpms`.
const WL_OUTPUT_MAX: u32 = 4;

/// One `wl_output` the compositor has advertised.
struct Output {
    /// The registry global name — the key `wl_registry.global_remove` reports, and the user data
    /// each `wl_output` event carries back so we know which head it describes.
    global: u32,
    proxy: WlOutput,
    /// `wl_output.name` (protocol v4): the compositor's own name for the head — `HDMI-A-1`,
    /// Hyprland's `PF-<pid>-<n>`, sway's `HEADLESS-N`. The protocol guarantees this is "the same
    /// output name for all clients", which is what lets us match the name `hyprctl`/`swaymsg`
    /// minted on the vdisplay side. `None` on a compositor stuck at v3, which has no name event at
    /// all — then there is nothing to match on and the pointer stays unbound.
    name: Option<String>,
}

/// Globals bound from the registry (the Wayland dispatch state).
#[derive(Default)]
struct Globals {
    pointer_mgr: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_mgr: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<WlSeat>,
    /// EVERY advertised output, in advertisement order — not just the first. The streamed head is
    /// created per session, so it is never the first one advertised (that is the operator's
    /// oldest physical head), and binding only the first is what aimed absolute input at the
    /// wrong screen on every EXTEND box.
    outputs: Vec<Output>,
}

/// Which advertised output — by position in `names`, which is advertisement order — the virtual
/// pointer should bind to for the published target `want`.
///
/// The rule has **no fallback on purpose**, and that absence is the fix: what this replaced was a
/// fallback ("bind whatever `wl_output` came first"), and the first-advertised output is the oldest
/// global, i.e. the operator's physical head — never the per-session headless one the client is
/// looking at. A target that matches nothing therefore yields `None`, which binds the pointer to no
/// output and maps absolute coordinates over the whole layout: wrong-ish, but reachable, where a
/// pin to the wrong head is unreachable.
///
/// Split out of [`Globals::output_named`] so the rule is testable — a `WlOutput` proxy cannot be
/// constructed without a live Wayland connection, but the decision it feeds can.
fn index_named<'a>(
    names: impl IntoIterator<Item = Option<&'a str>>,
    want: Option<&str>,
) -> Option<usize> {
    let want = want?;
    names.into_iter().position(|n| n == Some(want))
}

impl Globals {
    /// The `wl_output` whose compositor name is `want`, if it is currently advertised.
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
                    // The `name` event is the only thing that tells the streamed head from the
                    // operator's. Older compositors bind lower and stay nameless (harmless:
                    // `output_named` then matches nothing and the pointer maps over the layout).
                    // The registry global name rides along as user data so the events that follow
                    // land on the right entry.
                    let proxy = registry.bind(name, version.min(WL_OUTPUT_MAX), qh, name);
                    state.outputs.push(Output {
                        global: name,
                        proxy,
                        name: None,
                    });
                }
                _ => {}
            },
            // A head went away — a session's headless output being torn down is the common case,
            // and the pointer must stop being aimed at a dead object (`retarget` re-resolves and
            // falls back to the whole layout on the next absolute sample).
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
        // Only the name matters here: geometry/mode/scale are the compositor's problem, because
        // binding the pointer to an output makes IT do the mapping (see `retarget`).
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.iter_mut().find(|o| o.global == *global) {
                o.name = Some(name);
            }
        }
    }
}

// The managers, the two virtual devices and the seat emit no events we use.
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
    /// The compositor name of the output `pointer` is bound to, or `None` when it is bound to no
    /// output (absolute coordinates then span the whole layout). Compared against
    /// [`crate::stream_output`] on every absolute sample; a difference re-creates the pointer.
    bound_output: Option<String>,
    /// evdev codes of the mouse buttons currently held on `pointer`, so re-creating the device
    /// can release them first — the compositor has no reason to, and a virtual pointer destroyed
    /// mid-press leaves the host with a stuck mouse button.
    pressed: Vec<u32>,
    keyboard: ZwpVirtualKeyboardV1,
    xkb_state: xkb::State,
    _keymap_file: std::fs::File, // keep the memfd alive for the compositor's mmap
    /// Dedicated committed-text device ([`InputKind::TextInput`]), created on first use.
    text: Option<TextKeyboard>,
    start: Instant,
}

/// Resolve the published stream output ([`crate::stream_output`]) against the outputs this
/// connection has bound: `(proxy, name)` when the target is live, `(None, None)` otherwise.
///
/// `(None, None)` covers three cases that all want the same answer — nothing published yet (before
/// the first capture bring-up), the target's `wl_output` global not advertised yet (the injector
/// opens on the first input event, which can beat the session's display), and the target torn down
/// (session end). A pointer bound to no output maps absolute coordinates over the whole layout,
/// which on a single-output compositor is exactly that output and on a multi-head one at least
/// keeps the streamed head reachable — unlike a pin to a head nobody is streaming.
fn resolve_target(globals: &Globals) -> (Option<WlOutput>, Option<String>) {
    let Some(want) = crate::stream_output() else {
        return (None, None);
    };
    match globals.output_named(&want) {
        Some(proxy) => (Some(proxy), Some(want)),
        None => (None, None),
    }
}

/// Cap on distinct characters the dynamic text keymap holds before it restarts from scratch
/// (keycodes grow upward from 9; xkb tops out at 255, so stay well under).
const TEXT_KEYMAP_MAX: usize = 200;

/// The dedicated **text** virtual keyboard: types committed IME text (`InputKind::TextInput`,
/// one Unicode scalar per event) by growing a keymap of Unicode keysyms on demand and pressing
/// the character's keycode — the `wtype` model. A separate `zwp_virtual_keyboard` so keymap
/// re-uploads never disturb the main device's layout/modifier state that VK key events ride on.
struct TextKeyboard {
    keyboard: ZwpVirtualKeyboardV1,
    /// Characters in keycode order: `chars[i]` types on wire keycode `i + 1` (xkb `i + 9`).
    chars: Vec<char>,
    _keymap_file: Option<std::fs::File>, // keep the memfd alive for the compositor's mmap
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

        // A second roundtrip: the first only said WHICH globals exist. The `wl_output.name` events
        // that identify each head are emitted on the objects we bound *during* that roundtrip, so
        // they only land now — and the pointer's output has to be resolved before we create it.
        queue
            .roundtrip(&mut globals)
            .context("Wayland output-name roundtrip")?;

        let (target, bound_output) = resolve_target(&globals);
        let pointer =
            pointer_mgr.create_virtual_pointer_with_output(Some(&seat), target.as_ref(), &qh, ());
        let keyboard = keyboard_mgr.create_virtual_keyboard(&seat, &qh, ());

        // The keymap the compositor resolves our raw evdev keycodes with. The wire keys are
        // US-POSITIONAL, so this keymap is what decides the character each one finally types —
        // it has to be the layout printed on the client's keyboard, or ISO keys render as their
        // US neighbours (`#`→`\`, `ä`→`'`, `-`→`/`).
        //
        // Resolved from the box's own configuration (`crate::layout`), NOT from empty names:
        // empty defers to `XKB_DEFAULT_*`, which nothing on a Wayland session exports, so a
        // `localectl set-x11-keymap de` host silently compiled evdev/pc105/**us**. (Before that
        // it hardcoded "us" outright.) `XKB_DEFAULT_*` still wins when an operator sets it.
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

    /// Aim the virtual pointer at the output the session is streaming, re-creating it when that
    /// changes — the fix for absolute input landing on the operator's screen.
    ///
    /// The wlr protocol maps `motion_absolute` onto the output the pointer was **created with**
    /// and offers no way to re-aim one, so a change means destroy + create. Cheap and rare: the
    /// host publishes the target once per capture bring-up, so a re-create fires at most a couple
    /// of times per session. The no-change path — every other absolute sample — costs one `RwLock`
    /// read and a scan of the output list, which has one entry per head.
    ///
    /// Called from the `MouseMoveAbs` arm immediately BEFORE the motion is sent, so a re-created
    /// pointer gets its first position in the same batch rather than sitting wherever the
    /// compositor puts a brand-new device.
    ///
    /// Resolution is by NAME, never by size: `MouseMoveAbs`'s extent is the client's letterboxed
    /// content rect in ITS window, not the streamed mode, so no size ladder could identify the
    /// head. Falling back to no output at all (whole-layout mapping) when the target is unknown is
    /// deliberate — see [`crate::stream_output`]'s module doc.
    fn retarget(&mut self) {
        let (target, want) = resolve_target(&self.globals);
        if want == self.bound_output {
            return;
        }
        let (Some(mgr), Some(seat)) = (self.globals.pointer_mgr.clone(), self.globals.seat.clone())
        else {
            return; // cannot re-create without the manager/seat; keep the pointer we have
        };
        // Never destroy a device with a button held: nothing else will release it.
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

    /// Drain the compositor's half of the connection, then push our batch to it — run after every
    /// injected event.
    ///
    /// The **read** is the load-bearing half, and it used to be missing: `dispatch_pending`'s own
    /// documentation says it "will not perform reads on the Wayland socket", so the queue only
    /// ever held what [`Self::open`]'s roundtrips put there. Two consequences, both real. The
    /// injector could never learn about a `wl_output` created AFTER it opened — which is exactly
    /// the ordering the field report was captured in, and would have left [`Self::retarget`] with
    /// nothing to resolve. And everything the compositor sent us piled up unread in the socket
    /// buffer for the host's lifetime, including the protocol errors the code here claimed to be
    /// surfacing but structurally could not.
    ///
    /// Non-blocking by construction: `read()` is documented to answer `WouldBlock` when the socket
    /// has nothing for us, which is the common case at input rates and is not an error.
    fn pump(&mut self) -> Result<()> {
        // `prepare_read` will not hand out a guard while events are still queued, so dispatch first.
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

    /// Type one committed-text Unicode scalar on the dedicated text device (created lazily),
    /// growing its keymap when the character is new. Control characters are dropped — Enter,
    /// Backspace and Tab ride the VK key-event path.
    fn type_text(&mut self, cp: u32) -> Result<()> {
        let Some(ch) = char::from_u32(cp) else {
            return Ok(()); // lone surrogate / out of range
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
                    text.chars.clear(); // restart the map; old codes are re-assigned lazily
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
                    // The compositor maps these onto the pointer's bound output, so make sure that
                    // is the head this session streams before sending any. Checked here rather
                    // than per inject: only absolute motion depends on the binding, and a pointer
                    // swapped mid-drag is the one thing `retarget` has to work to be safe about.
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
            InputKind::TextInput => {
                self.type_text(event.code)?;
            }
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => {} // not yet injected
            // wlroots has no virtual-touch protocol wired here; touch is the libei path only.
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => {}
        }
        self.pump()
    }
}

/// Build a minimal xkb keymap whose keycode `i + 9` (wire code `i + 1`) types `chars[i]`, using
/// Unicode keysym names (`U<hex>` — xkbcommon resolves them for any scalar, emoji included).
/// Types/compat `include "complete"` mirrors `wtype`'s generated keymap — proven on wlroots
/// compositors, and the system XKB data is present (the main keymap compiled from it in `open`).
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

/// Create an anonymous in-memory file holding `s` + a trailing NUL (for the keymap fd).
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

    /// The live-box layout the field report came from: the operator's `HDMI-A-1` is advertised
    /// FIRST (it exists from compositor start), and the session's headless head is added later —
    /// so "first advertised" is always the wrong answer, whichever order the injector and the
    /// display happen to come up in.
    const HYPRLAND_BOX: [Option<&str>; 2] = [Some("HDMI-A-1"), Some("PF-87756-3")];

    #[test]
    fn binds_the_streamed_head_not_the_first_advertised_one() {
        assert_eq!(index_named(HYPRLAND_BOX, Some("PF-87756-3")), Some(1));
        assert_eq!(index_named(HYPRLAND_BOX, Some("HDMI-A-1")), Some(0));
        // sway's own naming, and a mirrored physical head, resolve the same way.
        let sway = [Some("HEADLESS-1"), Some("DP-2"), Some("HEADLESS-2")];
        assert_eq!(index_named(sway, Some("HEADLESS-2")), Some(2));
        assert_eq!(index_named(sway, Some("DP-2")), Some(1));
    }

    /// Every "we don't know" must land on NO output (whole-layout mapping), never on a guess —
    /// the regression this whole change exists to prevent.
    #[test]
    fn an_unknown_target_binds_nothing_rather_than_falling_back() {
        // Published but not advertised (yet, or any more — the injector opens on the first input
        // event, which can beat the display, and the head goes away at session end).
        assert_eq!(index_named(HYPRLAND_BOX, Some("PF-87756-9")), None);
        // Nothing published at all — before the first capture bring-up.
        assert_eq!(index_named(HYPRLAND_BOX, None), None);
        // A compositor older than wl_output v4 emits no `name` event, so nothing is matchable.
        assert_eq!(index_named([None, None], Some("PF-87756-3")), None);
        // …and a compositor advertising no outputs at all cannot resolve anything either.
        let headless: [Option<&str>; 0] = [];
        assert_eq!(index_named(headless, Some("PF-87756-3")), None);
    }
}
