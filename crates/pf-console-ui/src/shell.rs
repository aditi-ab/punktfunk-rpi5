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
use crate::platform::Platform;
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Bg, ConnectIntent, Ctx, Nav, Outbox, Screen};
use crate::store::SettingsStore;
use anyhow::{anyhow, Result};
use pf_client_core::console::OverlayAction;
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse, PadInfo};
use pf_client_core::trust;
use skia_safe::{Canvas, Color4f, Data, Rect, RuntimeEffect};
use std::collections::VecDeque;
use std::sync::Arc;
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

/// How far a finger may wander (design units × the frame's `k`) and still be a tap. Past
/// this the gesture is a drag and the lift acts on nothing. ~12dp is the classic touch
/// slop; in device pixels it lands near Android's own ViewConfiguration figure.
const TOUCH_SLOP_DP: f64 = 12.0;
/// One drag step (design units × `k`): each `DRAG_TICK_DP` of dominant-axis travel emits
/// one synthetic scroll tick. 56 is the menu list's row pitch (`widgets::ROW_H` + gap), so
/// a list under the finger moves about as far as the finger does. The on-glass tuning knob.
const DRAG_TICK_DP: f64 = 56.0;

/// The active touch gesture, tracked by [`Shell::pointer_input`] (see the `touch` flag on
/// `PointerInput::Down`). A mouse never enters this machine — its press acts immediately,
/// which is what a mouse means. A second finger while one gesture is live is ignored
/// (single-tracked; multi-touch gestures are a non-goal).
#[derive(Clone, Copy, Debug)]
enum TouchGesture {
    /// Finger down, still within slop of the anchor. A lift here is a tap: the Press is
    /// delivered AT THE ANCHOR — the focused item scrolls toward the centre, so the down
    /// point is where the user aimed and the lift point is where the content dragged
    /// their eye; widgets hit-test last frame's rects and already tolerate exactly this
    /// one-frame skew.
    Armed { x: f64, y: f64 },
    /// Slop exceeded: a drag, locked to the axis it left the slop on (diagonal jitter
    /// must not alternate a carousel with a list). `last` is the dominant-axis position
    /// the previous tick was emitted at.
    Drag {
        x: f64,
        y: f64,
        horizontal: bool,
        last: f64,
    },
}

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
    appear: f64,
    /// A request-access wait (parked on the host until the operator approves) — the
    /// takeover reads "Waiting for approval" rather than "Connecting".
    request_access: bool,
}

/// What the host hands the shell at construction.
pub struct ConsoleOptions {
    /// The machine's hostname — the default device name pairing registers.
    pub device_name: String,
    /// Steam Deck: Steam's keyboard types (SDL text input); ours never draws.
    pub deck: bool,
    /// Whether the host app has another interface to fall back to when the console is
    /// switched off — an Android phone/tablet's touch shell. Shows the console-off switch
    /// on the settings screen; false where this console is the only UI there is (the
    /// desktop session, an Android TV), where offering "off" would strand the user.
    pub fallback_ui: bool,
    /// Where settings persist and the profile catalog comes from. `None` = the desktop
    /// file store (`pf_client_core::trust`), which is what the Vulkan session wants and the
    /// only store there is on Linux/Windows; every other host must supply one.
    pub store: Option<Arc<dyn SettingsStore>>,
    /// Which platform this shell fronts — decides which settings rows exist and which
    /// platform-native screens the settings list may open.
    pub platform: Platform,
    /// Skia's GPU resource-cache budget, bytes — where decoded posters and glyph atlases
    /// live. The desktop's 160 MB ([`DEFAULT_GPU_CACHE_BYTES`]) is sized for
    /// a Deck; a 1 GB TV box wants a quarter of that (design/android-skia-console-port.md D11).
    pub gpu_cache_bytes: usize,
}

impl ConsoleOptions {
    /// The Vulkan session's options: file-backed settings, the desktop row set, the
    /// desktop cache budget.
    pub fn desktop(device_name: String, deck: bool) -> ConsoleOptions {
        ConsoleOptions {
            device_name,
            deck,
            fallback_ui: false,
            store: None,
            platform: Platform::Desktop,
            gpu_cache_bytes: DEFAULT_GPU_CACHE_BYTES,
        }
    }
}

/// Skia's GPU resource budget — poster art plus a few screen layers.
///
/// A CEILING, not an allocation: Skia grows into it only under demand, and the console's
/// demand is now small — with the library's posters cached at the size they are drawn
/// (`screens::library::art_cache_size`) a full grid at Deck scale asks for ~30 MB. What
/// matters is the HEADROOM. At 64 MB the budget sat under a full grid's working set: a
/// screenful of full-resolution covers is ~100 MB, so `GrResourceCache` evicted a third of
/// them on every submit and the next frame re-decoded them from JPEG on the render thread.
/// That was the grid's slideshow, and a cliff rather than a slope — which is exactly how it
/// was reported, smooth until the screen filled.
///
/// 160 MB clears the working set several times over at every scale a panel up to 1440p
/// produces, with room for the two render targets and the glyph atlases. The one arrangement
/// that can still crowd it is a 4K panel (`k` 2.7, 33 MB a render target) fed 1000×1500
/// SteamGridDB portraits, where full resolution is genuinely what gets drawn — and that is a
/// desktop GPU by the time it happens.
///
/// The desktop's number; a host with less memory (a TV box) passes its own through
/// [`ConsoleOptions::gpu_cache_bytes`].
pub const DEFAULT_GPU_CACHE_BYTES: usize = 160 << 20;

pub(crate) struct Shell {
    stack: Vec<Screen>,
    motion: Motion,
    console: ConsoleShared,
    library: LibraryShared,
    bus: ConsoleBus,
    actions: VecDeque<OverlayAction>,
    settings: trust::Settings,
    /// Where `settings` persists (see [`ConsoleOptions::store`]).
    store: Arc<dyn SettingsStore>,
    /// The platform this shell fronts (see [`ConsoleOptions::platform`]).
    pub(crate) platform: Platform,
    hosts: Vec<HostRow>,
    hosts_gen: u64,
    device_name: String,
    deck: bool,
    /// See [`ConsoleOptions::fallback_ui`].
    fallback_ui: bool,
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
    /// What drove the console LAST — a pad or keys — noted at the input seams
    /// ([`Shell::note_input_source`], [`Shell::key`]) and read by the per-frame glyph
    /// resolution, so the legend speaks the language of the device actually in use.
    /// `None` until anything drives: the style then follows the connected pad, or the
    /// platform's key device where there is none.
    input_source: Option<crate::console::InputSource>,
    chip: Option<String>,
    pads: Vec<PadInfo>,
    /// The settled top screen's hint-bar hit boxes, republished every frame by
    /// [`Shell::render`]. The legend is the console's only on-screen statement of what the
    /// face buttons do; for a pointer, which has none, it IS the button bar.
    hint_rects: Vec<(crate::glyphs::HintKey, Rect)>,
    /// The (left, top) inset the last frame laid out under — pointer coordinates arrive in
    /// surface pixels and are brought into the same space `hint_rects` and every screen's
    /// hit boxes were published in.
    last_insets: (f32, f32),
    /// The design-unit scale the last frame rendered at, for the touch tracker's slop and
    /// tick distances — gesture geometry must grow with the UI it drags.
    last_k: f64,
    /// The touch gesture in flight, if any (see [`TouchGesture`]).
    gesture: Option<TouchGesture>,
    /// Skia's resource-cache budget for the host that renders this shell (see
    /// [`ConsoleOptions::gpu_cache_bytes`]).
    pub(crate) gpu_cache_bytes: usize,
    t0: Instant,
    last_frame: Option<Instant>,
    /// Test-only: `(t, step)` — when set, the shell clock reads `t` and every frame advances
    /// it by `step` instead of wall time, so the screenshot dump renders the SAME pixels on
    /// any machine at any load (the aurora's phase is the clock; two real-time runs never agree
    /// past the first scene). Never set outside `shell/tests.rs`.
    #[cfg(test)]
    pub(crate) fake_clock: Option<(f64, f64)>,
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
        let store: Arc<dyn SettingsStore> = match opts.store {
            Some(store) => store,
            None => {
                #[cfg(any(target_os = "linux", windows))]
                {
                    Arc::new(crate::store::FileSettingsStore)
                }
                #[cfg(not(any(target_os = "linux", windows)))]
                {
                    anyhow::bail!("the console needs a settings store on this platform")
                }
            }
        };
        let settings = store.load();
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
            store,
            platform: opts.platform,
            hosts: Vec::new(),
            hosts_gen: u64::MAX,
            device_name: opts.device_name,
            deck: opts.deck,
            fallback_ui: opts.fallback_ui,
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
            input_source: None,
            chip: None,
            pads: Vec::new(),
            hint_rects: Vec::new(),
            last_insets: (0.0, 0.0),
            last_k: 1.0,
            gesture: None,
            gpu_cache_bytes: opts.gpu_cache_bytes,
            t0: Instant::now(),
            last_frame: None,
            #[cfg(test)]
            fake_clock: None,
        })
    }

    /// The live library model — for a host re-rooting the stack (see [`Self::replace_stack`]).
    pub(crate) fn library(&self) -> &LibraryShared {
        &self.library
    }

    /// Replace the whole screen stack (a deep link, a "back to that shelf" on return from a
    /// game). Cuts, no transition: this is a re-entry, not navigation the user watched.
    pub(crate) fn replace_stack(&mut self, stack: Vec<Screen>) {
        if stack.is_empty() {
            return;
        }
        self.stack = stack;
        self.motion = Motion::None;
        self.bg_mix = match self.stack.last().expect("non-empty").background() {
            Bg::Aurora => 0.0,
            Bg::Form => 1.0,
        };
    }

    /// The host-facing pointer vocabulary onto the shell's own: primary press/release,
    /// secondary-down = Back (its release is dropped, or a right-click would pop two
    /// screens), wheel = discrete scroll steps, cancel.
    ///
    /// A TOUCH primary down (`touch: true`) takes the gesture lane instead: the press is
    /// deferred, and the lift decides whether it was a tap (Press at the anchor) or a drag
    /// (scroll ticks were already emitted along the way, the lift acts on nothing). A press
    /// that acted on contact made every swipe across the settings list flip a value — the
    /// finger has to be allowed to mean "scroll" until it has said otherwise.
    pub(crate) fn pointer_input(&mut self, input: pf_client_core::console::PointerInput) -> bool {
        use pf_client_core::console::{PointerButton, PointerInput};
        let (x, y, kind) = match input {
            PointerInput::Move { x, y } => {
                if self.gesture.is_some() {
                    return self.gesture_move(f64::from(x), f64::from(y));
                }
                (x, y, PointerKind::Move)
            }
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Primary,
                touch,
            } => {
                if touch {
                    // A second finger while a gesture is live is ignored — single-tracked.
                    if self.gesture.is_none() {
                        self.gesture = Some(TouchGesture::Armed {
                            x: f64::from(x),
                            y: f64::from(y),
                        });
                    }
                    return true;
                }
                (x, y, PointerKind::Press)
            }
            PointerInput::Down {
                x,
                y,
                button: PointerButton::Secondary,
                ..
            } => (x, y, PointerKind::Back),
            PointerInput::Up {
                x,
                y,
                button: PointerButton::Primary,
            } => match self.gesture.take() {
                Some(TouchGesture::Armed { x, y }) => {
                    // A tap: the deferred Press lands now, at the anchor, followed by the
                    // Release the widgets ignore today (and a fling closes on tomorrow).
                    let consumed = self.pointer(Pointer {
                        x,
                        y,
                        kind: PointerKind::Press,
                    });
                    self.pointer(Pointer {
                        x,
                        y,
                        kind: PointerKind::Release,
                    });
                    return consumed;
                }
                // A drag ends where its last tick left it; the lift itself does nothing.
                Some(TouchGesture::Drag { .. }) => return true,
                None => (x, y, PointerKind::Release),
            },
            PointerInput::Up { .. } => return true,
            PointerInput::Wheel { x, y, dy } => {
                if dy == 0.0 {
                    return true;
                }
                (x, y, PointerKind::Scroll { up: dy > 0.0 })
            }
            PointerInput::Cancel => {
                self.gesture = None;
                (0.0, 0.0, PointerKind::Cancel)
            }
        };
        self.pointer(Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind,
        })
    }

    /// Advance the touch gesture by a Move. Within slop nothing happens; past it the
    /// gesture locks to its dominant axis and every [`DRAG_TICK_DP`]·k of travel becomes
    /// one synthetic scroll tick at the anchor. Direction reads as "content follows the
    /// finger": drag down/right = the previous item (a wheel-up), drag up/left = the next.
    fn gesture_move(&mut self, x: f64, y: f64) -> bool {
        let Some(gesture) = self.gesture else {
            return false;
        };
        match gesture {
            TouchGesture::Armed { x: ax, y: ay } => {
                let (dx, dy) = (x - ax, y - ay);
                if dx.hypot(dy) >= TOUCH_SLOP_DP * self.last_k {
                    let horizontal = dx.abs() > dy.abs();
                    self.gesture = Some(TouchGesture::Drag {
                        x: ax,
                        y: ay,
                        horizontal,
                        // Ticks count from where the slop was left, not from the anchor —
                        // the slop's travel was spent proving this is a drag.
                        last: if horizontal { x } else { y },
                    });
                }
                true
            }
            TouchGesture::Drag {
                x: ax,
                y: ay,
                horizontal,
                last,
            } => {
                let pos = if horizontal { x } else { y };
                let tick = DRAG_TICK_DP * self.last_k;
                let steps = ((pos - last) / tick).trunc();
                if steps != 0.0 {
                    self.gesture = Some(TouchGesture::Drag {
                        x: ax,
                        y: ay,
                        horizontal,
                        last: last + steps * tick,
                    });
                    let up = steps > 0.0;
                    for _ in 0..steps.abs() as u32 {
                        self.pointer(Pointer {
                            x: ax,
                            y: ay,
                            kind: PointerKind::Scroll { up },
                        });
                    }
                }
                true
            }
        }
    }

    /// The host reports a session edge. `Connecting` is a no-op — the shell raised the
    /// Launch itself and is already showing the takeover.
    pub(crate) fn session_phase(&mut self, phase: pf_client_core::console::SessionPhase) {
        use pf_client_core::console::SessionPhase;
        match phase {
            SessionPhase::Connecting => {}
            SessionPhase::Streaming => self.session_streaming(),
            SessionPhase::Failed(msg) => self.session_failed(msg),
            SessionPhase::Ended(reason) => self.session_ended(reason),
            SessionPhase::Reconnecting(msg) => self.session_reconnecting(msg),
        }
    }

    fn t(&self) -> f64 {
        #[cfg(test)]
        if let Some((t, _)) = self.fake_clock {
            return t;
        }
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
        // Coming back to a shelf means the player just left a game, which is the single moment
        // the running set is most likely to have changed — and the moment they are standing in
        // front of the badge that claims to know. Nothing else refreshes it: this screen stack
        // SURVIVES a stream (which is why the shelf needs no scroll restoration at all), so
        // without this the Resume badge would still be advertising the title they just quit.
        //
        // Only the running set, never the catalog: a re-fetch would reset the phase to Loading
        // and replace the shelf they are looking at with a spinner over a stream that just ended.
        if let Some(Screen::Library(lib)) = self.stack.last() {
            self.bus.send(ConsoleCmd::RefreshRunning {
                addr: lib.host_addr().to_string(),
                mgmt: lib.host_mgmt_port(),
                fp_hex: lib.host_fp_hex().to_string(),
            });
        }
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

        self.collections_handover();
    }

    /// "Start in collections": a shelf that has just learned it holds more than one collection
    /// stands aside for the collections screen.
    ///
    /// It lives here rather than in the screen because a screen cannot replace ITSELF — the
    /// decision needs the library model and the settings, which the shelf has, but the swap
    /// needs the stack, which only the shell has. The shelf answers the question and hands
    /// back a screen; this puts it where the shelf was standing.
    ///
    /// Guarded on a settled transition. Mid-flight the stack's top is not yet what the user
    /// is looking at, and swapping under a push the user has already reversed with B would
    /// land them on the collections of a host they just backed out of.
    fn collections_handover(&mut self) {
        if !matches!(self.motion, Motion::None) {
            return;
        }
        // Field borrows rather than clones: this runs every frame for the life of the shelf,
        // and `Settings` is a struct of owned Strings. `stack` is borrowed mutably while
        // `library` and `settings` are borrowed shared — disjoint fields, so the shelf can
        // read both while it is itself being held.
        let upgraded = match self.stack.last_mut() {
            Some(Screen::Library(shelf)) => {
                shelf.collections_upgrade(&self.library, &self.settings)
            }
            _ => None,
        };
        if let Some(screen) = upgraded {
            let n = self.stack.len();
            self.stack[n - 1] = Screen::Collections(screen);
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
        if self.connecting.is_some() {
            if ev == MenuEvent::Back {
                // The takeover comes down HERE, not when the host answers. It used to wait for
                // the next `session_phase` and show "Canceling…" until one arrived — and one is
                // not guaranteed to: the dial is a blocking call on the host's side of this
                // interface, so the wait was the whole connect budget (185 s on a request-access
                // connect the host parks pending approval), and an embedder that simply drops a
                // canceled dial never sends a phase at all. Either way the console sat on
                // "Canceling…" with no input that could reach it — only killing the app cleared
                // it. Cancel is the USER's decision and needs no confirmation from the wire; the
                // action below still goes out, and every host already handles a dial that lands
                // after it (quit-close the connector, route the end back silently).
                self.connecting = None;
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
                store: &*self.store,
                platform: self.platform,
                pads: &self.pads,
                deck: self.deck,
                fallback_ui: self.fallback_ui,
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
        // Surface pixels → the safe-area space the last frame laid out in.
        let p = Pointer {
            x: p.x - f64::from(self.last_insets.0),
            y: p.y - f64::from(self.last_insets.1),
            kind: p.kind,
        };
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
                    // ▼ is the same kind of hint: a direction that steers nothing, because
                    // the only screen publishing it is the home carousel, where down means
                    // "open Settings". A finger must be able to press what it advertises.
                    crate::glyphs::HintKey::Down => Some(MenuEvent::Move(MenuDir::Down)),
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
                store: &*self.store,
                platform: self.platform,
                pads: &self.pads,
                deck: self.deck,
                fallback_ui: self.fallback_ui,
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

    /// Note what produced the menu events now arriving — the hint legend follows it.
    /// Called by [`crate::console::Console::menu`] (which is told by its host) and the
    /// overlay's pad path; the keyboard path notes itself in [`Shell::key`].
    pub(crate) fn note_input_source(&mut self, source: crate::console::InputSource) {
        self.input_source = Some(source);
    }

    /// The keyboard fallback — the console is fully drivable with no pad. Arrows and
    /// Enter/Esc map onto menu events; Y/X mirror the pad's Secondary/Tertiary
    /// (suppressed while editing, where letters are text).
    ///
    /// `shift` only matters for Tab, whose two directions are one key.
    pub(crate) fn key(&mut self, key: crate::input::Key, shift: bool, repeat: bool) -> bool {
        use crate::input::Key as S;
        self.input_source = Some(crate::console::InputSource::Keys);
        if self.editing() {
            let mut ctx = Ctx {
                hosts: &self.hosts,
                library: &self.library,
                settings: &mut self.settings,
                store: &*self.store,
                platform: self.platform,
                pads: &self.pads,
                deck: self.deck,
                fallback_ui: self.fallback_ui,
                device_name: &self.device_name,
                t: self.t0.elapsed().as_secs_f64(),
            };
            if let Some(top) = self.stack.last_mut() {
                if top.edit_key(key, &mut ctx) {
                    return true;
                }
            }
            // Arrows etc. still drive the OSK grid below.
        }
        let editing = self.stack.last().is_some_and(Screen::editing);
        let ev = match key {
            S::Left => MenuEvent::Move(MenuDir::Left),
            S::Right => MenuEvent::Move(MenuDir::Right),
            S::Up => MenuEvent::Move(MenuDir::Up),
            S::Down => MenuEvent::Move(MenuDir::Down),
            S::Return | S::Space if !repeat => MenuEvent::Confirm,
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

    /// One command straight onto the bus — the in-stream ring's host actions, which have no
    /// screen and so no `Outbox`. Only the desktop overlay calls it; the Android console's
    /// ring is the editor alone.
    #[cfg_attr(target_os = "android", allow(dead_code))]
    pub(crate) fn send_cmd(&self, cmd: ConsoleCmd) {
        self.bus.send(cmd);
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
        let words = uniforms.map(f32::to_ne_bytes);
        let bytes = words.as_flattened();
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
