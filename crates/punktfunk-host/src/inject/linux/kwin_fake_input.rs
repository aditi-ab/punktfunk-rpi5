//! Headless input injection on KWin via the privileged `org_kde_kwin_fake_input` protocol — the
//! exact path KDE's own headless RDP server (`krdpserver`) uses. KWin advertises this restricted
//! global only to a client authorized through its installed `.desktop` `X-KDE-Wayland-Interfaces`
//! (we ship `io.unom.Punktfunk.Host.desktop`, which lists `org_kde_kwin_fake_input` alongside
//! `zkde_screencast_unstable_v1`). Binding the global IS the authorization, so injection needs **no
//! RemoteDesktop portal and no "Allow remote control?" dialog** — it works with no user present,
//! which the libei/portal path cannot. We connect as an ordinary Wayland client on the KWin session's
//! `$WAYLAND_DISPLAY` and translate events into fake-input requests; keyboard keys are raw Linux
//! evdev codes that KWin resolves through the session's own keymap (no keymap upload, unlike the wlr
//! virtual-keyboard path), and absolute pointer/touch coordinates are global compositor space — which
//! on a headless box (single per-session virtual output at the origin, scale 1) equals the streamed
//! output's pixels.

#![allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use super::{gs_button_to_evdev, vk_to_evdev, InputEvent, InputInjector};
use anyhow::{Context, Result};
use punktfunk_core::input::InputKind;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

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

/// Registry-bound globals (the Wayland dispatch state).
#[derive(Default)]
struct State {
    fake: Option<FakeInput>,
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
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "org_kde_kwin_fake_input" {
                state.fake = Some(registry.bind(name, version.min(MAX_VERSION), qh, ()));
            }
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

pub struct KwinFakeInjector {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    fake: FakeInput,
}

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
        fake.authenticate("punktfunk".into(), "remote streaming input".into());
        queue
            .roundtrip(&mut state)
            .context("fake_input authenticate roundtrip")?;
        conn.flush().ok();

        tracing::info!("KWin fake_input ready (headless keyboard/mouse/touch — no portal)");
        Ok(Self {
            conn,
            queue,
            state,
            fake,
        })
    }
}

impl InputInjector for KwinFakeInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        match event.kind {
            InputKind::MouseMove => {
                self.fake.pointer_motion(event.x as f64, event.y as f64);
            }
            InputKind::MouseMoveAbs => {
                let w = (event.flags >> 16) & 0xffff;
                let h = event.flags & 0xffff;
                if w > 0 && h > 0 {
                    let x = event.x.clamp(0, w as i32) as f64;
                    let y = event.y.clamp(0, h as i32) as f64;
                    self.fake.pointer_motion_absolute(x, y);
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
                let w = (event.flags >> 16) & 0xffff;
                let h = event.flags & 0xffff;
                if w > 0 && h > 0 {
                    let x = event.x.clamp(0, w as i32) as f64;
                    let y = event.y.clamp(0, h as i32) as f64;
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
            // Gamepads are injected through uinput, not the compositor.
            InputKind::GamepadButton | InputKind::GamepadAxis => {}
        }
        // Surface protocol errors / disconnects, then push the batch to the compositor.
        self.queue
            .dispatch_pending(&mut self.state)
            .context("wayland dispatch")?;
        self.conn.flush().context("wayland flush")?;
        Ok(())
    }
}
