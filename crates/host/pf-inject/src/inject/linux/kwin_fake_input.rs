//! Headless keyboard/mouse/touch on KWin via `org_kde_kwin_fake_input`.
//!
//! KWin advertises this restricted global only to a client whose installed `.desktop`
//! lists it under `X-KDE-Wayland-Interfaces` (`io.unom.punktfunk.host.desktop`). Binding
//! is the grant — no RemoteDesktop portal and no "Allow remote control?" dialog — so it
//! works with nobody at the seat. Connect on the session `$WAYLAND_DISPLAY`. Keys are
//! raw Linux evdev codes; KWin maps them through the session keymap (no keymap upload).
//! Committed text goes as keysyms (v6, Plasma 6.5+), which KWin types in the session layout.
//! Absolute pointer/touch use *logical* compositor pixels (post scale). At scale ≠ 1 the
//! logical edge is `physical / scale`; a streamed pixel coordinate then lands `scale×`
//! too far toward the bottom-right. Track each output's logical rectangle via
//! `xdg-output` and map the normalized client position into it. The target is the head
//! [`crate::head_pick::pick`] names; a miss maps `w×h` pixels at the origin.
//!
//! Pin: install the host `.desktop` and re-login (KWin caches the grant per-exe).
//! Same path as `krdpserver`. See `docs-site/content/docs/(guide)/(desktops)/kde.md`.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]

use super::{InputEvent, InputInjector};
use crate::head_pick::{self, HeadFacts};
use crate::plan::{Abs, Op, Plan};
use crate::scroll::{ScrollBackend, ScrollMapper, ScrollOp};
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

// Inline scanner (no build.rs). Path is relative to CARGO_MANIFEST_DIR.
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

/// `keyboard_keysym` arrived at v6; bind no higher.
const MAX_VERSION: u32 = 6;

const AXIS_VERTICAL: u32 = 0;
const AXIS_HORIZONTAL: u32 = 1;

struct OutputTrack {
    /// Registry id; also dispatch user-data so events find this entry.
    name: u32,
    wl_output: WlOutput,
    xdg_output: Option<ZxdgOutputV1>,
    geo: Geo,
}

/// `wl_output.name`, physical mode, and logical rectangle (abs coords).
/// `logical_w == 0` until xdg-output reports size.
#[derive(Default)]
struct Geo {
    output_name: Option<String>,
    mode_w: i32,
    mode_h: i32,
    logical_x: i32,
    logical_y: i32,
    logical_w: i32,
    logical_h: i32,
}

impl Geo {
    /// This head for [`head_pick::pick`]; `None` until xdg-output reports a logical size.
    fn facts(&self) -> Option<HeadFacts<'_>> {
        let size = |w: i32, h: i32| (w > 0 && h > 0).then(|| (w as u32, h as u32));
        let (logical_w, logical_h) = size(self.logical_w, self.logical_h)?;
        Some(HeadFacts {
            name: self.output_name.as_deref(),
            x: self.logical_x,
            y: self.logical_y,
            logical_w,
            logical_h,
            mode: size(self.mode_w, self.mode_h),
            mapping_id: None,
        })
    }
}

#[derive(Default)]
struct State {
    fake: Option<FakeInput>,
    xdg_mgr: Option<ZxdgOutputManagerV1>,
    outputs: Vec<OutputTrack>,
}

impl State {
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
                    // `mode` is v1, `name` v4; bind ≤ the proxy max (4).
                    let wl_output: WlOutput = registry.bind(name, version.min(4), qh, name);
                    let mut o = OutputTrack {
                        name,
                        wl_output,
                        xdg_output: None,
                        geo: Geo::default(),
                    };
                    if let Some(mgr) = state.xdg_mgr.clone() {
                        State::ensure_xdg_output(&mut o, &mgr, qh);
                    }
                    state.outputs.push(o);
                }
                "zxdg_output_manager_v1" => {
                    let mgr: ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
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
                        // The connection outlives every virtual head: release its proxy too.
                        if o.wl_output.version() >= 3 {
                            o.wl_output.release();
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
        let Some(o) = state.outputs.iter_mut().find(|o| o.name == *name) else {
            return;
        };
        match event {
            // A monitor also advertises non-current modes; only Current is the live size.
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => {
                o.geo.mode_w = width;
                o.geo.mode_h = height;
            }
            wl_output::Event::Name { name } => o.geo.output_name = Some(name),
            _ => {}
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
                    o.geo.logical_x = x;
                    o.geo.logical_y = y;
                }
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    o.geo.logical_w = width;
                    o.geo.logical_h = height;
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
    last_refresh: Option<Instant>,
    /// Normalized-scroll lowering onto the bare `axis` click channel.
    scroll: ScrollMapper,
    /// What this device holds, released before it goes ([`Drop`]).
    plan: Plan,
}

/// Cap geometry roundtrips at 2 Hz. A roundtrip on every mouse-move would stall the control path.
const GEO_REFRESH: Duration = Duration::from_millis(500);

/// The session's KWin socket, else `$WAYLAND_DISPLAY`.
fn connect() -> Result<Connection> {
    match pf_host_config::session::wayland_stream() {
        Some(Ok(stream)) => Connection::from_socket(stream),
        Some(Err(_)) => Err(wayland_client::ConnectError::NoCompositor),
        None => Connection::connect_to_env(),
    }
    .context("connect to KWin Wayland")
}

/// Whether KWin offers this host `keyboard_keysym` (fake_input v6), so committed text types.
/// Every Welcome asks, and the probe is a connect plus a blocking roundtrip, so KWin's first
/// answer is kept. A failed connect keeps nothing: the next Welcome asks again.
pub(crate) fn keysym_supported() -> bool {
    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    static ANSWER: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if let Some(&answer) = ANSWER.get() {
        return answer;
    }
    struct Probe;
    impl Dispatch<WlRegistry, GlobalListContents> for Probe {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wl_registry::Event,
            _: &GlobalListContents,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    let Ok(conn) = connect() else { return false };
    let Ok((globals, _queue)) = registry_queue_init::<Probe>(&conn) else {
        return false;
    };
    let answer = globals.contents().with_list(|list| {
        list.iter()
            .any(|g| g.interface == "org_kde_kwin_fake_input" && g.version >= MAX_VERSION)
    });
    *ANSWER.get_or_init(|| answer)
}

/// The keysym that types `cp`. `None` for a control character (Enter and Tab ride the VK path).
fn text_keysym(cp: u32) -> Option<u32> {
    char::from_u32(cp).filter(|c| !c.is_control())?;
    let sym = xkbcommon::xkb::utf32_to_keysym(cp).raw();
    (sym != xkbcommon::xkb::keysyms::KEY_NoSymbol).then_some(sym)
}

impl KwinFakeInjector {
    pub fn open() -> Result<Self> {
        let conn = connect()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip")?;

        let fake = state.fake.clone().context(
            "KWin does not expose org_kde_kwin_fake_input to this client — install the host's \
             .desktop (io.unom.punktfunk.host.desktop, X-KDE-Wayland-Interfaces) and re-login so \
             KWin authorizes it (the grant is cached per-exe on first connect), or this is not a \
             KWin session",
        )?;
        // Legacy handshake; an interface-authorized client is accepted with no dialog.
        fake.authenticate("Punktfunk".into(), "remote streaming input".into());
        queue
            .roundtrip(&mut state)
            .context("fake_input authenticate roundtrip")?;
        conn.flush().ok();

        // `logical_size` arrives after the registry roundtrip. Falls back to scale-1 if xdg-output is absent.
        let mut injector = Self {
            conn,
            queue,
            state,
            fake,
            last_refresh: None,
            scroll: ScrollMapper::new(ScrollBackend::Kwin),
            plan: Plan::default(),
        };
        injector.refresh_geometry();
        tracing::info!(
            outputs = injector.state.outputs.len(),
            "KWin fake_input ready (headless keyboard/mouse/touch — no portal)"
        );
        Ok(injector)
    }

    /// Throttled to [`GEO_REFRESH`]. A wl_output that appeared this round only gets
    /// `xdg_output` mid-dispatch, so `logical_size` lands on a later roundtrip — loop
    /// (bounded) until every output has a size.
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
            let pending = self.state.xdg_mgr.is_some()
                && self.state.outputs.iter().any(|o| o.geo.logical_w == 0);
            if !pending {
                break;
            }
        }
    }

    /// Where `abs` lands in logical compositor space: its fraction of the [`head_pick::pick`]ed
    /// head, else of `w×h` pixels at the origin (correct at scale 1).
    fn place(&mut self, abs: &Abs) -> (f64, f64) {
        self.refresh_geometry();
        let heads: Vec<HeadFacts> = self
            .state
            .outputs
            .iter()
            .filter_map(|o| o.geo.facts())
            .collect();
        let (lx, ly, lw, lh) = match head_pick::pick(
            &heads,
            &crate::stream_target_for(abs.screen),
            Some((abs.w, abs.h)),
        ) {
            Some(i) => {
                let h = heads[i];
                let (x, y) = (f64::from(h.x), f64::from(h.y));
                (x, y, f64::from(h.logical_w), f64::from(h.logical_h))
            }
            None => (0.0, 0.0, f64::from(abs.w), f64::from(abs.h)),
        };
        let (nx, ny) = abs.normalized();
        (lx + nx * lw, ly + ny * lh)
    }

    /// Emit one planned op. Keys are evdev codes; KWin owns the keymap and modifier state, so
    /// there is no modifiers request. Text is a keysym tap. A touch is one frame per event.
    fn emit(&mut self, op: Op) {
        match op {
            Op::Motion { dx, dy } => self.fake.pointer_motion(dx.into(), dy.into()),
            Op::Absolute(abs) => {
                let (x, y) = self.place(&abs);
                self.fake.pointer_motion_absolute(x, y);
            }
            Op::Button { evdev, down } => self.fake.button(evdev, down.into()),
            Op::Key { evdev, down } => self.fake.keyboard_key(evdev.into(), down.into()),
            Op::TouchDown { id, at } => {
                let (x, y) = self.place(&at);
                self.fake.touch_down(id, x, y);
                self.fake.touch_frame();
            }
            Op::TouchMove { id, at } => {
                let (x, y) = self.place(&at);
                self.fake.touch_motion(id, x, y);
                self.fake.touch_frame();
            }
            Op::TouchUp { id } => {
                self.fake.touch_up(id);
                self.fake.touch_frame();
            }
            // Below v6 there is no keysym request, and the host offers no text.
            Op::Text(cp) => {
                if let Some(sym) = text_keysym(cp).filter(|_| self.fake.version() >= MAX_VERSION) {
                    self.fake.keyboard_keysym(sym, 1);
                    self.fake.keyboard_keysym(sym, 0);
                }
            }
        }
    }

    /// Execute a normalized-scroll plan on the bare `axis` channel: the only
    /// op a KWin plan emits is a click-priced axis value.
    fn inject_scroll(&mut self, event: &InputEvent) {
        for op in self.scroll.plan(event) {
            if let ScrollOp::Continuous { horizontal, value } = op {
                let axis = if horizontal {
                    AXIS_HORIZONTAL
                } else {
                    AXIS_VERTICAL
                };
                self.fake.axis(axis, value);
            }
        }
    }
}

impl Drop for KwinFakeInjector {
    /// Release what the device still holds before it goes; a key or button KWin keeps
    /// pressed for a client that vanished is a stuck one.
    fn drop(&mut self) {
        for op in self.plan.release() {
            self.emit(op);
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
    }
}

impl InputInjector for KwinFakeInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        if matches!(event.kind, InputKind::MouseScroll | InputKind::Scroll) {
            // Legacy and normalized scroll lower through the shared mapper onto the bare axis;
            // the plan carries click-priced units and the Wayland vertical sign already applied.
            self.inject_scroll(event);
        } else if let Some(op) = self.plan.step(event) {
            self.emit(op);
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .context("wayland dispatch")?;
        self.conn.flush().context("wayland flush")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::text_keysym;

    #[test]
    fn text_maps_to_layout_free_keysyms() {
        assert_eq!(text_keysym('"' as u32), Some(0x22));
        assert_eq!(text_keysym('A' as u32), Some(0x41));
        assert_eq!(text_keysym('é' as u32), Some(0xe9));
        assert_eq!(text_keysym('€' as u32), Some(0x20ac));
        assert_eq!(text_keysym('😀' as u32), Some(0x0100_0000 | 0x1f600));
        assert_eq!(text_keysym('\n' as u32), None);
        assert_eq!(text_keysym(0xd800), None);
    }
}
