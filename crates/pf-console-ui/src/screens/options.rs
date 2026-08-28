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
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Action {
    Wake,
    /// One of the host's OWN actions — sleep / restart / shut down it
    /// (`design/host-actions.md` §7), indexed into [`HostRow::actions`], which the service
    /// thread filled from the host's discovery. Indexed rather than a variant per verb
    /// because the console must render an action this build has never heard of: the host
    /// already sent a label and said whether this device may run it.
    Host(usize),
    SendLogs,
    /// Open this host's game library — the same shelf the home carousel's Y opens, offered
    /// here because Y is a face button and a TV remote has none. Saved-and-paired only,
    /// exactly like that Y (an unpaired host has no shelf to fetch).
    Library,
    CopyLink,
    Edit,
    /// Choose the profile the host's primary tile connects with (opens the
    /// [`Screen::BindProfile`] chooser). Offered on saved primary tiles only — a pinned
    /// card's profile IS the card, and a title's menu addresses the title.
    BindProfile,
    /// Share this device's clipboard with THIS host while streaming
    /// (`KnownHost::clipboard_sync`). Per-host because it is a trust decision about that
    /// host — which is why it lives on the host and not in Settings. A toggle: the label
    /// carries the current state, activating flips it.
    Clipboard,
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
    /// The row currently armed, if any: an action with no undo arms on the first press and
    /// only fires on the second. Forget was the first; the host's own destructive actions
    /// (restart, shut down) join it. The other clients forget outright; a console is driven
    /// by a thumbstick from across a room, which is a good reason to be stricter than they
    /// are, and none at all to be looser.
    ///
    /// Holding WHICH action is armed, rather than a bare flag, is what stops an arming press
    /// on one destructive row from firing a different one the cursor then landed on.
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

    // `_platform` is the seam platform-conditional rows plug into (Send logs used it until
    // Android grew an uploader); unused today, kept so the next such row has its question
    // already answered at every call site.
    fn actions(&self, _platform: crate::platform::Platform) -> Vec<Action> {
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
        // …and the other half of that round trip, immediately below it: the host's own
        // actions, as IT reported them for this device. Nothing is decided here — the list is
        // empty unless the host is reachable and this device's access carries the grant, so
        // "Sleep host" appears exactly where "Wake host" was the evening before.
        a.extend((0..host.actions.len()).map(Action::Host));
        // "Send logs" needs a paired identity (the upload authenticates with the streaming
        // cert) and a reachable host — on anything else the row would only ever toast an
        // error. This is the log-escape hatch for platforms whose own filesystem the user
        // can't reach (Deck Gaming Mode, tvOS): the bundle lands on the host, listed in
        // its web console next to the host's own logs.
        // Every platform has an uploader now (Android's rides `nativeSendLogs` over the
        // same `logring` the desktop drains), so paired-and-reachable is the whole gate.
        if host.paired && host.online {
            a.push(Action::SendLogs);
        }
        // The shelf, on the same terms the carousel's Y offers it. Ahead of Copy link
        // because it is the one row here that goes somewhere rather than acting on the
        // host — and on a remote-only device it is the ONLY way to the library.
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

    /// One label per action for the whole console, so the same verb cannot end up worded two
    /// ways on two screens — which is the mess this menu exists to clear up.
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

    /// Whether a row reads as live. Only a host action can be dead: the host said it cannot
    /// run that verb right now (no suspend support, a foreign inhibitor, a second local user).
    /// The row stays — activating it explains why — because a row that quietly vanished would
    /// leave the person wondering whether they had imagined it (the host's own honesty rule:
    /// "unavailable, because X", never a dead switch and never a silence).
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
        // Moving off an armed row disarms it: an arming press is about THAT row, and leaving
        // it must not leave a live trigger behind — neither for the next visit, nor for the
        // destructive row the cursor happened to land on next.
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

    /// This subject's `punktfunk://` link, built from the store at ACTIVATION — never at open.
    /// The store is what holds the fingerprint and stable id, and the row the menu was raised
    /// on may have left it since; a link built early would be a link built from a lie.
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
                    // Only if the host left the store between opening this menu and now.
                    None => fx.toast = Some("This host isn't saved any more".into()),
                }
                fx.pop();
            }
            // Same two steps the home carousel's Y takes: ask for the shelf, then open it
            // on the epoch read BEFORE the command drains, so the screen can tell its own
            // fetch's titles from the ones already in the model. `replace`, not push — the
            // menu has said its piece, and Back from the shelf belongs on the carousel
            // rather than on a menu about the host you just left.
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
                    return; // the row list changed under the cursor — do nothing, silently
                };
                // A host the host itself says it cannot do right now: say why rather than
                // send a request we know it will refuse.
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
                // Restart and shut down lose whatever is running on that machine, so they take
                // the Forget treatment: arm, then fire. Sleep is reversible from the same menu
                // (Wake host), so it goes on one press.
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
        ctx: &mut Ctx,
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
            .actions(ctx.platform)
            .into_iter()
            .map(|a| RowSpec::action(self.label(a), self.enabled(a)))
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

    /// Activate one row. `run` reads the store, and — for Library — the shared library's
    /// fetch epoch; nothing else in this menu touches the context, so one throwaway is
    /// enough for every action test here.
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

    /// A host that reported the three power actions for this device, sleep available.
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

    /// "Send logs" is offered wherever a paired, reachable host can receive it — on BOTH
    /// platforms since Android's uploader landed (`SkiaConsole.sendLogs` → `nativeSendLogs`
    /// over the shared `logring`); before that the row was desktop-only, because a row that
    /// can only toast "not available" is a promise broken.
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

    /// The host's own actions sit right under Wake — the two halves of one round trip — and
    /// only exist because the HOST offered them: an empty list (older host, unreachable host,
    /// or a device without the grant) leaves the menu exactly as it was.
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
        // An id this build has never heard of still renders — the host sent the label.
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

    /// Sleep is reversible from this very menu, so it goes on one press. Restart and shut down
    /// are not, so they take Forget's arm-then-fire — and arming one must never leave the
    /// OTHER one live, which is exactly the bug a bare `armed` flag would have shipped.
    #[test]
    fn destructive_host_actions_arm_before_they_fire() {
        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(0), &mut fx); // sleep — one press
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.sleep"
        ));

        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx); // shut down — arms
        assert!(fx.cmds.is_empty(), "the first press only arms");
        assert_eq!(
            s.label(Action::Host(2)),
            "Shut down host \u{2014} press again"
        );
        // The armed row is that one row: moving to Restart and pressing must not shut down.
        assert_eq!(s.label(Action::Host(1)), "Restart host");
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(1), &mut fx);
        assert!(
            fx.cmds.is_empty(),
            "arming shut down must not leave restart armed"
        );
        // Pressing the armed row again fires it.
        let mut s = OptionsScreen::for_host(&powered());
        let mut fx = Outbox::default();
        run_action(&mut s, Action::Host(2), &mut fx);
        run_action(&mut s, Action::Host(2), &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::HostAction { action_id, .. }) if action_id == "power.shutdown"
        ));
    }

    /// An action the host says it cannot run right now stays on the menu, disabled, and
    /// explains itself — never a silent row and never a request we know will be refused.
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
        // …and its commands still address the HOST, not the pin's composite key.
        assert_eq!(s.host_key(), "aa");
    }

    /// The shelf is on this menu, which is the only route to it that survives a device with
    /// no face buttons: home's Y opens it too, but an Android TV remote has no Y. Offered on
    /// the same terms that Y is (saved AND paired), and it REPLACES the menu, so Back from
    /// the shelf lands on the carousel rather than on a menu about the host just left.
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

        // An unpaired host has no shelf to fetch — the row is absent, not inert.
        let unpaired = OptionsScreen::for_host(&HostRow {
            paired: false,
            ..host()
        });
        assert!(!unpaired
            .actions(crate::platform::Platform::Android)
            .contains(&Action::Library));
    }

    /// "Default profile…" swaps the menu for the chooser — a Replace like Edit's, and for
    /// the same reason — addressed to the HOST's plain key even from rows that carry a
    /// composite one.
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

    /// The clipboard toggle: the label says where the host stands, activating flips it —
    /// and both address the HOST's plain key.
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
            run_action(&mut s, Action::CopyLink, &mut fx);
            assert!(matches!(fx.nav, Some(Nav::Pop)));
            assert!(fx.toast.is_some());
        }
    }
}
