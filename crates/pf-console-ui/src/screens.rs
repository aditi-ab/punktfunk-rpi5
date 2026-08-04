//! The console's screens and their shared contract. Each screen owns its focus state
//! and rendering; the [`crate::shell::Shell`] owns the stack, the transitions, the
//! chrome, and the overlays — a screen never draws its own background or hint bar, so
//! every screen animates and reads identically.

pub(crate) mod add_host;
pub(crate) mod home;
pub(crate) mod library;
pub(crate) mod pair;
pub(crate) mod pin_hosts;
pub(crate) mod settings;

use crate::glyphs::Hint;
use crate::library::LibraryShared;
use crate::model::{ConsoleCmd, HostRow};
use crate::theme::Fonts;
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use pf_client_core::{gamepad::PadInfo, trust};
use skia_safe::{Canvas, Rect};

/// What a screen draws over (the shell crossfades between them on push/pop).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bg {
    /// The living mesh aurora (home, library).
    Aurora,
    /// The quiet indigo form backdrop (settings, add-host, pair).
    Form,
}

/// Everything a screen may read while handling input or rendering. Settings are
/// mutable — the settings screen edits and persists them in place.
pub(crate) struct Ctx<'a> {
    pub hosts: &'a [HostRow],
    /// The one live library model slot (the screen on top of the stack owns it).
    pub library: &'a LibraryShared,
    pub settings: &'a mut trust::Settings,
    pub pads: &'a [PadInfo],
    /// Steam Deck: never draw our keyboard — Steam's types via SDL text input.
    pub deck: bool,
    /// The name the HOST stores this client under when pairing (the machine's
    /// hostname, resolved by the binary).
    pub device_name: &'a str,
    /// The shell clock, seconds (spinners, pulses).
    pub t: f64,
}

/// A host a screen wants to start a session on (the shell turns this into an
/// `OverlayAction::Launch` + the connecting overlay).
pub(crate) struct ConnectIntent {
    pub addr: String,
    pub port: u16,
    pub fp_hex: String,
    /// Library title id (`None` streams the desktop).
    pub launch: Option<String>,
    /// What the connecting card says (host or game title).
    pub title: String,
    /// The no-PIN delegated-approval connect (the pair screen's "Request access"): the
    /// shell shows a "waiting for approval" takeover instead of "connecting", and the
    /// binary parks on a long budget and persists the host as paired once let in.
    pub request_access: bool,
    /// One-off settings-profile id for this launch (a pinned card's connect); `None`
    /// keeps the host's default binding.
    pub profile: Option<String>,
}

pub(crate) enum Nav {
    Push(Box<Screen>),
    /// Pop this screen; popping the root quits the console.
    Pop,
}

/// Everything a screen's input handling may ask of the shell, collected per event and
/// applied AFTER the dispatch (no re-entrant stack mutation).
#[derive(Default)]
pub(crate) struct Outbox {
    pub nav: Option<Nav>,
    pub connect: Option<ConnectIntent>,
    pub cmds: Vec<ConsoleCmd>,
    pub toast: Option<String>,
}

impl Outbox {
    pub(crate) fn push(&mut self, screen: Screen) {
        self.nav = Some(Nav::Push(Box::new(screen)));
    }

    pub(crate) fn pop(&mut self) {
        self.nav = Some(Nav::Pop);
    }
}

pub(crate) enum Screen {
    Home(home::HomeScreen),
    Library(library::LibraryScreen),
    Settings(settings::SettingsScreen),
    AddHost(add_host::AddHostScreen),
    Pair(pair::PairScreen),
    PinHosts(pin_hosts::PinHostsScreen),
}

impl Screen {
    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match self {
            Screen::Home(s) => s.menu(ev, ctx, fx),
            Screen::Library(s) => s.menu(ev, ctx, fx),
            Screen::Settings(s) => s.menu(ev, ctx, fx),
            Screen::AddHost(s) => s.menu(ev, ctx, fx),
            Screen::Pair(s) => s.menu(ev, ctx, fx),
            Screen::PinHosts(s) => s.menu(ev, ctx, fx),
        }
    }

    /// Committed text (SDL `TextInput` — hardware keyboards everywhere, Steam's
    /// keyboard under gamescope). Only the editing screens consume it.
    pub(crate) fn text_input(&mut self, text: &str) {
        match self {
            Screen::AddHost(s) => s.text_input(text),
            Screen::Pair(s) => s.text_input(text),
            _ => {}
        }
    }

    /// Raw key edits while a field is editing (Backspace repeats, Return = done).
    /// Returns true when consumed.
    pub(crate) fn edit_key(&mut self, sc: sdl3::keyboard::Scancode) -> bool {
        match self {
            Screen::AddHost(s) => s.edit_key(sc),
            Screen::Pair(s) => s.edit_key(sc),
            _ => false,
        }
    }

    /// A text field is being edited — the run loop keeps SDL text input started.
    pub(crate) fn editing(&self) -> bool {
        match self {
            Screen::AddHost(s) => s.editing(),
            Screen::Pair(s) => s.editing(),
            _ => false,
        }
    }

    pub(crate) fn background(&self) -> Bg {
        match self {
            Screen::Home(_) | Screen::Library(_) => Bg::Aurora,
            _ => Bg::Form,
        }
    }

    pub(crate) fn title(&self, _ctx: &Ctx) -> String {
        match self {
            Screen::Home(_) => "Select a Host".into(),
            Screen::Library(s) => s.host_name().to_string(),
            Screen::Settings(_) => "Settings".into(),
            Screen::AddHost(_) => "Add Host".into(),
            Screen::Pair(s) => format!("Pair with {}", s.host_name()),
            Screen::PinHosts(s) => format!("Pin \u{201c}{}\u{201d}", s.profile_name()),
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        match self {
            Screen::Home(s) => s.hints(ctx),
            Screen::Library(s) => s.hints(ctx),
            Screen::Settings(s) => s.hints(ctx),
            Screen::AddHost(s) => s.hints(ctx),
            Screen::Pair(s) => s.hints(ctx),
            Screen::PinHosts(s) => s.hints(ctx),
        }
    }

    /// Render the screen's content into `rect` (between the title bar and hint bar).
    /// Backgrounds and chrome are the shell's.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        match self {
            Screen::Home(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Library(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Settings(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::AddHost(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Pair(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PinHosts(s) => s.render(canvas, rect, k, dt, fonts, ctx),
        }
    }
}
