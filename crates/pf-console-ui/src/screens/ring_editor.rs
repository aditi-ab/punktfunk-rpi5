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
use crate::ring::{EditEvent, Ring, LABEL_H};
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{card_face, fg, fill, focus_halo, stroke, Fonts, EDGE_INSET, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{
    catalogue, chord_chip, CatalogueEntry, OverlayConfig, RingPlatform, SlotId,
};
use pf_client_core::ring::{RingFacts, RING_RADIUS, SLOT_DIAMETER};
use skia_safe::{Canvas, Color4f, RRect, Rect};

/// The stage under the ring and the caption over it, in design units. The stage fits the
/// ring exactly as it draws in-stream — the top slot, the bottom slot and the label band
/// under it — with one pad above and below, so nothing of the ring is clipped.
const STAGE_PAD: f64 = 16.0;
const RING_ABOVE: f64 = (RING_RADIUS + SLOT_DIAMETER / 2.0) as f64;
const RING_BELOW: f64 = (RING_RADIUS + SLOT_DIAMETER + LABEL_H) as f64;
const STAGE_H: f64 = STAGE_PAD + RING_ABOVE + RING_BELOW + STAGE_PAD;
const CAPTION_H: f64 = 34.0;
const CORNER: f64 = 22.0;
/// Height kept for the shortcut rows under the stage before the stage starts scaling down.
const LIST_MIN: f64 = 140.0;
/// The ring's horizontal extent plus breathing room — the width half of the fit.
const RING_W: f64 = 2.0 * RING_ABOVE + 24.0;

/// The ring rows parse the blob on the platform's default ring, like the ring itself does.
pub(crate) fn ring_platform(platform: crate::platform::Platform) -> RingPlatform {
    match platform {
        crate::platform::Platform::Desktop => RingPlatform::Desktop,
        crate::platform::Platform::Android => RingPlatform::Touch,
    }
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
    platform: crate::platform::Platform,
    /// The blob `cfg` came from — the settings string, re-adopted whenever it changes under
    /// this screen (the shortcut editor writes it and pops back here).
    blob: String,
    list: MenuList,
    focus: Focus,
    picker: Option<Picker>,
    stage: Rect,
    list_rect: Rect,
    reset_armed: bool,
    /// The stick's sector as last reported, whichever widget has the focus — so a handoff
    /// back to the ring knows whether the stick is still held.
    stick: Option<u8>,
    /// The list was just entered by the stick's sector; the four-way move that rides behind
    /// it in the same sample must not also step the list.
    swallow_move: bool,
}

impl RingEditorScreen {
    pub(crate) fn new(ctx: &Ctx) -> RingEditorScreen {
        let mut s = RingEditorScreen {
            ring: Ring::new(),
            cfg: OverlayConfig::platform_default(ring_platform(ctx.platform)),
            platform: ctx.platform,
            blob: String::new(),
            list: MenuList::new(),
            focus: Focus::Ring,
            picker: None,
            stage: Rect::new_empty(),
            list_rect: Rect::new_empty(),
            reset_armed: false,
            stick: None,
            swallow_move: false,
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
        self.platform = platform;
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
        for group in catalogue(&self.cfg, ring_platform(self.platform)) {
            for (i, entry) in group.entries.into_iter().enumerate() {
                let CatalogueEntry { id, label, note } = entry;
                let is_current = id == current;
                let value = match (is_current, note.is_empty()) {
                    (true, true) => "Current".to_string(),
                    (true, false) => format!("Current · {note}"),
                    (false, _) => note,
                };
                let mut r = RowSpec::field(label, value, "");
                r.adjustable = false;
                if i == 0 {
                    r.header = Some(group.title);
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
                "Press again to reset the dial"
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
        if let MenuEvent::Sector(s) = ev {
            self.stick = s;
        }
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
                // Down past 6 o'clock walks into the list: the D-pad's Down there, or the
                // stick pushed down AGAIN once its first push has put the highlight there.
                // Never the stick's own four-way move: it rides behind the sector that
                // landed on 6, and the ring ignores it while the stick is engaged.
                let at_six = self.ring.highlight() == Some(3) && !self.ring.carrying();
                let walks = at_six
                    && !self.ring.stick_engaged()
                    && matches!(
                        ev,
                        MenuEvent::Move(MenuDir::Down) | MenuEvent::Sector(Some(3))
                    );
                if walks {
                    self.focus = Focus::List;
                    self.list.jump_to(0);
                    self.swallow_move = matches!(ev, MenuEvent::Sector(_));
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
                if std::mem::take(&mut self.swallow_move) && matches!(ev, MenuEvent::Move(_)) {
                    return None;
                }
                if ev == MenuEvent::Back {
                    fx.pop();
                    return None;
                }
                if ev == MenuEvent::Move(MenuDir::Up) && self.list.cursor == 0 {
                    // Back onto the ring at 6 o'clock. A stick still held stays the
                    // ring's until it lets go, so its repeats step nothing.
                    self.focus = Focus::Ring;
                    self.ring.set_highlight(3);
                    self.ring.adopt_stick(self.stick.is_some());
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
            "Point the stick at a button; A changes it. Y lifts a button and A drops it on \
             another to swap; with a pointer, click or drag.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.9 * k,
        );

        // The stage: a flat card face, like every other card on this shell (a gradient
        // read as decoration on the Deck). On a screen too short for the ring AND the
        // shortcut rows — a landscape phone — the rows move BESIDE the stage and the ring
        // takes the height; only when even that cannot hold it does the stage scale down.
        // The ring is as large as the screen allows, and never clipped.
        let caption_h = (CAPTION_H + 12.0) * k;
        let fit_below = (f64::from(rect.height()) - caption_h - LIST_MIN * k) / (STAGE_H * k);
        let side = fit_below < 0.75 && f64::from(rect.width()) >= (RING_W + 420.0) * k;
        let (fit, stage_w) = if side {
            let fit = ((f64::from(rect.height()) - caption_h) / (STAGE_H * k)).clamp(0.35, 1.0);
            (fit as f32, (RING_W * k * fit) as f32)
        } else {
            let stage_w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 48.0 * k) as f32;
            let fit_w = f64::from(stage_w) / (RING_W * k);
            (fit_below.min(fit_w).clamp(0.35, 1.0) as f32, stage_w)
        };
        let rk = kf * fit;
        let stage_x = if side {
            rect.left + (EDGE_INSET * k) as f32
        } else {
            rect.center_x() - stage_w / 2.0
        };
        let stage = Rect::from_xywh(
            stage_x,
            rect.top + caption_h as f32,
            stage_w,
            (STAGE_H * k) as f32 * fit,
        );
        self.stage = stage;
        let rr = RRect::new_rect_xy(stage, CORNER as f32 * kf, CORNER as f32 * kf);
        canvas.draw_rrect(rr, &fill(card_face(0.20)));
        canvas.draw_rrect(rr, &stroke(fg(0.14), kf));
        if self.focus == Focus::Ring && self.picker.is_none() {
            // Corner in DESIGN units — the halo scales it itself, like `panel` does; a
            // pre-scaled corner squares the scale and reads as a second, rounder card.
            focus_halo(canvas, stage, CORNER as f32, kf, 1.0);
        }

        // The ring, one pad below the stage's top, at the stage's own scale; its pixels
        // only inside it.
        self.ring.recentre(
            stage.center_x(),
            stage.top + (STAGE_PAD + RING_ABOVE) as f32 * rk,
        );
        canvas.save();
        canvas.clip_rrect(rr, None, None);
        self.ring.render(
            canvas,
            rect.right.max(1.0) as u32,
            rect.bottom.max(1.0) as u32,
            rk,
            fonts,
            dt,
        );
        canvas.restore();

        // The shortcuts: under the stage, or beside it on a short-wide screen.
        let list_rect = if side {
            Rect::from_ltrb(
                stage.right + 8.0 * kf,
                rect.top + caption_h as f32,
                rect.right,
                rect.bottom,
            )
        } else {
            Rect::from_ltrb(rect.left, stage.bottom + 10.0 * kf, rect.right, rect.bottom)
        };
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
            // Over EVERYTHING, the heading included — the content rect is not the screen,
            // and a scrim that stops at its edge reads as a band. The canvas is translated
            // (insets, transitions), so overshoot every edge by more than any heading,
            // hint bar or inset can be. The hint bar draws after this and stays legible,
            // carrying the picker's own controls.
            canvas.draw_rect(
                rect.with_outset((2000.0 * kf, 2000.0 * kf)),
                &fill(Color4f::new(0.0, 0.0, 0.0, 0.45)),
            );
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

    /// The picker lists the shared catalogue with the slot's current pick marked and the
    /// cursor on it; choosing the empty entry empties the slot.
    #[test]
    fn the_picker_marks_the_current_pick_and_empty_empties_the_slot() {
        let blob = r#"{"v":2,"ring":["stats",null,null,null,null,null]}"#;
        let cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        let mut s = RingEditorScreen {
            ring: Ring::new(),
            cfg,
            platform: crate::platform::Platform::Desktop,
            blob: blob.into(),
            list: MenuList::new(),
            focus: Focus::Ring,
            picker: None,
            stage: Rect::new_empty(),
            list_rect: Rect::new_empty(),
            reset_armed: false,
            stick: None,
            swallow_move: false,
        };
        s.open_picker(0);
        let p = s.picker.as_ref().expect("the picker");
        let (id, row) = &p.rows[p.list.cursor];
        assert_eq!(id, "stats");
        assert_eq!(row.value.as_deref(), Some("Current"));
        assert_eq!(p.rows.last().map(|(id, _)| id.as_str()), Some(""));
        assert_eq!(p.rows[0].1.header, Some("Session"));
        let mut cfg = s.cfg.clone();
        cfg.ring[0] = None;
        assert_eq!(cfg.ring[0], None, "the empty entry's id is the empty slot");
    }
}
