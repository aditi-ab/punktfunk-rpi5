//! Headless input injection on KWin via the privileged `org_kde_kwin_fake_input` protocol — the
//! exact path KDE's own headless RDP server (`krdpserver`) uses. KWin advertises this restricted
//! global only to a client authorized through its installed `.desktop` `X-KDE-Wayland-Interfaces`
//! (we ship `io.unom.Punktfunk.Host.desktop`, which lists `org_kde_kwin_fake_input` alongside
//! `zkde_screencast_unstable_v1`). Binding the global IS the authorization, so injection needs **no
//! RemoteDesktop portal and no "Allow remote control?" dialog** — it works with no user present,
//! which the libei/portal path cannot. We connect as an ordinary Wayland client on the KWin session's
//! `$WAYLAND_DISPLAY` and translate events into fake-input requests; keyboard keys are raw Linux
//! evdev codes that KWin resolves through the session's own keymap (no keymap upload, unlike the wlr
//! virtual-keyboard path), and absolute pointer/touch coordinates are global compositor space.
//!
//! Global compositor space is *logical* pixels (post display-scaling), which only equals the streamed
//! output's physical pixels at scale 1. Under a fractional/integer scale the logical edge sits at
//! `physical / scale`, so feeding the raw streamed pixel coordinate lands the cursor `scale×` too far
//! toward the bottom-right (top-left stays put). We therefore track each output's logical geometry
//! (position + size) via `xdg-output` and map the normalized client position into the matching
//! output's logical rectangle — the same shape the libei backend uses with its EI region.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
use anyhow::{Context, Result};
use punktfunk_core::input::InputKind;
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

// Generate the client bindings for the vendored protocol XML inline (no build.rs), exactly like the
// KWin virtual-output backend. Path is relative to CARGO_MANIFEST_DIR.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod fake {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/fake-input.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/fake-input.xml");
}

use fake::org_kde_kwin_fake_input::OrgKdeKwinFakeInput as FakeInput;

/// Highest interface version we drive. `keyboard_key` arrived at v4; KWin advertises ≥4.
const MAX_VERSION: u32 = 4;

/// `wl_pointer.axis` values used by `axis`.
const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;
/// `code` value marking a horizontal scroll event (mirrors `gamestream::input` / the wlr backend).
const SCROLL_HORIZONTAL: u32 = 1;

/// One tracked output: its physical mode (to match the streamed resolution) and its logical geometry
/// (the global-compositor-space rectangle absolute coordinates are addressed in). `logical_w == 0`
/// means xdg-output hasn't reported its size yet.
struct OutputTrack {
    /// Registry global id — also the dispatch user-data, so events route back to this entry.
    name: u32,
    wl_output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    /// Physical pixel mode from `wl_output.mode` (the `current` mode); matched against the streamed WxH.
    mode_w: i32,
    mode_h: i32,
    /// Logical (post-scale) geometry from `xdg-output`.
    logical_x: i32,
    logical_y: i32,
    logical_w: i32,
    logical_h: i32,
}

/// Registry-bound globals (the Wayland dispatch state).
#[derive(Default)]
struct State {
    fake: Option<FakeInput>,
    xdg_mgr: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputTrack>,
}

impl State {
    /// Create the `xdg_output` for a tracked output once both it and the manager exist.
    fn ensure_xdg_output(o: &mut OutputTrack, mgr: &ZxdgOutputManagerV1, qh: &QueueHandle<State>) {
        if o.xdg_output.is_none() {
            o.xdg_output = Some(mgr.get_xdg_output(&o.wl_output, qh, o.name));
        }
    }
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
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
                "org_kde_kwin_fake_input" => {
                    state.fake = Some(registry.bind(name, version.min(MAX_VERSION), qh, ()));
                }
                "wl_output" => {
                    // v1 carries `mode` (all we need); bind no higher than the proxy's max (4).
                    let wl_output: WlOutput = registry.bind(name, version.min(4), qh, name);
                    let mut o = OutputTrack {
                        name,
                        wl_output,
                        xdg_output: None,
                        mode_w: 0,
                        mode_h: 0,
                        logical_x: 0,
                        logical_y: 0,
                        logical_w: 0,
                        logical_h: 0,
                    };
                    if let Some(mgr) = state.xdg_mgr.clone() {
                        State::ensure_xdg_output(&mut o, &mgr, qh);
                    }
                    state.outputs.push(o);
                }
                "zxdg_output_manager_v1" => {
                    let mgr: ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
                    // Outputs bound before the manager have no xdg_output yet — create them now.
                    for o in state.outputs.iter_mut() {
                        State::ensure_xdg_output(o, &mgr, qh);
                    }
                    state.xdg_mgr = Some(mgr);
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.retain(|o| {
                    if o.name == name {
                        if let Some(x) = &o.xdg_output {
                            x.destroy();
                        }
                        false
                    } else {
                        true
                    }
                });
            }
            _ => {}
        }
    }
}

// fake_input emits no events.
impl Dispatch<FakeInput, ()> for State {
    fn event(
        _: &mut Self,
        _: &FakeInput,
        _: <FakeInput as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only the *current* mode matters — a real monitor also advertises its other supported modes.
        if let wl_output::Event::Mode {
            flags: WEnum::Value(flags),
            width,
            height,
            ..
        } = event
        {
            if flags.contains(wl_output::Mode::Current) {
                if let Some(o) = state.outputs.iter_mut().find(|o| o.name == *name) {
                    o.mode_w = width;
                    o.mode_h = height;
                }
            }
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(o) = state.outputs.iter_mut().find(|o| o.name == *name) {
            match event {
                zxdg_output_v1::Event::LogicalPosition { x, y } => {
                    o.logical_x = x;
                    o.logical_y = y;
                }
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    o.logical_w = width;
                    o.logical_h = height;
                }
                _ => {}
            }
        }
    }
}

// The manager has no events.
impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

pub struct KwinFakeInjector {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    fake: FakeInput,
    /// When output geometry was last re-read; throttles the per-event roundtrip (see `refresh_geometry`).
    last_refresh: Option<Instant>,
}

/// How often the fake_input backend re-reads output geometry from the compositor. Output add/remove
/// (a new session's virtual output) and live scale/resolution changes are infrequent, so a lazy
/// poll on the injector's own thread is plenty and adds at most one local-socket roundtrip twice a
/// second — versus a blocking roundtrip on every single mouse-move event.
const GEO_REFRESH: Duration = Duration::from_millis(500);

impl KwinFakeInjector {
    pub fn open() -> Result<Self> {
        let conn = Connection::connect_to_env()
            .context("connect to KWin Wayland (is WAYLAND_DISPLAY set to the KWin socket?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip")?;

        let fake = state.fake.clone().context(
            "KWin does not expose org_kde_kwin_fake_input to this client — install the host's \
             .desktop (io.unom.Punktfunk.Host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it (the grant is cached per-exe on first connect), or this is not a \
             KWin session",
        )?;
        // Authenticate (the legacy handshake; for an interface-authorized client KWin accepts it
        // without a dialog — same as krdpserver/krfb headless).
        fake.authenticate("Punktfunk".into(), "remote streaming input".into());
        queue
            .roundtrip(&mut state)
            .context("fake_input authenticate roundtrip")?;
        conn.flush().ok();

        // Settle output geometry (wl_output + xdg-output were bound during the registry roundtrip
        // above; their logical_size arrives on a follow-up roundtrip). Best-effort — falls back to
        // scale-1 mapping if xdg-output is absent.
        let mut injector = Self {
            conn,
            queue,
            state,
            fake,
            last_refresh: None,
        };
        injector.refresh_geometry();
        tracing::info!(
            outputs = injector.state.outputs.len(),
            "KWin fake_input ready (headless keyboard/mouse/touch — no portal)"
        );
        Ok(injector)
    }

    /// Re-read output geometry, throttled to [`GEO_REFRESH`]. A `roundtrip` both flushes any pending
    /// `get_xdg_output` requests and reads the geometry events back. A wl_output that *appeared* this
    /// round only gets its xdg_output created mid-dispatch, so its `logical_size` lands on a later
    /// roundtrip — keep going (bounded) until every output is settled.
    fn refresh_geometry(&mut self) {
        let now = Instant::now();
        if let Some(t) = self.last_refresh {
            if now.duration_since(t) < GEO_REFRESH {
                return;
            }
        }
        self.last_refresh = Some(now);
        for _ in 0..3 {
            if self.queue.roundtrip(&mut self.state).is_err() {
                return;
            }
            let pending =
                self.state.xdg_mgr.is_some() && self.state.outputs.iter().any(|o| o.logical_w == 0);
            if !pending {
                break;
            }
        }
    }

    /// Resolve the logical (global-compositor-space) rectangle to map a normalized client position
    /// into. Prefer the output whose physical mode matches the streamed `phys_w`×`phys_h` (the
    /// per-session virtual output); fall back to the sole output, then — if xdg-output is unavailable
    /// — to the streamed pixels at the origin (the pre-scaling behavior, correct at scale 1).
    fn logical_target(&self, phys_w: i32, phys_h: i32) -> (f64, f64, f64, f64) {
        let usable = || {
            self.state
                .outputs
                .iter()
                .filter(|o| o.logical_w > 0 && o.logical_h > 0)
        };
        let chosen = usable()
            .find(|o| o.mode_w == phys_w && o.mode_h == phys_h)
            .or_else(|| {
                let mut it = usable();
                match (it.next(), it.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            });
        match chosen {
            Some(o) => (
                o.logical_x as f64,
                o.logical_y as f64,
                o.logical_w as f64,
                o.logical_h as f64,
            ),
            None => (0.0, 0.0, phys_w as f64, phys_h as f64),
        }
    }
}

impl InputInjector for KwinFakeInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event.kind {
            InputKind::MouseMove => {
                self.fake.pointer_motion(event.x as f64, event.y as f64);
            }
            InputKind::MouseMoveAbs => {
                let w = ((event.flags >> 16) & 0xffff) as i32;
                let h = (event.flags & 0xffff) as i32;
                if w > 0 && h > 0 {
                    self.refresh_geometry();
                    let (lx, ly, lw, lh) = self.logical_target(w, h);
                    // Normalize in the streamed (physical) pixel space, then place inside the output's
                    // logical rectangle — so display scaling no longer offsets the cursor.
                    let nx = (event.x as f64 / w as f64).clamp(0.0, 1.0);
                    let ny = (event.y as f64 / h as f64).clamp(0.0, 1.0);
                    self.fake
                        .pointer_motion_absolute(lx + nx * lw, ly + ny * lh);
                }
            }
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                if let Some(btn) = gs_button_to_evdev(event.code) {
                    let st = u32::from(event.kind == InputKind::MouseButtonDown);
                    self.fake.button(btn, st);
                }
            }
            InputKind::MouseScroll => {
                // GameStream sends WHEEL_DELTA(120)-scaled units; a notch ≈ 15px. Vertical flips
                // sign on the Wayland axis, horizontal passes through — same as the wlr backend.
                let horizontal = event.code == SCROLL_HORIZONTAL;
                let axis = if horizontal {
                    AXIS_HORIZONTAL
                } else {
                    AXIS_VERTICAL
                };
                let notches = event.x as f64 / 120.0;
                let sign = if horizontal { 1.0 } else { -1.0 };
                self.fake.axis(axis, sign * notches * 15.0);
            }
            InputKind::KeyDown | InputKind::KeyUp => {
                // Raw evdev keycode; KWin resolves it through the session's own keymap (and tracks
                // modifier state itself, so no separate modifiers request is needed).
                if let Some(evdev) = vk_to_evdev(event.code as u8) {
                    let st = u32::from(event.kind == InputKind::KeyDown);
                    self.fake.keyboard_key(evdev as u32, st);
                } else {
                    tracing::debug!(vk = event.code, "unmapped VK keycode — dropped");
                }
            }
            // Touch: id = event.code, coords in the client surface w×h packed into flags (same
            // absolute mapping as MouseMoveAbs). Each event is its own frame.
            InputKind::TouchDown | InputKind::TouchMove => {
                let w = ((event.flags >> 16) & 0xffff) as i32;
                let h = (event.flags & 0xffff) as i32;
                if w > 0 && h > 0 {
                    self.refresh_geometry();
                    let (lx, ly, lw, lh) = self.logical_target(w, h);
                    let nx = (event.x as f64 / w as f64).clamp(0.0, 1.0);
                    let ny = (event.y as f64 / h as f64).clamp(0.0, 1.0);
                    let x = lx + nx * lw;
                    let y = ly + ny * lh;
                    if event.kind == InputKind::TouchDown {
                        self.fake.touch_down(event.code, x, y);
                    } else {
                        self.fake.touch_motion(event.code, x, y);
                    }
                    self.fake.touch_frame();
                }
            }
            InputKind::TouchUp => {
                self.fake.touch_up(event.code);
                self.fake.touch_frame();
            }
            // fake_input can only press host-layout keycodes — no committed-text path (the
            // HOST_CAP_TEXT_INPUT cap is not advertised on this backend).
            InputKind::TextInput => {}
            // Gamepads are injected through uinput, not the compositor.
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival => {}
        }
        // Surface protocol errors / disconnects, then push the batch to the compositor.
        self.queue
            .dispatch_pending(&mut self.state)
            .context("wayland dispatch")?;
        self.conn.flush().context("wayland flush")?;
        Ok(())
    }
}
