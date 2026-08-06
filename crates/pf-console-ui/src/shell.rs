//! The console shell: the screen stack, the push/pop entrance/exit choreography, the
//! chrome every screen shares (pinned title, controller chip, hint bar), and the modal
//! overlays (connecting, waking, toasts). Screens draw CONTENT; the shell makes them
//! read — and move — as one coherent console.
//!
//! Transitions: a push slides the incoming screen up out of a fade while the outgoing
//! one recedes; a pop mirrors it. One eased 0→1 progress drives both layers (0.26 s,
//! ease-out cubic — the WinUI shell's entrance feel), each composited through a
//! `save_layer_alpha` so a screen fades as a unit, never element by element. The
//! backdrop crossfades in parallel when the screens disagree (aurora ↔ form).

use crate::anim::Progress;
use crate::glyphs::GlyphStyle;
use crate::library::{mesh_sksl, palette, LibraryShared};
use crate::model::{ConsoleBus, ConsoleCmd, ConsoleShared, HostRow, PairPhase, WakeStatus};
use crate::screens::{Bg, ConnectIntent, Ctx, Nav, Outbox, Screen};
use anyhow::{anyhow, Result};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse, PadInfo};
use pf_client_core::trust;
use pf_presenter::overlay::OverlayAction;
use skia_safe::{Canvas, Color4f, Data, Paint, Rect, RuntimeEffect};
use std::collections::VecDeque;
use std::time::Instant;

mod overlays;
mod render;

const TRANSITION_S: f64 = 0.26;
/// Chrome bands (design units): the pinned title above, hints below.
const TOP_BAND: f64 = 64.0;
const BOTTOM_BAND: f64 = 86.0;

enum Motion {
    None,
    Push(Progress),
    Pop { leaving: Box<Screen>, t: Progress },
}

struct Toast {
    text: String,
    at: f64,
}

struct Connecting {
    title: String,
    canceling: bool,
    appear: f64,
    /// A request-access wait (parked on the host until the operator approves) — the
    /// takeover reads "Waiting for approval" rather than "Connecting".
    request_access: bool,
}

/// What the session binary hands the shell at construction.
pub struct ConsoleOptions {
    /// The machine's hostname — the default device name pairing registers.
    pub device_name: String,
    /// Steam Deck: Steam's keyboard types (SDL text input); ours never draws.
    pub deck: bool,
}

pub(crate) struct Shell {
    stack: Vec<Screen>,
    motion: Motion,
    console: ConsoleShared,
    library: LibraryShared,
    bus: ConsoleBus,
    actions: VecDeque<OverlayAction>,
    settings: trust::Settings,
    hosts: Vec<HostRow>,
    hosts_gen: u64,
    device_name: String,
    deck: bool,
    pub(crate) in_stream: bool,
    connecting: Option<Connecting>,
    wake: Option<WakeStatus>,
    /// True while `wake` is the shell's own optimistic placeholder — raised the instant a
    /// screen queues `ConsoleCmd::Wake` (see [`Self::apply`]), before the service thread has
    /// round-tripped its first real `WakeStatus` (~100 ms–1 s). `sync` must not clear the
    /// placeholder in that window, or navigation would race the wake ungated (the "pressed A,
    /// cursor drifted to Add Host, then got thrust into the stream" bug).
    wake_optimistic: bool,
    toast: Option<Toast>,
    mesh: RuntimeEffect,
    /// The `ui_palette` the compiled `mesh` bakes. The settings screen can change the palette
    /// mid-frame-loop, so [`Self::sync`] recompiles when this falls out of step — the backdrop
    /// re-colours under the cursor as the row is stepped, which is the whole point of putting
    /// the picker on a screen the backdrop is behind.
    mesh_palette: String,
    /// The palette's ground × 0.4 — the calm lift, precomputed with `mesh`. Chosen so
    /// `col*0.6 + lift` leaves the ground EXACTLY where it was and pulls the bright pools down
    /// to it: the form screens lose the launcher's contrast, not its colour.
    mesh_lift: [f32; 3],
    /// The backdrop's scrim under this palette: rgb = what the vignette and scrims tend
    /// toward (black on a dark field, white on a pale one), a = how hard. Kept with the ink.
    mesh_scrim: [f32; 4],
    /// The text/accent/glass the palette calls for, published to the whole crate once per
    /// frame (see [`crate::theme::set_ink`]).
    ink: crate::theme::Ink,
    /// 0 = launcher aurora, 1 = the calm form field — chased, so the backdrop settles into
    /// (or out of) calm alongside the screen transition.
    bg_mix: f64,
    glyphs: GlyphStyle,
    chip: Option<String>,
    pads: Vec<PadInfo>,
    t0: Instant,
    last_frame: Option<Instant>,
}

impl Shell {
    pub(crate) fn new(
        console: ConsoleShared,
        library: LibraryShared,
        bus: ConsoleBus,
        opts: ConsoleOptions,
        stack: Vec<Screen>,
    ) -> Result<Shell> {
        anyhow::ensure!(!stack.is_empty(), "the console needs a root screen");
        let settings = trust::Settings::load();
        let (mesh, mesh_lift, mesh_scrim, ink) = build_mesh(&settings.ui_palette)?;
        let bg_mix = match stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
        Ok(Shell {
            stack,
            motion: Motion::None,
            console,
            library,
            bus,
            actions: VecDeque::new(),
            mesh_palette: settings.ui_palette.clone(),
            settings,
            hosts: Vec::new(),
            hosts_gen: u64::MAX,
            device_name: opts.device_name,
            deck: opts.deck,
            in_stream: false,
            connecting: None,
            wake: None,
            wake_optimistic: false,
            toast: None,
            mesh,
            mesh_lift,
            mesh_scrim,
            ink,
            bg_mix,
            glyphs: GlyphStyle::Keyboard,
            chip: None,
            pads: Vec::new(),
            t0: Instant::now(),
            last_frame: None,
        })
    }

    fn t(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    pub(crate) fn editing(&self) -> bool {
        !self.in_stream
            && self.connecting.is_none()
            && self.stack.last().is_some_and(Screen::editing)
    }

    pub(crate) fn take_action(&mut self) -> Option<OverlayAction> {
        self.actions.pop_front()
    }

    // --- Session lifecycle edges (from the overlay's `session_phase`) --------------------

    pub(crate) fn set_connecting(&mut self, title: Option<String>) {
        match title {
            Some(title) => {
                self.connecting = Some(Connecting {
                    title,
                    canceling: false,
                    appear: 0.0,
                    request_access: false,
                })
            }
            None => self.connecting = None,
        }
    }

    pub(crate) fn session_failed(&mut self, msg: &str) {
        self.connecting = None;
        self.in_stream = false;
        self.show_toast(format!("Couldn't connect — {msg}"));
    }

    pub(crate) fn session_streaming(&mut self) {
        self.connecting = None;
        self.in_stream = true;
    }

    pub(crate) fn session_ended(&mut self, reason: Option<&str>) {
        self.connecting = None;
        self.in_stream = false;
        if let Some(reason) = reason {
            self.show_toast(format!("Session ended — {reason}"));
        }
    }

    fn show_toast(&mut self, text: String) {
        self.toast = Some(Toast { text, at: self.t() });
    }

    // --- Model sync (hosts, pairing, wake) — before input and before render --------------

    fn sync(&mut self) {
        // The settings screen writes `ui_palette` straight into `self.settings`; recompiling
        // here is what makes the backdrop re-colour live under the row being stepped. A
        // rejected compile keeps the palette that IS drawing — the field never goes black
        // because someone picked a colour.
        if self.settings.ui_palette != self.mesh_palette {
            match build_mesh(&self.settings.ui_palette) {
                Ok((mesh, lift, scrim, ink)) => {
                    self.mesh = mesh;
                    self.mesh_lift = lift;
                    self.mesh_scrim = scrim;
                    self.ink = ink;
                    self.mesh_palette = self.settings.ui_palette.clone();
                }
                Err(e) => {
                    tracing::warn!(
                        "console: {} palette rejected: {e}",
                        self.settings.ui_palette
                    );
                    self.mesh_palette = self.settings.ui_palette.clone();
                }
            }
        }
        if self.console.hosts_gen() != self.hosts_gen {
            (self.hosts, self.hosts_gen) = self.console.hosts_snapshot();
        }

        let pair = self.console.pair();
        match &pair {
            PairPhase::Idle => {}
            PairPhase::Paired { key } => {
                let name = self
                    .hosts
                    .iter()
                    .find(|h| &h.key == key)
                    .map_or_else(|| "the host".to_string(), |h| h.name.clone());
                self.show_toast(format!("Paired with {name}"));
                self.console.set_pair(PairPhase::Idle);
                if matches!(self.stack.last(), Some(Screen::Pair(_))) {
                    self.apply_nav(Nav::Pop);
                }
            }
            phase => {
                if let Some(Screen::Pair(p)) = self.stack.last_mut() {
                    p.apply_phase(phase);
                }
                if matches!(phase, PairPhase::Failed(_)) {
                    self.console.set_pair(PairPhase::Idle);
                }
            }
        }

        match self.console.wake() {
            Some(w) => {
                self.wake_optimistic = false;
                self.wake = Some(w);
            }
            // No service status yet: keep an optimistic placeholder alive — clearing it here
            // would reopen the ungated window it exists to close.
            None if !self.wake_optimistic => self.wake = None,
            None => {}
        }
        if let Some(w) = &self.wake {
            if w.online {
                // Awake: stop the wake loop, and connect if that's what A meant.
                let intent = w.then_connect.then(|| {
                    self.hosts
                        .iter()
                        .find(|h| h.key == w.key)
                        .map(|h| ConnectIntent {
                            addr: h.addr.clone(),
                            port: h.port,
                            fp_hex: h.fp_hex.clone(),
                            launch: None,
                            // A wake started from a pinned card carries its profile
                            // through to the connect (the row's key found it again).
                            title: match &h.pin {
                                Some(p) => format!("{} · {}", h.name, p.name),
                                None => h.name.clone(),
                            },
                            request_access: false,
                            profile: h.pin.as_ref().map(|p| p.id.clone()),
                        })
                });
                self.bus.send(ConsoleCmd::CancelWake);
                self.wake = None;
                if let Some(Some(intent)) = intent {
                    self.start_connect(intent);
                    // The wake takeover was already full-screen; skip the connect fade-in so the
                    // Waking → Connecting handoff is seamless (no flash of the home behind).
                    if let Some(c) = &mut self.connecting {
                        c.appear = 1.0;
                    }
                }
            }
        }
    }

    fn start_connect(&mut self, intent: ConnectIntent) {
        self.set_connecting(Some(intent.title.clone()));
        if let Some(c) = &mut self.connecting {
            c.request_access = intent.request_access;
        }
        self.actions.push_back(OverlayAction::Launch {
            addr: intent.addr,
            port: intent.port,
            fp_hex: intent.fp_hex,
            launch: intent.launch,
            title: intent.title,
            request_access: intent.request_access,
            profile: intent.profile,
        });
    }

    // --- Input ---------------------------------------------------------------------------

    pub(crate) fn handle_menu(&mut self, ev: MenuEvent) -> Option<MenuPulse> {
        self.sync();
        // Modal precedence: the connect card, then the wake card, then the screens.
        if let Some(c) = &mut self.connecting {
            if ev == MenuEvent::Back && !c.canceling {
                c.canceling = true;
                self.actions.push_back(OverlayAction::CancelConnect);
                return Some(MenuPulse::Confirm);
            }
            return None;
        }
        if let Some(w) = &self.wake {
            match ev {
                MenuEvent::Back => {
                    self.bus.send(ConsoleCmd::CancelWake);
                    self.wake = None;
                    self.wake_optimistic = false;
                    return Some(MenuPulse::Confirm);
                }
                MenuEvent::Confirm if w.timed_out => {
                    self.bus.send(ConsoleCmd::Wake {
                        key: w.key.clone(),
                        then_connect: w.then_connect,
                    });
                    return Some(MenuPulse::Confirm);
                }
                _ => return None,
            }
        }
        // Mid-transition input is dropped — 0.26 s, and it keeps a double-tapped A
        // from pushing two screens.
        if !matches!(self.motion, Motion::None) {
            return None;
        }

        let mut fx = Outbox::default();
        let pulse = {
            let mut ctx = Ctx {
                hosts: &self.hosts,
                library: &self.library,
                settings: &mut self.settings,
                pads: &self.pads,
                deck: self.deck,
                device_name: &self.device_name,
                t: self.t0.elapsed().as_secs_f64(),
            };
            self.stack
                .last_mut()
                .expect("non-empty stack")
                .menu(ev, &mut ctx, &mut fx)
        };
        self.apply(fx);
        pulse
    }

    /// The keyboard fallback — the console is fully drivable with no pad. Arrows and
    /// Enter/Esc map onto menu events; Y/X mirror the pad's Secondary/Tertiary
    /// (suppressed while editing, where letters are text).
    pub(crate) fn key(&mut self, sc: sdl3::keyboard::Scancode, repeat: bool) -> bool {
        use sdl3::keyboard::Scancode as S;
        if self.editing() {
            if let Some(top) = self.stack.last_mut() {
                if top.edit_key(sc) {
                    return true;
                }
            }
            // Arrows etc. still drive the OSK grid below.
        }
        let editing = self.stack.last().is_some_and(Screen::editing);
        let ev = match sc {
            S::Left => MenuEvent::Move(MenuDir::Left),
            S::Right => MenuEvent::Move(MenuDir::Right),
            S::Up => MenuEvent::Move(MenuDir::Up),
            S::Down => MenuEvent::Move(MenuDir::Down),
            S::Return | S::KpEnter | S::Space if !repeat => MenuEvent::Confirm,
            S::Escape | S::Backspace if !repeat => MenuEvent::Back,
            S::PageUp if !repeat => MenuEvent::JumpBack,
            S::PageDown if !repeat => MenuEvent::JumpForward,
            S::Y if !repeat && !editing => MenuEvent::Secondary,
            S::X if !repeat && !editing => MenuEvent::Tertiary,
            _ => return false,
        };
        self.handle_menu(ev); // no pad to pulse
        true
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        if let Some(top) = self.stack.last_mut() {
            top.text_input(text);
        }
    }

    fn apply(&mut self, fx: Outbox) {
        for cmd in fx.cmds {
            // An input-initiated wake must gate input in the SAME call, exactly like
            // `start_connect` gates via `connecting`: the service's first WakeStatus is
            // ~100 ms–1 s away, and until it lands the screen would keep navigating —
            // then the arriving status freezes the UI wherever the cursor drifted, with
            // the "Waking…" card never shown for a fast wake. Raise it optimistically;
            // `sync` lets the service's real status supersede this placeholder.
            if let ConsoleCmd::Wake { key, then_connect } = &cmd {
                let name = self
                    .hosts
                    .iter()
                    .find(|h| &h.key == key)
                    .map(|h| h.name.clone())
                    .unwrap_or_default();
                self.wake = Some(WakeStatus {
                    key: key.clone(),
                    name,
                    seconds: 0,
                    timed_out: false,
                    online: false,
                    then_connect: *then_connect,
                });
                self.wake_optimistic = true;
            }
            self.bus.send(cmd);
        }
        if let Some(text) = fx.toast {
            self.show_toast(text);
        }
        if let Some(intent) = fx.connect {
            self.start_connect(intent);
        }
        if let Some(nav) = fx.nav {
            self.apply_nav(nav);
        }
    }

    fn apply_nav(&mut self, nav: Nav) {
        match nav {
            Nav::Push(screen) => {
                self.stack.push(*screen);
                self.motion = Motion::Push(Progress::new(TRANSITION_S));
            }
            Nav::Pop => {
                if self.stack.len() > 1 {
                    let leaving = self.stack.pop().expect("len > 1");
                    self.motion = Motion::Pop {
                        leaving: Box::new(leaving),
                        t: Progress::new(TRANSITION_S),
                    };
                } else {
                    // Popping the root quits the console (B at home).
                    self.actions.push_back(OverlayAction::Quit);
                }
            }
        }
    }

    /// The living backdrop. `calm` 0 = the launcher's aurora, 1 = the quiet field the form
    /// screens sit on; the shell chases it, so there is only ever ONE backdrop pass — the
    /// former aurora-over-static-form crossfade is now a single uniform.
    fn draw_aurora(&self, canvas: &Canvas, w: f64, h: f64, t: f64, calm: f64) {
        // Laid out to match the SkSL block: u_res (float2), u_tc (float2), u_lift (float4),
        // u_scrim (float4).
        let uniforms: [f32; 12] = [
            w as f32,
            h as f32,
            t as f32,
            calm as f32,
            self.mesh_lift[0],
            self.mesh_lift[1],
            self.mesh_lift[2],
            0.0,
            self.mesh_scrim[0],
            self.mesh_scrim[1],
            self.mesh_scrim[2],
            self.mesh_scrim[3],
        ];
        // SAFETY: `uniforms` is a local `[f32; 12]` — exactly 48 bytes — and `f32` has no padding
        // or invalid bit patterns, so reading it as bytes is sound; the slice is copied by
        // `Data::new_copy` before `uniforms` goes out of scope.
        let bytes = unsafe { std::slice::from_raw_parts(uniforms.as_ptr().cast::<u8>(), 48) };
        match self.mesh.make_shader(Data::new_copy(bytes), &[], None) {
            Some(shader) => {
                let mut paint = Paint::default();
                paint.set_shader(shader);
                canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &paint);
            }
            None => {
                canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
            }
        }
    }
}

/// Compile the mesh shader for a palette and resolve everything else that palette decides:
/// the calm lift, the scrim direction, and the ink the whole UI draws with.
/// `uniform_size` is checked rather than assumed: the byte buffer [`Shell::draw_aurora`]
/// hands Skia is hand-packed, and a silent layout change would feed the field garbage
/// instead of failing.
type MeshLook = (RuntimeEffect, [f32; 3], [f32; 4], crate::theme::Ink);

fn build_mesh(palette_id: &str) -> Result<MeshLook> {
    let p = palette(palette_id);
    let colors = p.mesh_colors();
    let effect = RuntimeEffect::make_for_shader(mesh_sksl(&colors), None)
        .map_err(|e| anyhow!("mesh-gradient SkSL: {e}"))?;
    anyhow::ensure!(
        effect.uniform_size() == 48,
        "mesh uniform block is {} bytes, expected 48 (u_res, u_tc, u_lift, u_scrim)",
        effect.uniform_size()
    );
    let ink = crate::theme::Ink::of(p);
    let g = p.ground;
    Ok((
        effect,
        [(g.0 * 0.4) as f32, (g.1 * 0.4) as f32, (g.2 * 0.4) as f32],
        [ink.scrim.r, ink.scrim.g, ink.scrim.b, ink.scrim.a],
        ink,
    ))
}

#[cfg(test)]
mod tests;
