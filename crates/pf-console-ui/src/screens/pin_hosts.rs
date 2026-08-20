//! "Pin “Work”" — choose which saved hosts show a profile as an extra connect card
//! (design/client-settings-profiles.md §5.2a), reached from the settings screen's
//! Profiles section. One toggle row per saved host; a toggle rides
//! [`ConsoleCmd::SetPin`] to the binary, which persists `KnownHost::pinned_profiles`
//! and refreshes the rows — the row's shown state follows the model, so what the list
//! says is always what the store holds (and what Decky's host list will render).

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

pub(crate) struct PinHostsScreen {
    profile_id: String,
    profile_name: String,
    list: MenuList,
}

/// The toggle rows' domain: every SAVED host, primary tiles only (a pinned card is the
/// OUTPUT of this screen, not a row in it), in the model's carousel order.
fn host_indices(ctx: &Ctx) -> Vec<usize> {
    ctx.hosts
        .iter()
        .enumerate()
        .filter(|(_, h)| h.saved && h.pin.is_none())
        .map(|(i, _)| i)
        .collect()
}

impl PinHostsScreen {
    pub(crate) fn new(profile_id: String, profile_name: String) -> PinHostsScreen {
        PinHostsScreen {
            profile_id,
            profile_name,
            list: MenuList::new(),
        }
    }

    pub(crate) fn profile_name(&self) -> &str {
        &self.profile_name
    }

    /// Is this profile currently pinned on the host at `ctx.hosts[host_idx]`? Read from
    /// the model — the pinned card's row IS the state, so the toggle can never disagree
    /// with what the carousel shows.
    fn pinned(&self, ctx: &Ctx, host_idx: usize) -> bool {
        let host = &ctx.hosts[host_idx];
        ctx.hosts.iter().any(|r| {
            r.addr == host.addr
                && r.port == host.port
                && r.pin.as_ref().is_some_and(|p| p.id == self.profile_id)
        })
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
        let indices = host_indices(ctx);
        let (msg, pulse) = self.list.menu(ev, indices.len());
        self.toggle(msg, pulse, &indices, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let indices = host_indices(ctx);
        let (msg, pulse) = self.list.pointer(p, indices.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.toggle(msg, pulse, &indices, ctx, fx);
        true
    }

    /// One list message against the focused host's pin — shared by both input paths.
    fn toggle(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        indices: &[usize],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(&host_idx) = indices.get(self.list.cursor) else {
            return pulse;
        };
        // Toggle semantics shared with the settings rows: left = unpin, right = pin,
        // A flips; asking for the state it's already in is a boundary thud.
        let target = match msg {
            ListMsg::Adjust(delta) => delta > 0,
            ListMsg::Activate => !self.pinned(ctx, host_idx),
            ListMsg::None => return pulse,
        };
        if self.pinned(ctx, host_idx) == target {
            return Some(MenuPulse::Boundary);
        }
        fx.cmds.push(ConsoleCmd::SetPin {
            key: ctx.hosts[host_idx].key.clone(),
            profile_id: self.profile_id.clone(),
            pin: target,
        });
        Some(MenuPulse::Move)
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if host_indices(ctx).is_empty() {
            return vec![Hint::new(HintKey::Back, "Done")];
        }
        vec![
            Hint::new(HintKey::Confirm, "Pin / Unpin"),
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
        let indices = host_indices(ctx);
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        if indices.is_empty() {
            fonts.centered(
                canvas,
                "No saved hosts yet — pair with a host first, then pin this profile to it.",
                W::Regular,
                14.0 * k,
                fg(0.55),
                cx,
                f64::from(rect.top) + f64::from(rect.height()) / 2.0,
                f64::from(rect.width()) * 0.7,
            );
            return;
        }
        // The explainer band under the list, like the settings screen's detail text.
        let detail_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom - detail_h as f32,
        );
        let rows: Vec<RowSpec> = indices
            .iter()
            .map(|&i| {
                let h = &ctx.hosts[i];
                let pinned = self.pinned(ctx, i);
                RowSpec {
                    header: None,
                    label: h.name.clone(),
                    value: Some(if pinned {
                        "Pinned".into()
                    } else {
                        "Off".into()
                    }),
                    value_dim: !pinned,
                    caret: false,
                    adjustable: true,
                    enabled: true,
                }
            })
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
        fonts.centered(
            canvas,
            "A pinned profile appears as its own card on the host — one press connects with it.",
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
    use crate::screens::Outbox;
    use pf_client_core::trust::Settings;

    fn host(key: &str, saved: bool, pin: Option<&str>) -> HostRow {
        HostRow {
            key: key.into(),
            name: key.into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: key.into(),
            paired: true,
            saved,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            pin: pin.map(|id| ProfileChip {
                id: id.into(),
                name: "Work".into(),
                accent: None,
            }),
            bound_profile: None,
        }
    }

    #[test]
    fn toggling_sends_set_pin_for_the_focused_host() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        let hosts = [host("aa", true, None), host("bb", true, None)];
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
        let mut s = PinHostsScreen::new("p1".into(), "Work".into());
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetPin {
                key: "aa".into(),
                profile_id: "p1".into(),
                pin: true,
            }]
        );

        // Left on an unpinned host = already off = boundary, no command.
        let mut fx = Outbox::default();
        let pulse = s.menu(
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Left),
            &mut ctx,
            &mut fx,
        );
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }

    #[test]
    fn state_reads_from_the_models_pinned_rows() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        // Host "aa" already carries a pinned card for p1; its primary row toggles OFF.
        let hosts = [host("aa", true, None), host("aa\0p1", true, Some("p1"))];
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
        let mut s = PinHostsScreen::new("p1".into(), "Work".into());
        // Only the primary row is a toggle row.
        assert_eq!(host_indices(&ctx).len(), 1);
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetPin {
                key: "aa".into(),
                profile_id: "p1".into(),
                pin: false,
            }]
        );
    }
}
