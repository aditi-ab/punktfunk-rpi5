//! The console's screens and their shared contract. Each screen owns its focus state
//! and rendering; the [`crate::shell::Shell`] owns the stack, the transitions, the
//! chrome, and the overlays — a screen never draws its own background or hint bar, so
//! every screen animates and reads identically.

pub(crate) mod add_host;
pub(crate) mod bind_profile;
pub(crate) mod collections;
pub(crate) mod controllers;
pub(crate) mod home;
pub(crate) mod library;
pub(crate) mod options;
pub(crate) mod pair;
pub(crate) mod pin_hosts;
pub(crate) mod settings;

/// The context menu under the name the home carousel still opens it by. Home predates the
/// generalisation and spells both the module and the type "host options"; it is the same
/// screen, and this goes the moment that call site says [`options::OptionsScreen::for_host`].
pub(crate) mod host_options {
    pub(crate) use super::options::OptionsScreen as HostOptionsScreen;

    impl HostOptionsScreen {
        pub(crate) fn new(host: &crate::model::HostRow) -> HostOptionsScreen {
            HostOptionsScreen::for_host(host)
        }
    }
}

use crate::glyphs::Hint;
use crate::library::LibraryShared;
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::theme::Fonts;
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::{menu_nav::PadInfo, trust};
use skia_safe::{Canvas, Rect};

/// What a screen draws over (the shell crossfades between them on push/pop).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bg {
    /// The living mesh aurora at full contrast (home, library).
    Aurora,
    /// The SAME living mesh, calmed — dimmed pools, lifted corners (settings, add-host,
    /// pair). Not a second backdrop: the shell chases one `calm` uniform between the two.
    Form,
}

/// Everything a screen may read while handling input or rendering. Settings are
/// mutable — the settings screen edits and persists them in place.
pub(crate) struct Ctx<'a> {
    pub hosts: &'a [HostRow],
    /// The one live library model slot (the screen on top of the stack owns it).
    pub library: &'a LibraryShared,
    pub settings: &'a mut trust::Settings,
    /// Where `settings` persists to, and where the profile catalog comes from — the host's
    /// store (desktop file, Android snapshot). Screens call `store.save(settings)` after a
    /// mutation and `store.load()` right before one (rebase).
    pub store: &'a dyn crate::store::SettingsStore,
    /// The platform this shell fronts — which settings rows exist, which native screens the
    /// list may open.
    pub platform: crate::platform::Platform,
    pub pads: &'a [PadInfo],
    /// Steam Deck: never draw our keyboard — Steam's types via SDL text input.
    pub deck: bool,
    /// The host app has another interface to fall back to when the console is switched
    /// off (an Android phone/tablet's touch shell) — see
    /// [`crate::shell::ConsoleOptions::fallback_ui`]. Gates the console-off settings row.
    pub fallback_ui: bool,
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
    /// Swap this screen for another, animated as a push. What "Edit\u{2026}" needs: the host
    /// menu has said its piece, and leaving it on the stack would make Back from the editor
    /// land on a menu describing the host as it was BEFORE the edit.
    Replace(Box<Screen>),
}

/// Everything a screen's input handling may ask of the shell, collected per event and
/// applied AFTER the dispatch (no re-entrant stack mutation).
#[derive(Default)]
pub(crate) struct Outbox {
    pub nav: Option<Nav>,
    pub connect: Option<ConnectIntent>,
    pub cmds: Vec<ConsoleCmd>,
    pub toast: Option<String>,
    /// Text for the system clipboard. Rides out to the run loop rather than the command
    /// bus because the clipboard belongs to SDL, which the service thread never touches.
    pub copy: Option<String>,
}

impl Outbox {
    pub(crate) fn push(&mut self, screen: Screen) {
        self.nav = Some(Nav::Push(Box::new(screen)));
    }

    pub(crate) fn pop(&mut self) {
        self.nav = Some(Nav::Pop);
    }

    pub(crate) fn replace(&mut self, screen: Screen) {
        self.nav = Some(Nav::Replace(Box::new(screen)));
    }

    /// Raise the context menu on what the screen is focused on — the console's one door for
    /// per-item actions. The screen names the SUBJECT
    /// ([`options::OptionsScreen::for_host`], [`options::OptionsScreen::for_game`]) and the
    /// menu owns the verbs, so the next action is a row there rather than another button in a
    /// legend that already holds six.
    ///
    /// Two callers: the home carousel's ▲, and the library's X. Both hand it a subject and
    /// neither names the screen variant, which is the point of the door.
    pub(crate) fn options(&mut self, menu: options::OptionsScreen) {
        self.push(Screen::HostOptions(menu));
    }
}

/// A saved host's `punktfunk://` link, built from the STORE so it carries the fingerprint
/// and stable id a screen doesn't hold — the same builder the desktop shells' "Copy link"
/// uses, so a link is identical whichever surface hands it to you. `launch` attaches a
/// library id, which is what makes a game's link a game's link. `None` if the host has left
/// the store since the menu was opened.
pub(crate) fn saved_host_link(
    store: &dyn crate::store::SettingsStore,
    fp_hex: &str,
    addr: &str,
    port: u16,
    profile: Option<&str>,
    launch: Option<&str>,
) -> Option<String> {
    let known = store.known_hosts();
    let host = (!fp_hex.is_empty())
        .then(|| known.find_by_fp(fp_hex))
        .flatten()
        .or_else(|| known.find_by_addr(addr, port))?;
    Some(pf_client_core::deeplink::DeepLink::for_host(host, launch, profile).to_url())
}

/// This row's link — the host itself, with a pinned card's profile when the row is one.
pub(crate) fn host_link(store: &dyn crate::store::SettingsStore, row: &HostRow) -> Option<String> {
    saved_host_link(
        store,
        &row.fp_hex,
        &row.addr,
        row.port,
        row.pin.as_ref().map(|p| p.id.as_str()),
        None,
    )
}

pub(crate) enum Screen {
    Home(home::HomeScreen),
    Library(library::LibraryScreen),
    /// The library's groups, one tile each — the drill-in that turns "group by console"
    /// into somewhere you can actually go.
    Collections(collections::CollectionsScreen),
    Settings(settings::SettingsScreen),
    AddHost(add_host::AddHostScreen),
    Pair(pair::PairScreen),
    PinHosts(pin_hosts::PinHostsScreen),
    /// "Default for <host>": which profile the host's primary tile connects with — the
    /// binding sibling of [`Screen::PinHosts`]'s presentation cards. Raised by the host
    /// menu's "Default profile…" action.
    BindProfile(bind_profile::BindProfileScreen),
    /// "Connected controllers": the attached pads and their identity lines, plus the grants
    /// and tests only the platform can perform. Android-reachable only — the settings row
    /// that opens it is in `settings::row_on`'s Android-only list.
    Controllers(controllers::ControllersScreen),
    /// The context menu: a subject and the actions that apply to it — a host's Wake / Copy
    /// link / Edit / Forget, a title's Copy link — raised by [`Outbox::options`]. It still
    /// carries the host menu's name because [`host_options`] does; both are one rename.
    HostOptions(options::OptionsScreen),
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
            Screen::Collections(s) => s.menu(ev, ctx, fx),
            Screen::Settings(s) => s.menu(ev, ctx, fx),
            Screen::AddHost(s) => s.menu(ev, ctx, fx),
            Screen::Pair(s) => s.menu(ev, ctx, fx),
            Screen::PinHosts(s) => s.menu(ev, ctx, fx),
            Screen::BindProfile(s) => s.menu(ev, ctx, fx),
            Screen::Controllers(s) => s.menu(ev, ctx, fx),
            Screen::HostOptions(s) => s.menu(ev, ctx, fx),
        }
    }

    /// Mouse/touch at a point, in device pixels. `true` = consumed.
    ///
    /// A screen answers `true` for anything landing on its own furniture even when the
    /// press does nothing, so a stray tap can't fall through to a layer underneath; `false`
    /// only for the empty backdrop.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match self {
            Screen::Home(s) => s.pointer(p, ctx, fx),
            Screen::Library(s) => s.pointer(p, ctx, fx),
            Screen::Collections(s) => s.pointer(p, ctx, fx),
            Screen::Settings(s) => s.pointer(p, ctx, fx),
            Screen::AddHost(s) => s.pointer(p, ctx, fx),
            Screen::Pair(s) => s.pointer(p, ctx, fx),
            Screen::PinHosts(s) => s.pointer(p, ctx, fx),
            Screen::BindProfile(s) => s.pointer(p, ctx, fx),
            Screen::Controllers(s) => s.pointer(p, ctx, fx),
            Screen::HostOptions(s) => s.pointer(p, ctx, fx),
        }
    }

    /// Committed text (SDL `TextInput` — hardware keyboards everywhere, Steam's
    /// keyboard under gamescope). Only the editing screens consume it.
    pub(crate) fn text_input(&mut self, text: &str) {
        match self {
            Screen::AddHost(s) => s.text_input(text),
            Screen::Pair(s) => s.text_input(text),
            Screen::Settings(s) => s.text_input(text),
            _ => {}
        }
    }

    /// Raw key edits while a field is editing (Backspace repeats, Return = done).
    /// Returns true when consumed.
    ///
    /// Takes the context because a field can commit into the settings store on close —
    /// the settings screen's typed bitrate does, where add-host and pair only hold text a
    /// later action row reads.
    pub(crate) fn edit_key(&mut self, key: crate::input::Key, ctx: &mut Ctx) -> bool {
        match self {
            Screen::AddHost(s) => s.edit_key(key),
            Screen::Pair(s) => s.edit_key(key),
            Screen::Settings(s) => s.edit_key(key, ctx),
            _ => false,
        }
    }

    /// A text field is being edited — the run loop keeps SDL text input started.
    pub(crate) fn editing(&self) -> bool {
        match self {
            Screen::AddHost(s) => s.editing(),
            Screen::Pair(s) => s.editing(),
            Screen::Settings(s) => s.editing(),
            _ => false,
        }
    }

    pub(crate) fn background(&self) -> Bg {
        match self {
            Screen::Home(_) | Screen::Library(_) | Screen::Collections(_) => Bg::Aurora,
            _ => Bg::Form,
        }
    }

    pub(crate) fn title(&self, _ctx: &Ctx) -> String {
        match self {
            Screen::Home(_) => "Select a Host".into(),
            Screen::Library(s) => s.title(),
            Screen::Collections(s) => s.title(),
            Screen::Settings(_) => "Settings".into(),
            Screen::AddHost(s) => s.title(),
            Screen::Pair(s) => format!("Pair with {}", s.host_name()),
            Screen::PinHosts(s) => format!("Pin \u{201c}{}\u{201d}", s.profile_name()),
            Screen::BindProfile(s) => format!("Default for {}", s.host_name()),
            Screen::Controllers(_) => "Connected controllers".into(),
            Screen::HostOptions(s) => s.title(),
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        match self {
            Screen::Home(s) => s.hints(ctx),
            Screen::Library(s) => s.hints(ctx),
            Screen::Collections(s) => s.hints(ctx),
            Screen::Settings(s) => s.hints(ctx),
            Screen::AddHost(s) => s.hints(ctx),
            Screen::Pair(s) => s.hints(ctx),
            Screen::PinHosts(s) => s.hints(ctx),
            Screen::BindProfile(s) => s.hints(ctx),
            Screen::Controllers(s) => s.hints(ctx),
            Screen::HostOptions(s) => s.hints(ctx),
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
            Screen::Collections(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Settings(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::AddHost(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Pair(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::PinHosts(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::BindProfile(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::Controllers(s) => s.render(canvas, rect, k, dt, fonts, ctx),
            Screen::HostOptions(s) => s.render(canvas, rect, k, dt, fonts, ctx),
        }
    }
}
