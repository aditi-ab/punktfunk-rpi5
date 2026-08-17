//! The console library's model and math — everything about the coverflow that isn't
//! Skia: the shared binary↔overlay state (games, phase, incoming art bytes), the
//! spring-driven motion and cursor arithmetic (ported verbatim from the GTK launcher,
//! tests included), and the geometry constants. Rendering lives in `skia_overlay`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// --- Geometry (the GTK launcher's constants — Apple coverflow parity) --------------------

/// Poster geometry: 2:3 covers, sized so the focused poster + detail panel + hint bar
/// fit a Deck's 1280×800 with air. Scaled uniformly for other window sizes.
pub const POSTER_W: f64 = 220.0;
pub const POSTER_H: f64 = 330.0;
/// Center of the focused card to the center of its first neighbor.
pub const FOCUS_GAP: f64 = 230.0;
/// Center-to-center distance between successive SIDE cards — much tighter than their
/// projected width, so the side stacks overlap like the classic coverflow shelf.
pub const SIDE_SPACING: f64 = 104.0;
/// Cards farther than this from the eased position aren't drawn at all.
pub const VISIBLE_RANGE: f64 = 5.5;
/// Neighbors recede to this scale…
pub const RECEDE_SCALE: f64 = 0.24;
/// …and swing this many degrees about their own vertical axis under perspective, side
/// cards facing the corridor (their inner edge recedes behind the focus).
pub const ROTATE_DEG: f64 = 38.0;
/// Perspective depth for the tilt, px (CSS `perspective()` semantics).
pub const PERSPECTIVE: f64 = 800.0;
/// The recede veil's max opacity (side cards stay opaque — they overlap). Cut twice: first
/// when the colour recede landed and this stopped having to carry the whole "further away"
/// reading on its own, and again when the three stacked mechanisms turned out to be summing
/// to literal black on a receded card. It is left doing the one job a flat wash is good at —
/// separating cards that overlap. See `theme::recede_matrix`.
///
/// No longer necessarily a DARKENING: the call site washes toward `theme::shade`, which is
/// black on a dark palette and white on a pale one, so this reinforces the matrix's direction
/// at both poles instead of greying out the lift on the six pale ones.
pub const RECEDE_DIM: f64 = 0.10;
/// Boundary recoil: a refused move deflects the strip this many px against the push.
pub const BUMP_PX: f64 = 16.0;
/// Mount entrance (see [`crate::anim::Entrance`]): a card arrives at this scale, this many
/// design units below its berth, and — where the surface can turn a card at all — this many
/// degrees away from the viewer. Shared by the home carousel and the library coverflow so
/// the two read as one console arriving, not two widgets each with its own idea.
pub const ENTER_SCALE: f64 = 0.74;
pub const ENTER_RISE: f64 = 34.0;
pub const ENTER_TURN_DEG: f64 = 62.0;
/// L1/R1 jump distance.
pub const JUMP: i32 = 5;

// The motion is spring-driven (semi-implicit Euler), not eased — velocity carries across
// retargets, so holding a direction glides and a release settles like a detent.
/// Cursor chase: ζ ≈ 0.85 — settles in ~0.3 s with a whisker of overshoot.
pub const SPRING_K: f64 = 200.0;
pub const SPRING_C: f64 = 24.0;
/// Boundary recoil: stiffer and more underdamped (ζ ≈ 0.55) — one visible wobble.
pub const BUMP_K: f64 = 600.0;
pub const BUMP_C: f64 = 27.0;

/// One semi-implicit-Euler step of a damped spring toward `target`.
fn spring_step(pos: f64, vel: f64, target: f64, k: f64, c: f64, dt: f64) -> (f64, f64) {
    let vel = vel + (k * (target - pos) - c * vel) * dt;
    (pos + vel * dt, vel)
}

/// Advance a damped spring by a whole frame, integrating in ≤ 8 ms substeps — a stalled
/// frame stays far inside the integrator's stability bound, so the motion feels
/// identical at any frame rate.
pub fn spring_advance(
    mut pos: f64,
    mut vel: f64,
    target: f64,
    k: f64,
    c: f64,
    dt: f64,
) -> (f64, f64) {
    let n = (dt / 0.008).ceil().max(1.0) as usize;
    let h = dt / n as f64;
    for _ in 0..n {
        (pos, vel) = spring_step(pos, vel, target, k, c, h);
    }
    (pos, vel)
}

/// Pure cursor arithmetic for a move/jump: `clamp` lands jumps on the ends, a plain
/// step refuses to leave them.
#[derive(Debug, PartialEq, Eq)]
pub enum StepResult {
    Moved(i32),
    Boundary,
}

pub fn step_cursor(cursor: i32, len: usize, delta: i32, clamp: bool) -> StepResult {
    if len == 0 {
        return StepResult::Boundary;
    }
    let max = len as i32 - 1;
    let target = if clamp {
        (cursor + delta).clamp(0, max)
    } else {
        cursor + delta
    };
    if target == cursor || target < 0 || target > max {
        StepResult::Boundary
    } else {
        StepResult::Moved(target)
    }
}

/// Which arrangement the library draws in.
///
/// The shelf is a browsing surface — one title at a time, big, with its artwork doing the
/// talking. The grid is a FINDING surface: ~18 covers at once instead of the coverflow's
/// legible three, for the moment you know what you want and just need to see it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LibraryView {
    #[default]
    Shelf,
    Grid,
}

impl LibraryView {
    /// Parse the persisted `library_view` value, leniently — an unknown string is a newer
    /// client's, and the right answer to one is the shelf everyone already has.
    pub fn parse(s: &str) -> LibraryView {
        match s {
            "grid" => LibraryView::Grid,
            _ => LibraryView::Shelf,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            LibraryView::Shelf => "shelf",
            LibraryView::Grid => "grid",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LibraryView::Shelf => "Shelf",
            LibraryView::Grid => "Grid",
        }
    }

    pub const ALL: [LibraryView; 2] = [LibraryView::Shelf, LibraryView::Grid];
}

/// Grid cell geometry: the same 2:3 as the coverflow poster at roughly two-thirds the size,
/// which is what puts three rows on a Deck's 800-tall panel with the detail band still
/// readable underneath.
pub const GRID_W: f64 = 150.0;
pub const GRID_H: f64 = 225.0;
pub const GRID_GAP: f64 = 16.0;

/// Which way a grid cursor is being pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridDir {
    Left,
    Right,
    Up,
    Down,
    PageBack,
    PageForward,
}

/// How many rows a shoulder press jumps.
pub const GRID_PAGE_ROWS: i32 = 3;

/// The grid's layout, in the one place both the cursor arithmetic and the renderer read it.
///
/// A field of covers is not a uniform grid: the launcher prefix (design D4) is given rows of
/// its own and the games section restarts at column 0 underneath it. While navigation did
/// that sum for itself — index modulo `cols` — the two models agreed only when the launcher
/// count happened to be a multiple of the column count, which with a Deck's seven columns and
/// the usual two launchers means never. Down out of a launcher landed five columns to the
/// right of the tile it sat under, Up out of the games band slid sideways instead of leaving
/// it, and a row end refused mid-row. Sharing the shape is the fix; the arithmetic below is
/// only its consequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridShape {
    /// Cells per row. A RENDER fact — it depends on the window — so the shape is built from
    /// what the last frame actually drew rather than derived twice from two widths.
    pub cols: usize,
    /// How many cells there are: the FILTERED count, the one the cursor indexes.
    pub len: usize,
    /// Where the games section starts, or 0 when the field is one continuous run.
    pub split: usize,
}

impl GridShape {
    /// `launchers` is the leading launcher run. The section only exists when BOTH halves do —
    /// an all-launcher or launcher-less field is a plain grid, and giving it a heading band
    /// and a gap it has no second group for would be a rule showing off.
    pub fn new(len: usize, cols: usize, launchers: usize) -> GridShape {
        let split = if launchers > 0 && launchers < len {
            launchers
        } else {
            0
        };
        GridShape { cols, len, split }
    }

    /// The first row of the games section (meaningless when there is no split).
    pub fn split_row(&self) -> usize {
        self.split.div_ceil(self.cols.max(1))
    }

    /// Which cell an index is drawn in.
    pub fn cell_of(&self, i: usize) -> (usize, usize) {
        let cols = self.cols.max(1);
        if self.split > 0 && i >= self.split {
            let j = i - self.split;
            (self.split_row() + j / cols, j % cols)
        } else {
            (i / cols, i % cols)
        }
    }

    pub fn rows(&self) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 {
            self.split_row() + (self.len - self.split).div_ceil(cols)
        } else {
            self.len.div_ceil(cols)
        }
    }

    /// The index of a row's first cell.
    pub fn row_start(&self, row: usize) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 && row >= self.split_row() {
            self.split + (row - self.split_row()) * cols
        } else {
            row * cols
        }
    }

    /// How many cells a row actually holds — the launcher section's last row stops where the
    /// games section begins, and the field's last row stops at the end of the library.
    pub fn row_len(&self, row: usize) -> usize {
        let start = self.row_start(row);
        let end = if self.split > 0 && row + 1 == self.split_row() {
            self.split
        } else {
            self.len
        };
        end.saturating_sub(start).min(self.cols.max(1))
    }
}

/// Cursor arithmetic for the grid, against the shape the renderer is drawing.
///
/// ONE rule, because an accretion of special cases is how this broke: horizontal moves walk
/// the row and refuse at THAT ROW's true ends; vertical moves and pages change row only,
/// carrying `col_hint` and clamping it into the target row's length. The only boundary is a
/// move that would leave the grid.
///
/// Left/right refusing rather than wrapping is the shelf's rule and the one a thumb already
/// knows — a held Right that wrapped would scan the whole library, and there are shoulders
/// for that. Vertical moves clamp instead of refusing because a short row is a layout
/// accident, not a boundary anyone chose to hit: Down from above the gap should land on the
/// last title, not thud.
///
/// `col_hint` is the column the user last CHOSE (see [`grid_col_hint`]) rather than the one
/// they happen to be standing in, so crossing a two-wide launcher row and coming back returns
/// to the column the crossing started from.
pub fn grid_step(cursor: i32, shape: GridShape, col_hint: usize, dir: GridDir) -> StepResult {
    if shape.len == 0 || shape.cols == 0 {
        return StepResult::Boundary;
    }
    // A cursor outside the field is a stale one (the library shortened under us); reading it
    // as the nearest real cell makes the next press heal it instead of compounding it.
    let (row, col) = shape.cell_of((cursor.max(0) as usize).min(shape.len - 1));
    let moved = |i: usize| {
        if i as i32 == cursor {
            StepResult::Boundary
        } else {
            StepResult::Moved(i as i32)
        }
    };
    match dir {
        GridDir::Left => {
            if col == 0 {
                StepResult::Boundary
            } else {
                moved(shape.row_start(row) + col - 1)
            }
        }
        GridDir::Right => {
            if col + 1 >= shape.row_len(row) {
                StepResult::Boundary
            } else {
                moved(shape.row_start(row) + col + 1)
            }
        }
        GridDir::Up | GridDir::Down | GridDir::PageBack | GridDir::PageForward => {
            let (d, paging) = match dir {
                GridDir::Up => (-1, false),
                GridDir::Down => (1, false),
                GridDir::PageBack => (-GRID_PAGE_ROWS, true),
                _ => (GRID_PAGE_ROWS, true),
            };
            let target = (row as i32 + d).clamp(0, shape.rows() as i32 - 1) as usize;
            if target == row {
                // A STEP at the edge refuses. A PAGE is a "take me there", so it lands on the
                // end of the row it is already on — the same reading `step_cursor`'s clamped
                // mode has, and the one the shoulders have always had here.
                if !paging {
                    return StepResult::Boundary;
                }
                let c = if d > 0 { shape.row_len(row) - 1 } else { 0 };
                return moved(shape.row_start(row) + c);
            }
            let c = col_hint.min(shape.row_len(target) - 1);
            moved(shape.row_start(target) + c)
        }
    }
}

/// The remembered column after a move.
///
/// A horizontal step CHOOSES a column; a vertical one only borrows it. Keeping the rule here
/// rather than at the call site is what stops the screen from holding a cursor and a column
/// that disagree — the same reason the layout itself is one shared shape.
pub fn grid_col_hint(shape: GridShape, prev: usize, dir: GridDir, landed: i32) -> usize {
    match dir {
        GridDir::Left | GridDir::Right => shape.cell_of(landed.max(0) as usize).1,
        _ => prev,
    }
}

// --- 4×4 matrix (row-major) — the coverflow card transform ------------------------------

/// `T(cx,cy) · P(depth) · Ry(angle) · S(s) · T(-w/2,-h/2)`: card-local (0..w, 0..h) →
/// screen, rotated about the card's own vertical center axis under perspective — the
/// GSK transform chain from the GTK launcher, as one row-major matrix for
/// `Canvas::concat_44`.
#[allow(clippy::too_many_arguments)]
pub fn card_matrix(
    cx: f64,
    cy: f64,
    angle_deg: f64,
    scale: f64,
    w: f64,
    h: f64,
    depth: f64,
) -> [f32; 16] {
    let t1 = translate(cx, cy);
    let p = perspective(depth);
    let r = rotate_y(angle_deg.to_radians());
    let s = scale_xy(scale);
    let t2 = translate(-w / 2.0, -h / 2.0);
    let m = mat_mul(&mat_mul(&mat_mul(&mat_mul(&t1, &p), &r), &s), &t2);
    core::array::from_fn(|i| m[i] as f32)
}

fn translate(x: f64, y: f64) -> [f64; 16] {
    let mut m = identity();
    m[3] = x;
    m[7] = y;
    m
}

fn perspective(d: f64) -> [f64; 16] {
    let mut m = identity();
    m[14] = -1.0 / d; // row 3, col 2 — w' = 1 − z/d (CSS convention)
    m
}

fn rotate_y(rad: f64) -> [f64; 16] {
    let (s, c) = rad.sin_cos();
    let mut m = identity();
    m[0] = c;
    m[2] = s;
    m[8] = -s;
    m[10] = c;
    m
}

fn scale_xy(s: f64) -> [f64; 16] {
    let mut m = identity();
    m[0] = s;
    m[5] = s;
    m
}

fn identity() -> [f64; 16] {
    let mut m = [0.0; 16];
    m[0] = 1.0;
    m[5] = 1.0;
    m[10] = 1.0;
    m[15] = 1.0;
    m
}

fn mat_mul(a: &[f64; 16], b: &[f64; 16]) -> [f64; 16] {
    let mut out = [0.0; 16];
    for r in 0..4 {
        for c in 0..4 {
            out[r * 4 + c] = (0..4).map(|k| a[r * 4 + k] * b[k * 4 + c]).sum();
        }
    }
    out
}

// --- Mesh-gradient background (the Swift `GamepadScreenBackground` MeshGradient, ported) --

/// The 16 mesh colours, row-major 4×4 (sRGB) — a verbatim port of the Swift client's
/// `meshColors`: dark-violet corners sink the frame, the edges carry mid-tone violets, and
/// the four interior points hold the bright brand family (warm pools left, cool right).
pub const MESH_COLORS: [(f64, f64, f64); 16] = [
    (0.075, 0.060, 0.160),
    (0.34, 0.27, 0.72),
    (0.30, 0.26, 0.74),
    (0.075, 0.060, 0.160),
    (0.42, 0.20, 0.54),
    (0.49, 0.39, 0.95),
    (0.28, 0.31, 0.84),
    (0.16, 0.26, 0.64),
    (0.45, 0.23, 0.60),
    (0.53, 0.31, 0.75),
    (0.35, 0.35, 0.91),
    (0.19, 0.28, 0.70),
    (0.075, 0.060, 0.160),
    (0.22, 0.18, 0.54),
    (0.24, 0.20, 0.58),
    (0.075, 0.060, 0.160),
];

/// The four interior control points that wander; the 12 boundary points stay pinned to the
/// frame (a drifting edge point would shrink the field and expose the black behind it). Each
/// row is `(base_ux, base_uy, amplitude, speed_x, speed_y, phase)` in unit UV / rad·s⁻¹ —
/// the exact `wob()` parameters from the Swift `meshPoints(at:)`. Their live displacement
/// `(amp·sin(t·sx+ph), amp·cos(t·sy+ph·1.3))` drives a domain warp, so the bright colour
/// pools follow the points as they breathe (periods ~90–130 s, out of phase so it never loops).
pub const MESH_INTERIOR: [(f64, f64, f64, f64, f64, f64); 4] = [
    (0.333, 0.333, 0.11, 0.049, 0.063, 0.4),
    (0.667, 0.333, 0.10, 0.055, 0.052, 2.1),
    (0.333, 0.667, 0.10, 0.058, 0.049, 3.6),
    (0.667, 0.667, 0.12, 0.047, 0.061, 5.0),
];

// --- Background palettes -------------------------------------------------------------------

/// One background colour family for the gamepad UI's living backdrop.
///
/// A palette is a short ordered ramp of [`Palette::stops`] — several DISTINCT hues, not one hue
/// at several brightnesses. The 4×4 mesh samples that ramp diagonally with a per-cell offset
/// ([`CELL_RAMP`]), so neighbouring cells land on different parts of it and the colours pool and
/// swirl the way a real gradient poster does; the interior points' existing domain warp then
/// drifts those pools around. An earlier version rotated ONE field's hue per palette, which is
/// why every non-default palette read as flat and monotone.
///
/// A palette also owns the UI it sits under: [`Palette::accent`] is the focus wash / selected
/// pill / switch colour, and [`Palette::light`] flips the ink (see [`crate::theme::Ink`]) so a
/// pale field gets dark text instead of white. The Apple and Android clients carry the same
/// table under the same ids, so one `ui_palette` value is one look everywhere.
pub struct Palette {
    /// The stored `ui_palette` value (see `trust::Settings::ui_palette`).
    pub id: &'static str,
    /// What the settings row shows.
    pub name: &'static str,
    /// The colour ramp, dark end first. `None` = use [`MESH_COLORS`] verbatim (the brand
    /// default, kept bit-identical to what every install already sees).
    pub stops: Option<&'static [(f64, f64, f64)]>,
    /// The field's ground — what the corners settle onto and what the calm mix lifts toward.
    pub ground: (f64, f64, f64),
    /// The UI accent: focus wash, selected tab pill, switch track, caret.
    pub accent: (f64, f64, f64),
    /// A pale field: the UI flips to dark ink and the legibility scrims go white.
    pub light: bool,
}

/// Where each of the 16 mesh cells samples the ramp. The base is the diagonal
/// `0.5·(x + y)` — top-left is the ramp's dark end, bottom-right its bright one, like both
/// reference gradients — and the per-cell nudges break the banding that a pure diagonal would
/// give, so hues pool instead of striping.
#[rustfmt::skip]
const CELL_RAMP: [f64; 16] = [
     0.10, -0.06,  0.04, -0.12,
    -0.08,  0.14, -0.10,  0.06,
     0.06, -0.12,  0.16, -0.04,
    -0.10,  0.08, -0.06,  0.12,
];

/// The thirteen shipped palettes: the brand default, six more dark fields, then six pale ones.
/// Cycling order runs dark → light, so stepping the row walks the whole range in one direction.
/// Adding one here adds it to every console settings screen; the Apple and Android tables must
/// gain the same entry to keep the `ui_palette` key portable.
#[rustfmt::skip]
pub const PALETTES: [Palette; 13] = [
    // --- dark fields (white ink) ---
    Palette {
        id: "violet", name: "Violet", stops: None,
        ground: (0.075, 0.060, 0.160), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        // For OLED and AMOLED panels, where a black pixel is a pixel switched off — no glow,
        // no power. The ramp's first two stops are literally (0,0,0), so the whole shaded half
        // of the field is genuinely off rather than "very dark grey", and the ground is pure
        // black too: the calm mix on the form screens lifts toward nothing, so settings and
        // pairing sit on an unlit panel. What is left is a faint indigo→violet ember in the
        // bright corner, dim enough to stay under a tenth of the other dark fields' mean
        // luminance while keeping the backdrop a field with somewhere to go rather than a
        // dead rectangle. The accent stays the brand violet — focus has to be findable on
        // black.
        // Named for the look, not the panel technology — black with a thin violet corona belongs
        // beside Nebula and Abyss. ⚠ The ID stays "oled": it is the stored `ui_palette` value and
        // the cross-client key, so renaming it would orphan saved choices and desync the clients.
        id: "oled", name: "Eclipse",
        stops: Some(&[
            (0.000, 0.000, 0.000), (0.000, 0.000, 0.000), (0.010, 0.020, 0.100),
            (0.045, 0.016, 0.115), (0.120, 0.024, 0.130),
        ]),
        ground: (0.0, 0.0, 0.0), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        // Deep indigo climbing through violet into a hot magenta.
        id: "nebula", name: "Nebula",
        stops: Some(&[
            (0.07, 0.05, 0.20), (0.26, 0.14, 0.54), (0.52, 0.20, 0.72),
            (0.82, 0.26, 0.62), (0.98, 0.46, 0.68),
        ]),
        ground: (0.055, 0.040, 0.135), accent: (0.95, 0.42, 0.72), light: false,
    },
    Palette {
        // Ink-blue water: teal → cerulean → a violet undertow.
        id: "abyss", name: "Abyss",
        stops: Some(&[
            (0.02, 0.10, 0.17), (0.04, 0.28, 0.42), (0.07, 0.46, 0.63),
            (0.16, 0.38, 0.78), (0.26, 0.22, 0.58),
        ]),
        ground: (0.018, 0.070, 0.130), accent: (0.26, 0.76, 0.92), light: false,
    },
    Palette {
        // Banked coals: plum embers → crimson → burnt orange → gold.
        id: "ember", name: "Ember",
        stops: Some(&[
            (0.16, 0.03, 0.10), (0.45, 0.06, 0.12), (0.72, 0.18, 0.06),
            (0.90, 0.42, 0.08), (0.95, 0.68, 0.18),
        ]),
        ground: (0.090, 0.035, 0.040), accent: (0.98, 0.62, 0.26), light: false,
    },
    Palette {
        // Forest floor into moss and a lime break.
        id: "moss", name: "Moss",
        stops: Some(&[
            (0.03, 0.11, 0.09), (0.06, 0.27, 0.20), (0.09, 0.45, 0.31),
            (0.28, 0.61, 0.28), (0.58, 0.77, 0.31),
        ]),
        ground: (0.025, 0.085, 0.070), accent: (0.48, 0.86, 0.46), light: false,
    },
    Palette {
        // Neutral, but never flat: barely-there saturation that still travels from a cool
        // charcoal to a warm stone, so even the restrained option has somewhere to go.
        id: "graphite", name: "Graphite",
        stops: Some(&[
            (0.06, 0.07, 0.11), (0.15, 0.18, 0.25), (0.30, 0.31, 0.35),
            (0.45, 0.42, 0.38), (0.60, 0.56, 0.49),
        ]),
        ground: (0.055, 0.055, 0.070), accent: (0.78, 0.80, 0.86), light: false,
    },
    // --- pale fields (dark ink) ---
    Palette {
        // The holographic foil: rose → lilac → periwinkle → aqua, with a white bloom.
        id: "holo", name: "Holo",
        stops: Some(&[
            (0.99, 0.72, 0.90), (0.80, 0.60, 0.98), (0.58, 0.62, 0.99),
            (0.55, 0.86, 0.98), (0.94, 0.98, 1.00),
        ]),
        ground: (0.96, 0.92, 0.99), accent: (0.42, 0.28, 0.86), light: true,
    },
    Palette {
        // The poster sunset: periwinkle → magenta → scarlet → tangerine → gold.
        id: "sunset", name: "Sunset",
        stops: Some(&[
            (0.55, 0.45, 0.92), (0.86, 0.31, 0.66), (0.97, 0.26, 0.34),
            (0.99, 0.51, 0.18), (1.00, 0.80, 0.22),
        ]),
        ground: (0.98, 0.74, 0.34), accent: (0.64, 0.13, 0.44), light: true,
    },
    Palette {
        // Peach into blush and lilac — the softest of the set.
        id: "bloom", name: "Bloom",
        stops: Some(&[
            (1.00, 0.86, 0.72), (0.99, 0.73, 0.79), (0.95, 0.65, 0.89),
            (0.82, 0.68, 0.96), (0.73, 0.79, 0.99),
        ]),
        ground: (0.99, 0.90, 0.89), accent: (0.72, 0.24, 0.55), light: true,
    },
    Palette {
        // First light: pale gold → coral → lilac.
        id: "dawn", name: "Dawn",
        stops: Some(&[
            (1.00, 0.92, 0.70), (1.00, 0.80, 0.62), (0.99, 0.66, 0.62),
            (0.90, 0.62, 0.78), (0.77, 0.69, 0.95),
        ]),
        ground: (1.00, 0.93, 0.82), accent: (0.82, 0.33, 0.28), light: true,
    },
    Palette {
        // Sea glass: mint → aqua → a pale sky.
        id: "mint", name: "Mint",
        stops: Some(&[
            (0.82, 0.98, 0.90), (0.62, 0.94, 0.88), (0.55, 0.88, 0.95),
            (0.63, 0.82, 0.99), (0.82, 0.87, 1.00),
        ]),
        ground: (0.90, 0.98, 0.96), accent: (0.04, 0.42, 0.40), light: true,
    },
    Palette {
        // Near-white, but iridescent rather than flat — rose, sky, mint and cream in turn.
        id: "opal", name: "Opal",
        stops: Some(&[
            (0.98, 0.92, 0.96), (0.87, 0.93, 0.99), (0.91, 0.99, 0.95),
            (0.99, 0.96, 0.88), (0.94, 0.90, 0.99),
        ]),
        ground: (0.97, 0.96, 0.99), accent: (0.36, 0.32, 0.44), light: true,
    },
];

/// The palette stored under `id`, falling back to the brand default — an unknown name is a
/// palette a newer client shipped, not a reason to draw nothing.
pub fn palette(id: &str) -> &'static Palette {
    PALETTES.iter().find(|p| p.id == id).unwrap_or(&PALETTES[0])
}

/// Sample an ordered colour ramp at `t` ∈ [0, 1] (linear between neighbouring stops). Ported
/// verbatim to Swift and Kotlin — keep the three copies in step or a palette drifts between
/// clients.
pub fn ramp(stops: &[(f64, f64, f64)], t: f64) -> (f64, f64, f64) {
    match stops.len() {
        0 => (0.0, 0.0, 0.0),
        1 => stops[0],
        n => {
            let x = t.clamp(0.0, 1.0) * (n - 1) as f64;
            let i = (x.floor() as usize).min(n - 2);
            let f = x - i as f64;
            let (a, b) = (stops[i], stops[i + 1]);
            (
                a.0 + (b.0 - a.0) * f,
                a.1 + (b.1 - a.1) * f,
                a.2 + (b.2 - a.2) * f,
            )
        }
    }
}

impl Palette {
    /// The 16 mesh colours for this palette: the ramp sampled per cell (see [`CELL_RAMP`]), or
    /// [`MESH_COLORS`] verbatim for the brand default.
    pub fn mesh_colors(&self) -> [(f64, f64, f64); 16] {
        let Some(stops) = self.stops else {
            return MESH_COLORS;
        };
        core::array::from_fn(|i| {
            let (x, y) = ((i % 4) as f64 / 3.0, (i / 4) as f64 / 3.0);
            ramp(stops, 0.5 * (x + y) + CELL_RAMP[i])
        })
    }

    /// Four drifting blob colours, for the clients that approximate the mesh with a blob field
    /// (Android). Spread across the ramp so the field still shows several hues at once.
    pub fn blob_colors(&self) -> [(f64, f64, f64); 4] {
        let stops = self.stops.unwrap_or(&VIOLET_BLOBS);
        core::array::from_fn(|i| ramp(stops, 0.15 + 0.25 * i as f64))
    }
}

/// The brand default's blob ramp — the four colours the pre-palette Android/legacy-Apple field
/// used, kept so `violet` is unchanged there too.
const VIOLET_BLOBS: [(f64, f64, f64); 5] = [
    (0.53, 0.47, 0.96),
    (0.24, 0.20, 0.72),
    (0.62, 0.30, 0.80),
    (0.22, 0.38, 0.86),
    (0.53, 0.47, 0.96),
];

/// The mesh gradient as SkSL, palette + motion baked into the source (resolution, time and
/// the calm mix are uniforms). A smooth bicubic blend of the 16 colours — a separable
/// cubic-Bézier basis in x then y, C∞ and edge-to-edge, the fragment-shader analogue of
/// SwiftUI's `MeshGradient(smoothsColors: true)`. The four interior points drive a
/// bounded (weighted-average) domain warp so the bright pools drift; then the whole field
/// gets the ±8°/~5-min hue sway, an elliptical vignette, and the vertical legibility scrim,
/// all matching the Swift `composite(at:)`. Runs on the GPU at full rate.
///
/// `u_tc.y` is the CALM mix, 0 → 1: at 1 the same living field is flattened toward its own
/// corner colour (`u_lift`), which is how the form screens (settings, add-host, pair) stay
/// restful while still drifting — the motion never changes speed, only the contrast, so the
/// crossfade between a launcher screen and a form screen can't make the field jump.
pub fn mesh_sksl(colors: &[(f64, f64, f64); 16]) -> String {
    // Colours as `float3(r, g, b)` literals, indices 0..15 (row-major 4×4).
    let c = |i: usize| {
        let (r, g, b) = colors[i];
        format!("float3({r}, {g}, {b})")
    };
    // The four interior-point domain-warp accumulators. Displacement matches Swift `wob()`:
    // x uses sin(t·sx+ph), y uses cos(t·sy+ph·1.3). SIG sets how far each point's pull
    // reaches; the warp is the weight-normalised average displacement, so |warp| ≤ max|amp|.
    let mut warp = String::new();
    for (bx, by, amp, sx, sy, ph) in MESH_INTERIOR {
        warp.push_str(&format!(
            "    q = uv - float2({bx}, {by});\n\
                 ww = exp(-dot(q, q) / (2.0 * 0.30 * 0.30));\n\
                 d = float2({amp} * sin(tt * {sx} + {ph}), \
                            {amp} * cos(tt * {sy} + {ph} * 1.3));\n\
                 wsum += d * ww; wtot += ww;\n",
        ));
    }
    format!(
        "uniform float2 u_res;\n\
         // x = seconds since the shell started, y = the calm mix (0 launcher, 1 form).\n\
         uniform float2 u_tc;\n\
         // rgb = the palette's corner colour scaled for the calm lift; a is unused (float4\n\
         // so the uniform block stays 16-byte aligned under any packing rule).\n\
         uniform float4 u_lift;\n\
         // rgb = what the vignette and scrims tend toward (black under a dark palette, white\n\
         // under a pale one — darkening a pastel field would strand the dark text on it), and\n\
         // a = how hard. A pale field needs far less: mixing toward white at the dark field's\n\
         // strength bleaches the chroma straight out of the gradient.\n\
         uniform float4 u_scrim;\n\
         \n\
         // Cubic-Bézier basis over four control values — the smooth 4-point blend per axis.\n\
         float bz(float t, float a, float b, float c, float d) {{\n\
         \x20   float u = 1.0 - t;\n\
         \x20   return u*u*u*a + 3.0*u*u*t*b + 3.0*u*t*t*c + t*t*t*d;\n\
         }}\n\
         float3 bz3(float t, float3 a, float3 b, float3 c, float3 d) {{\n\
         \x20   return float3(bz(t, a.r, b.r, c.r, d.r), bz(t, a.g, b.g, c.g, d.g), \
                              bz(t, a.b, b.b, c.b, d.b));\n\
         }}\n\
         // Hue rotation about the grey axis (Rodrigues) — the ±8° warm/cool sway.\n\
         float3 hue(float3 col, float a) {{\n\
         \x20   float3 k = float3(0.5773503);\n\
         \x20   float cs = cos(a); float sn = sin(a);\n\
         \x20   return col*cs + cross(k, col)*sn + k*dot(k, col)*(1.0 - cs);\n\
         }}\n\
         \n\
         half4 main(float2 xy) {{\n\
         \x20   float tt = u_tc.x; float calm = u_tc.y;\n\
         \x20   float2 uv = xy / u_res;\n\
         \x20   // Interior control points wander → bounded domain warp (pools follow them).\n\
         \x20   float2 wsum = float2(0.0); float wtot = 0.0; float2 q; float ww; float2 d;\n\
         {warp}\
         \x20   uv = clamp(uv - wsum / (wtot + 1e-4), 0.0, 1.0);\n\
         \n\
         \x20   // Bicubic blend of the 16 mesh colours: cubic-Bézier in x per row, then in y.\n\
         \x20   float3 r0 = bz3(uv.x, {c0}, {c1}, {c2}, {c3});\n\
         \x20   float3 r1 = bz3(uv.x, {c4}, {c5}, {c6}, {c7});\n\
         \x20   float3 r2 = bz3(uv.x, {c8}, {c9}, {c10}, {c11});\n\
         \x20   float3 r3 = bz3(uv.x, {c12}, {c13}, {c14}, {c15});\n\
         \x20   float3 col = bz3(uv.y, r0, r1, r2, r3);\n\
         \n\
         \x20   col = hue(col, sin(tt * 0.021) * 0.1396263);\n\
         \n\
         \x20   // Calm: flatten the field toward its own corner colour — the pools dim and the\n\
         \x20   // corners lift, so a form screen keeps real colour under its glass rows while\n\
         \x20   // losing the launcher's contrast. Motion is untouched (see the doc comment).\n\
         \x20   col = mix(col, col * 0.60 + u_lift.rgb, calm);\n\
         \n\
         \x20   // Elliptical vignette: clear at r=0.25 → black·0.42 at r=1.15 (aspect-fit ellipse).\n\
         \x20   // Halved under calm: a launcher's cards sit in the pooled centre, but a form\n\
         \x20   // screen's rows run out toward the edges, where crushing to black just eats them.\n\
         \x20   float2 e = (xy / u_res - 0.5) * 2.0;\n\
         \x20   float vig = clamp((length(e) - 0.25) / 0.90, 0.0, 1.0)\n\
         \x20             * mix(0.42, 0.21, calm) * u_scrim.a;\n\
         \x20   col = mix(col, u_scrim.rgb, vig);\n\
         \n\
         \x20   // Vertical legibility scrim: black 0.38/0.06/0.08/0.40 at 0/0.32/0.68/1.\n\
         \x20   float v = xy.y / u_res.y;\n\
         \x20   float s = v < 0.32 ? mix(0.38, 0.06, v / 0.32)\n\
         \x20           : v < 0.68 ? mix(0.06, 0.08, (v - 0.32) / 0.36)\n\
         \x20           : mix(0.08, 0.40, (v - 0.68) / 0.32);\n\
         \x20   col = mix(col, u_scrim.rgb, s * u_scrim.a);\n\
         \n\
         \x20   return half4(half3(col), 1.0);\n\
         }}\n",
        c0 = c(0), c1 = c(1), c2 = c(2), c3 = c(3),
        c4 = c(4), c5 = c(5), c6 = c(6), c7 = c(7),
        c8 = c(8), c9 = c(9), c10 = c(10), c11 = c(11),
        c12 = c(12), c13 = c(13), c14 = c(14), c15 = c(15),
    )
}

// --- The shared binary↔overlay model ------------------------------------------------------

#[derive(Clone, PartialEq)]
pub enum LibraryPhase {
    Loading,
    Error {
        title: String,
        body: String,
        can_retry: bool,
    },
    Empty,
    /// Games are loaded — the carousel.
    Ready,
}

#[derive(Clone)]
pub struct LibraryGame {
    pub id: String,
    pub title: String,
    pub store: String,
    /// This entry opens the launcher itself (Steam Big Picture, Heroic, Lutris) rather than a
    /// title — design D4. The host's `role` field, already reduced to a boolean by
    /// [`pf_client_core::library::GameEntry::is_launcher`] so the "anything that isn't
    /// `launcher` is a game" rule lives in exactly one place.
    pub launcher: bool,
    /// The token for this entry's brand mark (`"steam"`, `"heroic"`), already validated by
    /// [`pf_client_core::library::GameEntry::icon_token`]. Empty when the entry names no mark;
    /// a token we ship no art for simply draws nothing and the tile falls back to its name.
    pub icon: String,
    /// The system this title runs on (`"PC"`, `"PS2"`, …) — the host's own free-form display
    /// string, passed through untouched. `None` for a store-front game whose host said
    /// nothing, which is the common case: the rom-manager plugin populates this, Steam does
    /// not. [`crate::collate`] is where that `None` is given a meaning.
    pub platform: Option<String>,
    /// This title is already up on the host, so picking it RESUMES rather than starts — read
    /// from `/api/v1/status` and applied by [`LibraryShared::set_running`].
    ///
    /// Host state, never catalog state. It is deliberately not part of what the catalog cache
    /// persists (that stores `pf_client_core::library::GameEntry`, which has no such field), so
    /// a shelf served from disk can never claim a game is running because it was running the
    /// last time anyone looked.
    ///
    /// `false` for every older host and while the `/status` read is still in flight — the
    /// degradation is a Resume badge that appears a moment late, which is why the read is
    /// allowed to lag the catalog rather than hold it back.
    pub running: bool,
}

/// Whether the shelf on screen is an observation or a memory — and, if a memory, whether
/// anything is still being done about it.
///
/// Three states rather than a flag because the two cached ones want different words. "Waking the
/// host…" says a shelf is about to become current; "Last known library" says it isn't going to.
/// Telling a player the first thing while nothing is happening is the kind of lie a progress
/// indicator tells, and it is worth one extra variant to never tell it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stale {
    /// These titles came from the host just now.
    No,
    /// Served from the disk cache while the host is being woken and re-asked.
    Waking,
    /// Served from the disk cache, and the host never answered. Not an error phase: the titles
    /// are still the right ones to choose from, and replacing them with a red message because a
    /// box is asleep is precisely what the cache exists to prevent.
    Offline,
}

impl Stale {
    /// The line the shelf shows, or `None` when there is nothing to say.
    pub(crate) fn note(self) -> Option<&'static str> {
        match self {
            Stale::No => None,
            Stale::Waking => Some("Last known library \u{2014} waking the host\u{2026}"),
            Stale::Offline => Some("Last known library \u{2014} the host didn't answer"),
        }
    }
}

struct Shared {
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    /// Whether these titles came off the disk cache rather than off the host, and what the shelf
    /// should say about it. Reset to [`Stale::No`] by the live [`LibraryShared::set_games`] that
    /// reconciles a cached render.
    stale: Stale,
    /// Fetched poster bytes the renderer hasn't decoded yet (id, encoded image).
    art_in: VecDeque<(String, Vec<u8>)>,
    /// Bumped on phase/games changes so the renderer re-syncs its snapshot.
    generation: u64,
    /// Bumped once per FETCH, by [`LibraryShared::begin_fetch`].
    ///
    /// Exists so a shelf can tell "the list in this model is the one MY fetch produced" from
    /// "it is the previous host's, still sitting here" — WITHOUT having to catch a transient
    /// phase in the act. That used to be inferred by observing a non-`Ready` phase from the
    /// render loop, which held only because the first thing a fetch did was block on the network
    /// for hundreds of milliseconds. The catalog cache broke it: a warm cache publishes `Ready`
    /// a millisecond after `Loading`, well inside one 60 Hz frame, so a shelf could go from the
    /// previous host's `Ready` straight to its own and never see the `Loading` between them.
    /// A counter cannot be missed the way a passing state can.
    fetch_epoch: u64,
}

/// One consistent read of the shared model.
///
/// A struct rather than the tuple this used to be: `stale` made it a four-tuple, and by then
/// every call site was destructuring positionally into names it chose itself, which is how a
/// caller ends up reading the generation as the phase.
pub(crate) struct LibrarySnapshot {
    pub phase: LibraryPhase,
    pub games: Vec<LibraryGame>,
    pub stale: Stale,
    pub generation: u64,
}

/// The binary's write handle / the overlay's read handle — fetch threads push into it,
/// the renderer drains per frame. Cheap locks, no rendering data inside.
#[derive(Clone)]
pub struct LibraryShared(Arc<Mutex<Shared>>);

impl Default for LibraryShared {
    fn default() -> Self {
        LibraryShared(Arc::new(Mutex::new(Shared {
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            stale: Stale::No,
            art_in: VecDeque::new(),
            generation: 0,
            fetch_epoch: 0,
        })))
    }
}

impl LibraryShared {
    /// A fetch for a host is starting: the model goes to `Loading` and the epoch advances.
    ///
    /// Every fetch must come through here rather than through `set_phase(Loading)`, because the
    /// epoch is what tells a freshly-pushed shelf that the titles it is looking at are its own —
    /// see `Shared::fetch_epoch`. A fetch that only set the phase would be invisible to a shelf
    /// whose disk cache answered before the next frame.
    pub fn begin_fetch(&self) {
        let mut s = self.0.lock().unwrap();
        s.phase = LibraryPhase::Loading;
        // The previous host's staleness is not this fetch's: a cached render re-declares it a
        // moment from now, and until then the shelf must not carry a note about a library it is
        // no longer showing.
        s.stale = Stale::No;
        s.fetch_epoch += 1;
        s.generation += 1;
    }

    /// Which fetch the model is on. A shelf records this when it is pushed and compares later;
    /// any difference means a fetch has begun since, so what is in the model now belongs to it
    /// rather than to the host it replaced.
    pub(crate) fn fetch_epoch(&self) -> u64 {
        self.0.lock().unwrap().fetch_epoch
    }

    pub fn set_phase(&self, phase: LibraryPhase) {
        let mut s = self.0.lock().unwrap();
        s.phase = phase;
        s.generation += 1;
    }

    /// Loaded games → the carousel (empty = the empty scene). The titles came from the HOST, so
    /// whatever staleness a cached render declared is over.
    ///
    /// **Launcher entries are moved to the front, keeping the host's title order within each
    /// group.** Grouping here rather than in the renderer means the carousel's cursor arithmetic,
    /// the art pump and every future consumer of this model all inherit the invariant for free —
    /// a launcher tile is never buried in the middle of a 400-title shelf.
    pub fn set_games(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::No);
    }

    /// The same, for a catalog served from the on-disk cache while the host is still being
    /// asked. Identical in every way except that the shelf knows to say these titles are
    /// remembered rather than observed.
    ///
    /// Not an error state and not a lesser one: a cached library is a working library, and a host
    /// that is still waking is the case the cache exists to serve. The live fetch is always in
    /// flight behind it.
    pub fn set_games_cached(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::Waking);
    }

    /// Update what the shelf says about a cached catalog without disturbing the catalog itself —
    /// the retry window closing on a host that never answered. A no-op on a live shelf, so a late
    /// give-up from an abandoned fetch can never mark a freshly-fetched library stale.
    pub fn set_stale(&self, stale: Stale) {
        let mut s = self.0.lock().unwrap();
        if s.stale == stale || s.stale == Stale::No {
            return;
        }
        s.stale = stale;
        s.generation += 1;
    }

    fn put_games(&self, mut games: Vec<LibraryGame>, stale: Stale) {
        order(&mut games);
        let mut s = self.0.lock().unwrap();
        s.phase = if games.is_empty() {
            LibraryPhase::Empty
        } else {
            LibraryPhase::Ready
        };
        s.games = games;
        s.stale = stale;
        s.generation += 1;
    }

    /// Mark which titles the host currently has up, by library id, and re-order accordingly.
    ///
    /// Separate from [`set_games`] because it arrives separately: `/status` is read AFTER the
    /// catalog so a slow answer can't hold the titles back, and it is re-read when a stream ends
    /// (the player has just quit something, which is exactly when this changes). Called with an
    /// empty set on an older host or an unreachable one, which correctly clears every badge.
    ///
    /// A no-op — no generation bump, so no re-sync and no cursor disturbance — when nothing
    /// actually changed. That matters: this is polled, and a shelf that re-sorted every few
    /// seconds on identical data would be a shelf that flickers.
    pub fn set_running(&self, up: &std::collections::HashSet<String>) {
        let mut s = self.0.lock().unwrap();
        let mut changed = false;
        for g in &mut s.games {
            let now = up.contains(&g.id);
            if g.running != now {
                g.running = now;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let mut games = std::mem::take(&mut s.games);
        order(&mut games);
        s.games = games;
        s.generation += 1;
    }

    pub fn push_art(&self, id: String, bytes: Vec<u8>) {
        self.0.lock().unwrap().art_in.push_back((id, bytes));
    }

    /// Renderer side: the generation stamp (re-snapshot on change).
    pub(crate) fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    pub(crate) fn snapshot(&self) -> LibrarySnapshot {
        let s = self.0.lock().unwrap();
        LibrarySnapshot {
            phase: s.phase.clone(),
            games: s.games.clone(),
            stale: s.stale,
            generation: s.generation,
        }
    }

    /// Take at most `max` newly fetched posters, leaving the rest queued.
    ///
    /// Bounded because the renderer DECODES what this hands it, on the render thread. A
    /// library whose art all lands at once — the fake-library dev hook reads it off local
    /// disk, and a warm host proxy is nearly as fast — would otherwise put two hundred JPEG
    /// decodes in one frame. What stays behind costs the queue its ENCODED bytes, two orders
    /// of magnitude smaller than the raster they become.
    pub(crate) fn drain_art(&self, max: usize) -> Vec<(String, Vec<u8>)> {
        let mut s = self.0.lock().unwrap();
        let n = max.min(s.art_in.len());
        s.art_in.drain(..n).collect()
    }

    /// Take at most `max` queued posters FROM `want`, leaving every other one where it is.
    ///
    /// The collections screen is the caller, and it is the only screen that draws a handful
    /// of named covers rather than whatever arrives. Draining the queue wholesale there
    /// would be a quiet disaster: the bytes are pushed once per fetch and never re-sent, so
    /// everything it took and could not fan would be gone before the shelf a tile opens ever
    /// asked — a library of monograms, one screen further in. This takes the dozen covers
    /// that tile the collections and leaves the other four hundred queued for the shelf.
    pub(crate) fn take_art_for(
        &self,
        want: &std::collections::HashSet<String>,
        max: usize,
    ) -> Vec<(String, Vec<u8>)> {
        let mut s = self.0.lock().unwrap();
        let mut out = Vec::new();
        let mut i = 0;
        while i < s.art_in.len() && out.len() < max {
            if want.contains(&s.art_in[i].0) {
                out.extend(s.art_in.remove(i));
            } else {
                i += 1;
            }
        }
        out
    }
}

/// The shelf's display order, in the one place every writer of the model goes through.
///
/// Two rules, in this order:
/// 1. **Launcher entries lead** (design D4). Non-negotiable and applied FIRST, because the grid's
///    layout is built on it: [`GridShape`] is told a launcher COUNT and treats those entries as a
///    prefix, giving them rows of their own. Let a running game jump ahead of a launcher and the
///    cursor arithmetic and the renderer would be laying out two different fields.
/// 2. **Running titles lead within their group.** Getting back into what is already up should be
///    the first thing on the shelf rather than something to scroll for — and a launcher that is
///    up still belongs with the launchers, which is what makes the two rules compose instead of
///    fighting.
///
/// `sort_by_key` is stable, so this is a pair of partitions that preserves the host's own title
/// order inside each of the four resulting bands.
fn order(games: &mut [LibraryGame]) {
    games.sort_by_key(|g| (!g.launcher, !g.running));
}

/// Store id → display label (the GTK `ui_library` table).
pub fn store_label(store: &str) -> &'static str {
    match store {
        "steam" => "Steam",
        "custom" => "Custom",
        "heroic" => "Heroic",
        "lutris" => "Lutris",
        "epic" => "Epic",
        "gog" => "GOG",
        "xbox" => "Xbox",
        _ => "Game",
    }
}

/// Monogram for the placeholder tile: the first letters of the first two words.
pub fn initials(title: &str) -> String {
    title
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared console parity vectors — `clients/shared/console-vectors.json`, the sibling of
    /// `deeplink-vectors.json` and read the same way (`include_str!`, so a missing file is a
    /// compile error rather than a skipped test).
    ///
    /// This table lives in THREE hand-written copies — here, `GamepadPalette.kt` and
    /// `GamepadPalette.swift` — and until now nothing but prose held them together. What the file
    /// pins is not only the 13 palette definitions but the DERIVED 16-cell mesh each one produces,
    /// which is the half that actually reaches the screen and the half a transcription slip would
    /// change invisibly.
    #[test]
    fn shared_console_vectors() {
        let raw = include_str!("../../../clients/shared/console-vectors.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).expect("console-vectors.json must parse");
        let nums = |v: &serde_json::Value| -> Vec<f64> {
            v.as_array()
                .expect("array")
                .iter()
                .map(|n| n.as_f64().expect("number"))
                .collect()
        };
        let close = |what: &str, a: f64, b: f64| {
            assert!(
                (a - b).abs() < 1e-6,
                "{what}: vectors say {b}, this client computes {a}"
            );
        };

        assert_eq!(nums(&file["cell_ramp"]), CELL_RAMP.to_vec(), "CELL_RAMP");

        let interior = file["mesh_interior"].as_array().expect("mesh_interior");
        assert_eq!(interior.len(), MESH_INTERIOR.len(), "mesh interior count");
        for (w, p) in interior.iter().zip(MESH_INTERIOR.iter()) {
            let w = nums(w);
            let got = [p.0, p.1, p.2, p.3, p.4, p.5];
            for (i, (a, b)) in got.iter().zip(w.iter()).enumerate() {
                close(&format!("mesh_interior[{i}]"), *a, *b);
            }
        }

        let want = file["palettes"].as_array().expect("palettes");
        assert_eq!(want.len(), PALETTES.len(), "palette count");
        for (w, p) in want.iter().zip(PALETTES.iter()) {
            let id = w["id"].as_str().expect("id");
            assert_eq!(id, p.id, "palette order");
            assert_eq!(w["name"].as_str().expect("name"), p.name, "{id} name");
            assert_eq!(w["light"].as_bool().expect("light"), p.light, "{id} light");
            let g = nums(&w["ground"]);
            close(&format!("{id} ground.r"), p.ground.0, g[0]);
            close(&format!("{id} ground.g"), p.ground.1, g[1]);
            close(&format!("{id} ground.b"), p.ground.2, g[2]);
            let a = nums(&w["accent"]);
            close(&format!("{id} accent.r"), p.accent.0, a[0]);
            close(&format!("{id} accent.g"), p.accent.1, a[1]);
            close(&format!("{id} accent.b"), p.accent.2, a[2]);

            // The derived tables — the ones that reach the shader and the fallback field.
            let mesh = p.mesh_colors();
            let wm = w["mesh"].as_array().expect("mesh");
            assert_eq!(wm.len(), mesh.len(), "{id} mesh cells");
            for (i, (c, wc)) in mesh.iter().zip(wm.iter()).enumerate() {
                let wc = nums(wc);
                close(&format!("{id} mesh[{i}].r"), c.0, wc[0]);
                close(&format!("{id} mesh[{i}].g"), c.1, wc[1]);
                close(&format!("{id} mesh[{i}].b"), c.2, wc[2]);
            }
            let blobs = p.blob_colors();
            let wb = w["blobs"].as_array().expect("blobs");
            assert_eq!(wb.len(), blobs.len(), "{id} blob count");
            for (i, (c, wc)) in blobs.iter().zip(wb.iter()).enumerate() {
                let wc = nums(wc);
                close(&format!("{id} blob[{i}].r"), c.0, wc[0]);
                close(&format!("{id} blob[{i}].g"), c.1, wc[1]);
                close(&format!("{id} blob[{i}].b"), c.2, wc[2]);
            }
        }
    }

    /// The art queue hands the renderer a BOUNDED batch and keeps the rest, in arrival
    /// order. The renderer decodes what it takes, on the render thread, so an unbounded
    /// drain is a frame as long as the library is big — and the first frame after a fast
    /// host answered is exactly when the whole library lands at once.
    #[test]
    fn art_drains_in_bounded_batches_and_keeps_the_order() {
        let shared = LibraryShared::default();
        for i in 0..5 {
            shared.push_art(format!("g{i}"), vec![i as u8]);
        }
        let first: Vec<String> = shared.drain_art(2).into_iter().map(|(id, _)| id).collect();
        assert_eq!(first, ["g0", "g1"]);
        let rest: Vec<String> = shared.drain_art(9).into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            rest,
            ["g2", "g3", "g4"],
            "asking for more than is there is fine"
        );
        assert!(shared.drain_art(2).is_empty());
    }

    /// A selective take is the collections screen's whole art story: it takes the few covers
    /// it fans and LEAVES everything else queued, in order, for the shelf that opens next.
    /// The property is what stays behind — the poster bytes are pushed once per fetch and
    /// never re-sent, so anything taken by a screen that cannot draw it is lost for good.
    #[test]
    fn a_selective_take_leaves_everything_it_did_not_ask_for() {
        let shared = LibraryShared::default();
        for i in 0..6 {
            shared.push_art(format!("g{i}"), vec![i as u8]);
        }
        let want = ["g1".to_string(), "g4".to_string(), "g9".to_string()]
            .into_iter()
            .collect();
        let took: Vec<String> = shared
            .take_art_for(&want, 8)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            took,
            ["g1", "g4"],
            "an id that never arrived is not an error"
        );
        let rest: Vec<String> = shared.drain_art(9).into_iter().map(|(id, _)| id).collect();
        assert_eq!(rest, ["g0", "g2", "g3", "g5"], "the rest is untouched");
    }

    /// …and it is bounded the same way the plain drain is: the caller DECODES what it takes,
    /// on the render thread, so a library that lands all at once must not become one frame.
    #[test]
    fn a_selective_take_is_bounded_too() {
        let shared = LibraryShared::default();
        for i in 0..6 {
            shared.push_art(format!("g{i}"), vec![i as u8]);
        }
        let want: std::collections::HashSet<String> = (0..6).map(|i| format!("g{i}")).collect();
        assert_eq!(shared.take_art_for(&want, 2).len(), 2);
        assert_eq!(shared.take_art_for(&want, 2).len(), 2);
        assert_eq!(
            shared.take_art_for(&want, 9).len(),
            2,
            "and then it is empty"
        );
    }

    /// The GTK launcher's cursor tests, ported with the math.
    #[test]
    fn step_refuses_the_ends() {
        assert_eq!(step_cursor(0, 5, -1, false), StepResult::Boundary);
        assert_eq!(step_cursor(4, 5, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(2, 5, 1, false), StepResult::Moved(3));
        assert_eq!(step_cursor(0, 0, 1, false), StepResult::Boundary);
    }

    /// Every shape the grid can take: the launcher-less field, a Deck's seven columns with
    /// the usual two launchers, a launcher run that fills a row and a half, the degenerate
    /// two-cell sections, and a single column.
    const SHAPES: [(usize, usize, usize); 9] = [
        (11, 4, 0),
        (40, 5, 0),
        (30, 7, 2),
        (20, 4, 6),
        (4, 4, 2),
        (9, 3, 3),
        (7, 3, 7),
        (1, 3, 1),
        (13, 1, 2),
    ];

    /// The invariant the two old layout models broke: a cell's coordinates and its row's
    /// start have to be the same statement. Rows tile `0..len` in order, no index is in two
    /// of them, and none is in none — which is what makes "the ring is where the scroll
    /// says it is" true by construction rather than by coincidence.
    #[test]
    fn grid_rows_tile_the_field_exactly_once() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            let mut next = 0usize;
            for row in 0..s.rows() {
                let n = s.row_len(row);
                assert!((1..=cols).contains(&n), "{s:?} row {row} holds {n} cells");
                for col in 0..n {
                    let i = s.row_start(row) + col;
                    assert_eq!(i, next, "{s:?} row {row} does not follow the one above");
                    assert_eq!(s.cell_of(i), (row, col), "{s:?} disagrees about index {i}");
                    next += 1;
                }
            }
            assert_eq!(next, len, "{s:?} left cells in no row at all");
        }
    }

    /// The grid's two different boundary rules, which is the whole subtlety of the
    /// horizontal step: a row END refuses (like the shelf), and it is the row's TRUE end —
    /// not `cols`, which is a different number in every partial row and in the whole games
    /// section under a launcher prefix.
    #[test]
    fn grid_horizontal_moves_walk_the_row_and_refuse_its_true_ends() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                let (row, col) = s.cell_of(i);
                let want = |first: bool, to: i32| {
                    if first {
                        StepResult::Boundary
                    } else {
                        StepResult::Moved(to)
                    }
                };
                let i = i as i32;
                // The hint must not reach a horizontal move: it is the column you WOULD
                // return to, not the one you are walking out of.
                for hint in 0..cols {
                    assert_eq!(
                        grid_step(i, s, hint, GridDir::Left),
                        want(col == 0, i - 1),
                        "{s:?} Left from {i}"
                    );
                    assert_eq!(
                        grid_step(i, s, hint, GridDir::Right),
                        want(col + 1 == s.row_len(row), i + 1),
                        "{s:?} Right from {i}"
                    );
                }
            }
        }
    }

    /// Vertical moves change the ROW and nothing else. A move that has a row to go to always
    /// goes there — never sideways within the row it is already in, which is what crossing
    /// the launcher/games boundary used to do — and it arrives in the remembered column,
    /// clamped into whatever that row can hold.
    #[test]
    fn grid_vertical_moves_change_row_and_carry_the_column() {
        const VERTICAL: [(GridDir, i32); 4] = [
            (GridDir::Up, -1),
            (GridDir::Down, 1),
            (GridDir::PageBack, -GRID_PAGE_ROWS),
            (GridDir::PageForward, GRID_PAGE_ROWS),
        ];
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                let (row, _) = s.cell_of(i);
                for hint in 0..cols {
                    for (dir, d) in VERTICAL {
                        let want_row = (row as i32 + d).clamp(0, s.rows() as i32 - 1) as usize;
                        let what = format!("{s:?} {dir:?} from {i} with hint {hint}");
                        match grid_step(i as i32, s, hint, dir) {
                            StepResult::Moved(to) => {
                                let (r, c) = s.cell_of(to as usize);
                                assert_eq!(r, want_row, "{what} landed in row {r}");
                                if r != row {
                                    assert_eq!(c, hint.min(s.row_len(r) - 1), "{what} column");
                                }
                            }
                            // Only the field's own edges refuse; a page already at the edge
                            // still travels along the row it is on, so it refuses only from
                            // that row's end.
                            StepResult::Boundary => assert_eq!(want_row, row, "{what} refused"),
                        }
                    }
                }
            }
        }
    }

    /// Both sections stay reachable from anywhere in the other, in a bounded number of
    /// presses. This is the user's actual complaint — a launcher row you could see but not
    /// get back into — stated as a property.
    #[test]
    fn every_row_is_reachable_by_stepping() {
        for (len, cols, launchers) in SHAPES {
            let s = GridShape::new(len, cols, launchers);
            for i in 0..len {
                for (dir, end) in [(GridDir::Up, 0), (GridDir::Down, s.rows() - 1)] {
                    let mut cursor = i as i32;
                    for _ in 0..=s.rows() {
                        match grid_step(cursor, s, 0, dir) {
                            StepResult::Moved(to) => cursor = to,
                            StepResult::Boundary => break,
                        }
                    }
                    let (row, _) = s.cell_of(cursor as usize);
                    assert_eq!(row, end, "{s:?} {dir:?} from {i} stalled in row {row}");
                }
            }
        }
    }

    /// The Deck, exactly: 1280×800 gives seven columns, and a host with the usual two
    /// launchers puts them alone on row 0 with the games restarting at column 0 under them.
    /// Every value here is one the uniform-grid arithmetic got wrong.
    #[test]
    fn the_launcher_row_sits_squarely_above_the_games() {
        let s = GridShape::new(30, 7, 2);
        assert_eq!(s.rows(), 5);
        assert_eq!((s.row_len(0), s.row_len(1)), (2, 7));
        // Down from a launcher lands on the cover UNDER it, not five columns to the right.
        assert_eq!(grid_step(0, s, 0, GridDir::Down), StepResult::Moved(2));
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(3));
        // …and Up out of the games band leaves it, rather than sliding along it.
        assert_eq!(grid_step(2, s, 0, GridDir::Up), StepResult::Moved(0));
        assert_eq!(grid_step(3, s, 1, GridDir::Up), StepResult::Moved(1));
        assert_eq!(grid_step(6, s, 4, GridDir::Up), StepResult::Moved(1));
        // The games row's true ends are 2 and 8 — 6 and 7 are mid-row, and 8 is the end.
        assert_eq!(grid_step(6, s, 4, GridDir::Right), StepResult::Moved(7));
        assert_eq!(grid_step(7, s, 5, GridDir::Left), StepResult::Moved(6));
        assert_eq!(grid_step(8, s, 6, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 0, GridDir::Left), StepResult::Boundary);
    }

    /// The remembered column is what makes a crossing reversible: stepping down through a
    /// two-wide launcher row and back must return to the column you set out from, not pin
    /// you to the column the narrow row could hold.
    #[test]
    fn a_crossing_returns_to_the_column_it_started_from() {
        use GridDir::{Down, Right, Up};
        let s = GridShape::new(30, 7, 2);
        // The screen's own rule, in one place: `LibraryScreen::grid_move` steps the cursor
        // and re-reads the hint through exactly these two calls.
        let walk = |start: i32, dirs: &[GridDir]| {
            let (mut cursor, mut hint) = (start, s.cell_of(start.max(0) as usize).1);
            for &dir in dirs {
                if let StepResult::Moved(to) = grid_step(cursor, s, hint, dir) {
                    hint = grid_col_hint(s, hint, dir, to);
                    cursor = to;
                }
            }
            cursor
        };
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right]), 6);
        // Up parks in the launcher row's only reachable column…
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up]), 1);
        // …and Down restores the column, twice over — a vertical move never spends it.
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up, Down]), 6);
        assert_eq!(
            walk(0, &[Down, Right, Right, Right, Right, Up, Down, Up, Down]),
            6
        );
    }

    #[test]
    fn grid_pages_by_rows_and_lands_on_the_ends() {
        let s = GridShape::new(40, 5, 0);
        assert_eq!(
            grid_step(0, s, 0, GridDir::PageForward),
            StepResult::Moved(15)
        );
        // A page past the end lands ON the end rather than refusing — a jump is a
        // "take me there", the same reading `step_cursor`'s clamped mode has.
        assert_eq!(
            grid_step(35, s, 0, GridDir::PageForward),
            StepResult::Moved(39)
        );
        assert_eq!(
            grid_step(39, s, 4, GridDir::PageForward),
            StepResult::Boundary
        );
        assert_eq!(grid_step(3, s, 3, GridDir::PageBack), StepResult::Moved(0));
        assert_eq!(grid_step(0, s, 0, GridDir::PageBack), StepResult::Boundary);
    }

    /// The launcher-less grid, unchanged: this is the field the old arithmetic got right,
    /// and the proof that sharing the shape did not move it.
    #[test]
    fn grid_rows_refuse_at_their_ends_but_the_tail_row_clamps() {
        // 11 items, 4 columns: rows of 4, 4, 3.
        let s = GridShape::new(11, 4, 0);
        assert_eq!(grid_step(1, s, 1, GridDir::Right), StepResult::Moved(2));
        assert_eq!(grid_step(2, s, 2, GridDir::Left), StepResult::Moved(1));
        // At a row's ends: refused, NOT wrapped onto the neighbouring row.
        assert_eq!(grid_step(3, s, 3, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(4, s, 0, GridDir::Left), StepResult::Boundary);
        // Down from the top row lands directly below.
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(5));
        // Down into the SHORT tail row clamps to the last item rather than thudding —
        // column 3 does not exist down there.
        assert_eq!(grid_step(7, s, 3, GridDir::Down), StepResult::Moved(10));
        // …and once there, Down really is the end.
        assert_eq!(grid_step(10, s, 3, GridDir::Down), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 2, GridDir::Up), StepResult::Boundary);
        assert_eq!(grid_step(6, s, 2, GridDir::Up), StepResult::Moved(2));
    }

    /// The persisted view name is a FILE FORMAT, and an unknown one must land on the shelf
    /// — a newer client writing `"coverwall"` must not leave this one with no arrangement.
    #[test]
    fn library_view_parses_leniently() {
        assert_eq!(LibraryView::parse("grid"), LibraryView::Grid);
        assert_eq!(LibraryView::parse("shelf"), LibraryView::Shelf);
        assert_eq!(LibraryView::parse("coverwall"), LibraryView::Shelf);
        assert_eq!(LibraryView::parse(""), LibraryView::Shelf);
        assert_eq!(LibraryView::default(), LibraryView::Shelf);
        for v in LibraryView::ALL {
            assert_eq!(LibraryView::parse(v.id()), v, "{} round-trips", v.label());
        }
    }

    #[test]
    fn grid_step_is_safe_on_a_degenerate_grid() {
        let empty = GridShape::new(0, 4, 0);
        assert_eq!(grid_step(0, empty, 0, GridDir::Right), StepResult::Boundary);
        let colless = GridShape::new(5, 0, 0);
        assert_eq!(
            grid_step(0, colless, 0, GridDir::Right),
            StepResult::Boundary
        );
        // One column: left/right are always refused, up/down still walk.
        let thin = GridShape::new(5, 1, 0);
        assert_eq!(grid_step(1, thin, 0, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(1, thin, 0, GridDir::Down), StepResult::Moved(2));
        // A cursor the library outgrew reads as the nearest real cell, so the next press
        // heals it rather than compounding it.
        let s = GridShape::new(6, 3, 2);
        assert_eq!(grid_step(99, s, 0, GridDir::Up), StepResult::Moved(2));
        assert_eq!(grid_step(-4, s, 0, GridDir::Right), StepResult::Moved(1));
    }

    /// Design D4: launcher entries lead the shelf, and the host's title order survives within
    /// each group. The renderer's `launcher_count()` reads the launcher group as the prefix
    /// `0..n`, so an interleaved list would silently mislabel the group heading.
    #[test]
    fn set_games_groups_launchers_first_and_keeps_title_order() {
        let g = |title: &str, launcher: bool| LibraryGame {
            id: format!("steam:{title}"),
            title: title.to_string(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: None,
            running: false,
        };
        let shared = LibraryShared::default();
        shared.set_games(vec![
            g("Celeste", false),
            g("Big Picture", true),
            g("Portal 2", false),
            g("Heroic", true),
        ]);
        let snap = shared.snapshot();
        assert!(matches!(snap.phase, LibraryPhase::Ready));
        assert_eq!(snap.stale, Stale::No, "a live fetch is not a memory");
        let titles: Vec<&str> = snap.games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(titles, ["Big Picture", "Heroic", "Celeste", "Portal 2"]);
        assert_eq!(snap.games.iter().take_while(|g| g.launcher).count(), 2);
    }

    /// Running titles lead — but WITHIN their group, so the launcher prefix the grid's
    /// [`GridShape`] is built on survives. A running game jumping ahead of a launcher would put
    /// the cursor arithmetic and the renderer on two different fields.
    #[test]
    fn running_titles_lead_without_breaking_the_launcher_prefix() {
        let g = |title: &str, launcher: bool| LibraryGame {
            id: format!("steam:{title}"),
            title: title.to_string(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: None,
            running: false,
        };
        let shared = LibraryShared::default();
        shared.set_games(vec![
            g("Celeste", false),
            g("Big Picture", true),
            g("Portal 2", false),
            g("Heroic", true),
            g("Tunic", false),
        ]);
        // Portal 2 and Heroic are up — one of each group.
        let up: std::collections::HashSet<String> = ["steam:Portal 2", "steam:Heroic"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        shared.set_running(&up);
        let snap = shared.snapshot();
        let titles: Vec<&str> = snap.games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Heroic", "Big Picture", "Portal 2", "Celeste", "Tunic"],
            "running first inside each group; launchers still the prefix"
        );
        assert_eq!(
            snap.games.iter().take_while(|g| g.launcher).count(),
            2,
            "the launcher prefix GridShape depends on is intact"
        );
        assert!(snap.games[0].running && snap.games[2].running);

        // Re-applying the SAME set changes nothing and must not bump the generation — this is
        // polled, and a shelf that re-synced on identical data would be a shelf that flickers.
        let gen_before = snap.generation;
        shared.set_running(&up);
        assert_eq!(shared.snapshot().generation, gen_before);

        // The game quit: the badge clears and the shelf re-sorts back.
        shared.set_running(&std::collections::HashSet::new());
        let after = shared.snapshot();
        assert!(after.games.iter().all(|g| !g.running));
        assert!(after.generation > gen_before, "a real change does re-sync");
    }

    /// A cached catalog is a normal `Ready` shelf that merely knows it is a memory — never an
    /// error, and never a lesser phase. The live fetch reconciling it clears the flag.
    #[test]
    fn a_cached_catalog_is_ready_and_stale_until_the_host_answers() {
        let g = |t: &str| LibraryGame {
            id: format!("steam:{t}"),
            title: t.to_string(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            running: false,
        };
        let shared = LibraryShared::default();
        shared.set_games_cached(vec![g("Celeste"), g("Tunic")]);
        let cached = shared.snapshot();
        assert!(matches!(cached.phase, LibraryPhase::Ready));
        assert_eq!(cached.stale, Stale::Waking);
        assert!(cached.stale.note().is_some());
        // The retry window closed with no answer: same titles, different words.
        shared.set_stale(Stale::Offline);
        assert_eq!(shared.snapshot().stale, Stale::Offline);
        shared.set_games(vec![g("Celeste"), g("Tunic"), g("Hades")]);
        let live = shared.snapshot();
        assert_eq!(
            live.stale,
            Stale::No,
            "the host answered — these are observed now"
        );
        assert!(live.stale.note().is_none());
        assert_eq!(live.games.len(), 3);
        // A late give-up from an abandoned fetch must not mark a fresh library stale.
        shared.set_stale(Stale::Offline);
        assert_eq!(shared.snapshot().stale, Stale::No);
    }

    /// A library with no launcher entries is untouched — the whole point of the grouping being
    /// invisible until a plugin actually publishes a launcher tile.
    #[test]
    fn set_games_leaves_a_launcher_less_library_alone() {
        let shared = LibraryShared::default();
        shared.set_games(
            ["Celeste", "Portal 2", "Tunic"]
                .iter()
                .map(|t| LibraryGame {
                    id: format!("steam:{t}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: None,
                    running: false,
                })
                .collect(),
        );
        let titles: Vec<String> = shared
            .snapshot()
            .games
            .iter()
            .map(|g| g.title.clone())
            .collect();
        assert_eq!(titles, ["Celeste", "Portal 2", "Tunic"]);
    }

    #[test]
    fn jump_clamps_onto_the_ends() {
        assert_eq!(step_cursor(1, 5, -JUMP, true), StepResult::Moved(0));
        assert_eq!(step_cursor(3, 5, JUMP, true), StepResult::Moved(4));
        assert_eq!(step_cursor(0, 5, -JUMP, true), StepResult::Boundary);
    }

    /// Springs converge onto the target and stay finite through a stalled frame.
    #[test]
    fn springs_converge() {
        let (mut pos, mut vel) = (0.0, 0.0);
        for _ in 0..120 {
            (pos, vel) = spring_advance(pos, vel, 3.0, SPRING_K, SPRING_C, 1.0 / 60.0);
        }
        assert!((pos - 3.0).abs() < 0.01, "{pos}");
        let (p, v) = spring_advance(0.0, 0.0, 1.0, BUMP_K, BUMP_C, 0.05);
        assert!(
            p.is_finite() && v.is_finite() && p > 0.0 && p < 2.0,
            "{p}/{v}"
        );
    }

    /// The focused card (angle 0, scale 1) maps its center to (cx, cy) exactly.
    #[test]
    fn card_matrix_centers_the_focused_card() {
        let m = card_matrix(640.0, 400.0, 0.0, 1.0, POSTER_W, POSTER_H, PERSPECTIVE);
        // Apply to the card-local center (w/2, h/2, 0, 1).
        let (x, y) = (POSTER_W as f32 / 2.0, POSTER_H as f32 / 2.0);
        let px = m[0] * x + m[1] * y + m[3];
        let py = m[4] * x + m[5] * y + m[7];
        let pw = m[12] * x + m[13] * y + m[15];
        assert!((px / pw - 640.0).abs() < 0.01, "{}", px / pw);
        assert!((py / pw - 400.0).abs() < 0.01, "{}", py / pw);
    }

    /// A right-side card's INNER (left) edge recedes: its projected x compresses toward
    /// the center relative to the flat card — the coverflow corridor.
    #[test]
    fn side_card_inner_edge_recedes() {
        let flat = card_matrix(900.0, 400.0, 0.0, 1.0, POSTER_W, POSTER_H, PERSPECTIVE);
        let tilted = card_matrix(
            900.0,
            400.0,
            -ROTATE_DEG,
            1.0,
            POSTER_W,
            POSTER_H,
            PERSPECTIVE,
        );
        let project = |m: &[f32; 16], x: f32, y: f32| {
            let px = m[0] * x + m[1] * y + m[3];
            let pw = m[12] * x + m[13] * y + m[15];
            px / pw
        };
        // The inner edge is x=0 in card space. Perspective divide: receding (w < 1 side)
        // pushes it AWAY from the vanishing center — the edge reads as farther.
        let flat_left = project(&flat, 0.0, POSTER_H as f32 / 2.0);
        let tilt_left = project(&tilted, 0.0, POSTER_H as f32 / 2.0);
        let flat_right = project(&flat, POSTER_W as f32, POSTER_H as f32 / 2.0);
        let tilt_right = project(&tilted, POSTER_W as f32, POSTER_H as f32 / 2.0);
        // Tilt narrows the card's projected width (it turned away from the viewer).
        assert!((tilt_right - tilt_left) < (flat_right - flat_left) * 0.95);
    }

    #[test]
    fn initials_take_two_words() {
        assert_eq!(initials("Dota 2"), "D2");
        assert_eq!(initials("half-life"), "H");
    }

    /// The generated SkSL parses as far as syntax we control (sanity: balanced braces, all
    /// 16 colours baked in, the five bicubic evals and four interior warp terms present).
    #[test]
    fn mesh_sksl_shape() {
        let src = mesh_sksl(&MESH_COLORS);
        assert!(src.matches("float3(").count() >= 16, "16 colours baked");
        assert_eq!(src.matches("bz3(").count(), 6); // 1 definition + 5 call sites
        assert_eq!(src.matches("wtot +=").count(), 4); // one per interior point
        assert_eq!(src.matches('{').count(), src.matches('}').count());
    }

    /// The brand default must still be the SHIPPED field, colour for colour. Every install
    /// already sees it, and a palette table that quietly restyled the default would be a
    /// regression dressed as a feature.
    #[test]
    fn violet_is_the_untouched_shipped_field() {
        assert_eq!(PALETTES[0].id, "violet");
        assert!(
            PALETTES[0].stops.is_none(),
            "the default is the explicit grid"
        );
        assert_eq!(palette("violet").mesh_colors(), MESH_COLORS);
        // An unknown name is a newer client's palette, not an error.
        assert_eq!(palette("chartreuse").id, "violet");
        assert_eq!(palette("").id, "violet");
    }

    /// Hue angle in degrees, or `None` for something too grey to have one.
    fn hue(c: (f64, f64, f64)) -> Option<f64> {
        let (r, g, b) = c;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        if d < 0.04 {
            return None;
        }
        let h = if max == r {
            60.0 * (((g - b) / d) % 6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        Some((h + 360.0) % 360.0)
    }

    /// A palette must read as SEVERAL hues, not one hue at several brightnesses — that was
    /// exactly the complaint about the hue-rotation model this replaced. Measured as the
    /// widest gap between any two of the 16 mesh colours' hue angles.
    #[test]
    fn every_palette_is_multi_tone() {
        for p in &PALETTES {
            let hues: Vec<f64> = p.mesh_colors().iter().filter_map(|c| hue(*c)).collect();
            assert!(hues.len() >= 8, "{}: too few coloured cells", p.id);
            let spread = hues
                .iter()
                .flat_map(|a| {
                    hues.iter().map(move |b| {
                        let d = (a - b).abs() % 360.0;
                        d.min(360.0 - d)
                    })
                })
                .fold(0.0f64, f64::max);
            // Graphite and Opal are deliberately near-neutral; everything else must carry a
            // real hue journey.
            let floor = if matches!(p.id, "graphite" | "opal") {
                20.0
            } else {
                45.0
            };
            assert!(spread >= floor, "{} spans only {spread:.0}° of hue", p.id);
        }
    }

    /// Ids, order and the light/dark split are the cross-client contract — the Apple and
    /// Android tables must match this exactly.
    #[test]
    fn table_matches_the_other_clients() {
        let ids: Vec<&str> = PALETTES.iter().map(|p| p.id).collect();
        assert_eq!(
            ids,
            [
                "violet", "oled", "nebula", "abyss", "ember", "moss", "graphite", "holo", "sunset",
                "bloom", "dawn", "mint", "opal",
            ]
        );
        // Dark fields lead, pale ones follow, so stepping the row walks one direction.
        let first_light = PALETTES
            .iter()
            .position(|p| p.light)
            .expect("some are light");
        assert!(PALETTES[first_light..].iter().all(|p| p.light));
        assert_eq!(first_light, 7);
    }

    /// OLED is the one palette whose selling point is measurable: it has to be genuinely
    /// black, not merely the darkest of the dark fields. Pure black corners, a mean well
    /// under every other field's, and a ground that lifts to nothing on the form screens.
    #[test]
    fn oled_is_actually_black() {
        let luma = |c: (f64, f64, f64)| 0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2;
        let oled = palette("oled");
        assert_eq!(
            oled.ground,
            (0.0, 0.0, 0.0),
            "the calm lift must be nothing"
        );
        let cells = oled.mesh_colors();
        assert!(
            cells.iter().filter(|c| luma(**c) == 0.0).count() >= 3,
            "the shaded corner has to be switched off, not dimmed"
        );
        let mean = cells.iter().map(|c| luma(*c)).sum::<f64>() / 16.0;
        let darkest_other = PALETTES
            .iter()
            .filter(|p| p.id != "oled")
            .map(|p| p.mesh_colors().iter().map(|c| luma(*c)).sum::<f64>() / 16.0)
            .fold(f64::MAX, f64::min);
        assert!(
            mean < darkest_other / 2.0,
            "oled means {mean:.3}, only half a stop under {darkest_other:.3}"
        );
    }

    /// Every colour a palette produces stays in gamut, and a pale palette really is pale —
    /// its ink flips, so a mislabelled one would put dark text on a dark field.
    #[test]
    fn palettes_are_in_gamut_and_honest_about_lightness() {
        let luma = |c: (f64, f64, f64)| 0.2126 * c.0 + 0.7152 * c.1 + 0.0722 * c.2;
        for p in &PALETTES {
            for c in p.mesh_colors().iter().chain(p.blob_colors().iter()) {
                for v in [c.0, c.1, c.2] {
                    assert!((0.0..=1.0).contains(&v), "{} {c:?}", p.id);
                }
            }
            let mean = p.mesh_colors().iter().map(|c| luma(*c)).sum::<f64>() / 16.0;
            if p.light {
                assert!(mean > 0.5, "{} is flagged light but means {mean:.2}", p.id);
                assert!(luma(p.ground) > 0.6, "{}'s ground is dark", p.id);
            } else {
                assert!(mean < 0.45, "{} is flagged dark but means {mean:.2}", p.id);
                assert!(luma(p.ground) < 0.2, "{}'s ground is light", p.id);
            }
            // The accent tints glass of the OPPOSITE polarity to the field, so it has to be
            // legible there: dark accents on white frost, bright ones on dark glass.
            let a = luma(p.accent);
            if p.light {
                assert!(a < 0.45, "{}'s accent is too pale for white glass", p.id);
            } else {
                assert!(a > 0.25, "{}'s accent is too dark for dark glass", p.id);
            }
        }
    }

    /// The ramp is the shared sampling rule the Swift and Kotlin ports reproduce.
    #[test]
    fn ramp_interpolates_between_stops() {
        let stops = [(0.0, 0.0, 0.0), (1.0, 0.0, 0.0), (1.0, 1.0, 1.0)];
        assert_eq!(ramp(&stops, 0.0), (0.0, 0.0, 0.0));
        assert_eq!(ramp(&stops, 1.0), (1.0, 1.0, 1.0));
        assert_eq!(ramp(&stops, 0.5), (1.0, 0.0, 0.0));
        let q = ramp(&stops, 0.25);
        assert!((q.0 - 0.5).abs() < 1e-9 && q.1 == 0.0);
        // Out of range clamps rather than panicking.
        assert_eq!(ramp(&stops, -3.0), (0.0, 0.0, 0.0));
        assert_eq!(ramp(&stops, 9.0), (1.0, 1.0, 1.0));
        assert_eq!(ramp(&[], 0.5), (0.0, 0.0, 0.0));
    }
}
