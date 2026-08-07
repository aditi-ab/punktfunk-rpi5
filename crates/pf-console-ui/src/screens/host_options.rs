//! A saved host's own actions — Wake, Copy link, Edit…, Forget — reached with UP on its
//! carousel tile, and the console's answer to the overflow menu every other client hangs
//! off a host card.
//!
//! Until now the console could add a host and connect to one, and that was all: a renamed
//! machine or a host typed in with a fat-fingered address stayed wrong forever, because
//! the only surfaces that could edit or forget one were the desktop shells. The tile is
//! where a host is, so the tile is where its actions belong.
//!
//! UP is the gesture because the carousel is horizontal — left/right are spoken for and
//! up is free — and because the Android console already does exactly this, so the two
//! consoles are learned once. A pinned profile card offers only Unpin: it is a shortcut,
//! not a second host, and offering to forget the host from it would blur precisely the
//! distinction a pin exists to draw.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::Fonts;
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::gamepad::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Wake,
    CopyLink,
    Edit,
    Forget,
    Unpin,
    Cancel,
}

pub(crate) struct HostOptionsScreen {
    /// The row this menu was opened on, by value. Discovery rewrites the carousel every
    /// service pass; holding an index or a borrow would let the menu retarget itself onto
    /// whichever host slid into that slot, and "Forget" must never be able to do that.
    host: HostRow,
    list: MenuList,
    /// Forget is the one action here with no undo, so the row arms on the first press and
    /// only fires on the second. The other clients forget outright; a console is driven by
    /// a thumbstick from across a room, which is a good reason to be stricter than they
    /// are, and none at all to be looser.
    armed: bool,
}

impl HostOptionsScreen {
    pub(crate) fn new(host: &HostRow) -> HostOptionsScreen {
        HostOptionsScreen {
            host: host.clone(),
            list: MenuList::new(),
            armed: false,
        }
    }

    /// Is this row worth opening a menu for at all? Only saved hosts have anything to
    /// edit or forget; a discovered-but-unsaved one is not ours to change.
    pub(crate) fn available(host: &HostRow) -> bool {
        host.saved
    }

    pub(crate) fn title(&self) -> String {
        match &self.host.pin {
            Some(p) => format!("{} \u{b7} {}", self.host.name, p.name),
            None => self.host.name.clone(),
        }
    }

    /// A pinned card's key is the host's with the profile id appended past a NUL (see the
    /// service's row builder) — every command here addresses the HOST.
    fn host_key(&self) -> &str {
        self.host
            .key
            .split('\0')
            .next()
            .unwrap_or(self.host.key.as_str())
    }

    fn actions(&self) -> Vec<Action> {
        if self.host.pin.is_some() {
            return vec![Action::Unpin, Action::CopyLink, Action::Cancel];
        }
        let mut a = Vec::new();
        // Waking a host that is already answering would just sit there counting seconds.
        if self.host.can_wake && !self.host.online {
            a.push(Action::Wake);
        }
        a.extend([
            Action::CopyLink,
            Action::Edit,
            Action::Forget,
            Action::Cancel,
        ]);
        a
    }

    fn label(&self, a: Action) -> String {
        match a {
            Action::Wake => "Wake host".into(),
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
            Action::CopyLink => {
                match crate::screens::host_link(&self.host) {
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
                &self.host,
            ))),
            Action::Forget if !self.armed => self.armed = true,
            Action::Forget => {
                fx.cmds.push(ConsoleCmd::ForgetHost { key });
                fx.toast = Some(format!("Forgot {}", self.host.name));
                fx.pop();
            }
            Action::Unpin => {
                if let Some(p) = &self.host.pin {
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

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        _ctx: &mut Ctx,
    ) {
        let rows: Vec<RowSpec> = self
            .actions()
            .into_iter()
            .map(|a| RowSpec::action(self.label(a), true))
            .collect();
        self.list.render(canvas, rect, &rows, fonts, k, dt, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProfileChip;

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

    #[test]
    fn a_discovered_host_has_no_menu() {
        assert!(HostOptionsScreen::available(&host()));
        assert!(!HostOptionsScreen::available(&HostRow {
            saved: false,
            ..host()
        }));
    }

    #[test]
    fn wake_is_offered_only_when_it_would_do_something() {
        let awake = HostOptionsScreen::new(&HostRow {
            can_wake: true,
            online: true,
            ..host()
        });
        assert!(!awake.actions().contains(&Action::Wake));
        let asleep = HostOptionsScreen::new(&HostRow {
            can_wake: true,
            online: false,
            ..host()
        });
        assert!(asleep.actions().contains(&Action::Wake));
    }

    #[test]
    fn a_pinned_card_cannot_forget_or_edit_the_host() {
        let s = HostOptionsScreen::new(&pinned());
        assert_eq!(
            s.actions(),
            vec![Action::Unpin, Action::CopyLink, Action::Cancel]
        );
        // …and its commands still address the HOST, not the pin's composite key.
        assert_eq!(s.host_key(), "aa");
    }

    #[test]
    fn forget_needs_two_presses() {
        let mut s = HostOptionsScreen::new(&host());
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
        let mut s = HostOptionsScreen::new(&host());
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
}
