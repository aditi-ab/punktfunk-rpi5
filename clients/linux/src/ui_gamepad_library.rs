//! The gamepad library launcher (`--browse` — the Apple client's console UI ported):
//! a chrome-less, controller-driven coverflow of the host's game library over a drifting
//! "aurora" backdrop. A launches the focused title (the id rides the Hello), B quits back
//! to Gaming Mode, L1/R1 jump. Scope is deliberately library-only — host selection and
//! settings stay in the touch UI; the Decky plugin owns pairing (`--pair`).
//!
//! Input is the gamepad service's menu mode (`gamepad::MenuEvent` over an async channel,
//! same pattern as the stream page's escape events) plus a keyboard fallback
//! (arrows/Enter/Esc), so the launcher is fully drivable with no pad — that's also how CI
//! and the GPU-less dev VM exercise it. Zero popovers/dialogs anywhere: gamescope never
//! maps them (see `ui_settings::gamescope_session`) — every state renders in-page.

use crate::app::App;
use crate::gamepad::{MenuDir, MenuEvent, MenuPulse};
use crate::library::{self, GameEntry};
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use adw::prelude::*;
use gtk::{cairo, gdk, glib, graphene, gsk};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

/// Poster geometry: 2:3 covers, sized so the focused poster + detail panel + hint bar fit
/// a Deck's 1280×800 with air.
const POSTER_W: i32 = 220;
const POSTER_H: i32 = 330;
/// Center of the focused card to the center of its first neighbor — the "breathing room"
/// gap around the focus.
const FOCUS_GAP: f64 = 230.0;
/// Center-to-center distance between successive SIDE cards — much tighter than their
/// projected width, so the side stacks overlap like the classic coverflow shelf.
/// Overlap needs paint order: [`restack`] keeps cards closer to the cursor on top
/// (`gtk::Fixed` paints in child order).
const SIDE_SPACING: f64 = 104.0;
/// Cards farther than this from the eased position aren't laid out at all.
const VISIBLE_RANGE: f64 = 5.5;
/// Neighbors recede to this scale… (Apple coverflow parity).
const RECEDE_SCALE: f64 = 0.24;
/// …and swing this many degrees about their own vertical axis under perspective — the
/// actual coverflow tilt (Apple's ±38°), side cards facing the corridor (their inner edge
/// recedes behind the focus). Driven continuously by distance-from-center, so a card
/// flips through flat exactly as it crosses the focus point mid-animation.
const ROTATE_DEG: f64 = 38.0;
/// Perspective depth for the tilt, px — smaller is more dramatic (CSS `perspective()`
/// semantics; per-card vanishing point like Apple's rotation3DEffect).
const PERSPECTIVE: f32 = 800.0;
/// Side cards stay fully opaque — they OVERLAP, and any whole-card translucency bleeds
/// the stack through the card on top; the darkening veil (opaque black, inside the card)
/// carries the entire recede cue.
const RECEDE_DIM: f64 = 0.30;
/// Boundary recoil: a refused move deflects the strip this many px against the push.
const BUMP_PX: f64 = 16.0;
/// L1/R1 jump distance (Apple parity).
const JUMP: i32 = 5;

// The motion is spring-driven (semi-implicit Euler), not eased — velocity carries across
// retargets, so holding a direction glides and a release settles like a detent instead of
// a lerp. Damping = 2·ζ·√k; dt is clamped far inside the integrator's stability bound.
/// Cursor chase: ζ ≈ 0.85 — settles in ~0.3 s with a whisker of overshoot.
const SPRING_K: f64 = 200.0;
const SPRING_C: f64 = 24.0;
/// Boundary recoil: stiffer and more underdamped (ζ ≈ 0.55) — one visible wobble.
const BUMP_K: f64 = 600.0;
const BUMP_C: f64 = 27.0;

/// Everything the launcher re-renders from. Kept alive by `App::browse` for the app's
/// lifetime (browse mode never pops this page — streams push on top and return).
struct State {
    app: Rc<App>,
    /// The browse-target host; cards clone it and add `launch`. `fp_hex` is the stored
    /// pin (browse requires a paired host — enforced before any fetch).
    req: ConnectRequest,
    paired: bool,
    mgmt_port: u16,
    root: gtk::Overlay,
    stack: gtk::Stack,
    // Carousel: the integer cursor is the navigation authority (Apple pattern); the
    // eased float position chases it on a frame-clock tick.
    fixed: gtk::Fixed,
    /// The viewport the strip is centered on. `gtk::Fixed` measures its TRANSFORMED
    /// children, so far-out cards would otherwise inflate the whole page's minimum width
    /// past the window (observed: the top bar allocated wider than the screen, chip
    /// off-glass). A ScrolledWindow with External policy exists to swallow exactly that;
    /// it's `can_target(false)` so touch/wheel can never actually scroll the strip.
    scroller: gtk::ScrolledWindow,
    cards: RefCell<Vec<Card>>,
    cursor: Cell<i32>,
    anim_pos: Cell<f64>,
    anim_vel: Cell<f64>,
    bump: Cell<f64>,
    bump_vel: Cell<f64>,
    anim_active: Cell<bool>,
    last_tick: Cell<i64>,
    animations: bool,
    detail_title: gtk::Label,
    detail_store: gtk::Label,
    /// Transient error strip on the carousel scene (connect failures land here — the
    /// library is still loaded, so no scene swap).
    status: gtk::Label,
    error_title: gtk::Label,
    error_body: gtk::Label,
    hints: gtk::Box,
    chip: gtk::Label,
    games: RefCell<Vec<GameEntry>>,
    /// Poster cache (entry id → texture) — survives re-renders without refetching.
    art: RefCell<HashMap<String, gdk::Texture>>,
    /// The Picture each entry currently renders into, so async art lands on the right card.
    pics: RefCell<HashMap<String, gtk::Picture>>,
    /// The error scene's A action retries the fetch (fetch errors only — not unpaired).
    can_retry: Cell<bool>,
    /// A connect is in flight — hint bar shows the spinner, menu input is parked.
    connecting: Cell<bool>,
    /// Screenshot mode: render injected entries only, never touch network or gamepads.
    mock: Cell<bool>,
}

struct Card {
    root: gtk::Overlay,
    dim: gtk::Box,
}

/// The launcher page handle, held in `App::browse`.
pub struct LauncherUi {
    pub page: adw::NavigationPage,
    state: Rc<State>,
}

impl LauncherUi {
    /// A session that started from here ended: restore the hint bar, re-grab keyboard
    /// focus (menu mode never turned off — the gamepad worker re-snapshotted on detach).
    pub fn on_session_ended(&self) {
        self.state.connecting.set(false);
        show_scene(
            &self.state,
            current_scene(&self.state).as_deref().unwrap_or(""),
        );
        update_chip(&self.state);
        self.state.root.grab_focus();
    }

    /// Surface a connect/session error. With the library on screen the carousel stays put
    /// and the message lands on the transient status strip; otherwise the error scene.
    pub fn show_error(&self, msg: &str) {
        let state = &self.state;
        state.connecting.set(false);
        if current_scene(state).as_deref() == Some("carousel") {
            show_transient_error(state, msg);
            show_scene(state, "carousel");
        } else {
            state.error_title.set_text("Couldn't connect");
            state.error_body.set_text(msg);
            state.can_retry.set(false);
            show_scene(state, "error");
        }
    }
}

/// Open the launcher for a saved host and start the fetch. `paired` gates everything: an
/// unpaired target renders the pair-first scene instead (pairing is the plugin's job — no
/// ceremony can run under gamescope).
pub fn open(app: Rc<App>, req: ConnectRequest, paired: bool, mgmt_port: u16) -> Rc<LauncherUi> {
    let ui = build(app.clone(), req, paired, mgmt_port);
    app.gamepad.set_menu_mode(true);
    attach_menu_input(&ui.state);
    // The fake-library dev hook exists precisely for host-less/pairing-less UI work.
    if paired || std::env::var_os("PUNKTFUNK_FAKE_LIBRARY").is_some() {
        load(&ui.state);
    } else {
        ui.state.error_title.set_text("Not paired with this host");
        ui.state
            .error_body
            .set_text("Pair from the Punktfunk plugin first.");
        ui.state.can_retry.set(false);
        show_scene(&ui.state, "error");
    }
    ui
}

/// Screenshot-scene entry: render injected entries (plus pre-seeded textures, keyed by
/// entry id) with no host, no network, and no gamepad service — the CI `gamepad-library`
/// scene. The cursor starts at 1 so both recede directions are visible.
pub fn open_mock(
    app: Rc<App>,
    req: ConnectRequest,
    games: Vec<GameEntry>,
    art: Vec<(String, gdk::Texture)>,
) -> Rc<LauncherUi> {
    let ui = build(app, req, true, library::DEFAULT_MGMT_PORT);
    ui.state.mock.set(true);
    ui.state.art.borrow_mut().extend(art);
    if games.is_empty() {
        show_scene(&ui.state, "empty");
    } else {
        ui.state.cursor.set(1.min(games.len() as i32 - 1));
        *ui.state.games.borrow_mut() = games;
        render(&ui.state);
        show_scene(&ui.state, "carousel");
    }
    ui
}

fn build(app: Rc<App>, req: ConnectRequest, paired: bool, mgmt_port: u16) -> Rc<LauncherUi> {
    // Scene: loading.
    let loading = gtk::Box::new(gtk::Orientation::Vertical, 12);
    loading.set_valign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.start();
    spinner.set_halign(gtk::Align::Center);
    loading.append(&spinner);
    let loading_label = gtk::Label::new(Some("Loading library…"));
    loading_label.add_css_class("pf-gl-hint");
    loading.append(&loading_label);

    // Scene: error (fetch failures, the unpaired gate, off-carousel connect errors).
    let error_title = gtk::Label::new(None);
    error_title.add_css_class("pf-gl-error-title");
    let error_body = gtk::Label::new(None);
    error_body.add_css_class("pf-gl-hint");
    error_body.set_wrap(true);
    error_body.set_max_width_chars(60);
    error_body.set_justify(gtk::Justification::Center);
    let error = gtk::Box::new(gtk::Orientation::Vertical, 10);
    error.set_valign(gtk::Align::Center);
    error.append(&error_title);
    error.append(&error_body);

    // Scene: empty.
    let empty_title = gtk::Label::new(Some("No games found"));
    empty_title.add_css_class("pf-gl-error-title");
    let empty_body = gtk::Label::new(Some(
        "Install Steam titles or add custom entries in the host's web console.",
    ));
    empty_body.add_css_class("pf-gl-hint");
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 10);
    empty.set_valign(gtk::Align::Center);
    empty.append(&empty_title);
    empty.append(&empty_body);

    // Scene: the carousel + detail panel.
    let fixed = gtk::Fixed::new();
    fixed.set_vexpand(true);
    fixed.set_hexpand(true);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&fixed)
        .hexpand(true)
        .vexpand(true)
        .build();
    scroller.set_can_target(false);
    let detail_title = gtk::Label::new(Some(" "));
    detail_title.add_css_class("pf-gl-detail-title");
    detail_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let detail_store = gtk::Label::new(Some(" "));
    detail_store.add_css_class("pf-gl-detail-store");
    let status = gtk::Label::new(None);
    status.add_css_class("pf-gl-status");
    status.set_wrap(true);
    status.set_visible(false);
    let detail = gtk::Box::new(gtk::Orientation::Vertical, 4);
    detail.set_halign(gtk::Align::Center);
    detail.set_margin_bottom(12);
    detail.append(&detail_title);
    detail.append(&detail_store);
    detail.append(&status);
    let carousel_scene = gtk::Box::new(gtk::Orientation::Vertical, 0);
    carousel_scene.append(&scroller);
    carousel_scene.append(&detail);

    let stack = gtk::Stack::new();
    stack.set_vexpand(true);
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&error, Some("error"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&carousel_scene, Some("carousel"));

    // Chrome: host label + controller chip on top, button hints at the bottom.
    let host_label = gtk::Label::new(Some(&req.name));
    host_label.add_css_class("pf-gl-host");
    host_label.set_hexpand(true);
    host_label.set_halign(gtk::Align::Start);
    let chip = gtk::Label::new(None);
    chip.add_css_class("pf-gl-chip");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    top.set_margin_top(18);
    top.set_margin_start(24);
    top.set_margin_end(24);
    top.append(&host_label);
    top.append(&chip);

    let hints = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    hints.set_margin_bottom(16);
    hints.set_margin_start(24);
    hints.set_halign(gtk::Align::Start);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&top);
    content.append(&stack);
    content.append(&hints);

    let root = gtk::Overlay::new();
    root.add_css_class("pf-gl-page");
    root.set_child(Some(&build_aurora()));
    root.add_overlay(&content);
    root.set_focusable(true);

    let page = adw::NavigationPage::builder()
        .title(req.name.clone())
        .tag("launcher")
        .child(&root)
        .build();

    let state = Rc::new(State {
        app,
        req,
        paired,
        mgmt_port,
        root: root.clone(),
        stack,
        fixed,
        scroller,
        cards: RefCell::new(Vec::new()),
        cursor: Cell::new(0),
        anim_pos: Cell::new(0.0),
        anim_vel: Cell::new(0.0),
        bump: Cell::new(0.0),
        bump_vel: Cell::new(0.0),
        anim_active: Cell::new(false),
        last_tick: Cell::new(0),
        animations: animations_enabled(),
        detail_title,
        detail_store,
        status,
        error_title,
        error_body,
        hints,
        chip,
        games: RefCell::new(Vec::new()),
        art: RefCell::new(HashMap::new()),
        pics: RefCell::new(HashMap::new()),
        can_retry: Cell::new(false),
        connecting: Cell::new(false),
        mock: Cell::new(false),
    });

    // Keyboard fallback — same event vocabulary through the same handler, so the launcher
    // is fully drivable padless (dev VM, CI).
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let weak = Rc::downgrade(&state);
        key.connect_key_pressed(move |_, keyval, _, _| {
            let Some(state) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            use gtk::gdk::Key;
            let ev = match keyval {
                Key::Left => MenuEvent::Move(MenuDir::Left),
                Key::Right => MenuEvent::Move(MenuDir::Right),
                Key::Up => MenuEvent::Move(MenuDir::Up),
                Key::Down => MenuEvent::Move(MenuDir::Down),
                Key::Return | Key::KP_Enter | Key::space => MenuEvent::Confirm,
                Key::Escape | Key::BackSpace => MenuEvent::Back,
                Key::Page_Up => MenuEvent::JumpBack,
                Key::Page_Down => MenuEvent::JumpForward,
                _ => return glib::Propagation::Proceed,
            };
            handle_menu_event(&state, ev);
            glib::Propagation::Stop
        });
    }
    root.add_controller(key);
    {
        let root = root.clone();
        root.clone().connect_map(move |_| {
            root.grab_focus();
        });
    }

    // The aurora area resizes with the page — piggyback the carousel relayout on it
    // (gtk::Fixed has no resize signal of its own).
    if let Some(aurora) = root.child().and_downcast::<gtk::DrawingArea>() {
        let weak = Rc::downgrade(&state);
        aurora.connect_resize(move |_, _, _| {
            if let Some(state) = weak.upgrade() {
                kick_anim(&state);
            }
        });
    }

    update_chip(&state);
    // The chip tracks pad hot-plug lazily — nothing else needs a poll.
    {
        let weak = Rc::downgrade(&state);
        glib::timeout_add_seconds_local(2, move || match weak.upgrade() {
            Some(state) => {
                update_chip(&state);
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
    }

    Rc::new(LauncherUi { page, state })
}

/// Menu events from the gamepad worker → the same handler the keyboard feeds.
fn attach_menu_input(state: &Rc<State>) {
    let rx = state.app.gamepad.menu_events();
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            let Some(state) = weak.upgrade() else { break };
            handle_menu_event(&state, ev);
        }
    });
}

fn current_scene(state: &State) -> Option<glib::GString> {
    state.stack.visible_child_name()
}

/// Route one menu action by scene. Parked while a connect is in flight or a stream page
/// owns the app (`busy` — the worker also stops translating once attached; this covers
/// the connect window before attach).
fn handle_menu_event(state: &Rc<State>, ev: MenuEvent) {
    if state.app.busy.get() || state.connecting.get() {
        return;
    }
    match current_scene(state).as_deref() {
        Some("carousel") => match ev {
            MenuEvent::Move(MenuDir::Left) => step(state, -1, false),
            MenuEvent::Move(MenuDir::Right) => step(state, 1, false),
            MenuEvent::JumpBack => step(state, -JUMP, true),
            MenuEvent::JumpForward => step(state, JUMP, true),
            MenuEvent::Confirm => launch_selected(state),
            MenuEvent::Back => quit(state),
            // Single row: up/down are neither moves nor boundaries (Apple parity).
            MenuEvent::Move(_) | MenuEvent::Secondary | MenuEvent::Tertiary => {}
        },
        Some("error") => match ev {
            MenuEvent::Confirm if state.can_retry.get() => load(state),
            MenuEvent::Back => quit(state),
            _ => {}
        },
        Some("empty" | "loading") if ev == MenuEvent::Back => quit(state),
        _ => {}
    }
}

/// One semi-implicit-Euler step of a damped spring toward `target`: acceleration from
/// displacement and damping, velocity integrated before position — the standard stable
/// discretization.
fn spring_step(pos: f64, vel: f64, target: f64, k: f64, c: f64, dt: f64) -> (f64, f64) {
    let vel = vel + (k * (target - pos) - c * vel) * dt;
    (pos + vel * dt, vel)
}

/// Advance a damped spring by a whole frame, integrating in ≤ 8 ms substeps (unit-tested
/// for convergence and long-frame stability). A stalled frame (dt clamped to 50 ms) would
/// put the stiff bump spring at ω·dt ≈ 1.2 — inside the integrator's formal stability
/// bound but distorted enough to ring for ages; substeps keep ω·dt ≈ 0.2, so the motion
/// feels identical at any frame rate.
fn spring_advance(mut pos: f64, mut vel: f64, target: f64, k: f64, c: f64, dt: f64) -> (f64, f64) {
    let n = (dt / 0.008).ceil().max(1.0) as usize;
    let h = dt / n as f64;
    for _ in 0..n {
        (pos, vel) = spring_step(pos, vel, target, k, c, h);
    }
    (pos, vel)
}

/// Pure cursor arithmetic for a move/jump (unit-tested): `clamp` lands jumps on the ends,
/// a plain step refuses to leave them.
#[derive(Debug, PartialEq, Eq)]
enum StepResult {
    Moved(i32),
    Boundary,
}

fn step_cursor(cursor: i32, len: usize, delta: i32, clamp: bool) -> StepResult {
    if len == 0 {
        return StepResult::Boundary;
    }
    let max = len as i32 - 1;
    let target = if clamp {
        (cursor + delta).clamp(0, max)
    } else {
        cursor + delta
    };
    if target == cursor || target < 0 || target > max {
        StepResult::Boundary
    } else {
        StepResult::Moved(target)
    }
}

fn step(state: &Rc<State>, delta: i32, clamp: bool) {
    let len = state.games.borrow().len();
    match step_cursor(state.cursor.get(), len, delta, clamp) {
        StepResult::Moved(to) => {
            state.cursor.set(to);
            state.app.gamepad.menu_rumble(MenuPulse::Move);
            update_detail(state);
            restack(state);
            kick_anim(state);
        }
        StepResult::Boundary => {
            // Recoil against the push: advancing shifts cards left, so a refused
            // right-push deflects left (and vice versa); the bump spring wobbles it back.
            state.bump.set(-BUMP_PX * f64::from(delta.signum()));
            state.bump_vel.set(0.0);
            state.app.gamepad.menu_rumble(MenuPulse::Boundary);
            kick_anim(state);
        }
    }
}

/// A on the focused poster: request the session with the library id riding the Hello.
/// Direct `launch::start_session` — trust is already established (browse requires the
/// stored pin) and every other `initiate_connect` branch opens a dialog gamescope can't map.
fn launch_selected(state: &Rc<State>) {
    if state.mock.get() {
        return;
    }
    let (id, title) = {
        let games = state.games.borrow();
        let Some(g) = games.get(state.cursor.get() as usize) else {
            return;
        };
        (g.id.clone(), g.title.clone())
    };
    state.app.gamepad.menu_rumble(MenuPulse::Confirm);
    state.status.set_visible(false);
    state.connecting.set(true);
    show_scene(state, "carousel");
    let mut req = state.req.clone();
    req.launch = Some((id, title));
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    crate::launch::start_session(state.app.clone(), req, pin);
}

/// B at the launcher root: the app IS the "game" — closing it returns the Deck to
/// Gaming Mode (same mechanism as `quit_on_session_end`).
fn quit(state: &Rc<State>) {
    state.app.window.close();
}

/// Fetch the library off the main thread and route the result into a scene (the
/// `ui_library::load` pattern). `PUNKTFUNK_FAKE_LIBRARY=<file.json>` short-circuits with
/// entries from disk — the GPU-less/pairing-less dev path.
fn load(state: &Rc<State>) {
    if state.mock.get() {
        return;
    }
    if let Ok(path) = std::env::var("PUNKTFUNK_FAKE_LIBRARY") {
        load_fake(state, &path);
        return;
    }
    if !state.paired {
        return;
    }
    show_scene(state, "loading");
    let addr = state.req.addr.clone();
    let port = state.mgmt_port;
    let identity = state.app.identity.clone();
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("punktfunk-library".into())
        .spawn(move || {
            let _ = tx.send_blocking(library::fetch_games(&addr, port, &identity, pin));
        })
        .expect("spawn library thread");
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let Ok(result) = rx.recv().await else { return };
        let Some(state) = weak.upgrade() else { return };
        match result {
            Ok(games) if games.is_empty() => show_scene(&state, "empty"),
            Ok(games) => {
                state.cursor.set(0);
                *state.games.borrow_mut() = games;
                render(&state);
                show_scene(&state, "carousel");
                load_art(&state);
            }
            Err(e) => {
                state.error_title.set_text("Couldn't load the library");
                state.error_body.set_text(&e.to_string());
                state.can_retry.set(true);
                show_scene(&state, "error");
            }
        }
    });
}

/// Dev hook: entries from a JSON file; portrait paths starting with `/` load from disk.
fn load_fake(state: &Rc<State>, path: &str) {
    let games: Vec<GameEntry> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if games.is_empty() {
        show_scene(state, "empty");
        return;
    }
    {
        let mut art = state.art.borrow_mut();
        for g in &games {
            if let Some(p) = g.art.portrait.as_deref().filter(|p| p.starts_with('/')) {
                if let Ok(bytes) = std::fs::read(p) {
                    if let Ok(tex) = gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                        art.insert(g.id.clone(), tex);
                    }
                }
            }
        }
    }
    state.cursor.set(0);
    *state.games.borrow_mut() = games;
    render(state);
    show_scene(state, "carousel");
}

/// (Re)build the poster cards from the current games snapshot and lay them out.
fn render(state: &Rc<State>) {
    while let Some(child) = state.fixed.first_child() {
        state.fixed.remove(&child);
    }
    state.pics.borrow_mut().clear();
    let games = state.games.borrow();
    let mut cards = Vec::with_capacity(games.len());
    for game in games.iter() {
        let card = build_card(state, game);
        state.fixed.put(&card.root, 0.0, 0.0);
        cards.push(card);
    }
    drop(games);
    *state.cards.borrow_mut() = cards;
    update_detail(state);
    // Snap the sprung position onto the cursor — a fresh render has no old position to
    // animate from.
    state.anim_pos.set(f64::from(state.cursor.get()));
    state.anim_vel.set(0.0);
    restack(state);
    kick_anim(state);
}

/// One coverflow card: monogram placeholder → async poster → store badge, plus the black
/// `dim` veil whose opacity implements the brightness recede.
fn build_card(state: &Rc<State>, game: &GameEntry) -> Card {
    let monogram = gtk::Label::new(Some(&crate::ui_library::initials(&game.title)));
    monogram.add_css_class("pf-poster-monogram");
    monogram.set_halign(gtk::Align::Center);
    monogram.set_valign(gtk::Align::Center);
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    placeholder.append(&monogram);
    monogram.set_vexpand(true);

    let pic = gtk::Picture::new();
    pic.set_content_fit(gtk::ContentFit::Cover);
    if let Some(tex) = state.art.borrow().get(&game.id) {
        pic.set_paintable(Some(tex));
    }
    state.pics.borrow_mut().insert(game.id.clone(), pic.clone());

    let badge = gtk::Label::new(Some(crate::ui_library::store_label(&game.store)));
    badge.add_css_class("pf-pill");
    badge.add_css_class("pf-store-badge");
    badge.set_halign(gtk::Align::Start);
    badge.set_valign(gtk::Align::Start);
    badge.set_margin_start(8);
    badge.set_margin_top(8);

    let dim = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dim.add_css_class("pf-gl-dim");
    dim.set_opacity(0.0);
    dim.set_can_target(false);

    let root = gtk::Overlay::new();
    root.set_child(Some(&placeholder));
    root.add_overlay(&pic);
    root.add_overlay(&badge);
    root.add_overlay(&dim);
    root.add_css_class("pf-gl-poster");
    root.set_overflow(gtk::Overflow::Hidden);
    root.set_size_request(POSTER_W, POSTER_H);
    Card { root, dim }
}

/// Fetch poster art for every uncached entry (shared worker pool) and texture the cards
/// as results land.
fn load_art(state: &Rc<State>) {
    let base = library::base_url(&state.req.addr, state.mgmt_port);
    let jobs: VecDeque<(String, Vec<String>)> = {
        let cache = state.art.borrow();
        state
            .games
            .borrow()
            .iter()
            .filter(|g| !cache.contains_key(&g.id))
            .map(|g| (g.id.clone(), g.art.poster_candidates(&base)))
            .filter(|(_, candidates)| !candidates.is_empty())
            .collect()
    };
    if jobs.is_empty() {
        return;
    }
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let rx = library::spawn_art_fetch(base, state.app.identity.clone(), pin, jobs);
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        while let Ok((id, bytes)) = rx.recv().await {
            let Some(state) = weak.upgrade() else { break };
            match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                Ok(tex) => {
                    if let Some(pic) = state.pics.borrow().get(&id) {
                        pic.set_paintable(Some(&tex));
                    }
                    state.art.borrow_mut().insert(id, tex);
                }
                Err(e) => tracing::debug!(%id, error = %e, "undecodable poster"),
            }
        }
    });
}

/// The focused title + store tag under the strip — updated synchronously on every move
/// (the scroll chases; Apple parity). Single spaces keep the layout from jumping when
/// there's nothing to show.
fn update_detail(state: &State) {
    let games = state.games.borrow();
    match games.get(state.cursor.get() as usize) {
        Some(g) => {
            state.detail_title.set_text(&g.title);
            state
                .detail_store
                .set_text(&crate::ui_library::store_label(&g.store).to_uppercase());
        }
        None => {
            state.detail_title.set_text(" ");
            state.detail_store.set_text(" ");
        }
    }
}

fn update_chip(state: &State) {
    match state.app.gamepad.active() {
        Some(p) => state.chip.set_text(&p.name),
        None => state.chip.set_text("No controller — keyboard works too"),
    }
}

/// Swap the visible scene and set its hint bar. Hints double as the connect affordance.
fn show_scene(state: &Rc<State>, scene: &str) {
    if !scene.is_empty() {
        state.stack.set_visible_child_name(scene);
    }
    let hints = &state.hints;
    while let Some(c) = hints.first_child() {
        hints.remove(&c);
    }
    if state.connecting.get() {
        let spinner = gtk::Spinner::new();
        spinner.start();
        hints.append(&spinner);
        let label = gtk::Label::new(Some("Connecting…"));
        label.add_css_class("pf-gl-hint");
        hints.append(&label);
        return;
    }
    let items: &[(&str, &str)] = match current_scene(state).as_deref() {
        Some("carousel") => &[("A", "Play"), ("B", "Quit"), ("L1 · R1", "Jump")],
        Some("error") if state.can_retry.get() => &[("A", "Retry"), ("B", "Quit")],
        _ => &[("B", "Quit")],
    };
    for (glyph, text) in items {
        let g = gtk::Label::new(Some(glyph));
        g.add_css_class("pf-gl-glyph");
        let t = gtk::Label::new(Some(text));
        t.add_css_class("pf-gl-hint");
        let pair = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        pair.append(&g);
        pair.append(&t);
        hints.append(&pair);
    }
}

/// A connect failure with the carousel on screen: an inline strip, cleared after a few
/// seconds (never a dialog).
fn show_transient_error(state: &Rc<State>, msg: &str) {
    state.status.set_text(msg);
    state.status.set_visible(true);
    let weak = Rc::downgrade(state);
    glib::timeout_add_seconds_local_once(6, move || {
        if let Some(state) = weak.upgrade() {
            state.status.set_visible(false);
        }
    });
}

// ---- Carousel layout + animation ------------------------------------------------------

/// Start (or fast-path) the layout animation on a frame-clock tick: two damped springs —
/// the strip position chasing the cursor and the boundary bump returning to rest — and
/// the tick uninstalls itself once both settle, so the launcher idles at zero layout
/// work. Retargeting mid-flight just moves the spring's goal; velocity carries over, so
/// held-repeat scrolling glides instead of restarting a curve every step. Always
/// tick-driven (even with animations off): the first kick usually lands before
/// `gtk::Fixed` has an allocation (built → rendered → THEN pushed/mapped), so the tick
/// waits for a real size instead of laying out into 0×0 and leaving every card stacked
/// at the origin.
fn kick_anim(state: &Rc<State>) {
    if state.anim_active.replace(true) {
        return; // tick already running — it picks the new target up
    }
    state.last_tick.set(0);
    let weak = Rc::downgrade(state);
    state.fixed.add_tick_callback(move |_, clock| {
        let Some(state) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if state.scroller.width() <= 0 || state.scroller.height() <= 0 {
            return glib::ControlFlow::Continue; // not allocated yet — wait, don't settle
        }
        if !state.animations {
            state.anim_pos.set(f64::from(state.cursor.get()));
            state.anim_vel.set(0.0);
            state.bump.set(0.0);
            state.bump_vel.set(0.0);
            relayout(&state);
            state.anim_active.set(false);
            return glib::ControlFlow::Break;
        }
        let now = clock.frame_time();
        let last = state.last_tick.replace(now);
        // Clamped well inside the semi-implicit integrator's stability bound (ω·dt < 2;
        // the stiffer spring has ω ≈ 24.5 → dt must stay < 80 ms).
        let dt = if last == 0 {
            1.0 / 60.0
        } else {
            ((now - last) as f64 / 1e6).clamp(0.0, 0.05)
        };
        let target = f64::from(state.cursor.get());
        let (mut pos, mut vel) = spring_advance(
            state.anim_pos.get(),
            state.anim_vel.get(),
            target,
            SPRING_K,
            SPRING_C,
            dt,
        );
        if (target - pos).abs() < 0.001 && vel.abs() < 0.01 {
            pos = target;
            vel = 0.0;
        }
        state.anim_pos.set(pos);
        state.anim_vel.set(vel);
        let (mut bump, mut bvel) = spring_advance(
            state.bump.get(),
            state.bump_vel.get(),
            0.0,
            BUMP_K,
            BUMP_C,
            dt,
        );
        if bump.abs() < 0.3 && bvel.abs() < 4.0 {
            bump = 0.0;
            bvel = 0.0;
        }
        state.bump.set(bump);
        state.bump_vel.set(bvel);
        relayout(&state);
        if pos == target && bump == 0.0 {
            state.anim_active.set(false);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Re-stack the strip's paint order so cards CLOSER to the (integer) cursor draw on top —
/// the dense side stacks overlap and `gtk::Fixed` paints in child order. Runs on cursor
/// changes only (not per frame); re-inserting an existing child just repositions it in
/// the widget list, layout properties (position/transform) are untouched.
fn restack(state: &State) {
    let cards = state.cards.borrow();
    if cards.is_empty() {
        return;
    }
    let cur = state.cursor.get();
    let mut order: Vec<usize> = (0..cards.len()).collect();
    // Farthest first (painted first = bottom); stable, so the equidistant left/right
    // neighbors keep a deterministic order (they never overlap each other anyway).
    order.sort_by_key(|&i| std::cmp::Reverse((i as i32 - cur).abs()));
    let mut prev: Option<gtk::Widget> = None;
    for &i in &order {
        let w: gtk::Widget = cards[i].root.clone().upcast();
        w.insert_after(&state.fixed, prev.as_ref());
        prev = Some(w);
    }
}

/// Place every card from the eased position: center-focused scale/opacity recede, the
/// whole strip offset by the decaying boundary bump. Off-strip cards are hidden.
/// Centering is on the VIEWPORT (the scroller), not the Fixed — the Fixed's own
/// allocation grows with its transformed children (see `State::scroller`).
fn relayout(state: &State) {
    let w = f64::from(state.scroller.width());
    let h = f64::from(state.scroller.height());
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let pos = state.anim_pos.get();
    let bump = state.bump.get();
    for (i, card) in state.cards.borrow().iter().enumerate() {
        let d = i as f64 - pos;
        let a = d.abs();
        if a > VISIBLE_RANGE {
            card.root.set_visible(false);
            continue;
        }
        card.root.set_visible(true);
        let prox = a.min(1.0);
        let scale = 1.0 - prox * RECEDE_SCALE;
        // Coverflow tilt: side cards face the corridor (inner edge receding behind the
        // focus), flipping through flat exactly at the focus point. Rotation is about the
        // card's own vertical center axis, so the transform moves the origin to the card
        // center first and draws centered last.
        let angle = -d.clamp(-1.0, 1.0) * ROTATE_DEG;
        // Piecewise strip layout: a full FOCUS_GAP around the focused card, then the
        // dense side stacks (the classic coverflow shelf).
        let offset = if a <= 1.0 {
            d * FOCUS_GAP
        } else {
            d.signum() * (FOCUS_GAP + (a - 1.0) * SIDE_SPACING)
        };
        let cx = w / 2.0 + offset + bump;
        let cy = h / 2.0;
        card.dim.set_opacity(prox * RECEDE_DIM);
        let transform = gsk::Transform::new()
            .translate(&graphene::Point::new(cx as f32, cy as f32))
            .perspective(PERSPECTIVE)
            .rotate_3d(angle as f32, &graphene::Vec3::y_axis())
            .scale(scale as f32, scale as f32)
            .translate(&graphene::Point::new(
                -(POSTER_W as f32) / 2.0,
                -(POSTER_H as f32) / 2.0,
            ));
        state
            .fixed
            .set_child_transform(&card.root, Some(&transform));
    }
}

// ---- Aurora backdrop -------------------------------------------------------------------

/// Low-res render target for the blob field — radial gradients in software are cheap at
/// 256×160 (< 1 ms) and the bilinear upscale is exactly what a blurry gradient field wants.
const AURORA_W: i32 = 256;
const AURORA_H: i32 = 160;

/// One drifting color blob (the Swift `GamepadScreenBackground` table, verbatim): base
/// position + drift ellipse in unit coordinates, angular speeds in rad/s (30–90 s
/// periods), a breathing radius (fraction of the larger dimension), and the layer opacity.
struct Blob {
    rgb: (f64, f64, f64),
    center: (f64, f64),
    drift: (f64, f64),
    speed: (f64, f64),
    phase: (f64, f64),
    radius: f64,
    breathe: (f64, f64),
    opacity: f64,
}

/// The brand violet, a deeper indigo, a warmer plum, and a cool blue — related hues so
/// the field shifts within one temperature instead of strobing through the rainbow.
const BLOBS: [Blob; 4] = [
    Blob {
        rgb: (0.53, 0.47, 0.96), // brand violet
        center: (0.30, 0.24),
        drift: (0.16, 0.10),
        speed: (0.111, 0.083),
        phase: (0.0, 1.9),
        radius: 0.52,
        breathe: (0.07, 0.061),
        opacity: 0.52,
    },
    Blob {
        rgb: (0.24, 0.20, 0.72), // deep indigo
        center: (0.78, 0.66),
        drift: (0.13, 0.14),
        speed: (0.071, 0.096),
        phase: (2.4, 0.7),
        radius: 0.58,
        breathe: (0.08, 0.049),
        opacity: 0.55,
    },
    Blob {
        rgb: (0.62, 0.30, 0.80), // plum
        center: (0.16, 0.82),
        drift: (0.12, 0.09),
        speed: (0.089, 0.067),
        phase: (4.1, 3.2),
        radius: 0.44,
        breathe: (0.09, 0.078),
        opacity: 0.42,
    },
    Blob {
        rgb: (0.22, 0.38, 0.86), // cool blue
        center: (0.70, 0.12),
        drift: (0.10, 0.08),
        speed: (0.059, 0.104),
        phase: (1.2, 5.0),
        radius: 0.40,
        breathe: (0.06, 0.055),
        opacity: 0.38,
    },
];

/// Animations are wanted unless the desktop disabled them or a screenshot scene needs a
/// deterministic frame (then the field renders once, frozen at t = 0 — reduce-motion parity).
fn animations_enabled() -> bool {
    crate::cli::shot_scene().is_none()
        && gtk::Settings::default().is_none_or(|s| s.is_gtk_enable_animations())
}

/// The full-bleed aurora: a DrawingArea re-rendered at ~30 Hz off the frame clock (the
/// Swift TimelineView cadence — drift is centimeters per minute, display rate would be
/// wasted heat on a couch device).
fn build_aurora() -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    let t = Rc::new(Cell::new(0.0f64));
    let cache = RefCell::new(None::<cairo::ImageSurface>);
    {
        let t = t.clone();
        area.set_draw_func(move |_, cr, w, h| draw_aurora(cr, w, h, t.get(), &cache));
    }
    if animations_enabled() {
        let start = Cell::new(0i64);
        let last = Cell::new(0i64);
        area.add_tick_callback(move |area, clock| {
            let now = clock.frame_time();
            if start.get() == 0 {
                start.set(now);
            }
            if now - last.get() >= 33_000 {
                last.set(now);
                t.set((now - start.get()) as f64 / 1e6);
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
    }
    area
}

/// Black → additive blob field → legibility scrim, composed at low res and blitted
/// scaled. The scrim keeps the title (top) and detail/hints (bottom) on near-black
/// whatever the blobs are doing behind them.
fn draw_aurora(
    cr: &cairo::Context,
    w: i32,
    h: i32,
    t: f64,
    cache: &RefCell<Option<cairo::ImageSurface>>,
) {
    let mut cached = cache.borrow_mut();
    if cached.is_none() {
        *cached = cairo::ImageSurface::create(cairo::Format::ARgb32, AURORA_W, AURORA_H).ok();
    }
    let Some(surf) = cached.as_ref() else {
        let _ = cr.save();
        cr.set_source_rgb(0.0, 0.0, 0.0);
        let _ = cr.paint();
        let _ = cr.restore();
        return;
    };
    {
        let Ok(c) = cairo::Context::new(surf) else {
            return;
        };
        let (fw, fh) = (f64::from(AURORA_W), f64::from(AURORA_H));
        let side = fw.max(fh);
        c.set_source_rgb(0.0, 0.0, 0.0);
        let _ = c.paint();
        c.set_operator(cairo::Operator::Add);
        for b in &BLOBS {
            let x = (b.center.0 + b.drift.0 * (t * b.speed.0 + b.phase.0).sin()) * fw;
            let y = (b.center.1 + b.drift.1 * (t * b.speed.1 + b.phase.1).cos()) * fh;
            let r = side * b.radius * (1.0 + b.breathe.0 * (t * b.breathe.1 + b.phase.0).sin());
            let g = cairo::RadialGradient::new(x, y, 0.0, x, y, r / 2.0);
            g.add_color_stop_rgba(0.0, b.rgb.0, b.rgb.1, b.rgb.2, b.opacity);
            g.add_color_stop_rgba(1.0, b.rgb.0, b.rgb.1, b.rgb.2, 0.0);
            let _ = c.set_source(&g);
            let _ = c.paint();
        }
        c.set_operator(cairo::Operator::Over);
        let scrim = cairo::LinearGradient::new(0.0, 0.0, 0.0, fh);
        scrim.add_color_stop_rgba(0.0, 0.0, 0.0, 0.0, 0.55);
        scrim.add_color_stop_rgba(0.35, 0.0, 0.0, 0.0, 0.15);
        scrim.add_color_stop_rgba(0.65, 0.0, 0.0, 0.0, 0.20);
        scrim.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.60);
        let _ = c.set_source(&scrim);
        let _ = c.paint();
    }
    let _ = cr.save();
    cr.scale(
        f64::from(w) / f64::from(AURORA_W),
        f64::from(h) / f64::from(AURORA_H),
    );
    let _ = cr.set_source_surface(surf, 0.0, 0.0);
    let _ = cr.paint();
    let _ = cr.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_refuses_the_ends() {
        assert_eq!(step_cursor(0, 6, -1, false), StepResult::Boundary);
        assert_eq!(step_cursor(5, 6, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(0, 6, 1, false), StepResult::Moved(1));
        assert_eq!(step_cursor(5, 6, -1, false), StepResult::Moved(4));
    }

    #[test]
    fn jump_clamps_onto_the_ends() {
        assert_eq!(step_cursor(1, 6, -5, true), StepResult::Moved(0));
        assert_eq!(step_cursor(4, 6, 5, true), StepResult::Moved(5));
        // Already at the end: a clamped jump is a boundary, not a no-op move.
        assert_eq!(step_cursor(0, 6, -5, true), StepResult::Boundary);
        assert_eq!(step_cursor(5, 6, 5, true), StepResult::Boundary);
    }

    #[test]
    fn empty_list_is_all_boundary() {
        assert_eq!(step_cursor(0, 0, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(0, 0, -5, true), StepResult::Boundary);
    }

    /// Drive a spring from rest at 0 toward 1 and report (settle step, peak position).
    fn run_spring(k: f64, c: f64, dt: f64, max_steps: usize) -> (Option<usize>, f64) {
        let (mut pos, mut vel) = (0.0f64, 0.0f64);
        let mut peak = 0.0f64;
        for i in 0..max_steps {
            (pos, vel) = spring_advance(pos, vel, 1.0, k, c, dt);
            peak = peak.max(pos);
            if (1.0 - pos).abs() < 0.001 && vel.abs() < 0.01 {
                return (Some(i), peak);
            }
        }
        (None, peak)
    }

    #[test]
    fn cursor_spring_settles_fast_without_visible_overshoot() {
        let (settled, peak) = run_spring(SPRING_K, SPRING_C, 1.0 / 60.0, 600);
        let steps = settled.expect("spring never settled");
        // ~0.3 s at 60 Hz; ζ = 0.85's theoretical overshoot (~0.6 %) is under the settle
        // epsilon, so only bound it — the springy character comes from velocity carry.
        assert!(steps < 45, "settled in {steps} frames (> 0.75 s)");
        assert!(peak < 1.05, "overshoot too big: {peak}");
    }

    #[test]
    fn springs_converge_at_the_clamped_max_frame() {
        // dt is clamped to 50 ms in the tick; spring_advance's substeps must keep both
        // parameter sets convergent and bounded there (a raw 50 ms step would leave the
        // stiff bump spring ringing at ω·dt ≈ 1.2).
        for (k, c) in [(SPRING_K, SPRING_C), (BUMP_K, BUMP_C)] {
            let (settled, peak) = run_spring(k, c, 0.05, 600);
            assert!(settled.is_some(), "k={k}: no convergence at dt=0.05");
            assert!(peak < 1.2, "k={k}: unstable at dt=0.05 (peak {peak})");
        }
    }

    #[test]
    fn bump_spring_returns_to_rest_from_a_deflection() {
        // The boundary recoil starts displaced (±16 px) at zero velocity and must die out.
        let (mut pos, mut vel) = (-BUMP_PX, 0.0f64);
        for _ in 0..600 {
            (pos, vel) = spring_advance(pos, vel, 0.0, BUMP_K, BUMP_C, 1.0 / 60.0);
            if pos.abs() < 0.3 && vel.abs() < 4.0 {
                return;
            }
        }
        panic!("bump never settled (pos {pos}, vel {vel})");
    }
}
