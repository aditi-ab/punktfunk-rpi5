//! The quick-action ring's shortcuts on the console (design/touch-client-overlay.md §3.3, the
//! pad form): the blob's chords as a list, and an editor for one — a name typed on the
//! on-screen keyboard (Steam's on a Deck), the four modifiers as On/Off rows, the key stepped
//! with ◀ ▶ through everything `key_vk` knows, Save, and Remove. A new shortcut takes the first
//! empty ring slot, as on the phones. Writes go through the same load-then-save the settings
//! screen uses, so another writer's edits are never reverted.

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, EDGE_INSET, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec, ROW_MAX_W};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::overlay_actions::{
    chord_chip, key_legend, OverlayConfig, RingPlatform, Shortcut, SlotId,
};
use skia_safe::{Canvas, Rect};

const MODIFIERS: [&str; 4] = ["ctrl", "alt", "shift", "win"];

/// The keys a chord can end on, in stepping order — every name `key_vk` knows.
fn keys() -> Vec<String> {
    let mut v: Vec<String> = vec!["escape".into()];
    v.extend((1..=12).map(|n| format!("f{n}")));
    v.extend(('a'..='z').map(|c| c.to_string()));
    v.extend(('0'..='9').map(|c| c.to_string()));
    v.extend(
        [
            "tab",
            "space",
            "enter",
            "backspace",
            "delete",
            "insert",
            "home",
            "end",
            "pageup",
            "pagedown",
            "up",
            "down",
            "left",
            "right",
            "printscreen",
            "pause",
            "capslock",
        ]
        .into_iter()
        .map(String::from),
    );
    v
}

/// The editor's rows, in order.
const NAME: usize = 0;
const MOD0: usize = 1;
const KEY: usize = 5;
const SAVE: usize = 6;
const REMOVE: usize = 7;

/// One shortcut being edited: `id` is `None` for a new one.
struct Draft {
    id: Option<String>,
    label: String,
    mods: [bool; 4],
    key: usize,
}

pub(crate) struct RingShortcutsScreen {
    list: MenuList,
    keyboard: Keyboard,
    cfg: OverlayConfig,
    keys: Vec<String>,
    draft: Option<Draft>,
    editing_name: bool,
}

fn ring_platform(platform: crate::platform::Platform) -> RingPlatform {
    match platform {
        crate::platform::Platform::Desktop => RingPlatform::Desktop,
        crate::platform::Platform::Android => RingPlatform::Touch,
    }
}

/// The draft into the blob: over its own entry, or appended with the next `s<n>` id and into
/// the first empty slot. Pure, so a test can drive it without the shared settings file.
fn apply_draft(cfg: &mut OverlayConfig, d: &Draft, keys: &[String]) {
    let chord = RingShortcutsScreen::draft_keys(d, keys);
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

impl RingShortcutsScreen {
    pub(crate) fn new(ctx: &Ctx) -> RingShortcutsScreen {
        RingShortcutsScreen {
            list: MenuList::new(),
            keyboard: Keyboard::new(),
            cfg: OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform)),
            keys: keys(),
            draft: None,
            editing_name: false,
        }
    }

    pub(crate) fn title(&self) -> String {
        match &self.draft {
            Some(d) if d.id.is_some() => "Shortcut".into(),
            Some(_) => "New shortcut".into(),
            None => "Quick-action shortcuts".into(),
        }
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing_name
    }

    fn row_count(&self) -> usize {
        match &self.draft {
            Some(d) => {
                if d.id.is_some() {
                    REMOVE + 1
                } else {
                    SAVE + 1
                }
            }
            None => self.cfg.shortcuts.len() + 1,
        }
    }

    fn draft_keys(d: &Draft, keys: &[String]) -> Vec<String> {
        let mut v: Vec<String> = MODIFIERS
            .iter()
            .zip(d.mods)
            .filter(|(_, on)| *on)
            .map(|(m, _)| m.to_string())
            .collect();
        v.push(keys[d.key].clone());
        v
    }

    fn open(&mut self, sc: Option<&Shortcut>) {
        let draft = match sc {
            Some(sc) => Draft {
                id: Some(sc.id.clone()),
                label: sc.label.clone(),
                mods: [
                    sc.keys.iter().any(|k| k == "ctrl" || k == "control"),
                    sc.keys.iter().any(|k| k == "alt" || k == "option"),
                    sc.keys.iter().any(|k| k == "shift"),
                    sc.keys
                        .iter()
                        .any(|k| matches!(k.as_str(), "win" | "cmd" | "super" | "meta")),
                ],
                key: sc
                    .keys
                    .iter()
                    .rev()
                    .find_map(|k| self.keys.iter().position(|x| x == k))
                    .unwrap_or(0),
            },
            None => Draft {
                id: None,
                label: String::new(),
                mods: [false; 4],
                key: 0,
            },
        };
        self.draft = Some(draft);
        self.list.jump_to(0);
    }

    /// Write the draft into the blob: over its own entry, or appended and into the first
    /// empty slot. Rebases on the file first — this screen is one more whole-file writer.
    fn save(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        let Some(d) = self.draft.take() else { return };
        *ctx.settings = ctx.store.load();
        let mut cfg =
            OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform));
        apply_draft(&mut cfg, &d, &self.keys);
        ctx.settings.overlay_actions = cfg.to_json();
        ctx.store.save(ctx.settings);
        fx.toast = Some("Saved".into());
        self.cfg = cfg;
        self.list.jump_to(0);
    }

    fn remove(&mut self, ctx: &mut Ctx, fx: &mut Outbox) {
        let Some(d) = self.draft.take() else { return };
        let Some(id) = d.id else { return };
        *ctx.settings = ctx.store.load();
        let mut cfg =
            OverlayConfig::parse(&ctx.settings.overlay_actions, ring_platform(ctx.platform));
        remove_shortcut(&mut cfg, &id);
        ctx.settings.overlay_actions = cfg.to_json();
        ctx.store.save(ctx.settings);
        fx.toast = Some("Removed".into());
        self.cfg = cfg;
        self.list.jump_to(0);
    }

    fn type_char(&mut self, ch: char) -> bool {
        if !self.editing_name || !permits(Charset::Free, ch) {
            return false;
        }
        match self.draft.as_mut() {
            Some(d) => {
                d.label.push(ch);
                true
            }
            None => false,
        }
    }

    fn backspace(&mut self) -> bool {
        if !self.editing_name {
            return false;
        }
        self.draft.as_mut().is_some_and(|d| d.label.pop().is_some())
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

    /// ◀ ▶ on an editor row: a modifier toggles, the key steps (wrapping on A).
    fn adjust(&mut self, row: usize, delta: i32, wrap: bool) -> Option<MenuPulse> {
        let n = self.keys.len();
        let d = self.draft.as_mut()?;
        if (MOD0..MOD0 + 4).contains(&row) {
            let m = &mut d.mods[row - MOD0];
            let want = delta > 0;
            if *m == want && !wrap {
                return Some(MenuPulse::Boundary);
            }
            *m = if wrap { !*m } else { want };
            return Some(MenuPulse::Move);
        }
        if row == KEY {
            let next = d.key as i64 + delta as i64;
            if wrap {
                d.key = next.rem_euclid(n as i64) as usize;
            } else if next < 0 || next >= n as i64 {
                return Some(MenuPulse::Boundary);
            } else {
                d.key = next as usize;
            }
            return Some(MenuPulse::Move);
        }
        None
    }

    fn activate(&mut self, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let row = self.list.cursor;
        if self.draft.is_none() {
            let sc = self.cfg.shortcuts.get(row).cloned();
            self.open(sc.as_ref());
            return Some(MenuPulse::Confirm);
        }
        match row {
            NAME => {
                self.editing_name = true;
                Some(MenuPulse::Confirm)
            }
            SAVE => {
                self.save(ctx, fx);
                Some(MenuPulse::Confirm)
            }
            REMOVE => {
                self.remove(ctx, fx);
                Some(MenuPulse::Confirm)
            }
            _ => self.adjust(row, 1, true),
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
        let (msg, pulse) = self.list.pointer(p, self.row_count());
        match msg {
            ListMsg::Activate => {
                self.activate(ctx, fx);
                true
            }
            ListMsg::Adjust(delta) => {
                let row = self.list.cursor;
                self.adjust(row, delta, false);
                true
            }
            ListMsg::None => pulse.is_some(),
        }
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
        if ev == MenuEvent::Back {
            if self.draft.is_some() {
                // The editor peels back to the list, unsaved.
                self.draft = None;
                self.list.jump_to(0);
                return Some(MenuPulse::Confirm);
            }
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.row_count());
        match msg {
            ListMsg::Activate => self.activate(ctx, fx),
            ListMsg::Adjust(delta) => {
                let row = self.list.cursor;
                self.adjust(row, delta, false).or(pulse)
            }
            ListMsg::None => pulse,
        }
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
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(
                HintKey::Back,
                if self.draft.is_some() {
                    "Back"
                } else {
                    "Close"
                },
            ),
        ]
    }

    fn rows(&self) -> Vec<RowSpec> {
        match &self.draft {
            None => {
                let mut rows: Vec<RowSpec> = self
                    .cfg
                    .shortcuts
                    .iter()
                    .map(|sc| {
                        let mut r = RowSpec::field(
                            if sc.label.is_empty() {
                                chord_chip(&sc.keys)
                            } else {
                                sc.label.clone()
                            },
                            chord_chip(&sc.keys),
                            "",
                        );
                        r.adjustable = false;
                        r
                    })
                    .collect();
                rows.push(RowSpec::action("New shortcut", true));
                rows
            }
            Some(d) => {
                let mut name =
                    RowSpec::field("Name", d.label.clone(), "Optional — e.g. Task Manager");
                name.caret = self.editing_name;
                let mut rows = vec![name];
                for (i, m) in MODIFIERS.iter().enumerate() {
                    let mut r = RowSpec::field(
                        key_legend(m),
                        if d.mods[i] { "On" } else { "Off" }.to_string(),
                        "",
                    );
                    r.adjustable = true;
                    rows.push(r);
                }
                let mut key = RowSpec::field("Key", key_legend(&self.keys[d.key]), "");
                key.adjustable = true;
                rows.push(key);
                rows.push(RowSpec::action(
                    if d.id.is_some() {
                        "Save"
                    } else {
                        "Add shortcut"
                    },
                    true,
                ));
                if d.id.is_some() {
                    rows.push(RowSpec::action("Remove shortcut", true));
                }
                rows
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
        let lead = match &self.draft {
            None => {
                "The chords the quick-action ring can send. A new one takes the first empty slot."
            }
            Some(d) => {
                // Static text, so the legend goes in the Key row; here only the shape.
                let _ = d;
                "Hold the modifiers marked On, then the key. ◀ ▶ changes a row."
            }
        };
        fonts.leading(
            canvas,
            lead,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.72 * k,
        );
        let seat = self.keyboard.seat(self.editing_name && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + (34.0 * k) as f32,
            rect.right,
            rect.bottom - tray_h as f32,
        );
        let rows = self.rows();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, !self.editing_name);
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
    use pf_client_core::trust::Settings;

    fn with_ctx(settings: &mut Settings, f: impl FnOnce(&mut Ctx)) {
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            device_name: "t",
            t: 0.0,
        };
        f(&mut ctx);
    }

    /// A new shortcut: opened from the list, named on the keyboard path, Ctrl and Shift
    /// stepped on, the key left on Esc — applied, it is in the blob with its keys in send
    /// order and sits in the first empty slot; removed, that slot empties again. The blob is
    /// applied directly rather than through `save`: the store is one real file shared by
    /// every test in the process, and a round trip through it races the settings tests.
    #[test]
    fn a_new_shortcut_lands_in_the_blob_and_the_first_empty_slot() {
        let blob = r#"{"v":2,"ring":["end_stream",null,null,null,null,null]}"#;
        let mut settings = Settings {
            overlay_actions: blob.into(),
            ..Default::default()
        };
        with_ctx(&mut settings, |ctx| {
            let mut s = RingShortcutsScreen::new(ctx);
            let mut fx = Outbox::default();
            assert_eq!(s.rows().len(), 1, "no shortcuts yet: only the New row");
            s.activate(ctx, &mut fx); // New shortcut
            assert_eq!(s.title(), "New shortcut");
            s.editing_name = true;
            s.text_input("Task Manager");
            s.editing_name = false;
            s.adjust(MOD0, 1, false); // Ctrl on
            s.adjust(MOD0 + 2, 1, false); // Shift on
            assert!(
                matches!(s.adjust(KEY, -1, false), Some(MenuPulse::Boundary)),
                "◀ at the first key thuds"
            );
            let d = s.draft.take().expect("the draft");
            assert_eq!(d.key, 0, "Esc is the first key");
            let mut cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
            apply_draft(&mut cfg, &d, &s.keys);
            assert_eq!(cfg.shortcuts.len(), 1);
            assert_eq!(cfg.shortcuts[0].label, "Task Manager");
            assert_eq!(cfg.shortcuts[0].keys, vec!["ctrl", "shift", "escape"]);
            assert_eq!(cfg.ring[1], Some(SlotId::Shortcut("s1".into())));
            // Reopened for editing, its rows show what it holds; removed, the slot empties.
            s.cfg = cfg.clone();
            assert_eq!(s.rows().len(), 2, "the shortcut and the New row");
            s.list.jump_to(0);
            s.activate(ctx, &mut fx);
            assert_eq!(s.title(), "Shortcut");
            assert_eq!(s.rows().len(), REMOVE + 1, "an existing one carries Remove");
            let d = s.draft.take().expect("the draft");
            assert_eq!(d.mods, [true, false, true, false]);
            assert_eq!(d.label, "Task Manager");
            remove_shortcut(&mut cfg, "s1");
            assert!(cfg.shortcuts.is_empty());
            assert_eq!(cfg.ring[1], None);
        });
    }
}
