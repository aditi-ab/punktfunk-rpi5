//! The console's focusable widgets: the vertical menu list (settings / add-host / pair
//! screens) and the controller-driven on-screen keyboard — ports of the Swift
//! `GamepadMenuList` / `GamepadKeyboard`, immediate-mode: the WIDGET owns cursor,
//! springs and scroll, the SCREEN owns the domain (row content and what an activation
//! means), and every frame the screen hands the widget fresh row specs.

use crate::anim::{approach, springs, Spring, TRAY_C, TRAY_K};
use crate::library::{BUMP_C, BUMP_K};
use crate::pointer::{Pointer, PointerKind};
use crate::theme::{accent, fg, Fonts, PanelStroke, W};
use pf_client_core::gamepad::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Paint, PathBuilder, RRect, Rect};

// --- Menu list -----------------------------------------------------------------------------

/// What a menu-list consumed event means for the owning screen.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ListMsg {
    None,
    /// Left/right on the focused row (the screen returns whether the value changed).
    Adjust(i32),
    /// A on the focused row.
    Activate,
}

/// One row's look, rebuilt by the screen each frame (never stale).
pub(crate) struct RowSpec {
    /// Section header drawn above this row (the first row of each group carries it).
    pub header: Option<&'static str>,
    pub label: String,
    /// `None` = an action row (its label draws centered, brand-tinted).
    pub value: Option<String>,
    /// Placeholder styling for an empty field's value.
    pub value_dim: bool,
    /// The live-edit caret after the value (this row is what the keyboard types into).
    pub caret: bool,
    /// Show ‹ › chevrons while focused (left/right steps the value).
    pub adjustable: bool,
    /// Rows render dimmed when they aren't actionable: an action row's centered label loses
    /// its brand tint, a value row's label greys — the look for a setting that depends on
    /// another one being on (Echo cancellation under Microphone).
    pub enabled: bool,
}

impl RowSpec {
    pub(crate) fn field(label: impl Into<String>, value: String, placeholder: &str) -> RowSpec {
        let empty = value.is_empty();
        RowSpec {
            header: None,
            label: label.into(),
            value: Some(if empty {
                placeholder.to_string()
            } else {
                value
            }),
            value_dim: empty,
            caret: false,
            adjustable: false,
            enabled: true,
        }
    }

    pub(crate) fn action(label: impl Into<String>, enabled: bool) -> RowSpec {
        RowSpec {
            header: None,
            label: label.into(),
            value: None,
            value_dim: false,
            caret: false,
            adjustable: false,
            enabled,
        }
    }
}

pub(crate) const ROW_H: f64 = 50.0;
const ROW_GAP: f64 = 6.0;
const HEADER_H: f64 = 34.0;
pub(crate) const ROW_MAX_W: f64 = 620.0;

/// How far a stepped value slips before springing back, design units. The affordable 80 %
/// of Apple's option-band drum: one sprung position plus a crossfade, rather than
/// neighbours rendered in 3-D — our rows draw their value as a single text run, so a real
/// drum would mean laying out values we never otherwise measure.
const SLIP_DP: f64 = 14.0;
/// Accumulated slip is capped here so a held repeat reads as one accelerating travel
/// rather than throwing the value off the row.
const SLIP_MAX: f64 = 22.0;
/// The confirm dip's floor — the visual sibling of the haptic that already fires.
const PRESS_DIP: f64 = 0.97;

/// The focus list: authoritative cursor, spring recoil at the ends, a scroll offset
/// that chases the focused row, and a per-row focus amount for the scale/tint ease.
pub(crate) struct MenuList {
    pub cursor: usize,
    bump: Spring,
    scroll: f64,
    /// The COLOUR channel of focus (tint, alpha, chevrons), eased with `approach`.
    /// Deliberately not sprung: an overshooting colour would overshoot past the palette's
    /// accent into a tint that isn't in the palette at all.
    focus: Vec<f64>,
    /// The SCALE channel, sprung — the whisker of overshoot is the pop that makes a
    /// focused row read as picked up rather than merely tinted.
    focus_pop: Vec<Spring>,
    /// The activated row's dip, resting at 1.0. One spring, not one per row: only the
    /// focused row can be activated, so only one can ever be dipping.
    press: Spring,
    /// A stepped value's displacement, chasing 0 from ±[`SLIP_DP`]. Never reset on a new
    /// step — its velocity is what makes held repeats accumulate into one travel instead of
    /// restarting the crossfade.
    slip: Spring,
    /// The value the slip is sliding OUT: `(row, text, offset)`. The offset is where that
    /// text sits RELATIVE to the incoming one (∓[`SLIP_DP`], set from the step direction),
    /// so the pair keeps travelling the right way round even after the spring reverses
    /// through zero on an accumulated repeat.
    slip_prev: Option<(usize, String, f64)>,
    /// Direction of the step the list last emitted, consumed by the next render. The list
    /// arms the slip ITSELF by noticing the value changed, so no screen has to report it —
    /// and a refused adjust (the value didn't move) correctly produces no slip at all.
    step_dir: i32,
    /// Each row's value string as last drawn — the "before" half of the crossfade, and how
    /// the list detects that an adjust actually landed.
    shown: Vec<String>,
    /// Next render, seat the scroll and the focus ease instantly instead of chasing — see
    /// [`MenuList::jump_to`].
    snap: bool,
    /// Each row's rect as the last frame actually drew it, device px — what a pointer hit-
    /// tests against. One entry per row, `Rect::new_empty()` for rows scrolled out of view,
    /// so an index into this is an index into `rows`.
    geom: Vec<Rect>,
}

impl MenuList {
    pub(crate) fn new() -> MenuList {
        MenuList {
            cursor: 0,
            bump: Spring::rest(0.0),
            scroll: 0.0,
            focus: Vec::new(),
            focus_pop: Vec::new(),
            press: Spring::rest(1.0),
            slip: Spring::rest(0.0),
            slip_prev: None,
            step_dir: 0,
            shown: Vec::new(),
            snap: true,
            geom: Vec::new(),
        }
    }

    /// Move the cursor WITHOUT the scroll gliding there. For a tab switch, where the whole
    /// row set is replaced: chasing would sweep the viewport through rows that no longer
    /// exist, which reads as a glitch rather than as motion.
    pub(crate) fn jump_to(&mut self, cursor: usize) {
        self.cursor = cursor;
        self.snap = true;
    }

    /// Route a menu event. Up/down move focus (Boundary = recoil), left/right become
    /// [`ListMsg::Adjust`], A becomes [`ListMsg::Activate`]. B is the SCREEN's.
    pub(crate) fn menu(&mut self, ev: MenuEvent, len: usize) -> (ListMsg, Option<MenuPulse>) {
        match ev {
            MenuEvent::Move(MenuDir::Up) => (ListMsg::None, self.step(-1, len)),
            MenuEvent::Move(MenuDir::Down) => (ListMsg::None, self.step(1, len)),
            MenuEvent::Move(MenuDir::Left) => (ListMsg::Adjust(-1), self.armed(-1)),
            MenuEvent::Move(MenuDir::Right) => (ListMsg::Adjust(1), self.armed(1)),
            // A on a value row cycles it FORWARD, so the slip travels the same way a Right
            // would; on an action row nothing steps and the render simply finds no change.
            MenuEvent::Confirm => {
                self.armed(1);
                self.dip();
                (ListMsg::Activate, Some(MenuPulse::Confirm))
            }
            _ => (ListMsg::None, None),
        }
    }

    /// Note that a step went out in `dir`. Returns `None` so it drops into the pulse slot of
    /// the arms above without changing what they report — the SCREEN decides whether the
    /// step landed, and the next render finds out by looking at the value.
    fn armed(&mut self, dir: i32) -> Option<MenuPulse> {
        self.step_dir = dir;
        None
    }

    /// Kick the confirm dip. Separate from [`Self::armed`] because a press is worth showing
    /// even on an action row, where no value will change.
    fn dip(&mut self) {
        self.press.pos = PRESS_DIP;
    }

    /// A row's drawn rect, for tests that assert on what a press can actually reach.
    #[cfg(test)]
    pub(crate) fn row_rect(&self, i: usize) -> Option<Rect> {
        self.geom.get(i).copied().filter(|r| !r.is_empty())
    }

    /// Route a pointer. A press picks the row under it, focuses it AND activates it —
    /// one click does what the pad needs a move plus an A for, which is what a mouse user
    /// expects of a row that IS its control ("click Resolution, resolution changes").
    /// Because activation wraps, every value stays reachable by clicking alone.
    ///
    /// A press in the list's empty margin is swallowed, not passed on: it must not fall
    /// through to whatever the screen draws behind the list.
    pub(crate) fn pointer(&mut self, p: Pointer, len: usize) -> (ListMsg, Option<MenuPulse>) {
        match p.kind {
            PointerKind::Scroll { up } => (ListMsg::None, self.step(if up { -1 } else { 1 }, len)),
            PointerKind::Press => match p.pick(&self.geom) {
                Some(i) if i < len => {
                    self.cursor = i;
                    // Same forward cycle A performs, so a click reads the same as a press.
                    self.armed(1);
                    self.dip();
                    (ListMsg::Activate, Some(MenuPulse::Confirm))
                }
                _ => (ListMsg::None, None),
            },
            _ => (ListMsg::None, None),
        }
    }

    fn step(&mut self, delta: i32, len: usize) -> Option<MenuPulse> {
        let target = self.cursor as i32 + delta;
        if len == 0 || target < 0 || target >= len as i32 {
            // Refused at an end: the dull thud plus a short vertical recoil.
            self.bump = Spring {
                pos: -14.0 * f64::from(delta.signum()),
                vel: 0.0,
            };
            return Some(MenuPulse::Boundary);
        }
        self.cursor = target as usize;
        Some(MenuPulse::Move)
    }

    /// Render the rows into `rect`. `active` = this list owns focus (a keyboard tray on
    /// top parks it — rows keep their look but the focus ring rests).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        rows: &[RowSpec],
        fonts: &Fonts,
        k: f64,
        dt: f64,
        active: bool,
    ) {
        let reduce = crate::theme::reduce_motion();
        if self.snap {
            // A replaced row set has no shared history with the old one — start every row's
            // focus ease from scratch so the new cursor is simply THERE. The slip goes with
            // it: a value "changing" because the whole row set was swapped is not a step.
            self.focus.clear();
            self.focus_pop.clear();
            self.shown.clear();
            self.slip = Spring::rest(0.0);
            self.slip_prev = None;
            self.step_dir = 0;
        }
        self.focus.resize(rows.len(), 0.0);
        self.focus_pop.resize(rows.len(), Spring::rest(0.0));
        for (i, f) in self.focus.iter_mut().enumerate() {
            let target = if active && i == self.cursor { 1.0 } else { 0.0 };
            *f = if self.snap {
                target
            } else {
                approach(*f, target, dt, 0.06)
            };
        }
        for (i, s) in self.focus_pop.iter_mut().enumerate() {
            let target = if active && i == self.cursor { 1.0 } else { 0.0 };
            if self.snap || reduce {
                *s = Spring::rest(target);
            } else {
                s.step_spec(target, springs::FOCUS, dt);
                s.settle(target, 0.0005, 0.005);
            }
        }
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        if reduce {
            // Reduced motion keeps the recoil's MEANING (the move was refused) in the
            // haptic the screen already fired, and drops the travel.
            self.bump = Spring::rest(0.0);
            self.press = Spring::rest(1.0);
        } else {
            self.press.step_spec(1.0, springs::PRESS, dt);
            self.press.settle(1.0, 0.0005, 0.005);
        }

        // Arm the value slip. A step went out (`step_dir`) AND the value under the cursor
        // actually changed — comparing what we DREW last frame against what the screen just
        // handed us is what makes a refused adjust produce no motion at all, without any
        // screen having to report whether its edit landed.
        let dir = std::mem::take(&mut self.step_dir);
        if dir != 0 && !reduce {
            let now = rows
                .get(self.cursor)
                .and_then(|r| r.value.as_deref())
                .unwrap_or_default();
            if self.shown.get(self.cursor).is_some_and(|p| p != now) {
                let prev = self.shown[self.cursor].clone();
                // ADD rather than set, and never touch `vel`: two fast presses accumulate
                // into one accelerating travel instead of restarting the crossfade.
                self.slip.pos =
                    (self.slip.pos + SLIP_DP * f64::from(dir)).clamp(-SLIP_MAX, SLIP_MAX);
                self.slip_prev = Some((self.cursor, prev, -SLIP_DP * f64::from(dir)));
            }
        }
        self.slip.step_spec(0.0, springs::FOCUS, dt);
        self.slip.settle(0.0, 0.02, 0.2);
        if self.slip.pos == 0.0 {
            self.slip_prev = None;
        }
        // This frame's values become the "before" the next step compares against. Recorded
        // for EVERY row, not just the drawn ones: a row scrolled out of view still has a
        // value, and reading a stale one back later would fake a change.
        self.shown.clear();
        self.shown
            .extend(rows.iter().map(|r| r.value.clone().unwrap_or_default()));

        // Row tops (design units) incl. headers, so scroll math and drawing agree.
        let mut tops = Vec::with_capacity(rows.len());
        let mut y = 0.0;
        for row in rows {
            if row.header.is_some() {
                y += HEADER_H;
            }
            tops.push(y);
            y += ROW_H + ROW_GAP;
        }
        let content_h = (y - ROW_GAP).max(0.0) * k;
        let view_h = f64::from(rect.height());

        // The scroll chases the focused row into the middle band, clamped to content.
        let focused_center = tops.get(self.cursor).map_or(0.0, |t| (t + ROW_H / 2.0) * k);
        let target = (focused_center - view_h / 2.0).clamp(0.0, (content_h - view_h).max(0.0));
        self.scroll = if std::mem::take(&mut self.snap) {
            target
        } else {
            approach(self.scroll, target, dt, 0.08)
        };

        let row_w = (ROW_MAX_W * k).min(f64::from(rect.width()) - 48.0 * k);
        let x0 = f64::from(rect.left) + (f64::from(rect.width()) - row_w) / 2.0;

        canvas.save();
        canvas.clip_rect(rect, None, true);
        self.geom.clear();
        self.geom.resize(rows.len(), Rect::new_empty());
        for (i, row) in rows.iter().enumerate() {
            let f = self.focus[i];
            let top = f64::from(rect.top) + tops[i] * k - self.scroll + self.bump.pos * k;
            if top + ROW_H * k < f64::from(rect.top) - 8.0 * k
                || top > f64::from(rect.bottom) + 8.0 * k
            {
                continue;
            }
            if let Some(header) = row.header {
                fonts.draw_tracked(
                    canvas,
                    &header.to_uppercase(),
                    x0 + 16.0 * k,
                    top - 12.0 * k,
                    W::SemiBold,
                    12.0 * k,
                    1.4 * k,
                    fg(0.45),
                );
            }
            // Focus scale springs 0.98 → 1.0 about the row center, times the confirm dip.
            // Two channels rather than one because they answer different questions: the pop
            // says "this is the row", the dip says "and you just pressed it".
            let pop = self.focus_pop.get(i).map_or(f, |s| s.pos);
            let dip = if i == self.cursor {
                self.press.pos
            } else {
                1.0
            };
            let scale = (0.98 + 0.02 * pop) * dip;
            let (cx, cy) = (x0 + row_w / 2.0, top + ROW_H * k / 2.0);
            canvas.save();
            canvas.translate((cx as f32, cy as f32));
            canvas.scale((scale as f32, scale as f32));
            canvas.translate((-cx as f32, -cy as f32));
            let r = Rect::from_xywh(x0 as f32, top as f32, row_w as f32, (ROW_H * k) as f32);
            // The untransformed rect: the focus scale is a 2 % breath about the centre, far
            // inside the slop a finger brings, and clicking must not depend on the ease.
            self.geom[i] = r;
            let stroke = if row.caret {
                PanelStroke::Brand(0.7)
            } else {
                PanelStroke::Plain(0.06 + 0.22 * f as f32)
            };
            let tint = if row.caret {
                Some(accent(0.30))
            } else if f > 0.01 {
                Some(accent(0.30 * f as f32))
            } else {
                None
            };
            crate::theme::panel(canvas, r, 14.0, tint, stroke, k as f32);

            let baseline = cy + 16.0 * k * 0.36;
            if row.value.is_none() {
                // Action row: centered label, brand when actionable.
                let color = if row.enabled { accent(1.0) } else { fg(0.35) };
                let tw = fonts.measure(&row.label, W::SemiBold, 16.0 * k) as f64;
                fonts.draw(
                    canvas,
                    &row.label,
                    cx - tw / 2.0,
                    baseline,
                    W::SemiBold,
                    16.0 * k,
                    color,
                );
            } else {
                fonts.draw(
                    canvas,
                    &row.label,
                    x0 + 16.0 * k,
                    baseline,
                    W::SemiBold,
                    16.0 * k,
                    if row.enabled { fg(1.0) } else { fg(0.55) },
                );
                let value = row.value.as_deref().unwrap_or_default();
                let vcolor = if row.value_dim {
                    fg(0.35)
                } else if f > 0.5 {
                    fg(1.0)
                } else {
                    fg(0.6 + 0.4 * f as f32)
                };
                let chevron_w = if row.adjustable { 18.0 * k } else { 0.0 };
                let caret_w = if row.caret { 8.0 * k } else { 0.0 };
                let vmax = row_w * 0.55;
                let vw = (fonts.measure(value, W::Medium, 15.0 * k) as f64).min(vmax);
                let vx = x0 + row_w - 16.0 * k - chevron_w - caret_w - vw;
                // The sprung slip + crossfade. `slip_prev` names its ROW, so a value that
                // changed somewhere else (a profile row appearing, a dependent row
                // re-enabling) can't drag this one's text sideways.
                let slipping = self.slip_prev.as_ref().filter(|(row, ..)| *row == i);
                let dx = if slipping.is_some() {
                    self.slip.pos * k
                } else {
                    0.0
                };
                let gone = (self.slip.pos.abs() / SLIP_DP).clamp(0.0, 1.0) as f32;
                let alpha =
                    |c: skia_safe::Color4f, a: f32| skia_safe::Color4f::new(c.r, c.g, c.b, c.a * a);
                if let Some((_, prev, offset)) = slipping {
                    // The value being left, sliding out the way the step came from.
                    let prev_text = truncate_head(fonts, prev, W::Medium, 15.0 * k, vmax);
                    fonts.draw(
                        canvas,
                        &prev_text,
                        vx + dx + offset * k,
                        baseline,
                        W::Medium,
                        15.0 * k,
                        alpha(vcolor, gone),
                    );
                }
                // Head-truncate: keep the END of a long address visible while typing.
                let shown = truncate_head(fonts, value, W::Medium, 15.0 * k, vmax);
                fonts.draw(
                    canvas,
                    &shown,
                    vx + dx,
                    baseline,
                    W::Medium,
                    15.0 * k,
                    alpha(vcolor, 1.0 - gone),
                );
                if row.caret {
                    canvas.draw_rect(
                        Rect::from_xywh(
                            (vx + vw + 3.0 * k) as f32,
                            (cy - 9.0 * k) as f32,
                            (2.0 * k) as f32,
                            (18.0 * k) as f32,
                        ),
                        &Paint::new(accent(1.0), None),
                    );
                }
                if row.adjustable && f > 0.01 {
                    let alpha = 0.6 * f as f32;
                    chevron(canvas, vx - 11.0 * k, cy, 4.0 * k, true, alpha);
                    chevron(canvas, x0 + row_w - 16.0 * k, cy, 4.0 * k, false, alpha);
                }
            }
            canvas.restore();
        }
        canvas.restore();
    }
}

// --- Tab strip ---------------------------------------------------------------------------

/// The strip's design height, including the air under it before the first row.
pub(crate) const TAB_STRIP_H: f64 = 46.0;

/// The horizontal section switcher above a menu list. Purely presentational — the SCREEN
/// owns which tab is selected and what the shoulders do; this draws the pills and slides
/// one highlight between them, so switching sections reads as travel rather than a swap.
pub(crate) struct TabStrip {
    /// Chased highlight geometry `(x, width)` in device px, SPRUNG rather than eased so
    /// velocity carries across rapid L1/R1: a fast skim through the sections reads as one
    /// accelerating travel instead of a series of restarted eases that each begin at zero
    /// speed. (The option-band lesson from the Apple gamepad UI, applied to the one widget
    /// here that gets hammered.) `None` until the first render, so a freshly opened screen
    /// doesn't animate its highlight in from x = 0.
    indicator: Option<(Spring, Spring)>,
    /// Each pill's rect as last drawn, device px — the strip is the one part of a settings
    /// screen a pointer can reach directly, so it hit-tests against what it drew.
    pills: Vec<Rect>,
}

impl TabStrip {
    pub(crate) fn new() -> TabStrip {
        TabStrip {
            indicator: None,
            pills: Vec::new(),
        }
    }

    /// A pill's drawn rect, for tests that assert on what a press can actually reach.
    #[cfg(test)]
    pub(crate) fn pill(&self, i: usize) -> Option<Rect> {
        self.pills.get(i).copied()
    }

    /// The tab a press landed on, if any. Pills are small, so the hit box is the full
    /// strip height rather than the drawn pill — a tap that lands just above or below the
    /// text still selects, which on a touchscreen is the difference between working and
    /// not.
    pub(crate) fn pointer(&self, p: Pointer) -> Option<usize> {
        p.press().then(|| p.pick(&self.pills)).flatten()
    }

    /// Draw the pills centered in `rect`'s top band. Returns nothing — the caller already
    /// knows the band is [`TAB_STRIP_H`] tall.
    #[allow(clippy::too_many_arguments)] // the crate's render signature, same as MenuList's
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        labels: &[&str],
        selected: usize,
        fonts: &Fonts,
        k: f64,
        dt: f64,
    ) {
        if labels.is_empty() {
            return;
        }
        let size = 13.0 * k;
        let pad_x = 13.0 * k;
        let gap = 7.0 * k;
        let pill_h = 30.0 * k;
        let widths: Vec<f64> = labels
            .iter()
            .map(|l| f64::from(fonts.measure(l, W::SemiBold, size)) + 2.0 * pad_x)
            .collect();
        let total: f64 = widths.iter().sum::<f64>() + gap * (labels.len() - 1) as f64;
        let mut x = f64::from(rect.left) + (f64::from(rect.width()) - total) / 2.0;
        let top = f64::from(rect.top) + 2.0 * k;

        // Where the highlight wants to be, then the eased position it actually draws at.
        let sel = selected.min(labels.len() - 1);
        let target = (
            x + widths[..sel].iter().sum::<f64>() + gap * sel as f64,
            widths[sel],
        );
        if self.indicator.is_none() {
            self.indicator = Some((Spring::rest(target.0), Spring::rest(target.1)));
        }
        let (ix, iw) = {
            let (sx, sw) = self.indicator.as_mut().expect("seeded just above");
            if crate::theme::reduce_motion() {
                *sx = Spring::rest(target.0);
                *sw = Spring::rest(target.1);
            } else {
                sx.step_spec(target.0, springs::INDICATOR, dt);
                sw.step_spec(target.1, springs::INDICATOR, dt);
                // Epsilons in DEVICE PX (this geometry is already scaled by `k`), so the
                // pill stops sub-pixel-jittering rather than at some design-unit fraction.
                sx.settle(target.0, 0.05, 0.5);
                sw.settle(target.1, 0.05, 0.5);
            }
            (sx.pos, sw.pos)
        };
        crate::theme::panel(
            canvas,
            Rect::from_xywh(ix as f32, top as f32, iw as f32, pill_h as f32),
            (pill_h / 2.0 / k) as f32,
            Some(accent(0.85)),
            PanelStroke::Plain(0.22),
            k as f32,
        );

        let baseline = top + pill_h / 2.0 + size * 0.36;
        self.pills.clear();
        for (i, label) in labels.iter().enumerate() {
            // Fade each label toward white by how much the highlight actually covers it, so
            // the two labels a sliding highlight passes between light up together.
            let pill_x = x;
            // Full-height hit box (see `TabStrip::pointer`), and only ever grown from the
            // pill's own span so two neighbours can't both claim a press.
            self.pills.push(Rect::from_xywh(
                pill_x as f32,
                rect.top,
                widths[i] as f32,
                rect.height().max((pill_h + 4.0 * k) as f32),
            ));
            let overlap = (pill_x + widths[i]).min(ix + iw) - pill_x.max(ix);
            let covered = (overlap / widths[i]).clamp(0.0, 1.0) as f32;
            let tw = f64::from(fonts.measure(label, W::SemiBold, size));
            fonts.draw(
                canvas,
                label,
                pill_x + (widths[i] - tw) / 2.0,
                baseline,
                W::SemiBold,
                size,
                fg(0.5 + 0.5 * covered),
            );
            x += widths[i] + gap;
        }
    }
}

/// Middle-of-nowhere helper: drop chars from the FRONT until the tail fits.
fn truncate_head(fonts: &Fonts, text: &str, w: W, size: f64, max_w: f64) -> String {
    if f64::from(fonts.measure(text, w, size)) <= max_w {
        return text.to_string();
    }
    let mut s: Vec<char> = text.chars().collect();
    while s.len() > 1 {
        s.remove(0);
        let candidate: String = std::iter::once('…').chain(s.iter().copied()).collect();
        if f64::from(fonts.measure(&candidate, w, size)) <= max_w {
            return candidate;
        }
    }
    "…".into()
}

fn chevron(canvas: &Canvas, x: f64, cy: f64, r: f64, left: bool, alpha: f32) {
    let dir = if left { -1.0 } else { 1.0 };
    let mut p = Paint::new(fg(alpha), None);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width((1.8 * r / 4.0) as f32);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_anti_alias(true);
    let mut path = PathBuilder::new();
    path.move_to(((x - dir * r / 2.0) as f32, (cy - r) as f32));
    path.line_to(((x + dir * r / 2.0) as f32, cy as f32));
    path.line_to(((x - dir * r / 2.0) as f32, (cy + r) as f32));
    canvas.draw_path(&path.detach(), &p);
}

// --- On-screen keyboard ----------------------------------------------------------------------

/// What the keyboard may type per field (backspace always works).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Charset {
    Free,
    /// Addresses/hostnames — everything but whitespace.
    Hostname,
    Digits,
}

pub(crate) fn permits(charset: Charset, ch: char) -> bool {
    match charset {
        Charset::Free => true,
        Charset::Hostname => !ch.is_whitespace(),
        Charset::Digits => ch.is_ascii_digit(),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyMsg {
    None,
    Type(char),
    Backspace,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Key {
    Char(char),
    Space,
    Backspace,
    Done,
}

/// Digits first (addresses/ports), then letters; the last char column carries the
/// hostname/address punctuation — the Swift grid, verbatim.
fn key_rows() -> &'static [Vec<Key>] {
    use std::sync::OnceLock;
    static ROWS: OnceLock<Vec<Vec<Key>>> = OnceLock::new();
    ROWS.get_or_init(|| {
        let chars = |s: &str| s.chars().map(Key::Char).collect::<Vec<_>>();
        vec![
            chars("1234567890"),
            chars("qwertyuiop"),
            chars("asdfghjkl-"),
            chars("zxcvbnm._:"),
            vec![Key::Space, Key::Backspace, Key::Done],
        ]
    })
}

/// The controller keyboard: a fixed key grid in a bottom tray. Dpad/stick moves the key
/// cursor, A types, X backspaces, B/Y/Done confirms. Edits apply live — closing IS done.
pub(crate) struct Keyboard {
    row: usize,
    col: usize,
    /// Tray slide-in (0 hidden → 1 seated), the Swift `.spring(0.32, 0.86)`.
    tray: Spring,
    key_flash: f64,
    /// Each key's rect and identity as last drawn — the tray slides, so hit-testing has to
    /// read the drawn geometry rather than recompute a seated layout.
    keys: Vec<(Rect, Key)>,
}

impl Keyboard {
    pub(crate) fn new() -> Keyboard {
        Keyboard {
            row: 1, // opens on "q"
            col: 0,
            tray: Spring::rest(0.0),
            key_flash: 0.0,
            keys: Vec::new(),
        }
    }

    /// Route a pointer at the tray. A press types the key under it and moves the key
    /// cursor there, so a pad can carry on from wherever a finger left off. A press that
    /// lands on the tray but between keys is swallowed — the tray is modal, and a stray
    /// tap must not reach the list behind it.
    pub(crate) fn pointer(&mut self, p: Pointer) -> (KeyMsg, Option<MenuPulse>) {
        if !p.press() {
            return (KeyMsg::None, None);
        }
        let Some(i) = p.pick(&self.keys.iter().map(|(r, _)| *r).collect::<Vec<_>>()) else {
            return (KeyMsg::None, None);
        };
        let key = self.keys[i].1;
        // Re-seat the cursor from the key's identity, not the draw index: `key_rows` is the
        // one layout authority and the two must not be able to drift apart.
        if let Some((r, c)) = key_rows()
            .iter()
            .enumerate()
            .find_map(|(r, row)| row.iter().position(|k| *k == key).map(|c| (r, c)))
        {
            self.row = r;
            self.col = c;
        }
        self.key_flash = 1.0;
        match key {
            Key::Char(c) => (KeyMsg::Type(c), None),
            Key::Space => (KeyMsg::Type(' '), None),
            Key::Backspace => (KeyMsg::Backspace, None),
            Key::Done => (KeyMsg::Done, Some(MenuPulse::Confirm)),
        }
    }

    /// Does `p` land on the tray at all? The screen asks before routing, so a press
    /// outside a raised keyboard can dismiss it instead of falling through to the list.
    pub(crate) fn covers(&self, p: Pointer) -> bool {
        self.keys.iter().any(|(r, _)| p.hits(*r))
    }

    /// Route a menu event; the SCREEN applies `Type`/`Backspace` to its field (charset
    /// checks included — a refusal comes back as a boundary pulse from the screen).
    pub(crate) fn menu(&mut self, ev: MenuEvent) -> (KeyMsg, Option<MenuPulse>) {
        let rows = key_rows();
        match ev {
            MenuEvent::Move(dir) => {
                let (mut row, mut col) = (self.row as i32, self.col as i32);
                match dir {
                    MenuDir::Left => col -= 1,
                    MenuDir::Right => col += 1,
                    MenuDir::Up | MenuDir::Down => {
                        let next = row + if dir == MenuDir::Down { 1 } else { -1 };
                        if next < 0 || next >= rows.len() as i32 {
                            return (KeyMsg::None, Some(MenuPulse::Boundary));
                        }
                        // Map the column proportionally between rows of different
                        // widths (Done goes up to the rightmost letters, not "e").
                        let from = (rows[row as usize].len() - 1).max(1) as f64;
                        let to = (rows[next as usize].len() - 1) as f64;
                        col = (col as f64 * to / from).round() as i32;
                        row = next;
                    }
                }
                if row < 0
                    || row >= rows.len() as i32
                    || col < 0
                    || col >= rows[row as usize].len() as i32
                {
                    return (KeyMsg::None, Some(MenuPulse::Boundary));
                }
                self.row = row as usize;
                self.col = col as usize;
                (KeyMsg::None, Some(MenuPulse::Move))
            }
            MenuEvent::Confirm => {
                self.key_flash = 1.0;
                match rows[self.row][self.col] {
                    Key::Char(c) => (KeyMsg::Type(c), None),
                    Key::Space => (KeyMsg::Type(' '), None),
                    Key::Backspace => (KeyMsg::Backspace, None),
                    Key::Done => (KeyMsg::Done, Some(MenuPulse::Confirm)),
                }
            }
            MenuEvent::Tertiary => (KeyMsg::Backspace, None),
            MenuEvent::Secondary | MenuEvent::Back => (KeyMsg::Done, Some(MenuPulse::Confirm)),
            _ => (KeyMsg::None, None),
        }
    }

    /// Advance the tray spring toward shown/hidden. Returns the current seat 0..1 —
    /// at exactly 0 while hidden, so the caller can skip rendering.
    pub(crate) fn seat(&mut self, shown: bool, dt: f64) -> f64 {
        self.tray
            .step(if shown { 1.0 } else { 0.0 }, TRAY_K, TRAY_C, dt);
        self.tray.settle(if shown { 1.0 } else { 0.0 }, 0.001, 0.01);
        self.key_flash = approach(self.key_flash, 0.0, dt, 0.10);
        self.tray.pos.clamp(0.0, 1.2)
    }

    /// The tray's design height (pre-`k`), for layout above it.
    pub(crate) fn tray_height() -> f64 {
        5.0 * 42.0 + 4.0 * 7.0 + 2.0 * 14.0
    }

    /// Render the tray with its bottom edge at `bottom`, horizontally centered, slid by
    /// `seat` (0..1). The caller clips nothing — the tray rises from below the screen.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        fonts: &Fonts,
        w: f64,
        bottom: f64,
        seat: f64,
        k: f64,
    ) {
        let rows = key_rows();
        self.keys.clear();
        let tray_w = (560.0 * k).min(w - 32.0 * k);
        let tray_h = Self::tray_height() * k;
        let x0 = (w - tray_w) / 2.0;
        let y0 = bottom - tray_h * seat;
        let rect = Rect::from_xywh(x0 as f32, y0 as f32, tray_w as f32, tray_h as f32);
        crate::theme::panel(
            canvas,
            rect,
            22.0,
            Some(skia_safe::Color4f::new(0.05, 0.045, 0.09, 0.55)),
            PanelStroke::Plain(0.12),
            k as f32,
        );

        let pad = 14.0 * k;
        let gap = 7.0 * k;
        let key_h = 42.0 * k;
        for (r, row) in rows.iter().enumerate() {
            let n = row.len() as f64;
            let key_w = (tray_w - 2.0 * pad - (n - 1.0) * gap) / n;
            let y = y0 + pad + r as f64 * (key_h + gap);
            for (c, key) in row.iter().enumerate() {
                let x = x0 + pad + c as f64 * (key_w + gap);
                let focused = r == self.row && c == self.col;
                let kr = Rect::from_xywh(x as f32, y as f32, key_w as f32, key_h as f32);
                self.keys.push((kr, *key));
                let fill = if focused {
                    let mut b = accent(1.0);
                    if self.key_flash > 0.02 {
                        // A just-typed key flashes brighter, then eases back.
                        let f = self.key_flash as f32;
                        b = skia_safe::Color4f::new(
                            b.r + (1.0 - b.r) * 0.5 * f,
                            b.g + (1.0 - b.g) * 0.5 * f,
                            b.b,
                            1.0,
                        );
                    }
                    b
                } else {
                    fg(0.08)
                };
                canvas.draw_rrect(
                    RRect::new_rect_xy(kr, (9.0 * k) as f32, (9.0 * k) as f32),
                    &Paint::new(fill, None),
                );
                // The focused key is filled with the accent, so its letter needs ink that
                // reads on THAT, not on the field.
                let ink = if focused {
                    crate::theme::on_accent()
                } else {
                    fg(1.0)
                };
                let (cx, cy) = (x + key_w / 2.0, y + key_h / 2.0);
                match key {
                    Key::Char(ch) => {
                        let s = ch.to_string();
                        let size = 18.0 * k;
                        let tw = fonts.measure(&s, W::Medium, size) as f64;
                        fonts.draw(
                            canvas,
                            &s,
                            cx - tw / 2.0,
                            cy + size * 0.36,
                            W::Medium,
                            size,
                            ink,
                        );
                    }
                    Key::Space => draw_space_icon(canvas, cx, cy, k, ink),
                    Key::Backspace => draw_backspace_icon(canvas, cx, cy, k, ink),
                    Key::Done => {
                        let size = 15.0 * k;
                        let label = "Done";
                        let tw = fonts.measure(label, W::SemiBold, size) as f64;
                        let check_w = 14.0 * k;
                        let total = check_w + 6.0 * k + tw;
                        draw_check(canvas, cx - total / 2.0 + check_w / 2.0, cy, k, ink);
                        fonts.draw(
                            canvas,
                            label,
                            cx - total / 2.0 + check_w + 6.0 * k,
                            cy + size * 0.36,
                            W::SemiBold,
                            size,
                            ink,
                        );
                    }
                }
            }
        }
    }
}

fn stroke_paint(ink: skia_safe::Color4f, width: f32) -> Paint {
    let mut p = Paint::new(ink, None);
    p.set_style(skia_safe::PaintStyle::Stroke);
    p.set_stroke_width(width);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    p.set_stroke_join(skia_safe::PaintJoin::Round);
    p.set_anti_alias(true);
    p
}

fn draw_space_icon(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    // ⎵ — an underline bracket.
    let (w, h) = (16.0 * k, 5.0 * k);
    let p = stroke_paint(ink, (1.6 * k) as f32);
    let mut path = PathBuilder::new();
    path.move_to(((cx - w / 2.0) as f32, (cy - h / 2.0) as f32));
    path.line_to(((cx - w / 2.0) as f32, (cy + h / 2.0) as f32));
    path.line_to(((cx + w / 2.0) as f32, (cy + h / 2.0) as f32));
    path.line_to(((cx + w / 2.0) as f32, (cy - h / 2.0) as f32));
    canvas.draw_path(&path.detach(), &p);
}

fn draw_backspace_icon(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    // ⌫ — the left-pointing cap with an ✕ inside.
    let (w, h) = (18.0 * k, 12.0 * k);
    let nose = 6.0 * k;
    let p = stroke_paint(ink, (1.6 * k) as f32);
    let (l, r, t, b) = (cx - w / 2.0, cx + w / 2.0, cy - h / 2.0, cy + h / 2.0);
    let mut path = PathBuilder::new();
    path.move_to(((l + nose) as f32, t as f32));
    path.line_to((r as f32, t as f32));
    path.line_to((r as f32, b as f32));
    path.line_to(((l + nose) as f32, b as f32));
    path.line_to((l as f32, cy as f32));
    path.close();
    canvas.draw_path(&path.detach(), &p);
    let (xc, xr) = (cx + nose / 2.0, 2.6 * k);
    canvas.draw_line(
        ((xc - xr) as f32, (cy - xr) as f32),
        ((xc + xr) as f32, (cy + xr) as f32),
        &p,
    );
    canvas.draw_line(
        ((xc - xr) as f32, (cy + xr) as f32),
        ((xc + xr) as f32, (cy - xr) as f32),
        &p,
    );
}

fn draw_check(canvas: &Canvas, cx: f64, cy: f64, k: f64, ink: skia_safe::Color4f) {
    let p = stroke_paint(ink, (1.8 * k) as f32);
    let r = 5.0 * k;
    let mut path = PathBuilder::new();
    path.move_to(((cx - r) as f32, cy as f32));
    path.line_to(((cx - r * 0.25) as f32, (cy + r * 0.7) as f32));
    path.line_to(((cx + r) as f32, (cy - r * 0.7) as f32));
    canvas.draw_path(&path.detach(), &p);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kb() -> Keyboard {
        Keyboard::new()
    }

    #[test]
    fn keyboard_opens_on_q_and_types() {
        let mut k = kb();
        let (msg, _) = k.menu(MenuEvent::Confirm);
        assert_eq!(msg, KeyMsg::Type('q'));
    }

    #[test]
    fn keyboard_proportional_column_mapping() {
        let mut k = kb();
        // Walk to the digits row's far right ("0", col 9), then down to the bottom row:
        // col must land on Done (rightmost of 3), not clamp to some middle key.
        for _ in 0..9 {
            k.menu(MenuEvent::Move(MenuDir::Right));
        }
        k.menu(MenuEvent::Move(MenuDir::Up)); // digits row
        assert_eq!((k.row, k.col), (0, 9));
        for _ in 0..4 {
            k.menu(MenuEvent::Move(MenuDir::Down));
        }
        assert_eq!(k.row, 4);
        assert_eq!(k.col, 2, "rightmost column maps onto Done");
        let (msg, _) = k.menu(MenuEvent::Confirm);
        assert_eq!(msg, KeyMsg::Done);
    }

    #[test]
    fn keyboard_edges_refuse() {
        let mut k = kb();
        let (_, pulse) = k.menu(MenuEvent::Move(MenuDir::Left));
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        // X backspaces, B/Y are done — from anywhere.
        assert_eq!(k.menu(MenuEvent::Tertiary).0, KeyMsg::Backspace);
        assert_eq!(k.menu(MenuEvent::Secondary).0, KeyMsg::Done);
        assert_eq!(k.menu(MenuEvent::Back).0, KeyMsg::Done);
    }

    #[test]
    fn charsets() {
        assert!(permits(Charset::Digits, '7'));
        assert!(!permits(Charset::Digits, 'a'));
        assert!(permits(Charset::Hostname, '-'));
        assert!(!permits(Charset::Hostname, ' '));
        assert!(permits(Charset::Free, ' '));
    }

    #[test]
    fn menu_list_moves_and_recoils() {
        let mut l = MenuList::new();
        assert!(matches!(
            l.menu(MenuEvent::Move(MenuDir::Down), 3).1,
            Some(MenuPulse::Move)
        ));
        assert_eq!(l.cursor, 1);
        assert!(matches!(
            l.menu(MenuEvent::Move(MenuDir::Up), 3).1,
            Some(MenuPulse::Move)
        ));
        assert!(matches!(
            l.menu(MenuEvent::Move(MenuDir::Up), 3).1,
            Some(MenuPulse::Boundary)
        ));
        assert!(l.bump.pos.abs() > 1.0, "recoil engaged");
        assert_eq!(
            l.menu(MenuEvent::Move(MenuDir::Right), 3).0,
            ListMsg::Adjust(1)
        );
        assert_eq!(l.menu(MenuEvent::Confirm, 3).0, ListMsg::Activate);
    }

    const TABS: [&str; 7] = [
        "Stream",
        "Video",
        "Audio",
        "Controller",
        "Input",
        "Interface",
        "Profiles",
    ];

    /// The sprung tab pill must accelerate through a burst without ever leaving the strip.
    /// A spring that carries velocity CAN fly past its target, and the failure mode is a
    /// highlight that shoots off the end of the section list — this pins that it doesn't,
    /// and that it still lands exactly on the selected pill afterwards.
    #[test]
    fn tab_indicator_rides_a_burst_without_leaving_the_strip() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 120)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, TAB_STRIP_H as f32);
        let mut strip = TabStrip::new();
        let dt = 1.0 / 60.0;
        // Seat on the first tab, then a 5-step burst at one press per frame — far faster
        // than the spring can settle, which is the whole point.
        strip.render(surface.canvas(), rect, &TABS, 0, &fonts, 1.0, dt);
        let mut worst_left = f64::MAX;
        let mut worst_right = f64::MIN;
        for sel in 1..=5 {
            strip.render(surface.canvas(), rect, &TABS, sel, &fonts, 1.0, dt);
            let (ix, iw) = strip.indicator.map(|(x, w)| (x.pos, w.pos)).unwrap();
            worst_left = worst_left.min(ix);
            worst_right = worst_right.max(ix + iw);
        }
        // Then let it land.
        for _ in 0..240 {
            strip.render(surface.canvas(), rect, &TABS, 5, &fonts, 1.0, dt);
            let (ix, iw) = strip.indicator.map(|(x, w)| (x.pos, w.pos)).unwrap();
            worst_left = worst_left.min(ix);
            worst_right = worst_right.max(ix + iw);
        }
        assert!(
            worst_left >= f64::from(rect.left) - 0.5,
            "pill ran off the left: {worst_left}"
        );
        assert!(
            worst_right <= f64::from(rect.right) + 0.5,
            "pill ran off the right: {worst_right}"
        );
        let pill = strip.pill(5).expect("the selected pill was drawn");
        let (ix, iw) = strip.indicator.map(|(x, w)| (x.pos, w.pos)).unwrap();
        assert!(
            (ix - f64::from(pill.left)).abs() < 0.5 && (iw - f64::from(pill.width())).abs() < 0.5,
            "settled at ({ix}, {iw}), pill is at ({}, {})",
            pill.left,
            pill.width()
        );
    }

    fn value_row(value: &str) -> Vec<RowSpec> {
        vec![RowSpec {
            header: None,
            label: "Bitrate".into(),
            value: Some(value.into()),
            value_dim: false,
            caret: false,
            adjustable: true,
            enabled: true,
        }]
    }

    /// The value slip: armed only when a step actually CHANGED the value, and always
    /// settling back onto the row's own position. A slip that never returns to identity
    /// leaves the value permanently offset, which is the bug this shape can have.
    #[test]
    fn value_slip_arms_on_a_real_step_and_settles_to_identity() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 600)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, 600.0);
        let dt = 1.0 / 60.0;
        let mut list = MenuList::new();
        let mut frame = |list: &mut MenuList, rows: &[RowSpec]| {
            list.render(surface.canvas(), rect, rows, &fonts, 1.0, dt, true);
        };

        frame(&mut list, &value_row("10 Mbps"));
        assert_eq!(list.slip.pos, 0.0, "nothing has stepped yet");

        // A step the screen HONOURED: the value it hands back next frame differs.
        assert_eq!(
            list.menu(MenuEvent::Move(MenuDir::Right), 1).0,
            ListMsg::Adjust(1)
        );
        frame(&mut list, &value_row("20 Mbps"));
        assert!(list.slip.pos.abs() > 1.0, "armed: {}", list.slip.pos);
        assert!(
            list.slip_prev.is_some(),
            "the old value is held for the fade"
        );

        for _ in 0..240 {
            frame(&mut list, &value_row("20 Mbps"));
        }
        assert_eq!(list.slip.pos, 0.0, "settled back onto the row");
        assert!(list.slip_prev.is_none(), "and forgot the old value");

        // A step the screen REFUSED (value unchanged) must not move anything — this is why
        // the list detects the change itself instead of trusting the event.
        list.menu(MenuEvent::Move(MenuDir::Right), 1);
        frame(&mut list, &value_row("20 Mbps"));
        assert_eq!(list.slip.pos, 0.0);
        assert!(list.slip_prev.is_none());
    }

    /// Reduced motion keeps the STATE and drops the journey: the value is simply the new
    /// one, the focus is simply on the row, and a refused move leaves no recoil behind.
    #[test]
    fn reduce_motion_drops_travel_but_not_state() {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 600)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, 600.0);
        let dt = 1.0 / 60.0;
        crate::theme::set_reduce_motion(true);
        let mut list = MenuList::new();
        list.render(
            surface.canvas(),
            rect,
            &value_row("10 Mbps"),
            &fonts,
            1.0,
            dt,
            true,
        );
        list.menu(MenuEvent::Move(MenuDir::Right), 1);
        list.render(
            surface.canvas(),
            rect,
            &value_row("20 Mbps"),
            &fonts,
            1.0,
            dt,
            true,
        );
        assert_eq!(list.slip.pos, 0.0, "no slip under reduced motion");
        assert!(list.slip_prev.is_none());
        // A refused move still pulses (the screen's job) but leaves no recoil travel.
        assert!(matches!(
            list.menu(MenuEvent::Move(MenuDir::Up), 1).1,
            Some(MenuPulse::Boundary)
        ));
        list.render(
            surface.canvas(),
            rect,
            &value_row("20 Mbps"),
            &fonts,
            1.0,
            dt,
            true,
        );
        assert_eq!(list.bump.pos, 0.0, "recoil travel suppressed");
        // The focus channel still ARRIVES — reduced motion is not "unfocused".
        assert_eq!(list.focus_pop[0].pos, 1.0);
        crate::theme::set_reduce_motion(false);
    }

    #[test]
    fn head_truncation_keeps_the_tail() {
        let fonts = crate::theme::build_fonts().unwrap();
        let t = truncate_head(&fonts, "verylonghostname.local", W::Medium, 15.0, 60.0);
        assert!(t.starts_with('…'));
        assert!(t.ends_with("local"));
    }
}
