//! The quick-action ring's editor on the console (design/touch-client-overlay.md §3.3): the
//! editor IS the ring — the in-stream [`Ring`], the same type, full size over a gradient
//! backdrop, in its editing mode. A pad: stick or D-pad to a slot, A picks what it holds from
//! the catalogue by group, Y lifts a disc and A drops it on another to swap, B goes back. A
//! pointer: click a disc to pick, carry it onto another to swap. Under the ring the shortcuts
//! sit as rows — name and legend — with New shortcut and Reset to default; a row opens
//! [`super::shortcut_editor::ShortcutEditorScreen`]. Writes go through the same load-then-save
//! the settings screen uses, so another writer's edits are never reverted.

use crate::glyphs::{Hint, HintKey};
use crate::pointer::{Pointer, PointerKind};
use crate::ring::{EditEvent, Ring};
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, fill, focus_halo, stage_gradient, stroke, Fonts, EDGE_INSET, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{chord_chip, OverlayConfig, RingPlatform, SlotId};
use pf_client_core::ring::RingFacts;
use skia_safe::{Canvas, Color4f, RRect, Rect};

/// The stage under the ring and the caption over it, in design units.
const STAGE_H: f64 = 330.0;
const CAPTION_H: f64 = 34.0;
const CORNER: f64 = 22.0;

/// The ring rows parse the blob on the platform's default ring, like the ring itself does.
pub(crate) fn ring_platform(platform: crate::platform::Platform) -> RingPlatform {
    match platform {
        crate::platform::Platform::Desktop => RingPlatform::Desktop,
        crate::platform::Platform::Android => RingPlatform::Touch,
    }
}

/// One thing a slot can hold: `(id, label, note)`; the empty id is the empty slot.
type Entry = (String, String, String);

/// The catalogue by group (§3.3): what a slot can hold, with each entry's availability note
/// on this shell.
fn catalogue(cfg: &OverlayConfig) -> Vec<(&'static str, Vec<Entry>)> {
    let e =
        |id: &str, label: &str, note: &str| (id.to_string(), label.to_string(), note.to_string());
    let host = "Only where the host offers it";
    let mut g = vec![
        (
            "Session",
            vec![
                e("end_stream", "End stream", ""),
                e("disconnect_linger", "Disconnect, keep the game running", ""),
            ],
        ),
        (
            "Input",
            vec![
                e("touch_mode", "Touch mode", ""),
                e("keyboard", "Keyboard", ""),
                e("pad", "Virtual controller", "Phones and tablets only"),
                e("send_text", "Send text", "Not on this client yet"),
            ],
        ),
        ("View", vec![e("stats", "Statistics", "")]),
        ("Audio", vec![e("mic", "Microphone", "")]),
        (
            "Host",
            vec![
                e("host:power.sleep", "Sleep host", host),
                e("host:power.reboot", "Restart host", host),
                e("host:power.shutdown", "Shut down host", host),
            ],
        ),
    ];
    if !cfg.shortcuts.is_empty() {
        g.push((
            "Shortcuts",
            cfg.shortcuts
                .iter()
                .map(|sc| {
                    let chip = chord_chip(&sc.keys);
                    if sc.label.is_empty() {
                        (format!("shortcut:{}", sc.id), chip, String::new())
                    } else {
                        (format!("shortcut:{}", sc.id), sc.label.clone(), chip)
                    }
                })
                .collect(),
        ));
    }
    g.push(("Empty", vec![e("", "Empty slot", "")]));
    g
}

/// The picker over a slot: the catalogue as a list, the current pick marked.
struct Picker {
    slot: usize,
    list: MenuList,
    rows: Vec<(String, RowSpec)>,
    rect: Rect,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Ring,
    List,
}

pub(crate) struct RingEditorScreen {
    ring: Ring,
    cfg: OverlayConfig,
    /// The blob `cfg` came from — the settings string, re-adopted whenever it changes under
    /// this screen (the shortcut editor writes it and pops back here).
    blob: String,
    list: MenuList,
    focus: Focus,
    picker: Option<Picker>,
    stage: Rect,
    list_rect: Rect,
    reset_armed: bool,
}

impl RingEditorScreen {
    pub(crate) fn new(ctx: &Ctx) -> RingEditorScreen {
        let mut s = RingEditorScreen {
            ring: Ring::new(),
            cfg: OverlayConfig::platform_default(ring_platform(ctx.platform)),
            blob: String::new(),
            list: MenuList::new(),
            focus: Focus::Ring,
            picker: None,
            stage: Rect::new_empty(),
            list_rect: Rect::new_empty(),
            reset_armed: false,
        };
        s.ring.edit_at(0.0, 0.0);
        s.adopt(&ctx.settings.overlay_actions, ctx.platform);
        s
    }

    pub(crate) fn title(&self) -> String {
        "Quick actions".into()
    }

    /// Take the blob as the ring to edit. The ring parses on the desktop default, so an
    /// empty blob is handed the platform's own default written out.
    fn adopt(&mut self, blob: &str, platform: crate::platform::Platform) {
        self.blob = blob.to_string();
        self.cfg = OverlayConfig::parse(blob, ring_platform(platform));
        let effective = if blob.trim().is_empty() {
            self.cfg.to_json()
        } else {
            blob.to_string()
        };
        self.ring.set_facts(&RingFacts {
            overlay_actions: effective,
            touch_mode: "trackpad".into(),
            stats_tier: "Compact".into(),
            mic_available: true,
            mode: (1920, 1080, 60),
            native_mode: (1920, 1080, 60),
            ..RingFacts::default()
        });
    }

    /// Write `blob` rebased on the file (this screen is one more whole-file writer), and
    /// show it.
    fn write(&mut self, blob: String, ctx: &mut Ctx) {
        *ctx.settings = ctx.store.load();
        ctx.settings.overlay_actions = blob;
        ctx.store.save(ctx.settings);
        let b = ctx.settings.overlay_actions.clone();
        self.adopt(&b, ctx.platform);
    }

    fn pick(&mut self, slot: usize, id: &str, ctx: &mut Ctx) {
        let mut cfg = self.cfg.clone();
        cfg.ring[slot] = if id.is_empty() {
            None
        } else {
            SlotId::parse(id)
        };
        self.write(cfg.to_json(), ctx);
    }

    fn swap(&mut self, a: usize, b: usize, ctx: &mut Ctx) {
        let mut cfg = self.cfg.clone();
        cfg.ring.swap(a, b);
        self.write(cfg.to_json(), ctx);
    }

    /// Back to the platform ring: an empty blob, like the settings row did.
    fn reset(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        self.write(String::new(), ctx);
        fx.toast = Some("Quick actions reset".into());
    }

    fn open_picker(&mut self, slot: usize) {
        let current = self.cfg.ring[slot]
            .as_ref()
            .map(SlotId::id)
            .unwrap_or_default();
        let mut rows: Vec<(String, RowSpec)> = Vec::new();
        let mut cursor = 0;
        for (group, entries) in catalogue(&self.cfg) {
            for (i, (id, label, note)) in entries.into_iter().enumerate() {
                let is_current = id == current;
                let value = match (is_current, note.is_empty()) {
                    (true, true) => "Current".to_string(),
                    (true, false) => format!("Current · {note}"),
                    (false, _) => note,
                };
                let mut r = RowSpec::field(label, value, "");
                r.adjustable = false;
                if i == 0 {
                    r.header = Some(group);
                }
                if is_current {
                    cursor = rows.len();
                }
                rows.push((id, r));
            }
        }
        let mut list = MenuList::new();
        list.jump_to(cursor);
        self.picker = Some(Picker {
            slot,
            list,
            rows,
            rect: Rect::new_empty(),
        });
    }

    fn drain_edits(&mut self, ctx: &mut Ctx) {
        while let Some(ev) = self.ring.take_edit() {
            match ev {
                EditEvent::Pick(k) => self.open_picker(k),
                EditEvent::Swap(a, b) => self.swap(a, b, ctx),
            }
        }
    }

    /// The rows under the ring: the shortcuts, New shortcut, Reset to default.
    fn rows(&self) -> Vec<RowSpec> {
        let mut rows: Vec<RowSpec> = self
            .cfg
            .shortcuts
            .iter()
            .enumerate()
            .map(|(i, sc)| {
                let chip = chord_chip(&sc.keys);
                let mut r = if sc.label.is_empty() {
                    RowSpec::field(chip, String::new(), "")
                } else {
                    RowSpec::field(sc.label.clone(), chip, "")
                };
                r.adjustable = false;
                if i == 0 {
                    r.header = Some("Shortcuts");
                }
                r
            })
            .collect();
        let mut new = RowSpec::action("New shortcut", true);
        if rows.is_empty() {
            new.header = Some("Shortcuts");
        }
        rows.push(new);
        rows.push(RowSpec::action(
            if self.reset_armed {
                "Press again to reset the ring"
            } else {
                "Reset to default"
            },
            true,
        ));
        rows
    }

    fn row_count(&self) -> usize {
        self.cfg.shortcuts.len() + 2
    }

    fn activate_row(&mut self, i: usize, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let n = self.cfg.shortcuts.len();
        if i < n {
            fx.push(Screen::ShortcutEditor(
                super::shortcut_editor::ShortcutEditorScreen::new(
                    ctx,
                    Some(&self.cfg.shortcuts[i]),
                ),
            ));
            self.reset_armed = false;
            return Some(MenuPulse::Confirm);
        }
        if i == n {
            fx.push(Screen::ShortcutEditor(
                super::shortcut_editor::ShortcutEditorScreen::new(ctx, None),
            ));
            self.reset_armed = false;
            return Some(MenuPulse::Confirm);
        }
        if self.reset_armed {
            self.reset_armed = false;
            self.reset(ctx, fx);
            Some(MenuPulse::Confirm)
        } else {
            self.reset_armed = true;
            Some(MenuPulse::Boundary)
        }
    }

    fn picker_choose(&mut self, ctx: &mut Ctx) {
        let Some(p) = self.picker.take() else { return };
        let Some((id, _)) = p.rows.get(p.list.cursor) else {
            return;
        };
        let id = id.clone();
        self.pick(p.slot, &id, ctx);
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if let Some(p) = self.picker.as_mut() {
            if ev == MenuEvent::Back {
                self.picker = None;
                return Some(MenuPulse::Confirm);
            }
            let (msg, pulse) = p.list.menu(ev, p.rows.len());
            return match msg {
                ListMsg::Activate => {
                    self.picker_choose(ctx);
                    Some(MenuPulse::Confirm)
                }
                ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                ListMsg::None => pulse,
            };
        }
        if matches!(ev, MenuEvent::Move(_)) {
            self.reset_armed = false;
        }
        match self.focus {
            Focus::Ring => {
                // Down past 6 o'clock walks into the list; anywhere else the ring keeps it.
                if ev == MenuEvent::Move(MenuDir::Down)
                    && self.ring.highlight() == Some(3)
                    && !self.ring.carrying()
                {
                    self.focus = Focus::List;
                    self.list.jump_to(0);
                    return Some(MenuPulse::Move);
                }
                let pulse = self.ring.menu(ev);
                self.drain_edits(ctx);
                if pulse.is_none() && ev == MenuEvent::Back {
                    fx.pop();
                }
                pulse
            }
            Focus::List => {
                if ev == MenuEvent::Back {
                    fx.pop();
                    return None;
                }
                if ev == MenuEvent::Move(MenuDir::Up) && self.list.cursor == 0 {
                    self.focus = Focus::Ring;
                    self.ring.set_highlight(3);
                    return Some(MenuPulse::Move);
                }
                let (msg, pulse) = self.list.menu(ev, self.row_count());
                match msg {
                    ListMsg::Activate => self.activate_row(self.list.cursor, ctx, fx),
                    ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
                    ListMsg::None => pulse,
                }
            }
        }
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if let Some(pk) = self.picker.as_mut() {
            // A modal: what lands on the card is the list's, a press elsewhere dismisses it.
            if p.hits(pk.rect) || matches!(p.kind, PointerKind::Scroll { .. }) {
                let (msg, _) = pk.list.pointer(p, pk.rows.len());
                if msg == ListMsg::Activate {
                    self.picker_choose(ctx);
                }
            } else if p.press() {
                self.picker = None;
            }
            return true;
        }
        if self.ring.pointer(p) {
            self.focus = Focus::Ring;
            self.reset_armed = false;
            self.drain_edits(ctx);
            return true;
        }
        if p.hits(self.list_rect) || matches!(p.kind, PointerKind::Scroll { .. }) {
            if p.press() {
                self.focus = Focus::List;
            }
            let (msg, pulse) = self.list.pointer(p, self.row_count());
            return match msg {
                ListMsg::Activate => {
                    self.activate_row(self.list.cursor, ctx, fx);
                    true
                }
                ListMsg::Adjust(_) => true,
                ListMsg::None => pulse.is_some(),
            };
        }
        false
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        if self.picker.is_some() {
            return vec![
                Hint::new(HintKey::Confirm, "Choose"),
                Hint::new(HintKey::Back, "Cancel"),
            ];
        }
        match self.focus {
            Focus::Ring => {
                let carrying = self.ring.carrying();
                vec![
                    Hint::new(
                        HintKey::Confirm,
                        if carrying { "Drop here" } else { "Change" },
                    ),
                    Hint::new(
                        HintKey::Secondary,
                        if carrying { "Put down" } else { "Lift" },
                    ),
                    Hint::new(HintKey::Back, if carrying { "Put down" } else { "Back" }),
                ]
            }
            Focus::List => {
                let n = self.cfg.shortcuts.len();
                let label = match self.list.cursor {
                    i if i < n => "Edit",
                    i if i == n => "Add",
                    _ => "Reset",
                };
                vec![
                    Hint::new(HintKey::Confirm, label),
                    Hint::new(HintKey::Back, "Back"),
                ]
            }
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
        if ctx.settings.overlay_actions != self.blob {
            let b = ctx.settings.overlay_actions.clone();
            self.adopt(&b, ctx.platform);
        }
        self.ring.tick();
        let kf = k as f32;
        fonts.leading(
            canvas,
            "A on a button changes it. Y lifts a button and A drops it on another to swap; \
             with a pointer, click or drag.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.9 * k,
        );

        // The stage: a soft, colourful gradient — glass needs something behind it to read
        // as glass, and a flat colour would lie about the look.
        let stage_w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 48.0 * k) as f32;
        let stage = Rect::from_xywh(
            rect.center_x() - stage_w / 2.0,
            rect.top + (CAPTION_H + 12.0) as f32 * kf,
            stage_w,
            (STAGE_H * k) as f32,
        );
        self.stage = stage;
        let rr = RRect::new_rect_xy(stage, CORNER as f32 * kf, CORNER as f32 * kf);
        canvas.draw_rrect(rr, &stage_gradient(stage));
        canvas.draw_rrect(rr, &stroke(fg(0.14), kf));
        if self.focus == Focus::Ring && self.picker.is_none() {
            focus_halo(canvas, stage, CORNER as f32 * kf, kf, 1.0);
        }

        // The ring, centred on the stage; its own pixels only inside it.
        self.ring
            .recentre(stage.center_x(), stage.top + stage.height() * 0.46);
        canvas.save();
        canvas.clip_rrect(rr, None, None);
        self.ring.render(
            canvas,
            rect.right.max(1.0) as u32,
            rect.bottom.max(1.0) as u32,
            kf,
            fonts,
            dt,
        );
        canvas.restore();

        // The shortcuts under it.
        let list_rect =
            Rect::from_ltrb(rect.left, stage.bottom + 10.0 * kf, rect.right, rect.bottom);
        self.list_rect = list_rect;
        let rows = self.rows();
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.focus == Focus::List && self.picker.is_none(),
        );

        // The picker: a card over everything, the way the ring's own sheet sits.
        if let Some(pk) = self.picker.as_mut() {
            canvas.draw_rect(rect, &fill(Color4f::new(0.0, 0.0, 0.0, 0.45)));
            let w = (520.0 * kf).min(rect.width() * 0.92);
            let h = ((pk.rows.len() as f32 * 50.0 + 60.0) * kf).min(rect.height() * 0.9);
            let card = Rect::from_xywh(rect.center_x() - w / 2.0, rect.center_y() - h / 2.0, w, h);
            canvas.draw_rrect(
                RRect::new_rect_xy(card, 16.0 * kf, 16.0 * kf),
                &fill(Color4f::new(0.06, 0.055, 0.1, 0.96)),
            );
            canvas.draw_rrect(
                RRect::new_rect_xy(card, 16.0 * kf, 16.0 * kf),
                &stroke(fg(0.18), kf),
            );
            let inner = Rect::from_ltrb(
                card.left + 8.0 * kf,
                card.top + 12.0 * kf,
                card.right - 8.0 * kf,
                card.bottom - 12.0 * kf,
            );
            let specs: Vec<RowSpec> = pk.rows.iter().map(|(_, r)| r.clone()).collect();
            canvas.save();
            canvas.clip_rect(inner, None, None);
            pk.list.render(canvas, inner, &specs, fonts, k, dt, true);
            canvas.restore();
            pk.rect = card;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalogue is the phones' list in the phones' order, notes where the shell has
    /// them, the blob's shortcuts by name (legend as the note), and Empty last.
    #[test]
    fn the_catalogue_is_grouped_and_ends_with_empty() {
        let blob = r#"{"v":2,"ring":[],"shortcuts":[{"id":"s1","label":"Task Manager","keys":["ctrl","shift","escape"]},{"id":"s2","keys":["alt","f4"]}]}"#;
        let cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        let groups = catalogue(&cfg);
        let names: Vec<&str> = groups.iter().map(|(g, _)| *g).collect();
        assert_eq!(
            names,
            [
                "Session",
                "Input",
                "View",
                "Audio",
                "Host",
                "Shortcuts",
                "Empty"
            ]
        );
        let input = &groups[1].1;
        assert_eq!(input[2].0, "pad");
        assert_eq!(input[2].2, "Phones and tablets only");
        let shortcuts = &groups[5].1;
        assert_eq!(
            shortcuts[0],
            (
                "shortcut:s1".to_string(),
                "Task Manager".to_string(),
                "Ctrl+Shift+Esc".to_string()
            )
        );
        assert_eq!(
            shortcuts[1],
            (
                "shortcut:s2".to_string(),
                "Alt+F4".to_string(),
                String::new()
            )
        );
        assert_eq!(groups[6].1[0].0, "");
        let empty = OverlayConfig::parse("", RingPlatform::Desktop);
        assert!(
            catalogue(&empty).iter().all(|(g, _)| *g != "Shortcuts"),
            "no group for no shortcuts"
        );
    }
}
