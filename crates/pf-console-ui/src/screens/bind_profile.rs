//! Bind the profile a plain A-press on a saved host connects with
//! (`KnownHost::profile_id`). Reached from the host tile's Options menu.
//!
//! Choosing emits [`ConsoleCmd::BindProfile`]; the checkmark is read back from
//! the host row, never stored here. Pinning is the sibling (`pin_hosts.rs`):
//! a pin adds a card, this changes what the primary tile itself does.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

pub(crate) struct BindProfileScreen {
    /// Host primary key (fingerprint or `addr:port`), never a pinned-card composite.
    host_key: String,
    host_name: String,
    /// Catalog snapshot at construction. The console cannot create profiles, so
    /// this list cannot change while the screen is open.
    profiles: Vec<(String, String)>,
    list: MenuList,
}

impl BindProfileScreen {
    pub(crate) fn new(
        host_key: String,
        host_name: String,
        profiles: Vec<(String, String)>,
    ) -> BindProfileScreen {
        BindProfileScreen {
            host_key,
            host_name,
            profiles,
            list: MenuList::new(),
        }
    }

    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    fn bound(&self, ctx: &Ctx) -> Option<String> {
        ctx.hosts
            .iter()
            .find(|r| r.key == self.host_key)
            .and_then(|r| r.bound_profile.as_ref())
            .map(|p| p.id.clone())
    }

    fn choice(&self, i: usize) -> Option<Option<&str>> {
        if i == 0 {
            Some(None)
        } else {
            self.profiles.get(i - 1).map(|(id, _)| Some(id.as_str()))
        }
    }

    fn len(&self) -> usize {
        self.profiles.len() + 1
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
        let (msg, pulse) = self.list.menu(ev, self.len());
        self.choose(msg, pulse, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, self.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.choose(msg, pulse, ctx, fx);
        true
    }

    /// Radio, not toggle: re-selecting the bound row is a boundary; ◀/▶ do nothing.
    fn choose(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(choice) = self.choice(self.list.cursor) else {
            return pulse;
        };
        match msg {
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
            ListMsg::Activate => {
                let current = self.bound(ctx);
                if current.as_deref() == choice {
                    return Some(MenuPulse::Boundary);
                }
                fx.cmds.push(ConsoleCmd::BindProfile {
                    key: self.host_key.clone(),
                    profile_id: choice.map(str::to_owned),
                });
                Some(MenuPulse::Confirm)
            }
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        if self.profiles.is_empty() {
            return vec![Hint::new(HintKey::Back, "Done")];
        }
        vec![
            Hint::new(HintKey::Confirm, "Set default"),
            Hint::new(HintKey::Back, "Done"),
        ]
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
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        if self.profiles.is_empty() {
            fonts.centered(
                canvas,
                "No profiles yet \u{2014} create them in the desktop app, then choose one here.",
                W::Regular,
                14.0 * k,
                fg(0.55),
                cx,
                f64::from(rect.top) + f64::from(rect.height()) / 2.0,
                f64::from(rect.width()) * 0.7,
            );
            return;
        }
        // 34 px band under the list for the explainer, matching settings detail text.
        let detail_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom - detail_h as f32,
        );
        let bound = self.bound(ctx);
        let rows: Vec<RowSpec> = (0..self.len())
            .map(|i| {
                let (label, id) = if i == 0 {
                    ("No default".to_string(), None)
                } else {
                    let (id, name) = &self.profiles[i - 1];
                    (name.clone(), Some(id.as_str()))
                };
                let current = bound.as_deref() == id;
                RowSpec {
                    header: None,
                    label,
                    value: Some(if current {
                        "Default".into()
                    } else {
                        String::new()
                    }),
                    value_dim: !current,
                    caret: false,
                    adjustable: false,
                    enabled: true,
                }
            })
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
        fonts.centered(
            canvas,
            "What a plain press on this host's tile connects with. Pinned cards keep their own.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            cx,
            f64::from(rect.bottom) - detail_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostRow, ProfileChip};
    use pf_client_core::menu_nav::MenuDir;
    use pf_client_core::trust::Settings;

    fn host(bound: Option<&str>) -> HostRow {
        HostRow {
            key: "aa".into(),
            name: "Desk".into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_profile: bound.map(|id| ProfileChip {
                id: id.into(),
                name: "Work".into(),
                accent: None,
            }),
        }
    }

    fn screen() -> BindProfileScreen {
        BindProfileScreen::new(
            "aa".into(),
            "Desk".into(),
            vec![("p1".into(), "Work".into()), ("p2".into(), "Game".into())],
        )
    }

    #[test]
    fn choosing_a_profile_binds_and_no_default_clears() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        let hosts = [host(Some("p1"))];
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = screen();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::BindProfile {
                key: "aa".into(),
                profile_id: Some("p2".into()),
            }]
        );
        assert!(matches!(pulse, Some(MenuPulse::Confirm)));

        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::BindProfile {
                key: "aa".into(),
                profile_id: None,
            }]
        );
    }

    #[test]
    fn re_choosing_the_current_binding_is_a_boundary_not_a_command() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        let hosts = [host(Some("p1"))];
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = screen();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));

        let hosts = [host(None)];
        let mut settings = Settings::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        let mut s = screen();
        let mut fx = Outbox::default();
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }
}
