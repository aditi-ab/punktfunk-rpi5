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

use crate::anim::{approach, ease_out_cubic, Progress};
use crate::glyphs::{hint_bar, GlyphStyle, Hint, HintKey};
use crate::library::{mesh_sksl, LibraryShared};
use crate::model::{ConsoleBus, ConsoleCmd, ConsoleShared, HostRow, PairPhase, WakeStatus};
use crate::screens::{Bg, ConnectIntent, Ctx, Nav, Outbox, Screen};
use crate::theme::{white, Fonts, PanelStroke, DIM, W, WHITE};
use anyhow::{anyhow, Result};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse, PadInfo};
use pf_client_core::trust;
use pf_presenter::overlay::OverlayAction;
use skia_safe::{
    gradient_shader, Canvas, Color4f, Data, Paint, Point, Rect, RuntimeEffect, TileMode,
};
use std::collections::VecDeque;
use std::time::Instant;

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
    /// 0 = aurora, 1 = form — chased, so backdrops crossfade with the transition.
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
        let mesh = RuntimeEffect::make_for_shader(mesh_sksl(), None)
            .map_err(|e| anyhow!("mesh-gradient SkSL: {e}"))?;
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
            settings: trust::Settings::load(),
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
                            title: h.name.clone(),
                            request_access: false,
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

    // --- Rendering -------------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        width: u32,
        height: u32,
        fonts: &Fonts,
        pad: Option<&str>,
        pad_pref: Option<punktfunk_core::config::GamepadPref>,
        pads: &[PadInfo],
    ) {
        let now = Instant::now();
        let dt = self
            .last_frame
            .replace(now)
            .map_or(1.0 / 60.0, |t| (now - t).as_secs_f64().clamp(0.0, 0.05));
        self.sync();
        self.pads = pads.to_vec();
        self.glyphs = GlyphStyle::from_pref(pad_pref);
        self.chip = Some(pad.map_or_else(
            || "No controller — keyboard works too".to_string(),
            str::to_owned,
        ));

        let (w, h) = (f64::from(width), f64::from(height));
        let k = (h / 800.0).clamp(0.75, 3.0);
        let t = self.t();

        // Advance the transition; a finished pop finally drops its leaving screen.
        let motion_p = match &mut self.motion {
            Motion::None => None,
            Motion::Push(p) => {
                p.advance(dt);
                let v = p.value();
                if p.done() {
                    self.motion = Motion::None;
                    None
                } else {
                    Some(v)
                }
            }
            Motion::Pop { t, .. } => {
                t.advance(dt);
                let v = t.value();
                if t.done() {
                    self.motion = Motion::None;
                    None
                } else {
                    Some(v)
                }
            }
        };

        // Backdrop crossfade follows the top screen.
        let bg_target = match self.stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
        self.bg_mix = approach(self.bg_mix, bg_target, dt, 0.12);
        if (self.bg_mix - bg_target).abs() < 0.005 {
            self.bg_mix = bg_target;
        }
        if self.bg_mix < 1.0 {
            self.draw_aurora(canvas, w, h, t);
        } else {
            canvas.clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
        }
        if self.bg_mix > 0.0 {
            canvas.save_layer_alpha_f(None, self.bg_mix as f32);
            crate::theme::draw_form_background(canvas, w, h);
            canvas.restore();
        }

        // The screens, through the transition choreography.
        let content = Rect::from_ltrb(
            0.0,
            (TOP_BAND * k) as f32,
            w as f32,
            (h - BOTTOM_BAND * k) as f32,
        );
        // One paint recipe per layer: (alpha, slide, scale). Everything below borrows
        // disjoint fields of `self` per call, so the borrow checker stays happy.
        let mut env = LayerEnv {
            canvas,
            w,
            h,
            content,
            k,
            dt,
            fonts,
            hosts: &self.hosts,
            library: &self.library,
            settings: &mut self.settings,
            pads: &self.pads,
            deck: self.deck,
            device_name: &self.device_name,
            t,
            glyphs: self.glyphs,
            // A modal card owns B/A while it's up — the screen's legend would lie.
            show_hints: self.connecting.is_none() && self.wake.is_none(),
        };
        match (&mut self.motion, motion_p) {
            (Motion::Push(_), Some(raw)) => {
                let p = ease_out_cubic(raw);
                let n = self.stack.len();
                // Outgoing recedes underneath…
                if n >= 2 {
                    let (below, top) = self.stack.split_at_mut(n - 1);
                    env.paint(&mut below[n - 2], 1.0 - p, 0.0, 1.0 - 0.04 * p);
                    // …while the incoming slides up out of a fade.
                    env.paint(&mut top[0], p, 36.0 * k * (1.0 - p), 0.985 + 0.015 * p);
                } else {
                    env.paint(
                        &mut self.stack[0],
                        p,
                        36.0 * k * (1.0 - p),
                        0.985 + 0.015 * p,
                    );
                }
            }
            (Motion::Pop { leaving, .. }, Some(raw)) => {
                let p = ease_out_cubic(raw);
                // The revealed screen grows back in…
                let n = self.stack.len();
                env.paint(&mut self.stack[n - 1], 0.4 + 0.6 * p, 0.0, 0.96 + 0.04 * p);
                // …while the leaving one slides down into a fade.
                env.paint(leaving.as_mut(), 1.0 - p, 36.0 * k * p, 1.0);
            }
            _ => {
                let n = self.stack.len();
                env.paint(&mut self.stack[n - 1], 1.0, 0.0, 1.0);
            }
        }

        // Persistent chrome: the controller chip (top-right, above every layer).
        if let Some(chip) = &self.chip {
            let size = 12.0 * k;
            let tw = f64::from(fonts.measure(chip, W::Medium, size));
            let (bh, pad_x) = (24.0 * k, 12.0 * k);
            let bx = w - 24.0 * k - tw - 2.0 * pad_x;
            let rect = Rect::from_xywh(
                bx as f32,
                (18.0 * k) as f32,
                (tw + 2.0 * pad_x) as f32,
                bh as f32,
            );
            crate::theme::panel(
                canvas,
                rect,
                (bh / 2.0 / k) as f32,
                None,
                PanelStroke::Plain(0.12),
                k as f32,
            );
            fonts.draw(
                canvas,
                chip,
                bx + pad_x,
                18.0 * k + 16.0 * k,
                W::Medium,
                size,
                white(0.7),
            );
        }

        self.draw_overlays(canvas, w, h, k, dt, t, fonts);
    }

    fn draw_aurora(&self, canvas: &Canvas, w: f64, h: f64, t: f64) {
        let uniforms: [f32; 3] = [w as f32, h as f32, t as f32];
        let bytes = unsafe { std::slice::from_raw_parts(uniforms.as_ptr().cast::<u8>(), 12) };
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

    // --- Overlays (connecting / waking / toast) --------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn draw_overlays(
        &mut self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        dt: f64,
        t: f64,
        fonts: &Fonts,
    ) {
        // Resolve the connect/wake takeover — the two phases of reaching a host — into one
        // full-screen shape (spinner, title, one detail line, its own hints). Connecting flows
        // straight out of a wake (see `sync`) so they share the same backdrop and never blink
        // between them. Mirrors the Android client's unified `ConnectOverlay`.
        let takeover: Option<(f64, bool, String, String, Vec<Hint>)> =
            if let Some(c) = &mut self.connecting {
                c.appear = approach(c.appear, 1.0, dt, 0.07);
                if c.canceling {
                    Some((
                        c.appear,
                        true,
                        "Canceling…".to_string(),
                        String::new(),
                        vec![],
                    ))
                } else if c.request_access {
                    Some((
                        c.appear,
                        true,
                        "Waiting for approval…".to_string(),
                        format!(
                            "Approve this device in {}'s console or web UI — no PIN needed.",
                            c.title
                        ),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ))
                } else {
                    Some((
                        c.appear,
                        true,
                        format!("Connecting to {}…", c.title),
                        "Starting the stream in this window.".to_string(),
                        vec![Hint::new(HintKey::Back, "Cancel")],
                    ))
                }
            } else if let Some(wk) = &self.wake {
                // Service-driven, so it appears settled (no fade-in).
                if wk.timed_out {
                    Some((
                        1.0,
                        false,
                        format!("{} didn't wake", wk.name),
                        "Check its power settings, or wake it manually and try again.".to_string(),
                        vec![
                            Hint::new(HintKey::Confirm, "Try Again"),
                            Hint::new(HintKey::Back, "Cancel"),
                        ],
                    ))
                } else {
                    Some((
                        1.0,
                        true,
                        format!("Waking {}…", wk.name),
                        format!("Waiting for it to come online · {} s", wk.seconds),
                        // A wake-only wait (no dial after) offers "Stop Waiting"; a wake-&-connect
                        // is a plain "Cancel".
                        vec![Hint::new(
                            HintKey::Back,
                            if wk.then_connect {
                                "Cancel"
                            } else {
                                "Stop Waiting"
                            },
                        )],
                    ))
                }
            } else {
                None
            };
        if let Some((appear, spinner, title, body, hints)) = takeover {
            self.draw_takeover(
                canvas, w, h, k, appear, t, fonts, spinner, &title, &body, &hints,
            );
        }

        // The toast: a transient pill above the hint bar; slides in, fades out.
        if self.toast.as_ref().is_some_and(|toast| t - toast.at > 4.0) {
            self.toast = None;
        }
        if let Some(toast) = &self.toast {
            let age = t - toast.at;
            {
                let slide = ease_out_cubic((age / 0.25).min(1.0));
                let fade = if age > 3.4 {
                    (1.0 - (age - 3.4) / 0.6).max(0.0)
                } else {
                    1.0
                };
                let alpha = (slide * fade) as f32;
                let size = 13.0 * k;
                let tw = f64::from(fonts.measure(&toast.text, W::Medium, size));
                let (pad_x, bh) = (16.0 * k, 34.0 * k);
                let bw = tw + 2.0 * pad_x;
                let bx = (w - bw) / 2.0;
                let by = h - BOTTOM_BAND * k - bh - 8.0 * k + (1.0 - slide) * 12.0 * k;
                canvas.save_layer_alpha_f(None, alpha);
                let rect = Rect::from_xywh(bx as f32, by as f32, bw as f32, bh as f32);
                canvas.draw_rrect(
                    skia_safe::RRect::new_rect_xy(rect, (bh / 2.0) as f32, (bh / 2.0) as f32),
                    &Paint::new(Color4f::new(0.0, 0.0, 0.0, 0.6), None),
                );
                crate::theme::panel(
                    canvas,
                    rect,
                    (bh / 2.0 / k) as f32,
                    None,
                    PanelStroke::Plain(0.14),
                    k as f32,
                );
                fonts.draw(
                    canvas,
                    &toast.text,
                    bx + pad_x,
                    by + bh / 2.0 + size * 0.36,
                    W::Medium,
                    size,
                    white(0.92),
                );
                canvas.restore();
            }
        }
    }
}

/// Everything one screen layer needs to paint — bundled so the transition arms stay
/// readable and each `paint` call borrows `Shell` fields disjointly.
struct LayerEnv<'a> {
    canvas: &'a Canvas,
    w: f64,
    h: f64,
    content: Rect,
    k: f64,
    dt: f64,
    fonts: &'a Fonts,
    hosts: &'a [HostRow],
    library: &'a LibraryShared,
    settings: &'a mut trust::Settings,
    pads: &'a [PadInfo],
    deck: bool,
    device_name: &'a str,
    t: f64,
    glyphs: GlyphStyle,
    show_hints: bool,
}

impl LayerEnv<'_> {
    /// One screen composited as a unit: `alpha` fade, `dy` vertical slide, `scale`
    /// about the screen center — its pinned title and hint bar ride inside the layer,
    /// so chrome travels with content through a transition.
    fn paint(&mut self, screen: &mut Screen, alpha: f64, dy: f64, scale: f64) {
        let canvas = self.canvas;
        canvas.save_layer_alpha_f(None, alpha.clamp(0.0, 1.0) as f32);
        canvas.translate((0.0, dy as f32));
        let (cx, cy) = ((self.w / 2.0) as f32, (self.h / 2.0) as f32);
        canvas.translate((cx, cy));
        canvas.scale((scale as f32, scale as f32));
        canvas.translate((-cx, -cy));

        let mut ctx = Ctx {
            hosts: self.hosts,
            library: self.library,
            settings: self.settings,
            pads: self.pads,
            deck: self.deck,
            device_name: self.device_name,
            t: self.t,
        };
        self.fonts.centered(
            canvas,
            &screen.title(&ctx),
            W::Bold,
            30.0 * self.k,
            WHITE,
            self.w / 2.0,
            18.0 * self.k,
            self.w * 0.7,
        );
        screen.render(canvas, self.content, self.k, self.dt, self.fonts, &mut ctx);
        if self.show_hints {
            let hints = screen.hints(&ctx);
            hint_bar(
                canvas,
                self.fonts,
                &hints,
                self.glyphs,
                18.0 * self.k,
                self.h - 18.0 * self.k,
                self.k,
            );
        }
        canvas.restore();
    }
}

impl Shell {
    /// A full-screen connect/wake takeover: a fresh aurora over everything (so the carousel and
    /// chrome fall away), a centered spinner (or none, when a wake has timed out), a title, one
    /// detail line, and its own bottom hint row. `appear` fades the whole thing in over the home;
    /// a wake that hands off to a connect passes 1.0 so the two never blink between them. The
    /// console counterpart of the Android/Apple `ConnectOverlay` — one full-screen shape, not a
    /// centered modal card.
    #[allow(clippy::too_many_arguments)]
    fn draw_takeover(
        &self,
        canvas: &Canvas,
        w: f64,
        h: f64,
        k: f64,
        appear: f64,
        t: f64,
        fonts: &Fonts,
        spinner: bool,
        title: &str,
        body: &str,
        hints: &[Hint],
    ) {
        let cx = w / 2.0;
        canvas.save_layer_alpha_f(None, appear as f32);
        // Opaque aurora — the same living backdrop the home wears, so the takeover reads as the
        // console taking over rather than a card popping up.
        self.draw_aurora(canvas, w, h, t);
        // A soft pool of shade under the centre seats the white text against a bright aurora.
        let mut vignette = Paint::default();
        vignette.set_shader(gradient_shader::radial(
            Point::new(cx as f32, (h / 2.0) as f32),
            (w.max(h) * 0.42) as f32,
            gradient_shader::GradientShaderColors::Colors(&[
                Color4f::new(0.0, 0.0, 0.0, 0.5).to_color(),
                Color4f::new(0.0, 0.0, 0.0, 0.0).to_color(),
            ]),
            None,
            TileMode::Clamp,
            None,
            None,
        ));
        canvas.draw_rect(Rect::from_wh(w as f32, h as f32), &vignette);

        // Centre the spinner + title + detail as a group around the middle of the screen.
        let title_y = h / 2.0 + if spinner { 14.0 * k } else { 0.0 };
        if spinner {
            crate::theme::spinner(canvas, cx, title_y - 52.0 * k, 22.0 * k, t);
        }
        fonts.centered(
            canvas,
            title,
            W::SemiBold,
            23.0 * k,
            WHITE,
            cx,
            title_y,
            w * 0.82,
        );
        if !body.is_empty() {
            fonts.centered(
                canvas,
                body,
                W::Regular,
                14.0 * k,
                DIM,
                cx,
                title_y + 32.0 * k,
                w * 0.66,
            );
        }
        if !hints.is_empty() {
            // Centered near the bottom, where every console screen's legend sits.
            let probe = hint_bar(canvas, fonts, hints, self.glyphs, -10_000.0, -10_000.0, k);
            hint_bar(
                canvas,
                fonts,
                hints,
                self.glyphs,
                cx - probe.0 / 2.0,
                h - 34.0 * k,
                k,
            );
        }
        canvas.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WakeStatus;
    use crate::screens::home::HomeScreen;
    use crate::screens::library::LibraryScreen;
    use punktfunk_core::config::GamepadPref;

    /// Point the settings/known-hosts stores at a throwaway HOME — the settings screen
    /// SAVES on adjust, and a test must never write the developer's real config.
    fn fake_home() {
        use std::sync::OnceLock;
        static HOME: OnceLock<std::path::PathBuf> = OnceLock::new();
        let dir = HOME.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pf-console-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("HOME", &dir);
            dir.clone()
        });
        std::env::set_var("HOME", dir);
    }

    fn hosts() -> Vec<HostRow> {
        let base = HostRow {
            key: String::new(),
            name: String::new(),
            addr: "10.0.0.20".into(),
            port: 9777,
            fp_hex: String::new(),
            paired: false,
            saved: true,
            online: false,
            mgmt_port: 47990,
            can_wake: false,
            last_used: None,
        };
        vec![
            HostRow {
                key: "aa11".into(),
                name: "Living Room PC".into(),
                fp_hex: "aa11".into(),
                paired: true,
                online: true,
                last_used: Some(1),
                ..base.clone()
            },
            HostRow {
                key: "bb22".into(),
                name: "Office Tower".into(),
                addr: "10.0.0.21".into(),
                fp_hex: "bb22".into(),
                paired: true,
                can_wake: true,
                ..base.clone()
            },
            HostRow {
                key: "10.0.0.30:9777".into(),
                name: "steambox".into(),
                addr: "10.0.0.30".into(),
                saved: false,
                online: true,
                ..base
            },
        ]
    }

    fn shell(stack: Vec<Screen>) -> (Shell, ConsoleShared, LibraryShared) {
        fake_home();
        let console = ConsoleShared::default();
        console.set_hosts(hosts());
        let library = LibraryShared::default();
        let bus = ConsoleBus::default();
        let shell = Shell::new(
            console.clone(),
            library.clone(),
            bus,
            ConsoleOptions {
                device_name: "deck".into(),
                deck: false,
            },
            stack,
        )
        .unwrap();
        (shell, console, library)
    }

    /// The shell survives a full navigation lap (a smoke test over every screen's
    /// input handling — no rendering, no GPU).
    #[test]
    fn navigation_lap() {
        let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
        s.sync();
        // Home → Settings (X), adjust something, back out.
        s.handle_menu(MenuEvent::Tertiary);
        assert_eq!(s.stack.len(), 2);
        finish_motion(&mut s);
        s.handle_menu(MenuEvent::Move(MenuDir::Down));
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
        s.handle_menu(MenuEvent::Back);
        finish_motion(&mut s);
        assert_eq!(s.stack.len(), 1);
        // Home → Library on the paired host (Y), then back.
        s.handle_menu(MenuEvent::Secondary);
        assert_eq!(s.stack.len(), 2);
        finish_motion(&mut s);
        s.handle_menu(MenuEvent::Back);
        finish_motion(&mut s);
        assert_eq!(s.stack.len(), 1);
        // B at the root quits.
        s.handle_menu(MenuEvent::Back);
        assert!(matches!(s.take_action(), Some(OverlayAction::Quit)));
    }

    #[test]
    fn connect_flow_raises_launch_and_cancel() {
        let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
        s.sync();
        s.handle_menu(MenuEvent::Confirm); // paired+online host focused first
        assert!(matches!(
            s.take_action(),
            Some(OverlayAction::Launch { launch: None, .. })
        ));
        assert!(s.connecting.is_some());
        // While connecting: B cancels exactly once.
        s.handle_menu(MenuEvent::Back);
        assert!(matches!(
            s.take_action(),
            Some(OverlayAction::CancelConnect)
        ));
        s.handle_menu(MenuEvent::Back);
        assert!(s.take_action().is_none(), "cancel is idempotent");
        // The canceled dial ends silently.
        s.session_ended(None);
        assert!(s.connecting.is_none());
    }

    fn finish_motion(s: &mut Shell) {
        // Transitions block input; tests fast-forward them.
        s.motion = Motion::None;
    }

    #[test]
    fn wake_gates_input_in_the_same_press() {
        let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
        s.sync();
        // Focus "Office Tower" (offline + wakeable), then A: the wake starts.
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
        s.handle_menu(MenuEvent::Confirm);
        let w = s
            .wake
            .as_ref()
            .expect("Waking card raised in the SAME call as the A press");
        assert_eq!(w.name, "Office Tower");
        assert!(!w.online);
        // The very next input is modal-gated — the cursor can't drift onto Add Host —
        // and sync (which runs first in handle_menu) must not clear the placeholder
        // before the service thread reports its first real status.
        assert!(s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_none());
        assert!(
            s.wake.is_some(),
            "optimistic card survived a sync with no service status"
        );
        // B cancels: the gate releases and navigation works again.
        s.handle_menu(MenuEvent::Back);
        assert!(s.wake.is_none());
        assert!(s.handle_menu(MenuEvent::Move(MenuDir::Left)).is_some());
    }

    /// Render every console scene to PNGs for the eyeball pass (ignored; run with
    /// `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --release -- --ignored dump`).
    /// CPU raster — the SkSL aurora, layers and text all run without a GPU.
    #[test]
    #[ignore]
    fn dump_console_screens() {
        let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
        let fonts = crate::theme::build_fonts().unwrap();
        let (w, h) = (1280, 800);
        let pads: Vec<PadInfo> = Vec::new();
        let dump = |shell: &mut Shell, frames: usize, sleep_ms: u64, name: &str, pad: bool| {
            let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
            for _ in 0..frames {
                shell.render(
                    surface.canvas(),
                    w as u32,
                    h as u32,
                    &fonts,
                    pad.then_some("Xbox Wireless Controller"),
                    pad.then_some(GamepadPref::Xbox360),
                    &pads,
                );
                std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
            }
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
        };

        // Home, settled, with a pad (Letters glyphs).
        let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
        dump(&mut s, 40, 8, "01-home", true);

        // Mid-push into Settings (the transition still): a couple of fast frames land
        // the capture around p ≈ 0.4 — both layers visible.
        s.handle_menu(MenuEvent::Tertiary);
        dump(&mut s, 3, 25, "02-transition", true);
        dump(&mut s, 40, 8, "03-settings", true);

        // Add Host with the keyboard tray up (keyboard glyph style: no pad).
        s.handle_menu(MenuEvent::Back);
        dump(&mut s, 40, 8, "_back", true);
        for _ in 0..3 {
            s.handle_menu(MenuEvent::Move(MenuDir::Right));
        }
        s.handle_menu(MenuEvent::Confirm); // Add Host screen
        dump(&mut s, 40, 8, "04-addhost", false);
        s.handle_menu(MenuEvent::Confirm); // open the Name keyboard
        for ev in [
            MenuEvent::Move(MenuDir::Down),
            MenuEvent::Confirm,
            MenuEvent::Confirm,
        ] {
            s.handle_menu(ev);
        }
        dump(&mut s, 40, 8, "05-addhost-keyboard", false);

        // Pair (focused on the unpaired discovered host).
        s.handle_menu(MenuEvent::Back); // close keyboard
        s.handle_menu(MenuEvent::Back); // leave add-host
        dump(&mut s, 40, 8, "_back2", true);
        s.handle_menu(MenuEvent::Move(MenuDir::Left)); // onto "steambox"
        s.handle_menu(MenuEvent::Confirm);
        dump(&mut s, 40, 8, "06-pair", true);

        // Library with placeholder posters.
        library.set_games(
            [
                "Hades II",
                "Elden Ring",
                "Hollow Knight",
                "Baldur's Gate 3",
                "Celeste",
                "Deep Rock Galactic",
                "Portal 2",
            ]
            .iter()
            .enumerate()
            .map(|(i, t)| crate::library::LibraryGame {
                id: format!("steam:{i}"),
                title: (*t).to_string(),
                store: "steam".into(),
            })
            .collect(),
        );
        let (mut s2, _c2, _l2) = {
            let console2 = ConsoleShared::default();
            console2.set_hosts(hosts());
            let bus = ConsoleBus::default();
            let sh = Shell::new(
                console2.clone(),
                library.clone(),
                bus,
                ConsoleOptions {
                    device_name: "deck".into(),
                    deck: false,
                },
                vec![
                    Screen::Home(HomeScreen::new()),
                    Screen::Library(LibraryScreen::new(&hosts()[0])),
                ],
            )
            .unwrap();
            (sh, console2, library.clone())
        };
        s2.handle_menu(MenuEvent::Move(MenuDir::Right));
        s2.handle_menu(MenuEvent::Move(MenuDir::Right));
        dump(&mut s2, 40, 8, "07-library", true);

        // The wake and connecting overlays + a toast.
        console.set_wake(Some(WakeStatus {
            key: "bb22".into(),
            name: "Office Tower".into(),
            seconds: 12,
            timed_out: false,
            online: false,
            then_connect: true,
        }));
        dump(&mut s, 10, 8, "08-waking", true);
        console.set_wake(Some(WakeStatus {
            key: "bb22".into(),
            name: "Office Tower".into(),
            seconds: 90,
            timed_out: true,
            online: false,
            then_connect: true,
        }));
        dump(&mut s, 10, 8, "08b-wake-timed-out", true);
        console.set_wake(None);
        s.set_connecting(Some("Elden Ring".into()));
        dump(&mut s, 10, 8, "09-connecting", true);
        s.set_connecting(None);
        s.session_failed("Connection timed out");
        dump(&mut s, 10, 8, "10-toast", true);
    }
}
