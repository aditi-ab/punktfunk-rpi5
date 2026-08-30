//! A shortcut's editor on the console (design/touch-client-overlay.md §3.3, the pad form):
//! the disc as the ring will draw it, a name typed on the on-screen keyboard (Steam's on a
//! Deck), the four modifiers as chips, and the key on a keyboard-shaped grid you walk with the
//! stick or click — every name `key_vk` knows, laid out the way a keyboard lays them out. Save,
//! and Remove for an existing one. A new shortcut takes the first empty ring slot, as on the
//! phones. Writes go through the same load-then-save the settings screen uses.

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::ring::draw_keycap_disc;
use crate::screens::{Ctx, Outbox};
use crate::theme::{accent, fg, fill, focus_halo, on_accent, stroke, Fonts, EDGE_INSET, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{chord_chip, key_legend, OverlayConfig, Shortcut, SlotId};
use skia_safe::{Canvas, RRect, Rect};

use super::ring_editor::ring_platform;

const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "win"];

/// The key grid, row by row, the way a keyboard lays them out — every name `key_vk` knows
/// that is not a modifier.
const GRID: [&[&str]; 6] = [
    &[
        "escape", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
    ],
    &[
        "1",
        "2",
        "3",
        "4",
        "5",
        "6",
        "7",
        "8",
        "9",
        "0",
        "backspace",
    ],
    &[
        "tab", "q", "w", "e", "r", "t", "y", "u", "i", "o", "p", "insert", "delete",
    ],
    &[
        "capslock", "a", "s", "d", "f", "g", "h", "j", "k", "l", "enter",
    ],
    &[
        "z", "x", "c", "v", "b", "n", "m", "home", "end", "pageup", "pagedown",
    ],
    &[
        "space",
        "left",
        "up",
        "down",
        "right",
        "printscreen",
        "pause",
    ],
];

/// Where the pad is: the name field, a modifier chip, a key, or Save / Remove.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Zone {
    Name,
    Mods(usize),
    Grid(usize, usize),
    Actions(usize),
}

/// One shortcut being edited: `id` is `None` for a new one.
pub(crate) struct Draft {
    pub id: Option<String>,
    pub label: String,
    pub mods: [bool; 4],
    pub key: Option<String>,
}

impl Draft {
    /// A draft of `sc`, or an empty one for a new shortcut.
    fn of(sc: Option<&Shortcut>) -> Draft {
        let Some(sc) = sc else {
            return Draft {
                id: None,
                label: String::new(),
                mods: [false; 4],
                key: None,
            };
        };
        let has = |names: &[&str]| sc.keys.iter().any(|k| names.contains(&k.as_str()));
        Draft {
            id: Some(sc.id.clone()),
            label: sc.label.clone(),
            mods: [
                has(&["ctrl", "control"]),
                has(&["alt", "option"]),
                has(&["shift"]),
                has(&["win", "cmd", "super", "meta"]),
            ],
            key: sc
                .keys
                .iter()
                .rev()
                .find(|k| GRID.iter().any(|row| row.contains(&k.as_str())))
                .cloned(),
        }
    }

    /// The chord in send order: the modifiers marked on, then the key.
    fn keys(&self) -> Vec<String> {
        let mut v: Vec<String> = MODIFIERS
            .iter()
            .zip(self.mods)
            .filter(|(_, on)| *on)
            .map(|(m, _)| m.to_string())
            .collect();
        v.extend(self.key.clone());
        v
    }
}

/// The draft into the blob: over its own entry, or appended with the next `s<n>` id and into
/// the first empty slot. Pure, so a test can drive it without the shared settings file.
fn apply_draft(cfg: &mut OverlayConfig, d: &Draft) {
    let chord = d.keys();
    let label = d.label.trim().to_string();
    match &d.id {
        Some(id) => {
            if let Some(sc) = cfg.shortcuts.iter_mut().find(|s| &s.id == id) {
                sc.label = label;
                sc.keys = chord;
            }
        }
        None => {
            let next = cfg
                .shortcuts
                .iter()
                .filter_map(|s| s.id.trim_start_matches('s').parse::<u32>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            let id = format!("s{next}");
            if let Some(slot) = cfg.ring.iter_mut().find(|s| s.is_none()) {
                *slot = Some(SlotId::Shortcut(id.clone()));
            }
            cfg.shortcuts.push(Shortcut {
                id,
                label,
                keys: chord,
            });
        }
    }
}

/// Drop a shortcut and empty the slot that pointed at it (`parse` would on the next read;
/// doing it here shows it at once).
fn remove_shortcut(cfg: &mut OverlayConfig, id: &str) {
    cfg.shortcuts.retain(|s| s.id != id);
    for slot in cfg.ring.iter_mut() {
        if matches!(slot, Some(SlotId::Shortcut(s)) if s == id) {
            *slot = None;
        }
    }
}

/// The key in `row` whose centre is nearest `x`, so Up and Down keep the column a keyboard
/// would (the rows are staggered).
fn nearest_col(row: &[Rect], x: f32) -> usize {
    row.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| {
            (a.center_x() - x)
                .abs()
                .total_cmp(&(b.center_x() - x).abs())
        })
        .map_or(0, |(i, _)| i)
}

pub(crate) struct ShortcutEditorScreen {
    draft: Draft,
    zone: Zone,
    keyboard: Keyboard,
    editing_name: bool,
    // Hit rects as drawn last frame.
    name_rect: Rect,
    mod_rects: [Rect; 4],
    key_rects: Vec<Vec<Rect>>,
    action_rects: Vec<Rect>,
}

impl ShortcutEditorScreen {
    pub(crate) fn new(_ctx: &Ctx, existing: Option<&Shortcut>) -> ShortcutEditorScreen {
        let draft = Draft::of(existing);
        ShortcutEditorScreen {
            zone: if draft.key.is_none() {
                Zone::Grid(0, 0)
            } else {
                Zone::Name
            },
            draft,
            keyboard: Keyboard::new(),
            editing_name: false,
            name_rect: Rect::new_empty(),
            mod_rects: [Rect::new_empty(); 4],
            key_rects: GRID
                .iter()
                .map(|r| vec![Rect::new_empty(); r.len()])
                .collect(),
            action_rects: Vec::new(),
        }
    }

    pub(crate) fn title(&self) -> String {
        if self.draft.id.is_some() {
            "Shortcut".into()
        } else {
            "New shortcut".into()
        }
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing_name
    }

    fn action_count(&self) -> usize {
        if self.draft.id.is_some() {
            2
        } else {
            1
        }
    }

    fn save(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        if self.draft.key.is_none() {
            fx.toast = Some("Pick a key first".into());
            self.zone = Zone::Grid(0, 0);
            return;
        }
        *ctx.settings = ctx.store.load();
        let mut cfg =
            OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform));
        apply_draft(&mut cfg, &self.draft);
        ctx.settings.overlay_actions = cfg.to_json();
        ctx.store.save(ctx.settings);
        fx.toast = Some("Saved".into());
        fx.pop();
    }

    fn remove(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        let Some(id) = self.draft.id.clone() else {
            return;
        };
        *ctx.settings = ctx.store.load();
        let mut cfg =
            OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform));
        remove_shortcut(&mut cfg, &id);
        ctx.settings.overlay_actions = cfg.to_json();
        ctx.store.save(ctx.settings);
        fx.toast = Some("Removed".into());
        fx.pop();
    }

    fn type_char(&mut self, ch: char) -> bool {
        if !self.editing_name || !permits(Charset::Free, ch) {
            return false;
        }
        self.draft.label.push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        self.editing_name && self.draft.label.pop().is_some()
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    pub(crate) fn edit_key(&mut self, key: crate::input::Key) -> bool {
        use crate::input::Key as K;
        if !self.editing_name {
            return false;
        }
        match key {
            K::Backspace => {
                self.backspace();
                true
            }
            K::Return | K::Escape => {
                self.editing_name = false;
                true
            }
            _ => false,
        }
    }

    /// A on the zone: the name opens the keyboard, a chip toggles, a key is chosen, Save
    /// and Remove do what they say.
    fn activate(&mut self, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match self.zone {
            Zone::Name => {
                self.editing_name = true;
                Some(MenuPulse::Confirm)
            }
            Zone::Mods(i) => {
                self.draft.mods[i] = !self.draft.mods[i];
                Some(MenuPulse::Confirm)
            }
            Zone::Grid(r, c) => {
                self.draft.key = Some(GRID[r][c].to_string());
                Some(MenuPulse::Confirm)
            }
            Zone::Actions(0) => {
                self.save(ctx, fx);
                Some(MenuPulse::Confirm)
            }
            Zone::Actions(_) => {
                self.remove(ctx, fx);
                Some(MenuPulse::Confirm)
            }
        }
    }

    /// The stick between the zones: Up and Down walk name → modifiers → the grid's rows →
    /// the actions, keeping the column a keyboard would; Left and Right walk within a row.
    fn step(&mut self, dir: MenuDir) -> Option<MenuPulse> {
        let last_row = GRID.len() - 1;
        let next = match (self.zone, dir) {
            (Zone::Name, MenuDir::Down) => Zone::Mods(0),
            (Zone::Mods(i), MenuDir::Left) if i > 0 => Zone::Mods(i - 1),
            (Zone::Mods(i), MenuDir::Right) if i + 1 < MODIFIERS.len() => Zone::Mods(i + 1),
            (Zone::Mods(_), MenuDir::Up) => Zone::Name,
            (Zone::Mods(i), MenuDir::Down) => {
                let x = self.mod_rects[i].center_x();
                Zone::Grid(0, nearest_col(&self.key_rects[0], x))
            }
            (Zone::Grid(r, c), MenuDir::Left) if c > 0 => Zone::Grid(r, c - 1),
            (Zone::Grid(r, c), MenuDir::Right) if c + 1 < GRID[r].len() => Zone::Grid(r, c + 1),
            (Zone::Grid(r, c), MenuDir::Up) => {
                let x = self.key_rects[r][c].center_x();
                if r == 0 {
                    Zone::Mods(nearest_col(&self.mod_rects, x))
                } else {
                    Zone::Grid(r - 1, nearest_col(&self.key_rects[r - 1], x))
                }
            }
            (Zone::Grid(r, c), MenuDir::Down) => {
                let x = self.key_rects[r][c].center_x();
                if r == last_row {
                    Zone::Actions(0)
                } else {
                    Zone::Grid(r + 1, nearest_col(&self.key_rects[r + 1], x))
                }
            }
            (Zone::Actions(i), MenuDir::Left) if i > 0 => Zone::Actions(i - 1),
            (Zone::Actions(i), MenuDir::Right) if i + 1 < self.action_count() => {
                Zone::Actions(i + 1)
            }
            (Zone::Actions(_), MenuDir::Up) => Zone::Grid(last_row, 0),
            _ => return Some(MenuPulse::Boundary),
        };
        self.zone = next;
        Some(MenuPulse::Move)
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if self.editing_name {
            if ctx.deck {
                return match ev {
                    MenuEvent::Back | MenuEvent::Confirm => {
                        self.editing_name = false;
                        Some(MenuPulse::Confirm)
                    }
                    _ => None,
                };
            }
            let (msg, pulse) = self.keyboard.menu(ev);
            return match msg {
                KeyMsg::Type(c) => Some(if self.type_char(c) {
                    MenuPulse::Move
                } else {
                    MenuPulse::Boundary
                }),
                KeyMsg::Backspace => Some(if self.backspace() {
                    MenuPulse::Move
                } else {
                    MenuPulse::Boundary
                }),
                KeyMsg::Done => {
                    self.editing_name = false;
                    Some(MenuPulse::Confirm)
                }
                KeyMsg::None => pulse,
            };
        }
        match ev {
            MenuEvent::Back => {
                // Unsaved: the editor peels back to the ring.
                fx.pop();
                None
            }
            MenuEvent::Confirm => self.activate(ctx, fx),
            MenuEvent::Move(dir) => self.step(dir),
            _ => None,
        }
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.editing_name && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.editing_name = false;
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keyboard.pointer(p);
            match msg {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    self.backspace();
                }
                KeyMsg::Done => self.editing_name = false,
                KeyMsg::None => {}
            }
            return true;
        }
        if !p.press() {
            return false;
        }
        let zone = if p.hits(self.name_rect) {
            Some(Zone::Name)
        } else if let Some(i) = p.pick(&self.mod_rects) {
            Some(Zone::Mods(i))
        } else if let Some(i) = p.pick(&self.action_rects) {
            Some(Zone::Actions(i))
        } else {
            self.key_rects
                .iter()
                .enumerate()
                .find_map(|(r, row)| p.pick(row).map(|c| Zone::Grid(r, c)))
        };
        let Some(zone) = zone else {
            return false;
        };
        self.zone = zone;
        self.activate(ctx, fx);
        true
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing_name {
            if ctx.deck {
                return vec![
                    Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                    Hint::new(HintKey::Confirm, "Done"),
                    Hint::new(HintKey::Back, "Done"),
                ];
            }
            return vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        let label = match self.zone {
            Zone::Name => "Name",
            Zone::Mods(_) => "Toggle",
            Zone::Grid(..) => "Choose key",
            Zone::Actions(0) => "Save",
            Zone::Actions(_) => "Remove",
        };
        vec![
            Hint::new(HintKey::Confirm, label),
            Hint::new(HintKey::Back, "Back"),
        ]
    }

    /// A rounded key, chip or button: filled with the accent when `on`, haloed when focused.
    #[allow(clippy::too_many_arguments)]
    fn cap(
        canvas: &Canvas,
        fonts: &Fonts,
        rect: Rect,
        text: &str,
        size: f64,
        on: bool,
        focused: bool,
        k: f32,
    ) {
        let corner = 8.0 * k;
        let rr = RRect::new_rect_xy(rect, corner, corner);
        canvas.draw_rrect(rr, &fill(if on { accent(1.0) } else { fg(0.10) }));
        canvas.draw_rrect(rr, &stroke(fg(if on { 0.0 } else { 0.16 }), k));
        if focused {
            focus_halo(canvas, rect, corner, k, 1.0);
        }
        let color = if on { on_accent() } else { fg(0.92) };
        let tw = fonts.measure(text, W::Medium, size);
        fonts.draw(
            canvas,
            text,
            f64::from(rect.center_x() - tw / 2.0),
            f64::from(rect.center_y()) + size * 0.36,
            W::Medium,
            size,
            color,
        );
    }

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
        let kf = k as f32;
        let x0 = f64::from(rect.left) + EDGE_INSET * k;
        let content_w = (ROW_MAX_W * 1.15 * k).min(f64::from(rect.width()) - 2.0 * EDGE_INSET * k);
        let cx = f64::from(rect.center_x());
        let left = (cx - content_w / 2.0) as f32;
        let right = (cx + content_w / 2.0) as f32;
        fonts.leading(
            canvas,
            "Hold the modifiers marked on, then the key. Pick the key on the grid.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            x0,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.9 * k,
        );
        let focused = |z: Zone| self.zone == z && !self.editing_name;

        // The disc as the ring draws it, the legend beside it, the name to the right.
        let top = rect.top + 34.0 * kf;
        let r = 34.0 * kf;
        let chip = chord_chip(&self.draft.keys());
        draw_keycap_disc(
            canvas,
            fonts,
            left + r + 4.0 * kf,
            top + r + 6.0 * kf,
            r,
            kf,
            &chip,
        );
        let legend_x = f64::from(left + 2.0 * r + 22.0 * kf);
        fonts.draw(
            canvas,
            if chip.is_empty() {
                "Pick a key"
            } else {
                chip.as_str()
            },
            legend_x,
            f64::from(top) + 34.0 * k,
            W::SemiBold,
            20.0 * k,
            fg(if chip.is_empty() { 0.45 } else { 0.95 }),
        );
        fonts.draw(
            canvas,
            "How the ring will draw it",
            legend_x,
            f64::from(top) + 56.0 * k,
            W::Regular,
            12.0 * k,
            fg(0.5),
        );
        let name_w = (280.0 * kf).min(content_w as f32 * 0.45);
        let name = Rect::from_xywh(right - name_w, top + 6.0 * kf, name_w, 62.0 * kf);
        self.name_rect = name;
        let name_rr = RRect::new_rect_xy(name, 12.0 * kf, 12.0 * kf);
        canvas.draw_rrect(name_rr, &fill(fg(0.08)));
        canvas.draw_rrect(name_rr, &stroke(fg(0.16), kf));
        if focused(Zone::Name) || self.editing_name {
            focus_halo(canvas, name, 12.0 * kf, kf, 1.0);
        }
        fonts.draw(
            canvas,
            "Name",
            f64::from(name.left) + 14.0 * k,
            f64::from(name.top) + 20.0 * k,
            W::Medium,
            11.0 * k,
            fg(0.5),
        );
        let shown = if self.draft.label.is_empty() && !self.editing_name {
            "Optional — e.g. Task Manager".to_string()
        } else if self.editing_name {
            format!("{}|", self.draft.label)
        } else {
            self.draft.label.clone()
        };
        fonts.draw_clipped(
            canvas,
            &shown,
            f64::from(name.left) + 14.0 * k,
            f64::from(name.top) + 44.0 * k,
            W::Regular,
            15.0 * k,
            fg(if self.draft.label.is_empty() && !self.editing_name {
                0.4
            } else {
                0.95
            }),
            f64::from(name.width()) - 28.0 * k,
        );

        // The modifiers as chips.
        let mods_y = top + 92.0 * kf;
        fonts.draw(
            canvas,
            "Modifiers",
            f64::from(left),
            f64::from(mods_y) + 12.0 * k,
            W::Medium,
            11.0 * k,
            fg(0.5),
        );
        let mut x = left + 84.0 * kf;
        for (i, m) in MODIFIERS.iter().enumerate() {
            let legend = key_legend(m);
            let w = fonts.measure(&legend, W::Medium, 14.0 * k) + 30.0 * kf;
            let chip_rect = Rect::from_xywh(x, mods_y - 4.0 * kf, w, 32.0 * kf);
            self.mod_rects[i] = chip_rect;
            Self::cap(
                canvas,
                fonts,
                chip_rect,
                &legend,
                14.0 * k,
                self.draft.mods[i],
                focused(Zone::Mods(i)),
                kf,
            );
            x += w + 10.0 * kf;
        }

        // The key grid, each row centred, wide keys as wide as their word.
        let key_h = 38.0 * kf;
        let gap = 6.0 * kf;
        let unit = 34.0 * kf;
        let mut y = mods_y + 44.0 * kf;
        for (ri, row) in GRID.iter().enumerate() {
            let legends: Vec<String> = row.iter().map(|n| key_legend(n)).collect();
            let widths: Vec<f32> = legends
                .iter()
                .map(|l| (fonts.measure(l, W::Medium, 13.0 * k) + 16.0 * kf).max(unit))
                .collect();
            let total: f32 = widths.iter().sum::<f32>() + gap * (row.len() as f32 - 1.0);
            let mut x = cx as f32 - total / 2.0;
            for (ci, (name, w)) in row.iter().zip(&widths).enumerate() {
                let key_rect = Rect::from_xywh(x, y, *w, key_h);
                self.key_rects[ri][ci] = key_rect;
                Self::cap(
                    canvas,
                    fonts,
                    key_rect,
                    &legends[ci],
                    13.0 * k,
                    self.draft.key.as_deref() == Some(*name),
                    focused(Zone::Grid(ri, ci)),
                    kf,
                );
                x += w + gap;
            }
            y += key_h + gap;
        }

        // Save, and Remove for an existing one.
        let actions: Vec<&str> = if self.draft.id.is_some() {
            vec!["Save", "Remove shortcut"]
        } else {
            vec!["Add shortcut"]
        };
        let y = y + 10.0 * kf;
        let mut x = left;
        self.action_rects.clear();
        for (i, a) in actions.iter().enumerate() {
            let w = fonts.measure(a, W::Medium, 15.0 * k) + 44.0 * kf;
            let button = Rect::from_xywh(x, y, w, 40.0 * kf);
            self.action_rects.push(button);
            Self::cap(
                canvas,
                fonts,
                button,
                a,
                15.0 * k,
                i == 0,
                focused(Zone::Actions(i)),
                kf,
            );
            x += w + 12.0 * kf;
        }

        // The keyboard tray, while the name is being typed on a screen without Steam's.
        let seat = self.keyboard.seat(self.editing_name && !ctx.deck, dt);
        if seat > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat,
                k,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::overlay_actions::RingPlatform;

    /// A new shortcut: Ctrl and Shift marked on, Esc chosen on the grid — applied, it is in
    /// the blob with its keys in send order and sits in the first empty slot; reopened, the
    /// draft reads it back; removed, that slot empties again. The blob is applied directly
    /// rather than through `save`: the store is one real file shared by every test in the
    /// process, and a round trip through it races the settings tests.
    #[test]
    fn a_new_shortcut_lands_in_the_blob_and_the_first_empty_slot() {
        let blob = r#"{"v":2,"ring":["end_stream",null,null,null,null,null]}"#;
        let mut d = Draft::of(None);
        d.label = "Task Manager".into();
        d.mods[0] = true;
        d.mods[2] = true;
        d.key = Some("escape".into());
        assert_eq!(d.keys(), vec!["ctrl", "shift", "escape"]);
        let mut cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        apply_draft(&mut cfg, &d);
        assert_eq!(cfg.shortcuts.len(), 1);
        assert_eq!(cfg.shortcuts[0].label, "Task Manager");
        assert_eq!(cfg.shortcuts[0].keys, vec!["ctrl", "shift", "escape"]);
        assert_eq!(cfg.ring[1], Some(SlotId::Shortcut("s1".into())));
        let back = Draft::of(Some(&cfg.shortcuts[0]));
        assert_eq!(back.id.as_deref(), Some("s1"));
        assert_eq!(back.mods, [true, false, true, false]);
        assert_eq!(back.key.as_deref(), Some("escape"));
        remove_shortcut(&mut cfg, "s1");
        assert!(cfg.shortcuts.is_empty());
        assert_eq!(cfg.ring[1], None);
    }

    /// Every key on the grid is one `key_vk` knows, none is a modifier, and none repeats.
    #[test]
    fn the_grid_holds_every_key_once_and_no_modifier() {
        use pf_client_core::overlay_actions::key_vk;
        let mut seen: Vec<&str> = Vec::new();
        for row in GRID {
            for name in row {
                assert!(key_vk(name).is_some(), "{name} is unknown to the wire");
                assert!(!MODIFIERS.contains(name), "{name} is a modifier");
                assert!(!seen.contains(name), "{name} twice");
                seen.push(name);
            }
        }
        assert_eq!(seen.len(), 66);
    }

    /// Up and Down keep the column a keyboard would: the nearest centre in the next row.
    #[test]
    fn the_grid_cursor_keeps_its_column_across_staggered_rows() {
        let row: Vec<Rect> = (0..5)
            .map(|i| Rect::from_xywh(i as f32 * 40.0 + 20.0, 0.0, 34.0, 30.0))
            .collect();
        assert_eq!(nearest_col(&row, 0.0), 0);
        assert_eq!(nearest_col(&row, 118.0), 2);
        assert_eq!(nearest_col(&row, 999.0), 4);
        assert_eq!(nearest_col(&[], 10.0), 0);
    }
}
