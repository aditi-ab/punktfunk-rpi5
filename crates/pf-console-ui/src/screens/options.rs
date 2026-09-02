//! Context menu for whatever the console is looking at: a saved host, a pinned
//! profile card, or a library title. One screen; the subject names the object
//! and [`OptionsScreen::actions`] owns the verbs.
//!
//! Which face button raises it is per screen (carousel ▲, library X); both
//! legends say "Options". A pinned card offers Unpin only — it is a shortcut,
//! not a second host.
//!
//! Evidence: `design/host-actions.md`. Tests in this module pin the arm-then-fire
//! rule, the host-key NUL split, and the padless Library row.

use crate::glyphs::{Hint, HintKey};
use crate::library::LibraryGame;
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, EDGE_INSET, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Wake,
    /// Indexed into [`HostRow::actions`]. Not a variant per id: this build must
    /// render a label the host sent that we have never heard of.
    Host(usize),
    SendLogs,
    /// Same gate as the carousel's Y (saved and paired). Here because a TV
    /// remote has no Y.
    Library,
    CopyLink,
    Edit,
    /// [`Screen::BindProfile`] for the primary tile. Not on a pin: the pin is
    /// the profile.
    BindProfile,
    /// Per-host [`KnownHost::clipboard_sync`]. Lives on the host, not Settings:
    /// the other end of the pipe is this machine.
    Clipboard,
    Forget,
    Unpin,
    Cancel,
}

/// What the menu was raised on. A third kind is a variant plus a row list, not
/// another screen.
pub(crate) enum Subject {
    /// Saved host tile, or a pinned profile card (the pin rides in the row).
    Host(HostRow),
    /// Title on a shelf, with the serving host (pin included) so a link off a
    /// pinned card still streams as that card does.
    Game {
        host: HostRow,
        id: String,
        title: String,
    },
}

pub(crate) struct OptionsScreen {
    /// Subject by value. Discovery rewrites the carousel and the shelf while
    /// this is up; an index or borrow would retarget Forget onto whatever slid
    /// into the slot.
    subject: Subject,
    list: MenuList,
    /// Destructive row armed on first press, fires on second. `Option<Action>`
    /// not a bool: arming Forget must not fire Restart if the cursor moved.
    armed: Option<Action>,
}

impl OptionsScreen {
    pub(crate) fn for_host(host: &HostRow) -> OptionsScreen {
        OptionsScreen::on(Subject::Host(host.clone()))
    }

    fn on(subject: Subject) -> OptionsScreen {
        OptionsScreen {
            subject,
            list: MenuList::new(),
            armed: None,
        }
    }

    /// Saved hosts only. A discovered-but-unsaved row is not ours to edit.
    /// Titles skip this: a shelf only exists for a saved host.
    pub(crate) fn available(host: &HostRow) -> bool {
        host.saved
    }

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

    /// Pinned-card keys append the profile id past a NUL (service row builder).
    /// Commands address the host half.
    fn host_key(&self) -> &str {
        let key = self.host().key.as_str();
        key.split('\0').next().unwrap_or(key)
    }

    // Unused: seam for platform-conditional rows. Call sites already pass it.
    fn actions(&self, _platform: crate::platform::Platform) -> Vec<Action> {
        let host = match &self.subject {
            Subject::Host(h) => h,
            // Not Play: the tile's A already launches. Copy link first: cursor starts at 0.
            Subject::Game { .. } => return vec![Action::CopyLink, Action::Cancel],
        };
        if host.pin.is_some() {
            return vec![Action::Unpin, Action::CopyLink, Action::Cancel];
        }
        let mut a = Vec::new();
        // Online already: Wake would sit there counting seconds.
        if host.can_wake && !host.online {
            a.push(Action::Wake);
        }
        a.extend((0..host.actions.len()).map(Action::Host));
        // Upload authenticates with the streaming cert and needs a live host;
        // anything else would only toast an error.
        if host.paired && host.online {
            a.push(Action::SendLogs);
        }
        // Same gate as carousel Y. Ahead of Copy link: this row navigates, and
        // on a padless remote it is the only way to the shelf.
        if host.paired && host.saved {
            a.push(Action::Library);
        }
        a.extend([
            Action::CopyLink,
            Action::Edit,
            Action::BindProfile,
            Action::Clipboard,
            Action::Forget,
            Action::Cancel,
        ]);
        a
    }

    fn label(&self, a: Action) -> String {
        match a {
            Action::Wake => "Wake host".into(),
            Action::Host(i) => match self.host().actions.get(i) {
                Some(act) if self.armed == Some(a) => {
                    format!("{} \u{2014} press again", act.label)
                }
                Some(act) => act.label.clone(),
                None => String::new(),
            },
            Action::SendLogs => "Send logs to host".into(),
            Action::Library => "Library".into(),
            Action::CopyLink => "Copy link".into(),
            Action::Edit => "Edit\u{2026}".into(),
            Action::BindProfile => "Default profile\u{2026}".into(),
            Action::Clipboard => format!(
                "Shared clipboard: {}",
                if self.host().clipboard_sync {
                    "On"
                } else {
                    "Off"
                }
            ),
            Action::Forget if self.armed == Some(Action::Forget) => {
                "Forget \u{2014} press again".into()
            }
            Action::Forget => "Forget".into(),
            Action::Unpin => "Unpin card".into(),
            Action::Cancel => "Cancel".into(),
        }
    }

    /// Host-reported verbs can be unavailable. The row stays; activating it
    /// toasts why. Vanishing would look like the verb never existed.
    fn enabled(&self, a: Action) -> bool {
        match a {
            Action::Host(i) => self.host().actions.get(i).is_none_or(|act| act.available),
            _ => true,
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
        let actions = self.actions(ctx.platform);
        let (msg, pulse) = self.list.menu(ev, actions.len());
        self.dispatch(msg, pulse, &actions, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let actions = self.actions(ctx.platform);
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
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(action) = actions.get(self.list.cursor).copied() else {
            return pulse;
        };
        // Arming is per row. Leaving it must not leave a live trigger on the
        // next destructive row the cursor lands on.
        if !matches!(msg, ListMsg::Activate) && self.armed != Some(action) {
            self.armed = None;
        }
        match msg {
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
            ListMsg::Activate => {
                self.run(action, ctx, fx);
                pulse
            }
        }
    }

    /// `punktfunk://` from the store at activation, never at open. The row may
    /// have left the store while the menu was up.
    fn link(&self, store: &dyn crate::store::SettingsStore) -> Option<String> {
        match &self.subject {
            Subject::Host(h) => crate::screens::host_link(store, h),
            Subject::Game { host, id, .. } => crate::screens::saved_host_link(
                store,
                &host.fp_hex,
                &host.addr,
                host.port,
                host.pin.as_ref().map(|p| p.id.as_str()),
                Some(id.as_str()),
            ),
        }
    }

    fn run(&mut self, action: Action, ctx: &Ctx, fx: &mut Outbox) {
        let store = ctx.store;
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
                match self.link(store) {
                    Some(url) => {
                        fx.copy = Some(url);
                        fx.toast = Some("Link copied".into());
                    }
                    // Host left the store between open and now.
                    None => fx.toast = Some("This host isn't saved any more".into()),
                }
                fx.pop();
            }
            // Fetch, then open on the epoch taken *before* the command drains
            // so this fetch's titles are distinct from ones already in the
            // model. Replace, not push: Back from the shelf is the carousel.
            Action::Library => {
                let host = self.host();
                fx.cmds.push(ConsoleCmd::FetchLibrary {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                });
                let epoch = ctx.library.fetch_epoch();
                fx.replace(Screen::Library(super::library::LibraryScreen::new(
                    self.host(),
                    epoch,
                )));
            }
            Action::Edit => fx.replace(Screen::AddHost(super::add_host::AddHostScreen::edit(
                self.host(),
            ))),
            Action::BindProfile => fx.replace(Screen::BindProfile(
                super::bind_profile::BindProfileScreen::new(
                    key,
                    self.host().name.clone(),
                    store.profiles(),
                ),
            )),
            Action::Clipboard => {
                let host = self.host();
                let on = !host.clipboard_sync;
                fx.toast = Some(if on {
                    format!("Clipboard shared with {}", host.name)
                } else {
                    format!("Clipboard no longer shared with {}", host.name)
                });
                fx.cmds.push(ConsoleCmd::SetClipboard { key, on });
                fx.pop();
            }
            Action::Forget if self.armed != Some(Action::Forget) => {
                self.armed = Some(Action::Forget)
            }
            Action::Forget => {
                fx.cmds.push(ConsoleCmd::ForgetHost { key });
                fx.toast = Some(format!("Forgot {}", self.host().name));
                fx.pop();
            }
            Action::Host(i) => {
                let host = self.host();
                let Some(act) = host.actions.get(i) else {
                    return; // row list changed under the cursor
                };
                // Host says no: toast why rather than send a request it will refuse.
                if !act.available {
                    let why = act.unavailable_reason.clone();
                    fx.toast = Some(if why.is_empty() {
                        format!("{} isn't available right now", act.label)
                    } else {
                        why
                    });
                    fx.pop();
                    return;
                }
                // `danger` (restart, shut down) arms then fires. Sleep is
                // reversible via Wake, so one press.
                if act.danger && self.armed != Some(action) {
                    self.armed = Some(action);
                    return;
                }
                fx.cmds.push(ConsoleCmd::HostAction {
                    addr: host.addr.clone(),
                    mgmt: host.mgmt_port,
                    fp_hex: host.fp_hex.clone(),
                    host_name: host.name.clone(),
                    action_id: act.id.clone(),
                    label: act.label.clone(),
                });
                fx.toast = Some(format!(
                    "{} \u{2014} asking {}\u{2026}",
                    act.label, host.name
                ));
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
        ctx: &mut Ctx,
    ) {
        // Air under the title so the first row does not sit on it.
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
            .actions(ctx.platform)
            .into_iter()
            .map(|a| RowSpec::action(self.label(a), self.enabled(a)))
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
    }
}

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

    /// Drive `run` with a throwaway `Ctx`. Only the store and, for Library, the
    /// fetch epoch are read.
    fn run_action(s: &mut OptionsScreen, action: Action, fx: &mut Outbox) {
        let mut settings = pf_client_core::trust::Settings::default();
        let library = crate::library::LibraryShared::default();
        let ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &[],
            deck: false,
            fallback_ui: false,
            device_name: "test",
            t: 0.0,
        };
        s.run(action, &ctx, fx);
    }

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
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_profile: None,
        }
    }

    fn powered() -> HostRow {
        let act = |id: &str, label: &str, danger: bool, available: bool| crate::model::HostAction {
            id: id.into(),
            label: label.into(),
            danger,
            available,
            unavailable_reason: if available {
                String::new()
            } else {
                "this machine does not support sleep".into()
            },
        };
        HostRow {
            actions: vec![
                act("power.sleep", "Sleep host", false, true),
                act("power.reboot", "Restart host", true, true),
                act("power.shutdown", "Shut down host", true, true),
            ],
            ..host()
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
            running: false,
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
        assert!(!awake
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::Wake));
        let asleep = OptionsScreen::for_host(&HostRow {
            can_wake: true,
            online: false,
            ..host()
        });
        assert!(asleep
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::Wake));
    }

    #[test]
    fn send_logs_is_offered_on_every_platform_with_an_uploader() {
        let reachable = OptionsScreen::for_host(&HostRow {
            paired: true,
            online: true,
            ..host()
        });
        assert!(reachable
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::SendLogs));
        assert!(reachable
            .actions(crate::platform::Platform::Android)
            .contains(&Action::SendLogs));
    }

    #[test]
    fn host_actions_appear_only_when_the_host_offered_them() {
        let none = OptionsScreen::for_host(&host());
        assert!(!none
            .actions(crate::platform::Platform::Desktop)
            .iter()
            .any(|a| matches!(a, Action::Host(_))));

        let s = OptionsScreen::for_host(&powered());
        let rows = s.actions(crate::platform::Platform::Desktop);
        assert_eq!(
            rows.iter().filter(|a| matches!(a, Action::Host(_))).count(),
            3
        );
        assert_eq!(s.label(Action::Host(0)), "Sleep host");
        // Unknown id still renders: the host sent the label.
        let future = OptionsScreen::for_host(&HostRow {
            actions: vec![crate::model::HostAction {
                id: "plugin:vpn:toggle".into(),
                label: "Toggle the VPN".into(),
                danger: false,
                available: true,
                unavailable_reason: String::new(),
            }],
            ..host()
        });
        assert_eq!(future.label(Action::Host(0)), "Toggle the VPN");
    }

    /// Sleep fires on one press. Restart/shut down arm. Arming one must not
    /// leave the other live (`armed` is `Option<Action>`, not a bool).
    #[test]
    fn destructive_host_actions_arm_before_they_fire() {
        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(0), &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.sleep"
        ));

        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(
            s.label(Action::Host(2)),
            "Shut down host \u{2014} press again"
        );
        assert_eq!(s.label(Action::Host(1)), "Restart host");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(1), &mut fx);
        assert!(
            fx.cmds.is_empty(),
            "arming shut down must not leave restart armed"
        );
        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx);
        run_action(&mut s, Action::Host(2), &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.shutdown"
        ));
    }

    #[test]
    fn an_unavailable_action_explains_itself_instead_of_firing() {
        let mut s = OptionsScreen::for_host(&HostRow {
            actions: vec![crate::model::HostAction {
                id: "power.sleep".into(),
                label: "Sleep host".into(),
                danger: false,
                available: false,
                unavailable_reason: "this machine does not support sleep".into(),
            }],
            ..host()
        });
        assert!(!s.enabled(Action::Host(0)));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(0), &mut fx);
        assert!(fx.cmds.is_empty(), "no request the host would refuse");
        assert_eq!(
            fx.toast.as_deref(),
            Some("this machine does not support sleep")
        );
    }

    #[test]
    fn a_pinned_card_cannot_forget_or_edit_the_host() {
        let s = OptionsScreen::for_host(&pinned());
        assert_eq!(
            s.actions(crate::platform::Platform::Desktop),
            vec![Action::Unpin, Action::CopyLink, Action::Cancel]
        );
        // Commands address the host, not the pin's composite key.
        assert_eq!(s.host_key(), "aa");
    }

    /// Padless path to the shelf (no Y on a TV remote). Saved and paired;
    /// Replace so Back lands on the carousel.
    #[test]
    fn the_library_hangs_off_the_menu_for_a_padless_device() {
        let mut s = OptionsScreen::for_host(&host());
        assert!(s
            .actions(crate::platform::Platform::Android)
            .contains(&Action::Library));

        let mut fx = Outbox::default();
        run_action(&mut s, Action::Library, &mut fx);
        assert!(
            matches!(fx.cmds.first(), Some(ConsoleCmd::FetchLibrary { .. })),
            "opening the shelf asks for it first"
        );
        match fx.nav {
            Some(Nav::Replace(screen)) => assert!(matches!(*screen, Screen::Library(_))),
            _ => panic!("expected the shelf to replace the menu"),
        }

        // Unpaired: row absent, not inert.
        let unpaired = OptionsScreen::for_host(&HostRow {
            paired: false,
            ..host()
        });
        assert!(!unpaired
            .actions(crate::platform::Platform::Android)
            .contains(&Action::Library));
    }

    #[test]
    fn default_profile_opens_the_chooser_on_the_hosts_plain_key() {
        let mut s = OptionsScreen::for_host(&host());
        assert!(s
            .actions(crate::platform::Platform::Desktop)
            .contains(&Action::BindProfile));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::BindProfile, &mut fx);
        match fx.nav {
            Some(crate::screens::Nav::Replace(screen)) => match *screen {
                Screen::BindProfile(b) => assert_eq!(b.host_name(), "Desk"),
                _ => panic!("expected the bind-profile chooser"),
            },
            _ => panic!("expected a replace"),
        }
    }

    #[test]
    fn the_clipboard_toggle_flips_the_stored_state() {
        let mut s = OptionsScreen::for_host(&host());
        assert!(s.label(Action::Clipboard).ends_with("Off"));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Clipboard, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetClipboard {
                key: "aa".into(),
                on: true,
            }]
        );
        let mut s = OptionsScreen::for_host(&HostRow {
            clipboard_sync: true,
            ..host()
        });
        assert!(s.label(Action::Clipboard).ends_with("On"));
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Clipboard, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetClipboard {
                key: "aa".into(),
                on: false,
            }]
        );
    }

    #[test]
    fn forget_needs_two_presses() {
        let mut s = OptionsScreen::for_host(&host());
        let actions = s.actions(crate::platform::Platform::Desktop);
        let i = actions.iter().position(|a| *a == Action::Forget).unwrap();
        s.list.cursor = i;
        let mut fx = Outbox::default();

        run_action(&mut s, Action::Forget, &mut fx);
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(s.armed, Some(Action::Forget));
        assert!(s.label(Action::Forget).contains("press again"));

        run_action(&mut s, Action::Forget, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::ForgetHost { key: "aa".into() }],
            "the second press forgets"
        );
    }

    #[test]
    fn leaving_the_forget_row_disarms_it() {
        let mut s = OptionsScreen::for_host(&host());
        let actions = s.actions(crate::platform::Platform::Desktop);
        s.armed = Some(Action::Forget);
        s.list.cursor = actions.iter().position(|a| *a == Action::Cancel).unwrap();
        let mut ctx_settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &crate::library::LibraryShared::default(),
            settings: &mut ctx_settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &[],
            deck: false,
            fallback_ui: false,
            device_name: "test",
            t: 0.0,
        };
        let mut fx = Outbox::default();
        s.dispatch(ListMsg::None, None, &actions, &mut ctx, &mut fx);
        assert_eq!(
            s.armed, None,
            "a cursor move off the row cancels the arming"
        );
    }

    #[test]
    fn a_title_offers_the_link_and_nothing_its_cover_already_does() {
        let s = OptionsScreen::for_game(&host(), &game());
        assert_eq!(
            s.actions(crate::platform::Platform::Desktop),
            vec![Action::CopyLink, Action::Cancel]
        );
        // Cursor starts at 0: Copy link is already under confirm.
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
            assert_eq!(
                s.actions(crate::platform::Platform::Desktop).last(),
                Some(&Action::Cancel)
            );
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
        // Host-addressed commands still use the host half of a pin key.
        assert_eq!(s.host_key(), "aa");
    }

    #[test]
    fn copy_link_always_closes_the_menu_and_says_what_happened() {
        // Always Pop. Staying open on failure would leave a row that can only fail again.
        for mut s in [
            OptionsScreen::for_host(&host()),
            OptionsScreen::for_game(&host(), &game()),
        ] {
            let mut fx = Outbox::default();
            run_action(&mut s, Action::CopyLink, &mut fx);
            assert!(matches!(fx.nav, Some(Nav::Pop)));
            assert!(fx.toast.is_some());
        }
    }
}
