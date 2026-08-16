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

use crate::anim::{springs, Spring};
use crate::glyphs::GlyphStyle;
use crate::library::{mesh_sksl, palette, LibraryShared};
use crate::model::{ConsoleBus, ConsoleCmd, ConsoleShared, HostRow, PairPhase, WakeStatus};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Bg, ConnectIntent, Ctx, Nav, Outbox, Screen};
use anyhow::{anyhow, Result};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse, PadInfo};
use pf_client_core::trust;
use pf_presenter::overlay::OverlayAction;
use skia_safe::{Canvas, Color4f, Data, Rect, RuntimeEffect};
use std::collections::VecDeque;
use std::time::Instant;

mod overlays;
mod render;

/// The reduced-motion transition: quick and critically damped, drawn as a pure crossfade
/// (no slide, no scale — see `render.rs`). Still a transition and not a cut, because the
/// screen stack needs to stay legible: an instant swap loses the "you went somewhere"
/// reading that is the only spatial cue a console shell has.
const REDUCED_NAV: crate::anim::SpringSpec = crate::anim::SpringSpec {
    response: 0.22,
    damping: 1.0,
};
/// The push/pop choreography, in design units. Named rather than inlined at the paint
/// sites because `console-vectors.json` claims to pin them for all three clients, and a
/// literal buried in a paint recipe is a literal no test can reach.
const NAV_SLIDE_DP: f64 = 36.0;
/// The incoming screen's starting scale on a push…
const NAV_ENTER_SCALE: f64 = 0.985;
/// …and the outgoing/revealed one's, which is the deeper of the two because the screen
/// being left behind should read as further away than the one arriving.
const NAV_EXIT_SCALE: f64 = 0.96;
/// How visible the revealed screen is at the START of a pop — not 0, because it was
/// already there behind the screen coming off.
const NAV_REVEAL_ALPHA: f64 = 0.4;
/// How far a transition must have travelled before it accepts anything other than Back.
/// Not a time: with a sprung transition "how far along" and "how long ago" are different
/// questions, and the one that matters for input is whether the screen under the cursor is
/// the one the user is aiming at.
const NAV_INPUT_OPENS: f64 = 0.85;
/// Chrome bands (design units): the pinned title above, hints below.
const TOP_BAND: f64 = 64.0;
const BOTTOM_BAND: f64 = 86.0;

/// Which way a transition is choreographed. The paint recipes differ (a push slides the
/// incoming screen up out of a fade; a pop grows the revealed one back while the leaving
/// one drops away), so the kind outlives the direction the spring happens to be heading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NavKind {
    Push,
    Pop,
}

/// One transition, as a single sprung scalar rather than a timer.
///
/// The scalar is what makes it INTERRUPTIBLE. A Back pressed mid-push retargets this very
/// spring from 1.0 to 0.0 — same recipe, same velocity, so the screen visibly turns around
/// and goes back where it came from instead of finishing its arrival and then playing a
/// separate dismissal. A tween could not do that: its progress is a function of elapsed
/// time, and reversing it means either a snap or a second animation.
enum Motion {
    None,
    Nav {
        spring: Spring,
        /// Where the spring is heading: 1.0 = the transition completing, 0.0 = it being
        /// undone. Only a push is ever retargeted to 0.0 (see [`Shell::nav_back`]).
        target: f64,
        kind: NavKind,
        /// The screen leaving the stack, when it is no longer ON the stack to be drawn from.
        /// A pop always carries one. A plain push never does — its parent stays put and the
        /// renderer finds it at `n - 2` — but a REPLACE does, because the screen it swapped
        /// out is gone and `n - 2` is that screen's parent, a level too far.
        leaving: Option<Box<Screen>>,
    },
}

/// What a toast is REPORTING, which is the thing the old single style couldn't say: a
/// pairing that worked and a connect that failed were the same grey pill, so the only way
/// to tell them apart was to read. Each kind carries a mark and a hairline colour.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    /// Something happened (a session ended, a scan started). The default.
    Info,
    /// Something the user asked for succeeded.
    Success,
    /// Something failed. The one kind that does NOT take its colour from the palette — a
    /// pale field's accent can be a cheerful mint, and a failure must not read as one.
    Error,
}

/// The shape drawn ahead of a toast's text. Three marks, deliberately geometric: the
/// crate's glyph art is hand-built Skia paths, and a mark that has to survive from 0.75×
/// to 3× `k` on a TV across the room can't rely on fine detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ToastMark {
    Dot,
    Check,
    Bang,
}

impl ToastKind {
    /// The tint and mark this kind draws with.
    pub(crate) fn look(self) -> (Color4f, ToastMark) {
        match self {
            ToastKind::Info => (crate::theme::fg(0.55), ToastMark::Dot),
            ToastKind::Success => (crate::theme::accent(1.0), ToastMark::Check),
            // Fixed, not palette-derived, and that is the point: `moss`'s accent is a green
            // and `ember`'s is an orange — either would report a failure in the colour the
            // UI uses for "this is fine".
            ToastKind::Error => (Color4f::new(0.93, 0.31, 0.28, 1.0), ToastMark::Bang),
        }
    }
}

struct Toast {
    text: String,
    at: f64,
    kind: ToastKind,
    /// Slide-in seat, 0 → 1 on the tray spring. The same gesture as the keyboard tray
    /// (something arriving from off-screen and settling), so it takes the same spec.
    seat: Spring,
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
    /// The last host title a connect was raised for, kept past the connect itself so
    /// [`Self::session_reconnecting`] can name the host it is re-dialing — that flow
    /// raises no `Launch` of its own and therefore never passes a title through.
    last_connect_title: Option<String>,
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
    /// The settled top screen's hint-bar hit boxes, republished every frame by
    /// [`Shell::render`]. The legend is the console's only on-screen statement of what the
    /// face buttons do; for a pointer, which has none, it IS the button bar.
    hint_rects: Vec<(crate::glyphs::HintKey, Rect)>,
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
            last_connect_title: None,
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
            hint_rects: Vec::new(),
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
                self.last_connect_title = Some(title.clone());
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
        self.show_toast_kind(format!("Couldn't connect — {msg}"), ToastKind::Error);
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

    /// The stream stopped and the client is dialing again on its own (M8's codec
    /// fallback). Says what changed — the picture is about to come back as a different
    /// codec and silence would read as a glitch — and raises the connecting modal.
    ///
    /// The modal is not cosmetic. Nothing raises a `Launch` for this retry (the run loop
    /// starts the pump itself), so without it the shell would be in a state no other flow
    /// produces: not streaming, not connecting, and a live pump behind the console. All
    /// three gates would open at once — menu events flowing, the console drawn
    /// full-screen over a frozen picture, and no modal interlock — and pressing A would
    /// launch a SECOND session on top of the running one. This is also what gives B
    /// somewhere to go: the modal's Back raises `CancelConnect`, which the run loop
    /// applies to the retry's pump exactly as it does to a first dial.
    ///
    /// `appear = 1.0`: the takeover is already the thing on screen (the retry follows a
    /// live stream), so fading it in would read as a flash rather than a transition.
    pub(crate) fn session_reconnecting(&mut self, msg: &str) {
        self.in_stream = false;
        self.connecting = Some(Connecting {
            // The host this session was dialed to. `None` only if the shell never raised
            // the connect itself (a `--connect` run has no console at all, so it never
            // reaches here) — name the codec change instead of an empty string.
            title: self
                .last_connect_title
                .clone()
                .unwrap_or_else(|| "the host".to_string()),
            canceling: false,
            appear: 1.0,
            request_access: false,
        });
        self.show_toast(msg.to_string());
    }

    fn show_toast(&mut self, text: String) {
        self.show_toast_kind(text, ToastKind::Info);
    }

    fn show_toast_kind(&mut self, text: String, kind: ToastKind) {
        self.toast = Some(Toast {
            text,
            at: self.t(),
            kind,
            seat: Spring::rest(0.0),
        });
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

        // Service-worker notices (e.g. the log-upload result) become plain toasts.
        if let Some(text) = self.console.take_notice() {
            self.show_toast(text);
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
                self.show_toast_kind(format!("Paired with {name}"), ToastKind::Success);
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
        // A transition no longer walls input off, it only filters it.
        //
        // Back is always heard, and is answered by the TRANSITION rather than the screens
        // (see `nav_back`): mid-push it reverses the push, mid-pop it starts the next one.
        // That is the whole point of the work — holding B to back out of a deep stack used
        // to stutter at every level, because each press landed inside the previous
        // transition and was thrown away.
        //
        // Everything else is still dropped until the incoming screen is nearly seated,
        // which is what keeps a double-tapped A from pushing two screens. The threshold is
        // on the spring's POSITION, not on elapsed time, so it means "far enough along to
        // be the thing you are aiming at" whatever the transition's velocity.
        if !matches!(self.motion, Motion::None) {
            if ev == MenuEvent::Back {
                if self.nav_back() {
                    return Some(MenuPulse::Confirm);
                }
            } else if self.nav_pos() < NAV_INPUT_OPENS {
                return None;
            }
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

    /// Mouse and touch, in device pixels. `true` = consumed.
    ///
    /// The precedence mirrors [`Self::handle_menu`] exactly, and for the same reasons: a
    /// modal card owns input while it is up, and a screen in motion takes none at all. The
    /// one addition is the hint bar, which sits above the screens because a pointer has no
    /// face buttons and the legend is where those actions live.
    pub(crate) fn pointer(&mut self, p: Pointer) -> bool {
        self.sync();
        // The right button is the pointer's B, everywhere — including on the modal cards,
        // where Back is the only thing that answers at all.
        //
        // With ONE exception: B at the root quits the launcher, and a right-click is far
        // easier to fire by accident than a thumb on B. Quitting stays an explicit act —
        // the legend's "Quit" is clickable, and that is the pointer's way out.
        if p.kind == PointerKind::Back {
            if self.stack.len() > 1 || self.connecting.is_some() || self.wake.is_some() {
                self.handle_menu(MenuEvent::Back);
            }
            return true;
        }
        // A modal swallows the rest: clicking "past" a connect takeover onto the library
        // behind it would start a second session, which is the same hole the menu path
        // closes by returning early here.
        if self.connecting.is_some() || self.wake.is_some() {
            return true;
        }
        if !matches!(self.motion, Motion::None) {
            return true;
        }
        if p.press() {
            if let Some((key, _)) = self.hint_rects.iter().find(|(_, r)| p.hits(*r)) {
                // A hint is clickable when it names an ACTION. Shoulders and Adjust name a
                // DIRECTION, and the thing they steer — the tab strip, a row's value — is
                // already under the pointer's finger; inventing a side for a click here
                // would just be a worse way to press what it can already press.
                let ev = match key {
                    crate::glyphs::HintKey::Confirm => Some(MenuEvent::Confirm),
                    crate::glyphs::HintKey::Back => Some(MenuEvent::Back),
                    crate::glyphs::HintKey::Secondary => Some(MenuEvent::Secondary),
                    crate::glyphs::HintKey::Tertiary => Some(MenuEvent::Tertiary),
                    // ▲ is drawn as a direction and read as one, but it steers nothing: the
                    // only screen that publishes it is the home carousel, where up is not
                    // navigation but "open this tile's menu". Without this the context menu —
                    // and with it the only way to copy a host's link — is pad-only.
                    crate::glyphs::HintKey::Up => Some(MenuEvent::Move(MenuDir::Up)),
                    _ => None,
                };
                if let Some(ev) = ev {
                    self.handle_menu(ev);
                }
                return true;
            }
        }

        let mut fx = Outbox::default();
        let consumed = {
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
                .pointer(p, &mut ctx, &mut fx)
        };
        self.apply(fx);
        consumed
    }

    /// The keyboard fallback — the console is fully drivable with no pad. Arrows and
    /// Enter/Esc map onto menu events; Y/X mirror the pad's Secondary/Tertiary
    /// (suppressed while editing, where letters are text).
    ///
    /// `shift` only matters for Tab, whose two directions are one key.
    pub(crate) fn key(&mut self, sc: sdl3::keyboard::Scancode, shift: bool, repeat: bool) -> bool {
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
            // Tab is what a keyboard reaches for to change section, and the settings tabs
            // were otherwise on PgUp/PgDn alone — a binding the legend only ever spells out
            // when NO pad is attached, so with a controller plugged in there was nothing to
            // discover. Shift+Tab goes back, as everywhere else.
            S::Tab if !repeat && shift => MenuEvent::JumpBack,
            S::Tab if !repeat => MenuEvent::JumpForward,
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
        if let Some(text) = fx.copy {
            self.actions.push_back(OverlayAction::CopyText(text));
        }
        if let Some(intent) = fx.connect {
            self.start_connect(intent);
        }
        if let Some(nav) = fx.nav {
            self.apply_nav(nav);
        }
    }

    /// The spring the next push/pop runs on. Read at NAV time rather than per frame so a
    /// transition can't change feel under itself if the setting is stepped mid-flight.
    ///
    /// Reduced motion keeps a spring rather than switching integrators — one code path
    /// stays one code path — and simply picks a critically damped, quicker one. Combined
    /// with the flattened geometry in `render.rs` (no slide, no scale) that reads as the
    /// plain crossfade the setting promises.
    fn nav_spec(&self) -> crate::anim::SpringSpec {
        if self.settings.reduce_motion {
            REDUCED_NAV
        } else {
            springs::NAV
        }
    }

    fn begin_nav(&mut self, kind: NavKind, leaving: Option<Box<Screen>>) {
        self.motion = Motion::Nav {
            spring: Spring::rest(0.0),
            target: 1.0,
            kind,
            leaving,
        };
    }

    fn apply_nav(&mut self, nav: Nav) {
        match nav {
            Nav::Push(screen) => {
                self.stack.push(*screen);
                self.begin_nav(NavKind::Push, None);
            }
            Nav::Replace(screen) => {
                // Swap under the SAME push choreography: the outgoing screen is dropped
                // rather than parked, so Back from the incoming one lands where the
                // replaced screen was reached from.
                //
                // It is CARRIED through the transition rather than dropped on the spot,
                // which is the whole difference between this reading right and reading
                // wrong. A push paints the screen BENEATH the incoming one as its receding
                // layer; drop the replaced screen first and that is its parent, so choosing
                // "Edit…" in a host's menu animated the editor in over HOME — the host list
                // flashing up for the length of the transition, as if the menu had been
                // dismissed and something else opened. Handing it over as the leaving layer
                // means the menu itself recedes, which is what actually happened.
                let leaving = self.stack.pop().map(Box::new);
                self.stack.push(*screen);
                self.begin_nav(NavKind::Push, leaving);
            }
            Nav::Pop => {
                if self.stack.len() > 1 {
                    let leaving = self.stack.pop().expect("len > 1");
                    self.begin_nav(NavKind::Pop, Some(Box::new(leaving)));
                } else {
                    // Popping the root quits the console (B at home).
                    self.actions.push_back(OverlayAction::Quit);
                }
            }
        }
    }

    /// How far the transition in flight has travelled, 0 when there is none.
    fn nav_pos(&self) -> f64 {
        match &self.motion {
            Motion::None => 1.0,
            Motion::Nav { spring, .. } => spring.pos,
        }
    }

    /// Back, pressed while a transition is still in flight. Returns `true` when the
    /// transition itself answered it, so the event never reaches the screens.
    ///
    /// This is the method the whole work package exists for. Before it, every mid-flight
    /// press was swallowed, so holding B to back out of a deep stack stuttered at each
    /// level: press, wait 0.26 s, press again.
    fn nav_back(&mut self) -> bool {
        let Motion::Nav {
            spring,
            target,
            kind,
            ..
        } = &mut self.motion
        else {
            return false;
        };
        match *kind {
            // Reverse the push ON THE SAME SPRING. Velocity carries, so the entering screen
            // decelerates, turns, and goes back down — one continuous motion rather than an
            // arrival followed by a dismissal. `finish_nav` takes it off the stack when the
            // spring lands on 0.
            //
            // Refused at the root, where there is no parent to fall back to and B means
            // quit: that press belongs to the normal path.
            NavKind::Push if *target == 1.0 && self.stack.len() > 1 => {
                *target = 0.0;
                true
            }
            // Already reversing — nothing further to say, and letting this through would
            // pop the parent out from under a screen that is still on its way off.
            NavKind::Push if *target == 0.0 => true,
            NavKind::Push => false,
            // A pop already going the right way: honour the press as "and the next one
            // too". The current pop's bookkeeping is finished on the spot (the stack is
            // already correct; only `leaving` is still being carried) and a fresh pop
            // starts, which is exactly what makes a held B walk out of a stack smoothly.
            NavKind::Pop => {
                let _ = spring;
                self.motion = Motion::None;
                self.apply_nav(Nav::Pop);
                true
            }
        }
    }

    /// Advance the transition by `dt`. Returns how far along it is, or `None` once it has
    /// settled — by which point [`Self::finish_nav`] has already run.
    ///
    /// Separate from `render` so it can be driven at a chosen `dt`: the render path takes
    /// its `dt` from the wall clock, and a test that called it in a tight loop would
    /// advance the spring by microseconds per frame and never see it move.
    fn advance_nav(&mut self, dt: f64) -> Option<f64> {
        let spec = self.nav_spec();
        let p = match &mut self.motion {
            Motion::None => None,
            Motion::Nav { spring, target, .. } => {
                spring.step_spec(*target, spec, dt);
                spring.settle(*target, 0.001, 0.01);
                // `settle` snaps both to rest, so this is an exact test rather than an
                // epsilon one — and it is the only way out of the state.
                (spring.pos != *target || spring.vel != 0.0).then_some(spring.pos)
            }
        };
        if p.is_none() {
            self.finish_nav();
        }
        p
    }

    /// A settled transition's bookkeeping. Called once the spring has landed on its target.
    fn finish_nav(&mut self) {
        if let Motion::Nav {
            kind,
            target,
            leaving,
            ..
        } = &mut self.motion
        {
            // A push that was reversed mid-flight never happened: take its screen back off.
            // Guarded on length because the root must never be popped here — `nav_back`
            // refuses to reverse there, and this is the belt to that's braces.
            if *kind == NavKind::Push && *target == 0.0 && self.stack.len() > 1 {
                self.stack.pop();
                // A reversed REPLACE puts back what it swapped out. The transition showed
                // that screen receding and then coming home again, so landing anywhere else
                // would contradict what was on glass — and "undo that navigation" means the
                // menu you were standing in, not the screen one level further out.
                if let Some(back) = leaving.take() {
                    self.stack.push(*back);
                }
            }
        }
        // A completed pop drops the screen it was carrying, exactly as before.
        self.motion = Motion::None;
    }

    /// The living backdrop. `calm` 0 = the launcher's aurora, 1 = the quiet field the form
    /// screens sit on; the shell chases it, so there is only ever ONE backdrop pass — the
    /// former aurora-over-static-form crossfade is now a single uniform.
    /// The clock the backdrop shader runs on. Reduced motion freezes the field at a fixed
    /// phase rather than removing it: the colour IS the palette the user picked, and a
    /// still gradient is also the OLED-friendly thing to leave on a screen for an hour.
    ///
    /// The CALM mix is deliberately not frozen with it — that tracks which screen is up,
    /// a state change rather than decoration.
    fn field_clock(&self, t: f64) -> f64 {
        if self.settings.reduce_motion {
            0.0
        } else {
            t
        }
    }

    fn draw_aurora(&self, canvas: &Canvas, w: f64, h: f64, t: f64, calm: f64) {
        // Gated at the one place the shader's clock is read, so the takeover's own
        // `draw_aurora` call inherits it and a third caller can't forget.
        let t = self.field_clock(t);
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
                let mut paint = crate::theme::shaded();
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
