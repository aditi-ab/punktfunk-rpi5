//! The console's context menu: something the user is looking at, and the actions that apply
//! to it. One screen, any subject.
//!
//! It arrived as the saved host's own menu — Wake, Copy link, Edit…, Forget — because a
//! renamed machine or an address typed in with a fat thumb stayed wrong forever otherwise,
//! and the tile is where a host is, so the tile is where its actions belong. Generalising it
//! is what stops the console growing a second idiom per verb: the library had "Copy link"
//! wired straight to X, so one action had two shapes, and a console that answers every new
//! action with another face button runs out of buttons long before it runs out of actions.
//! Here a screen names a SUBJECT and the menu owns the verbs, which makes the next one — hide
//! a title, add it to Steam, override its profile — a row in [`OptionsScreen::actions`].
//!
//! Which button raises it is per screen, because the screens differ; only the WORD is
//! load-bearing, and both legends say "Options". Home's carousel is horizontal, so up is the
//! one free direction and ▲ opens it — the gesture the Android console already teaches. The
//! library spends up on grid rows, so there it is X, which is free precisely because copying
//! a link stopped being a button. X also keeps the menu reachable with a mouse: the hint bar
//! turns face-button hints into presses, and ▲ only became one of those alongside this change.
//!
//! A pinned profile card offers only Unpin: it is a shortcut, not a second host, and offering
//! to forget the host from it would blur precisely the distinction a pin exists to draw.

use crate::glyphs::{Hint, HintKey};
use crate::library::LibraryGame;
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, EDGE_INSET, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Wake,
    SendLogs,
    CopyLink,
    Edit,
    Forget,
    Unpin,
    Cancel,
}

/// What the menu was raised ON. Every difference between two menus in this file is a match on
/// this enum and nothing else, which is what keeps a third kind of menu to a variant and a
/// row list rather than another screen with its own list, dispatch and legend.
pub(crate) enum Subject {
    /// A saved host's carousel tile, or a pinned profile card (the pin rides in the row).
    Host(HostRow),
    /// One title on a shelf, and the host serving it — that host's pin included, so a link
    /// taken off a pinned card's shelf still streams the way that card does.
    Game {
        host: HostRow,
        id: String,
        title: String,
    },
}

pub(crate) struct OptionsScreen {
    /// The subject this menu was opened on, by value. Discovery rewrites the carousel and the
    /// library re-collates its shelf while the menu is up; holding an index or a borrow would
    /// let the menu retarget itself onto whatever slid into that slot, and "Forget" must never
    /// be able to do that.
    subject: Subject,
    list: MenuList,
    /// Forget is the one action here with no undo, so the row arms on the first press and
    /// only fires on the second. The other clients forget outright; a console is driven by
    /// a thumbstick from across a room, which is a good reason to be stricter than they
    /// are, and none at all to be looser.
    armed: bool,
}

impl OptionsScreen {
    pub(crate) fn for_host(host: &HostRow) -> OptionsScreen {
        OptionsScreen::on(Subject::Host(host.clone()))
    }

    fn on(subject: Subject) -> OptionsScreen {
        OptionsScreen {
            subject,
            list: MenuList::new(),
            armed: false,
        }
    }

    /// Is this row worth opening a menu for at all? Only saved hosts have anything to
    /// edit or forget; a discovered-but-unsaved one is not ours to change. A title needs no
    /// such gate — its one action fails soft, and a shelf only exists for a saved host.
    pub(crate) fn available(host: &HostRow) -> bool {
        host.saved
    }

    /// The host every action here ultimately addresses — a title's is the one serving it.
    fn host(&self) -> &HostRow {
        match &self.subject {
            Subject::Host(h) => h,
            Subject::Game { host, .. } => host,
        }
    }

    pub(crate) fn title(&self) -> String {
        match &self.subject {
            Subject::Host(h) => match &h.pin {
                Some(p) => format!("{} \u{b7} {}", h.name, p.name),
                None => h.name.clone(),
            },
            Subject::Game { title, .. } => title.clone(),
        }
    }

    /// A pinned card's key is the host's with the profile id appended past a NUL (see the
    /// service's row builder) — every command here addresses the HOST.
    fn host_key(&self) -> &str {
        let key = self.host().key.as_str();
        key.split('\0').next().unwrap_or(key)
    }

    fn actions(&self) -> Vec<Action> {
        let host = match &self.subject {
            Subject::Host(h) => h,
            // Deliberately not [Play, …]: the host menu does not repeat its tile's own A
            // press either, and duplicating the primary action is the one thing a menu about
            // consistency should not start life doing. Copy link leads so the cursor, which
            // starts on row 0, is already on the row nearly everyone came for.
            Subject::Game { .. } => return vec![Action::CopyLink, Action::Cancel],
        };
        if host.pin.is_some() {
            return vec![Action::Unpin, Action::CopyLink, Action::Cancel];
        }
        let mut a = Vec::new();
        // Waking a host that is already answering would just sit there counting seconds.
        if host.can_wake && !host.online {
            a.push(Action::Wake);
        }
        // "Send logs" needs a paired identity (the upload authenticates with the streaming
        // cert) and a reachable host — on anything else the row would only ever toast an
        // error. This is the log-escape hatch for platforms whose own filesystem the user
        // can't reach (Deck Gaming Mode, tvOS): the bundle lands on the host, listed in
        // its web console next to the host's own logs.
        if host.paired && host.online {
            a.push(Action::SendLogs);
        }
        a.extend([
            Action::CopyLink,
            Action::Edit,
            Action::Forget,
            Action::Cancel,
        ]);
        a
    }

    /// One label per action for the whole console, so the same verb cannot end up worded two
    /// ways on two screens — which is the mess this menu exists to clear up.
    fn label(&self, a: Action) -> String {
        match a {
            Action::Wake => "Wake host".into(),
            Action::SendLogs => "Send logs to host".into(),
            Action::CopyLink => "Copy link".into(),
            Action::Edit => "Edit\u{2026}".into(),
            Action::Forget if self.armed => "Forget \u{2014} press again".into(),
            Action::Forget => "Forget".into(),
            Action::Unpin => "Unpin card".into(),
            Action::Cancel => "Cancel".into(),
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let actions = self.actions();
        let (msg, pulse) = self.list.menu(ev, actions.len());
        self.dispatch(msg, pulse, &actions, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let actions = self.actions();
        let (msg, pulse) = self.list.pointer(p, actions.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.dispatch(msg, pulse, &actions, ctx, fx);
        true
    }

    fn dispatch(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        actions: &[Action],
        _ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(action) = actions.get(self.list.cursor).copied() else {
            return pulse;
        };
        // Moving off the armed Forget row disarms it: an arming press is about THAT row,
        // and leaving it must not leave a live trigger behind for the next visit.
        if !matches!(msg, ListMsg::Activate) && action != Action::Forget {
            self.armed = false;
        }
        match msg {
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
            ListMsg::Activate => {
                self.run(action, fx);
                pulse
            }
        }
    }

    /// This subject's `punktfunk://` link, built from the store at ACTIVATION — never at open.
    /// The store is what holds the fingerprint and stable id, and the row the menu was raised
    /// on may have left it since; a link built early would be a link built from a lie.
    fn link(&self) -> Option<String> {
        match &self.subject {
            Subject::Host(h) => crate::screens::host_link(h),
            Subject::Game { host, id, .. } => crate::screens::saved_host_link(
                &host.fp_hex,
                &host.addr,
                host.port,
                host.pin.as_ref().map(|p| p.id.as_str()),
                Some(id.as_str()),
            ),
        }
    }

    fn run(&mut self, action: Action, fx: &mut Outbox) {
        let key = self.host_key().to_string();
        match action {
            Action::Wake => {
                fx.cmds.push(ConsoleCmd::Wake {
                    key,
                    then_connect: false,
                });
                fx.pop();
            }
            Action::SendLogs => {
                let host = self.host();
                fx.cmds.push(ConsoleCmd::SendLogs {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                    host_name: host.name.clone(),
                });
                fx.toast = Some(format!("Sending logs to {}\u{2026}", host.name));
                fx.pop();
            }
            Action::CopyLink => {
                match self.link() {
                    Some(url) => {
                        fx.copy = Some(url);
                        fx.toast = Some("Link copied".into());
                    }
                    // Only if the host left the store between opening this menu and now.
                    None => fx.toast = Some("This host isn't saved any more".into()),
                }
                fx.pop();
            }
            Action::Edit => fx.replace(Screen::AddHost(super::add_host::AddHostScreen::edit(
                self.host(),
            ))),
            Action::Forget if !self.armed => self.armed = true,
            Action::Forget => {
                fx.cmds.push(ConsoleCmd::ForgetHost { key });
                fx.toast = Some(format!("Forgot {}", self.host().name));
                fx.pop();
            }
            Action::Unpin => {
                if let Some(p) = &self.host().pin {
                    fx.cmds.push(ConsoleCmd::SetPin {
                        key,
                        profile_id: p.id.clone(),
                        pin: false,
                    });
                    fx.toast = Some(format!("Unpinned {}", p.name));
                }
                fx.pop();
            }
            Action::Cancel => fx.pop(),
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, "Choose"),
            Hint::new(HintKey::Back, "Close"),
        ]
    }

    /// What this menu is FOR, in a line. Per subject, because a menu that opened from a cover
    /// and one that opened from a host tile are answering two different questions.
    fn blurb(&self) -> String {
        match &self.subject {
            Subject::Host(h) if h.pin.is_some() => {
                "This card is a shortcut to one profile on this host. Unpinning it changes \
                 nothing about the host or the profile."
                    .into()
            }
            Subject::Host(_) => "Manage this saved host.".into(),
            Subject::Game { host, .. } => format!("Actions for this title on {}.", host.name),
        }
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        _ctx: &mut Ctx,
    ) {
        // The explainer line, as on Add Host — it says what this menu is FOR, and the air it
        // takes is what keeps the first row off the title.
        fonts.leading(
            canvas,
            &self.blurb(),
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.72 * k,
        );
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + (34.0 * k) as f32,
            rect.right,
            rect.bottom,
        );
        let rows: Vec<RowSpec> = self
            .actions()
            .into_iter()
            .map(|a| RowSpec::action(self.label(a), true))
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
    }
}

/// The title half of the menu — what the library's X raises, where the host half is what the
/// home carousel's ▲ raises.
impl OptionsScreen {
    pub(crate) fn for_game(host: &HostRow, game: &LibraryGame) -> OptionsScreen {
        OptionsScreen::on(Subject::Game {
            host: host.clone(),
            id: game.id.clone(),
            title: game.title.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProfileChip;
    use crate::screens::Nav;

    fn host() -> HostRow {
        HostRow {
            key: "aa".into(),
            name: "Desk".into(),
            addr: "10.0.0.5".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 9778,
            can_wake: false,
            last_used: None,
            os: String::new(),
            pin: None,
            bound_profile: None,
        }
    }

    fn pinned() -> HostRow {
        HostRow {
            key: "aa\u{0}prof-1".into(),
            pin: Some(ProfileChip {
                id: "prof-1".into(),
                name: "4K".into(),
                accent: None,
            }),
            ..host()
        }
    }

    fn game() -> LibraryGame {
        LibraryGame {
            id: "steam:367520".into(),
            title: "Hollow Knight".into(),
            store: "steam".into(),
            launcher: false,
            icon: "steam".into(),
            platform: None,
        }
    }

    #[test]
    fn a_discovered_host_has_no_menu() {
        assert!(OptionsScreen::available(&host()));
        assert!(!OptionsScreen::available(&HostRow {
            saved: false,
            ..host()
        }));
    }

    #[test]
    fn wake_is_offered_only_when_it_would_do_something() {
        let awake = OptionsScreen::for_host(&HostRow {
            can_wake: true,
            online: true,
            ..host()
        });
        assert!(!awake.actions().contains(&Action::Wake));
        let asleep = OptionsScreen::for_host(&HostRow {
            can_wake: true,
            online: false,
            ..host()
        });
        assert!(asleep.actions().contains(&Action::Wake));
    }

    #[test]
    fn a_pinned_card_cannot_forget_or_edit_the_host() {
        let s = OptionsScreen::for_host(&pinned());
        assert_eq!(
            s.actions(),
            vec![Action::Unpin, Action::CopyLink, Action::Cancel]
        );
        // …and its commands still address the HOST, not the pin's composite key.
        assert_eq!(s.host_key(), "aa");
    }

    #[test]
    fn forget_needs_two_presses() {
        let mut s = OptionsScreen::for_host(&host());
        let actions = s.actions();
        let i = actions.iter().position(|a| *a == Action::Forget).unwrap();
        s.list.cursor = i;
        let mut fx = Outbox::default();

        s.run(Action::Forget, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert!(s.armed);
        assert!(s.label(Action::Forget).contains("press again"));

        s.run(Action::Forget, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::ForgetHost { key: "aa".into() }],
            "the second press forgets"
        );
    }

    #[test]
    fn leaving_the_forget_row_disarms_it() {
        let mut s = OptionsScreen::for_host(&host());
        let actions = s.actions();
        s.armed = true;
        s.list.cursor = actions.iter().position(|a| *a == Action::Cancel).unwrap();
        let mut ctx_settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &crate::library::LibraryShared::default(),
            settings: &mut ctx_settings,
            pads: &[],
            deck: false,
            device_name: "test",
            t: 0.0,
        };
        let mut fx = Outbox::default();
        s.dispatch(ListMsg::None, None, &actions, &mut ctx, &mut fx);
        assert!(!s.armed, "a cursor move off the row cancels the arming");
    }

    #[test]
    fn a_title_offers_the_link_and_nothing_its_cover_already_does() {
        let s = OptionsScreen::for_game(&host(), &game());
        assert_eq!(s.actions(), vec![Action::CopyLink, Action::Cancel]);
        // The cursor starts on row 0, so the row nearly everyone opened this for is the row
        // the confirm press is already on.
        assert_eq!(s.list.cursor, 0);
        assert_eq!(s.title(), "Hollow Knight");
    }

    #[test]
    fn every_menu_can_be_left_without_doing_anything() {
        for s in [
            OptionsScreen::for_host(&host()),
            OptionsScreen::for_host(&pinned()),
            OptionsScreen::for_game(&host(), &game()),
            OptionsScreen::for_game(&pinned(), &game()),
        ] {
            assert_eq!(s.actions().last(), Some(&Action::Cancel));
        }
    }

    #[test]
    fn a_titles_menu_keeps_the_shelfs_whole_host_so_a_pinned_cards_profile_survives() {
        let s = OptionsScreen::for_game(&pinned(), &game());
        let Subject::Game { host, id, .. } = &s.subject else {
            panic!("built as a title menu");
        };
        assert_eq!(id, "steam:367520", "the link's launch id");
        assert_eq!(
            host.pin.as_ref().map(|p| p.id.as_str()),
            Some("prof-1"),
            "a link taken off a pinned card's shelf carries that card's profile"
        );
        // Host-addressed commands still reach past the pin's composite key.
        assert_eq!(s.host_key(), "aa");
    }

    #[test]
    fn copy_link_always_closes_the_menu_and_says_what_happened() {
        // Whether the store still knows the host decides WHICH of the two things it says,
        // and nothing else: a menu that stayed open on failure would leave the user pressing
        // a row that can only fail again.
        for mut s in [
            OptionsScreen::for_host(&host()),
            OptionsScreen::for_game(&host(), &game()),
        ] {
            let mut fx = Outbox::default();
            s.run(Action::CopyLink, &mut fx);
            assert!(matches!(fx.nav, Some(Nav::Pop)));
            assert!(fx.toast.is_some());
        }
    }
}
