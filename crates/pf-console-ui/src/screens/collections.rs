//! The library's collections: one tile per group, and the sort that orders them all.
//!
//! This is the drill-in the whole of Part C exists for — group by platform, walk the
//! platforms, pick PS3, see its games. It deliberately borrows the home carousel's tile
//! language rather than inventing a third one: a collection is a place you go, exactly like
//! a host is, and the console should have one idea of what "a place you go" looks like.
//!
//! It owns no model. The groups come from [`crate::collate`] over the library screen's own
//! snapshot, and opening one pushes a `LibraryScreen` with a FILTER rather than a filtered
//! copy of the library — so the art pump, the fetch flow and the shared model never learn
//! that collections exist.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::collate::{collate, GroupBy, GroupKey, SortKey};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    initials, step_cursor, LibraryGame, LibraryShared, StepResult, BUMP_C, BUMP_K, BUMP_PX,
    ENTER_RISE, ENTER_SCALE, SPRING_C, SPRING_K,
};
use crate::model::HostRow;
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{accent, fg, Fonts, PanelStroke, W};
use crate::widgets::TabStrip;
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Image, Paint, RRect, Rect};
use std::collections::HashMap;

const TILE_W: f64 = 300.0;
const TILE_H: f64 = 210.0;
const TILE_GAP: f64 = 26.0;
const TILE_CORNER: f64 = 24.0;
/// How many poster thumbs fan across a tile. Three is enough to say "this is a shelf of
/// covers" and few enough that they stay recognisable at tile size.
const FAN: usize = 3;

pub(crate) struct CollectionsScreen {
    /// Everything needed to build the filtered library this screen pushes into. Copied at
    /// construction from the shelf that opened it, so a collection inherits its host AND
    /// its pinned profile — a drill-in off a pinned card's shelf must still launch the way
    /// that card does.
    host: HostRow,
    cursor: i32,
    anim: Spring,
    bump: Spring,
    sort: SortKey,
    sort_tabs: TabStrip,
    /// Group labels/keys/counts as of the last sync, and the poster ids that fan on each.
    groups: Vec<GroupTile>,
    generation: u64,
    /// Decoded posters, borrowed by id from whatever the library screen already fetched —
    /// this screen never asks for art of its own. A group whose covers have not arrived
    /// yet simply fans nothing and shows its monogram, which is also what a platform of
    /// art-less ROM entries looks like permanently.
    art: HashMap<String, Image>,
    geom: Vec<Rect>,
    entrance: Option<Entrance>,
    entrance_armed: bool,
}

struct GroupTile {
    key: GroupKey,
    label: String,
    count: usize,
    /// Up to [`FAN`] game ids whose posters, if decoded, fan across the tile.
    fan: Vec<String>,
}

impl CollectionsScreen {
    pub(crate) fn new(host: &HostRow, sort: SortKey) -> CollectionsScreen {
        CollectionsScreen {
            host: host.clone(),
            cursor: 0,
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            sort,
            sort_tabs: TabStrip::new(),
            groups: Vec::new(),
            generation: u64::MAX,
            art: HashMap::new(),
            geom: Vec::new(),
            entrance: None,
            entrance_armed: false,
        }
    }

    pub(crate) fn title(&self) -> String {
        self.host.name.clone()
    }

    /// Re-derive the tiles when the library moved under us (a rescan, a late fetch) or the
    /// sort changed. Cheap enough to do on a generation bump: collation is one pass and
    /// the tiles hold labels and ids, not games.
    fn sync(&mut self, library: &LibraryShared) {
        if library.generation() == self.generation {
            return;
        }
        let (_, games, generation) = library.snapshot();
        self.generation = generation;
        self.groups = collate(&games, self.sort, Some(GroupBy::Platform))
            .into_iter()
            .map(|g| GroupTile {
                count: g.games.len(),
                fan: g
                    .games
                    .iter()
                    .filter_map(|&i| games.get(i))
                    .map(|game: &LibraryGame| game.id.clone())
                    .take(FAN)
                    .collect(),
                key: g.key,
                label: g.label,
            })
            .collect();
        self.cursor = self.cursor.clamp(0, (self.groups.len() as i32 - 1).max(0));
    }

    /// Adopt whatever posters the library screen has already decoded. Read from the shared
    /// model's art queue would be wrong — that queue is drained by the shelf — so this
    /// takes a snapshot handed down at construction time instead.
    pub(crate) fn adopt_art(&mut self, art: HashMap<String, Image>) {
        self.art = art;
    }

    fn step(&mut self, delta: i32) -> Option<MenuPulse> {
        match step_cursor(self.cursor, self.groups.len(), delta, false) {
            StepResult::Moved(c) => {
                self.cursor = c;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                self.bump = Spring {
                    pos: -BUMP_PX * f64::from(delta.signum()),
                    vel: 0.0,
                };
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// Step the sort and persist it. The shelf reads `library_sort` every frame, so the
    /// collection tiles AND the shelf waiting behind this screen both re-order at once —
    /// which is the point of putting the pills here rather than in a dialog.
    fn step_sort(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = SortKey::ALL;
        let at = all.iter().position(|s| *s == self.sort).unwrap_or(0);
        let next = (at as i32 + delta).rem_euclid(all.len() as i32) as usize;
        self.sort = all[next];
        ctx.settings.library_sort = self.sort.id().to_string();
        ctx.settings.save();
        // Force a re-collate: the sort changed, not the library.
        self.generation = u64::MAX;
        Some(MenuPulse::Move)
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.sync(ctx.library);
        match ev {
            MenuEvent::Move(MenuDir::Left) => self.step(-1),
            MenuEvent::Move(MenuDir::Right) => self.step(1),
            MenuEvent::JumpBack => self.step_sort(-1, ctx),
            MenuEvent::JumpForward => self.step_sort(1, ctx),
            MenuEvent::Confirm => {
                let g = self.groups.get(self.cursor.max(0) as usize)?;
                let mut shelf = super::library::LibraryScreen::new(&self.host);
                shelf.set_filter(g.key.clone(), g.label.clone());
                fx.push(Screen::Library(shelf));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop();
                None
            }
            _ => None,
        }
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        self.sync(ctx.library);
        match p.kind {
            PointerKind::Scroll { up } => {
                self.step(if up { -1 } else { 1 });
                true
            }
            PointerKind::Press => {
                if let Some(i) = self.sort_tabs.pointer(p) {
                    let all = SortKey::ALL;
                    if let Some(&s) = all.get(i) {
                        self.sort = s;
                        ctx.settings.library_sort = s.id().to_string();
                        ctx.settings.save();
                        self.generation = u64::MAX;
                    }
                    return true;
                }
                match p.pick(&self.geom).filter(|i| *i < self.groups.len()) {
                    // The carousel's rule: a press brings a tile to the centre, and a press
                    // on the CENTRED tile opens it.
                    Some(i) if i == self.cursor as usize => {
                        self.menu(MenuEvent::Confirm, ctx, fx);
                        true
                    }
                    Some(i) => {
                        self.cursor = i as i32;
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, "Open"),
            Hint::new(HintKey::Shoulders, "Sort"),
            Hint::new(HintKey::Back, "Back"),
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
        self.sync(ctx.library);
        let labels: Vec<&str> = SortKey::ALL.iter().map(|s| s.label()).collect();
        let selected = SortKey::ALL
            .iter()
            .position(|s| *s == self.sort)
            .unwrap_or(0);
        let strip = Rect::from_xywh(rect.left, rect.top, rect.width(), (46.0 * k) as f32);
        self.sort_tabs
            .render(canvas, strip, &labels, selected, fonts, k, dt);

        let field = Rect::from_ltrb(rect.left, strip.bottom, rect.right, rect.bottom);
        self.anim
            .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
        self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        if crate::theme::reduce_motion() {
            self.bump = Spring::rest(0.0);
        }
        if !self.entrance_armed {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(
                entrances::CARDS,
                self.cursor.max(0) as usize,
                ctx.t,
            ));
        }
        if self.entrance.is_some_and(|e| e.done(ctx.t)) {
            self.entrance = None;
        }

        let w = f64::from(field.width());
        let tile_w = (TILE_W * k).min(w * 0.8);
        let tile_h = (TILE_H * k).min(f64::from(field.height()) - 40.0 * k);
        let pitch = tile_w + TILE_GAP * k;
        let cx0 = f64::from(field.left) + w / 2.0 + self.bump.pos * k;
        let cy = f64::from(field.top) + f64::from(field.height()) / 2.0;

        self.geom.clear();
        self.geom.resize(self.groups.len(), Rect::new_empty());
        for i in 0..self.groups.len() {
            let d = i as f64 - self.anim.pos;
            if d.abs() > 2.6 {
                continue;
            }
            let f = 1.0 - d.abs().min(1.0);
            let ent = self
                .entrance
                .map_or(EntranceAt::SETTLED, |e| e.at(i, ctx.t));
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let scale = (0.88 + 0.12 * f) * arrive;
            let alpha = (0.78 + 0.22 * f) * ent.fade;
            let cxx = cx0 + d * pitch;
            let cyy = cy + (1.0 - ent.travel) * ENTER_RISE * k;
            let tile = Rect::from_xywh(
                (cxx - tile_w / 2.0) as f32,
                (cyy - tile_h / 2.0) as f32,
                tile_w as f32,
                tile_h as f32,
            );
            self.geom[i] = Rect::from_xywh(
                (cxx - tile_w * scale / 2.0) as f32,
                (cyy - tile_h * scale / 2.0) as f32,
                (tile_w * scale) as f32,
                (tile_h * scale) as f32,
            );
            canvas.save();
            canvas.translate((cxx as f32, cyy as f32));
            canvas.scale((scale as f32, scale as f32));
            canvas.translate((-cxx as f32, -cyy as f32));
            let recede = 1.0 - f;
            let mut lp = Paint::default();
            lp.set_alpha_f(alpha as f32);
            if recede > 0.001 {
                lp.set_color_filter(skia_safe::color_filters::matrix_row_major(
                    &crate::theme::recede_matrix(recede),
                    None,
                ));
            }
            canvas.save_layer(&skia_safe::canvas::SaveLayerRec::default().paint(&lp));
            crate::theme::focus_halo(canvas, tile, TILE_CORNER as f32, k as f32, f as f32);
            if f > 0.4 {
                crate::theme::drop_shadow(
                    canvas,
                    tile,
                    TILE_CORNER as f32,
                    k as f32,
                    0.45 * f as f32,
                );
            }
            self.draw_tile(canvas, fonts, i, tile, k);
            canvas.restore();
            canvas.restore();
        }

        if self.groups.is_empty() {
            fonts.centered(
                canvas,
                "Nothing to collect yet — this library is still loading.",
                W::Regular,
                14.0 * k,
                fg(0.55),
                f64::from(field.left) + w / 2.0,
                cy,
                w * 0.7,
            );
        }
    }

    fn draw_tile(&self, canvas: &Canvas, fonts: &Fonts, i: usize, rect: Rect, k: f64) {
        let Some(g) = self.groups.get(i) else { return };
        crate::theme::panel(
            canvas,
            rect,
            TILE_CORNER as f32,
            Some(accent(0.16)),
            PanelStroke::Gradient,
            k as f32,
        );
        crate::theme::panel_highlight(canvas, rect, TILE_CORNER as f32, k as f32);

        // The fan: up to three covers overlapping to the right, newest on top — the visual
        // shorthand for "a stack of games" that says more about a collection than any count
        // does. Falls back to the group's monogram when none of them have art, which is
        // also the permanent look of a platform full of art-less ROM entries.
        let pad = 18.0 * k;
        let thumb_h = f64::from(rect.height()) - 2.0 * pad - 34.0 * k;
        let thumb_w = thumb_h * 2.0 / 3.0;
        let have: Vec<&Image> = g.fan.iter().filter_map(|id| self.art.get(id)).collect();
        let (l, t) = (f64::from(rect.left) + pad, f64::from(rect.top) + pad);
        if have.is_empty() {
            let badge = Rect::from_xywh(l as f32, t as f32, thumb_w as f32, thumb_h as f32);
            canvas.draw_rrect(
                RRect::new_rect_xy(badge, (10.0 * k) as f32, (10.0 * k) as f32),
                &Paint::new(Color4f::new(0.118, 0.118, 0.145, 1.0), None),
            );
            let mono = initials(&g.label);
            let size = thumb_h * 0.3;
            let font = fonts.font(W::Bold, size);
            let tw = font.measure_str(&mono, None).0;
            canvas.draw_str(
                &mono,
                skia_safe::Point::new(
                    badge.center_x() - tw / 2.0,
                    badge.center_y() + (size * 0.36) as f32,
                ),
                &font,
                &Paint::new(fg(0.45), None),
            );
        } else {
            for (n, img) in have.iter().enumerate() {
                let x = l + n as f64 * thumb_w * 0.42;
                let cell = Rect::from_xywh(x as f32, t as f32, thumb_w as f32, thumb_h as f32);
                canvas.save();
                canvas.clip_rrect(
                    RRect::new_rect_xy(cell, (10.0 * k) as f32, (10.0 * k) as f32),
                    None,
                    true,
                );
                let (iw, ih) = (img.width() as f32, img.height() as f32);
                let aspect = cell.width() / cell.height();
                let src = if iw / ih > aspect {
                    let sw = ih * aspect;
                    Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                } else {
                    let sh = iw / aspect;
                    Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                };
                canvas.draw_image_rect(
                    *img,
                    Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                    cell,
                    &Paint::default(),
                );
                canvas.restore();
            }
        }

        let text_x =
            l + thumb_w + (have.len().saturating_sub(1)) as f64 * thumb_w * 0.42 + 16.0 * k;
        let max_w = f64::from(rect.right) - pad - text_x;
        fonts.draw_clipped(
            canvas,
            &g.label,
            text_x,
            t + 26.0 * k,
            W::Bold,
            21.0 * k,
            fg(1.0),
            max_w,
        );
        let count = if g.count == 1 {
            "1 title".to_string()
        } else {
            format!("{} titles", g.count)
        };
        fonts.draw_clipped(
            canvas,
            &count,
            text_x,
            t + 50.0 * k,
            W::Regular,
            13.0 * k,
            fg(0.55),
            max_w,
        );
        // What KIND of collection this is, so "Steam" as a platform bucket and "Steam" as
        // a store read as the same thing they are.
        let kind = match &g.key {
            GroupKey::Launchers => "LAUNCHERS",
            GroupKey::Platform(_) => "PLATFORM",
            GroupKey::Store(_) => "STORE",
        };
        fonts.draw_tracked(
            canvas,
            kind,
            text_x,
            f64::from(rect.bottom) - pad,
            W::SemiBold,
            11.0 * k,
            1.4 * k,
            fg(0.45),
        );
    }
}
