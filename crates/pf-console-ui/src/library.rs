//! Library model and coverflow math — everything the overlay shares that is not Skia.
//!
//! Games, phase, incoming art, generation and fetch epoch live in [`LibraryShared`].
//! Fetch threads write; the renderer drains per frame. Cursor and grid arithmetic are
//! ported from the GTK launcher and tested here. Geometry, the 4×4 card transform, and
//! the mesh-gradient palettes sit alongside.
//!
//! Rendering is `skia_overlay`. Palette ids and the derived 16-cell meshes are pinned by
//! `clients/shared/console-vectors.json`, and by `GamepadPalette.kt` / `.swift`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// --- Geometry (GTK launcher / Apple coverflow parity) ---

/// 2:3 covers. Sized so the focused poster, detail panel and hint bar fit 1280×800 with air.
pub const POSTER_W: f64 = 220.0;
pub const POSTER_H: f64 = 330.0;
pub const FOCUS_GAP: f64 = 230.0;
/// Center-to-center between successive side cards; tighter than projected width so they overlap.
pub const SIDE_SPACING: f64 = 104.0;
pub const VISIBLE_RANGE: f64 = 5.5;
pub const RECEDE_SCALE: f64 = 0.24;
/// Side-card yaw about its own vertical axis; inner edge recedes behind the focus.
pub const ROTATE_DEG: f64 = 38.0;
/// Perspective depth for the tilt, px (CSS `perspective()` semantics).
pub const PERSPECTIVE: f64 = 800.0;
/// Recede-veil max opacity — overlap separator, not distance. Washes toward `theme::shade`.
pub const RECEDE_DIM: f64 = 0.10;
/// Refused-move recoil, px against the push.
pub const BUMP_PX: f64 = 16.0;
/// Mount entrance ([`crate::anim::Entrance`]): arrival scale, rise (design units), yaw. Shared with the home carousel.
pub const ENTER_SCALE: f64 = 0.74;
pub const ENTER_RISE: f64 = 34.0;
pub const ENTER_TURN_DEG: f64 = 62.0;
pub const JUMP: i32 = 5;

// Semi-implicit Euler, not eased: velocity carries across retargets.
/// Cursor chase: ζ ≈ 0.85 — settles in ~0.3 s with a whisker of overshoot.
pub const SPRING_K: f64 = 200.0;
pub const SPRING_C: f64 = 24.0;
/// Boundary recoil: stiffer and more underdamped (ζ ≈ 0.55) — one visible wobble.
pub const BUMP_K: f64 = 600.0;
pub const BUMP_C: f64 = 27.0;

fn spring_step(pos: f64, vel: f64, target: f64, k: f64, c: f64, dt: f64) -> (f64, f64) {
    let vel = vel + (k * (target - pos) - c * vel) * dt;
    (pos + vel * dt, vel)
}

/// One frame of a damped spring, in ≤ 8 ms substeps so a stalled frame stays inside the integrator's stability bound.
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

/// `clamp` lands jumps on the ends; a plain step refuses to leave them.
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LibraryView {
    #[default]
    Shelf,
    Grid,
}

impl LibraryView {
    /// Persisted `library_view`. Unknown → [`LibraryView::Shelf`] (a newer client's name).
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

/// Grid cell: same 2:3 as the poster at ~⅔ size, so three rows plus a readable detail band at 800-tall.
pub const GRID_W: f64 = 150.0;
pub const GRID_H: f64 = 225.0;
pub const GRID_GAP: f64 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridDir {
    Left,
    Right,
    Up,
    Down,
    PageBack,
    PageForward,
}

/// Shoulder jump, rows (≈ one screen).
pub const GRID_PAGE_ROWS: i32 = 3;

/// Layout both cursor math and the renderer read.
///
/// The launcher prefix occupies its own rows; the games section restarts at column 0.
/// A uniform `index % cols` grid only agrees when `launchers` is a multiple of `cols`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridShape {
    /// Cells per row, from the last frame actually drawn — not derived twice from two widths.
    pub cols: usize,
    /// Filtered count; the cursor indexes this.
    pub len: usize,
    /// Where the games section starts, or 0 when the field is one continuous run.
    pub split: usize,
}

impl GridShape {
    /// `launchers` is the leading run. Split only when both halves exist; otherwise a plain grid.
    pub fn new(len: usize, cols: usize, launchers: usize) -> GridShape {
        let split = if launchers > 0 && launchers < len {
            launchers
        } else {
            0
        };
        GridShape { cols, len, split }
    }

    /// First row of the games section; ignore when `split == 0`.
    pub fn split_row(&self) -> usize {
        self.split.div_ceil(self.cols.max(1))
    }

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

    pub fn row_start(&self, row: usize) -> usize {
        let cols = self.cols.max(1);
        if self.split > 0 && row >= self.split_row() {
            self.split + (row - self.split_row()) * cols
        } else {
            row * cols
        }
    }

    /// Cells this row holds. The last launcher row ends at `split`; the last field row at `len`.
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

/// Grid cursor against the shape the renderer is drawing.
///
/// Horizontal: walk the row, refuse at that row's true ends (no wrap). Vertical and page:
/// change row only, carrying `col_hint` clamped into the target row. Only leaving the
/// grid is a boundary. A short row is a layout accident — Down clamps onto the last title.
/// `col_hint` is the column last chosen ([`grid_col_hint`]), so a two-wide launcher row
/// is reversible.
pub fn grid_step(cursor: i32, shape: GridShape, col_hint: usize, dir: GridDir) -> StepResult {
    if shape.len == 0 || shape.cols == 0 {
        return StepResult::Boundary;
    }
    // Outside the field: library shortened. Nearest real cell, so the next press heals it.
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
                // Step at the edge refuses. Page is clamped `step_cursor`: land on this row's end.
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

/// Remembered column after a move: a horizontal step chooses it; a vertical step only borrows it.
pub fn grid_col_hint(shape: GridShape, prev: usize, dir: GridDir, landed: i32) -> usize {
    match dir {
        GridDir::Left | GridDir::Right => shape.cell_of(landed.max(0) as usize).1,
        _ => prev,
    }
}

// --- 4×4 matrix (row-major) — coverflow card transform ---

/// `T(cx,cy) · P(depth) · Ry(angle) · S(s) · T(-w/2,-h/2)`: card-local (0..w, 0..h) → screen.
/// Rotation is about the card's vertical centre. Row-major for `Canvas::concat_44`.
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

// --- Mesh-gradient background (Swift `GamepadScreenBackground` MeshGradient) ---

/// 16 mesh colours, row-major 4×4 sRGB. Verbatim Swift `meshColors`.
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

/// Four interior control points; the 12 boundary points stay pinned (a drifting edge exposes black).
/// Each row is `(base_ux, base_uy, amplitude, speed_x, speed_y, phase)` in UV / rad·s⁻¹ —
/// Swift `meshPoints(at:)` `wob()`. Periods ~90–130 s, out of phase so the warp does not loop.
pub const MESH_INTERIOR: [(f64, f64, f64, f64, f64, f64); 4] = [
    (0.333, 0.333, 0.11, 0.049, 0.063, 0.4),
    (0.667, 0.333, 0.10, 0.055, 0.052, 2.1),
    (0.333, 0.667, 0.10, 0.058, 0.049, 3.6),
    (0.667, 0.667, 0.12, 0.047, 0.061, 5.0),
];

// --- Background palettes -------------------------------------------------------------------

/// One background colour family.
///
/// `stops` is several distinct hues, not one hue at several brightnesses. The 4×4 mesh
/// samples that ramp diagonally with [`CELL_RAMP`]. [`Palette::accent`] is the focus wash;
/// [`Palette::light`] flips ink ([`crate::theme::Ink`]). Apple and Android tables share these ids.
pub struct Palette {
    /// The stored `ui_palette` value (see `trust::Settings::ui_palette`).
    pub id: &'static str,
    pub name: &'static str,
    /// Colour ramp, dark end first. `None` = [`MESH_COLORS`] verbatim.
    pub stops: Option<&'static [(f64, f64, f64)]>,
    /// The field's ground — what the corners settle onto and what the calm mix lifts toward.
    pub ground: (f64, f64, f64),
    pub accent: (f64, f64, f64),
    /// Pale field: dark ink, white scrims.
    pub light: bool,
}

/// Per-cell ramp offset on top of the diagonal `0.5·(x + y)`. Nudges stop a pure diagonal from banding.
#[rustfmt::skip]
const CELL_RAMP: [f64; 16] = [
     0.10, -0.06,  0.04, -0.12,
    -0.08,  0.14, -0.10,  0.06,
     0.06, -0.12,  0.16, -0.04,
    -0.10,  0.08, -0.06,  0.12,
];

/// Brand default, six dark fields, then six pale. Dark → light is cycle order.
/// Adding a row here is not enough: Apple and Android tables must gain the same `ui_palette` id.
#[rustfmt::skip]
pub const PALETTES: [Palette; 13] = [
    // --- dark fields (white ink) ---
    Palette {
        id: "violet", name: "Violet", stops: None,
        ground: (0.075, 0.060, 0.160), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        // First two stops are (0,0,0): OLED pixels off, not dark grey. Ground is black so calm lifts to nothing.
        // Id stays `"oled"` — stored `ui_palette` key; renaming orphans saved choices.
        id: "oled", name: "Eclipse",
        stops: Some(&[
            (0.000, 0.000, 0.000), (0.000, 0.000, 0.000), (0.010, 0.020, 0.100),
            (0.045, 0.016, 0.115), (0.120, 0.024, 0.130),
        ]),
        ground: (0.0, 0.0, 0.0), accent: (0.525, 0.471, 0.961), light: false,
    },
    Palette {
        id: "nebula", name: "Nebula",
        stops: Some(&[
            (0.07, 0.05, 0.20), (0.26, 0.14, 0.54), (0.52, 0.20, 0.72),
            (0.82, 0.26, 0.62), (0.98, 0.46, 0.68),
        ]),
        ground: (0.055, 0.040, 0.135), accent: (0.95, 0.42, 0.72), light: false,
    },
    Palette {
        id: "abyss", name: "Abyss",
        stops: Some(&[
            (0.02, 0.10, 0.17), (0.04, 0.28, 0.42), (0.07, 0.46, 0.63),
            (0.16, 0.38, 0.78), (0.26, 0.22, 0.58),
        ]),
        ground: (0.018, 0.070, 0.130), accent: (0.26, 0.76, 0.92), light: false,
    },
    Palette {
        id: "ember", name: "Ember",
        stops: Some(&[
            (0.16, 0.03, 0.10), (0.45, 0.06, 0.12), (0.72, 0.18, 0.06),
            (0.90, 0.42, 0.08), (0.95, 0.68, 0.18),
        ]),
        ground: (0.090, 0.035, 0.040), accent: (0.98, 0.62, 0.26), light: false,
    },
    Palette {
        id: "moss", name: "Moss",
        stops: Some(&[
            (0.03, 0.11, 0.09), (0.06, 0.27, 0.20), (0.09, 0.45, 0.31),
            (0.28, 0.61, 0.28), (0.58, 0.77, 0.31),
        ]),
        ground: (0.025, 0.085, 0.070), accent: (0.48, 0.86, 0.46), light: false,
    },
    Palette {
        id: "graphite", name: "Graphite",
        stops: Some(&[
            (0.06, 0.07, 0.11), (0.15, 0.18, 0.25), (0.30, 0.31, 0.35),
            (0.45, 0.42, 0.38), (0.60, 0.56, 0.49),
        ]),
        ground: (0.055, 0.055, 0.070), accent: (0.78, 0.80, 0.86), light: false,
    },
    // --- pale fields (dark ink) ---
    Palette {
        id: "holo", name: "Holo",
        stops: Some(&[
            (0.99, 0.72, 0.90), (0.80, 0.60, 0.98), (0.58, 0.62, 0.99),
            (0.55, 0.86, 0.98), (0.94, 0.98, 1.00),
        ]),
        ground: (0.96, 0.92, 0.99), accent: (0.42, 0.28, 0.86), light: true,
    },
    Palette {
        id: "sunset", name: "Sunset",
        stops: Some(&[
            (0.55, 0.45, 0.92), (0.86, 0.31, 0.66), (0.97, 0.26, 0.34),
            (0.99, 0.51, 0.18), (1.00, 0.80, 0.22),
        ]),
        ground: (0.98, 0.74, 0.34), accent: (0.64, 0.13, 0.44), light: true,
    },
    Palette {
        id: "bloom", name: "Bloom",
        stops: Some(&[
            (1.00, 0.86, 0.72), (0.99, 0.73, 0.79), (0.95, 0.65, 0.89),
            (0.82, 0.68, 0.96), (0.73, 0.79, 0.99),
        ]),
        ground: (0.99, 0.90, 0.89), accent: (0.72, 0.24, 0.55), light: true,
    },
    Palette {
        id: "dawn", name: "Dawn",
        stops: Some(&[
            (1.00, 0.92, 0.70), (1.00, 0.80, 0.62), (0.99, 0.66, 0.62),
            (0.90, 0.62, 0.78), (0.77, 0.69, 0.95),
        ]),
        ground: (1.00, 0.93, 0.82), accent: (0.82, 0.33, 0.28), light: true,
    },
    Palette {
        id: "mint", name: "Mint",
        stops: Some(&[
            (0.82, 0.98, 0.90), (0.62, 0.94, 0.88), (0.55, 0.88, 0.95),
            (0.63, 0.82, 0.99), (0.82, 0.87, 1.00),
        ]),
        ground: (0.90, 0.98, 0.96), accent: (0.04, 0.42, 0.40), light: true,
    },
    Palette {
        id: "opal", name: "Opal",
        stops: Some(&[
            (0.98, 0.92, 0.96), (0.87, 0.93, 0.99), (0.91, 0.99, 0.95),
            (0.99, 0.96, 0.88), (0.94, 0.90, 0.99),
        ]),
        ground: (0.97, 0.96, 0.99), accent: (0.36, 0.32, 0.44), light: true,
    },
];

/// Palette for `id`, or the brand default. Unknown is a newer client's name, not an empty draw.
pub fn palette(id: &str) -> &'static Palette {
    PALETTES.iter().find(|p| p.id == id).unwrap_or(&PALETTES[0])
}

/// Sample a ramp at `t` ∈ [0, 1], linear between neighbouring stops. Swift and Kotlin copies must match.
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
    pub fn mesh_colors(&self) -> [(f64, f64, f64); 16] {
        let Some(stops) = self.stops else {
            return MESH_COLORS;
        };
        mesh_colors_of(stops)
    }

    /// Four blob colours for Android's mesh approximation, spread across the ramp.
    pub fn blob_colors(&self) -> [(f64, f64, f64); 4] {
        let stops = self.stops.unwrap_or(&VIOLET_BLOBS);
        core::array::from_fn(|i| ramp(stops, 0.15 + 0.25 * i as f64))
    }
}

/// 16 mesh cells from any 5-stop ramp, including the runtime OS field (`shell::build_mesh_os`).
pub(crate) fn mesh_colors_of(stops: &[(f64, f64, f64)]) -> [(f64, f64, f64); 16] {
    core::array::from_fn(|i| {
        let (x, y) = ((i % 4) as f64 / 3.0, (i / 4) as f64 / 3.0);
        ramp(stops, 0.5 * (x + y) + CELL_RAMP[i])
    })
}

/// Pre-palette Android / legacy-Apple blob colours, so `violet` stays bit-identical there.
const VIOLET_BLOBS: [(f64, f64, f64); 5] = [
    (0.53, 0.47, 0.96),
    (0.24, 0.20, 0.72),
    (0.62, 0.30, 0.80),
    (0.22, 0.38, 0.86),
    (0.53, 0.47, 0.96),
];

/// Mesh gradient as SkSL: palette and motion baked in; resolution, time and calm are uniforms.
///
/// Bicubic 16-colour blend (SwiftUI `MeshGradient(smoothsColors: true)`). Interior points
/// drive a bounded domain warp; then ±8° / ~5 min hue sway, elliptical vignette, vertical scrim
/// — Swift `composite(at:)`.
///
/// `u_tc.y` is the calm mix (0 launcher, 1 form): flatten toward `u_lift` so a screen
/// crossfade never jumps the field. Motion speed is unchanged.
pub fn mesh_sksl(colors: &[(f64, f64, f64); 16]) -> String {
    let c = |i: usize| {
        let (r, g, b) = colors[i];
        format!("float3({r}, {g}, {b})")
    };
    // Interior domain-warp, matching Swift `wob()`: x = sin(t·sx+ph), y = cos(t·sy+ph·1.3).
    // Weight-normalised average, so |warp| ≤ max|amp|.
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

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LibraryPhase {
    Loading,
    Error {
        title: String,
        body: String,
        can_retry: bool,
    },
    Empty,
    Ready,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LibraryGame {
    pub id: String,
    pub title: String,
    pub store: String,
    /// Opens the launcher itself, not a title. Host `role`, reduced by [`pf_client_core::library::GameEntry::is_launcher`].
    pub launcher: bool,
    /// Brand mark token, already validated by [`pf_client_core::library::GameEntry::icon_token`]. Empty or unknown draws nothing.
    pub icon: String,
    /// Host free-form display string (`"PC"`, `"PS2"`, …). `None` until [`crate::collate`] assigns a bucket.
    pub platform: Option<String>,
    /// Already up on the host — pick resumes. From `/api/v1/status` via [`LibraryShared::set_running`].
    ///
    /// Host state, not catalog state: not on `GameEntry`, not persisted. A disk shelf cannot
    /// claim a title is running because it was last time. `false` on older hosts and while
    /// `/status` is in flight; the badge may appear a frame late rather than hold the catalog.
    pub running: bool,
}

/// Observation vs memory, and whether a memory is still being fetched.
///
/// Three states because Waking and Offline need different shelf copy. A boolean would
/// say "waking" while nothing is happening.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Stale {
    No,
    /// Served from the disk cache while the host is being woken and re-asked.
    Waking,
    /// Disk cache; the host never answered. Not an error: these are still the titles to pick from.
    Offline,
}

impl Stale {
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
    /// Disk cache vs live host. Live [`LibraryShared::set_games`] resets this to [`Stale::No`].
    stale: Stale,
    /// Fetched poster bytes the renderer hasn't decoded yet (id, encoded image).
    art_in: VecDeque<(String, Vec<u8>)>,
    /// Bumped on phase/games changes so the renderer re-syncs its snapshot.
    generation: u64,
    /// Bumped once per fetch, by [`LibraryShared::begin_fetch`].
    ///
    /// Distinguishes "this shelf's list" from "the previous host's, still here" without
    /// catching `Loading`. A warm cache publishes `Ready` inside one 60 Hz frame, so a
    /// phase edge can be missed; a counter cannot.
    fetch_epoch: u64,
}

pub(crate) struct LibrarySnapshot {
    pub phase: LibraryPhase,
    pub games: Vec<LibraryGame>,
    pub stale: Stale,
    pub generation: u64,
}

/// Binary write handle / overlay read handle. Fetch threads push; the renderer drains per frame.
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
    /// A fetch is starting: `Loading`, and the epoch advances.
    ///
    /// Must go through here, not `set_phase(Loading)`. A cache that answers before the next
    /// frame would otherwise leave the epoch unchanged and the shelf on the previous host.
    pub fn begin_fetch(&self) {
        let mut s = self.0.lock().unwrap();
        s.phase = LibraryPhase::Loading;
        // Previous host's stale note is not this fetch's; a cached render re-declares it.
        s.stale = Stale::No;
        s.fetch_epoch += 1;
        s.generation += 1;
    }

    /// Fetch the model is on. A shelf records this at push; a later difference means a new fetch owns the list.
    pub(crate) fn fetch_epoch(&self) -> u64 {
        self.0.lock().unwrap().fetch_epoch
    }

    pub fn set_phase(&self, phase: LibraryPhase) {
        let mut s = self.0.lock().unwrap();
        s.phase = phase;
        s.generation += 1;
    }

    /// Host titles → carousel (empty = empty scene). Clears cached staleness.
    ///
    /// Launchers move to the front, host title order kept within each group. Grouping here
    /// so cursor math, the art pump, and [`GridShape`] all see the same prefix.
    pub fn set_games(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::No);
    }

    /// Disk-cache catalog while the host is still being asked. Live fetch stays in flight.
    pub fn set_games_cached(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::Waking);
    }

    /// Shelf copy about a cached catalog, catalog unchanged. No-op on a live shelf, so a late
    /// abandoned-fetch give-up cannot mark a fresh library stale.
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

    /// Titles currently up, by library id; re-order. `/status` is read after the catalog.
    /// Empty set clears every badge (older or unreachable host).
    ///
    /// No-op — no generation bump — when nothing changed. This is polled.
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

    /// At most `max` newly fetched posters; the rest stay queued.
    ///
    /// Bounded: the renderer decodes on the render thread. Encoded bytes left behind are
    /// two orders smaller than the rasters they become.
    pub(crate) fn drain_art(&self, max: usize) -> Vec<(String, Vec<u8>)> {
        let mut s = self.0.lock().unwrap();
        let n = max.min(s.art_in.len());
        s.art_in.drain(..n).collect()
    }

    /// At most `max` queued posters in `want`; every other entry stays.
    ///
    /// Bytes are pushed once per fetch and never re-sent. A wholesale drain on collections
    /// would drop covers the shelf still needs.
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

/// Shelf display order, the one path every writer uses.
///
/// Launchers first (the [`GridShape`] prefix). Running titles lead within their group.
/// `sort_by_key` is stable, so host title order survives inside each band.
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

    /// Parity with `clients/shared/console-vectors.json` (`include_str!`: missing file fails compile).
    ///
    /// Three copies: here, `GamepadPalette.kt`, `GamepadPalette.swift`. The file pins the
    /// 13 palettes and the derived 16-cell mesh each one produces.
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

    /// Poster bytes are pushed once per fetch and never re-sent; anything taken and not drawn is gone.
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

    #[test]
    fn step_refuses_the_ends() {
        assert_eq!(step_cursor(0, 5, -1, false), StepResult::Boundary);
        assert_eq!(step_cursor(4, 5, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(2, 5, 1, false), StepResult::Moved(3));
        assert_eq!(step_cursor(0, 0, 1, false), StepResult::Boundary);
    }

    /// Launcher-less, prefix, partial launcher row, degenerate two-cell, and single-column fields.
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

    /// Rows tile `0..len` once: `cell_of` and `row_start` agree; no index in two rows or in none.
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

    /// Horizontal step refuses at the row's true end, not at `cols` (partial rows, launcher prefix).
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
                // Hint is the column you would return to, not the one a horizontal step walks out of.
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
                            // Field edges refuse. A page already at the edge travels the current row.
                            StepResult::Boundary => assert_eq!(want_row, row, "{what} refused"),
                        }
                    }
                }
            }
        }
    }

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

    /// Two launchers on seven columns: alone on row 0, games restart at column 0.
    #[test]
    fn the_launcher_row_sits_squarely_above_the_games() {
        let s = GridShape::new(30, 7, 2);
        assert_eq!(s.rows(), 5);
        assert_eq!((s.row_len(0), s.row_len(1)), (2, 7));
        // Down from a launcher lands on the cover under it, not five columns right.
        assert_eq!(grid_step(0, s, 0, GridDir::Down), StepResult::Moved(2));
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(3));
        // Up out of the games band leaves it, rather than sliding along it.
        assert_eq!(grid_step(2, s, 0, GridDir::Up), StepResult::Moved(0));
        assert_eq!(grid_step(3, s, 1, GridDir::Up), StepResult::Moved(1));
        assert_eq!(grid_step(6, s, 4, GridDir::Up), StepResult::Moved(1));
        // Games row true ends are 2 and 8 — 6 and 7 are mid-row.
        assert_eq!(grid_step(6, s, 4, GridDir::Right), StepResult::Moved(7));
        assert_eq!(grid_step(7, s, 5, GridDir::Left), StepResult::Moved(6));
        assert_eq!(grid_step(8, s, 6, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 0, GridDir::Left), StepResult::Boundary);
    }

    #[test]
    fn a_crossing_returns_to_the_column_it_started_from() {
        use GridDir::{Down, Right, Up};
        let s = GridShape::new(30, 7, 2);
        // Mirrors `LibraryScreen::grid_move`: step, then `grid_col_hint`.
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
        assert_eq!(walk(0, &[Down, Right, Right, Right, Right, Up]), 1);
        // A vertical move never spends the hint.
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
        // Page past the end lands on the end (clamped `step_cursor`), it does not refuse.
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

    #[test]
    fn grid_rows_refuse_at_their_ends_but_the_tail_row_clamps() {
        // 11 items, 4 columns: rows of 4, 4, 3.
        let s = GridShape::new(11, 4, 0);
        assert_eq!(grid_step(1, s, 1, GridDir::Right), StepResult::Moved(2));
        assert_eq!(grid_step(2, s, 2, GridDir::Left), StepResult::Moved(1));
        // At a row's ends: refused, not wrapped onto the neighbouring row.
        assert_eq!(grid_step(3, s, 3, GridDir::Right), StepResult::Boundary);
        assert_eq!(grid_step(4, s, 0, GridDir::Left), StepResult::Boundary);
        assert_eq!(grid_step(1, s, 1, GridDir::Down), StepResult::Moved(5));
        // Down into the short tail row clamps to the last item — column 3 does not exist there.
        assert_eq!(grid_step(7, s, 3, GridDir::Down), StepResult::Moved(10));
        assert_eq!(grid_step(10, s, 3, GridDir::Down), StepResult::Boundary);
        assert_eq!(grid_step(2, s, 2, GridDir::Up), StepResult::Boundary);
        assert_eq!(grid_step(6, s, 2, GridDir::Up), StepResult::Moved(2));
    }

    /// Persisted view name is a file format. Unknown → shelf.
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
        // A cursor the library outgrew reads as the nearest real cell; the next press heals it.
        let s = GridShape::new(6, 3, 2);
        assert_eq!(grid_step(99, s, 0, GridDir::Up), StepResult::Moved(2));
        assert_eq!(grid_step(-4, s, 0, GridDir::Right), StepResult::Moved(1));
    }

    /// Launchers lead; host title order survives within each group. `launcher_count()` is prefix `0..n`.
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

    /// Running titles lead within their group; the launcher prefix [`GridShape`] counts stays intact.
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

        // Same set: no generation bump. This is polled.
        let gen_before = snap.generation;
        shared.set_running(&up);
        assert_eq!(shared.snapshot().generation, gen_before);

        shared.set_running(&std::collections::HashSet::new());
        let after = shared.snapshot();
        assert!(after.games.iter().all(|g| !g.running));
        assert!(after.generation > gen_before, "a real change does re-sync");
    }

    /// Cached catalog is `Ready` + stale, never an error. Live `set_games` clears the flag.
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

    /// Stay finite through a stalled frame (0.05 s).
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

    /// Focused card (angle 0, scale 1) maps its centre to (cx, cy) exactly.
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

    /// Right-side card: inner (left) edge recedes; projected x compresses toward the centre.
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

    /// Generated SkSL: 16 colours baked, five bicubic evals, four interior warp terms, braces balanced.
    #[test]
    fn mesh_sksl_shape() {
        let src = mesh_sksl(&MESH_COLORS);
        assert!(src.matches("float3(").count() >= 16, "16 colours baked");
        assert_eq!(src.matches("bz3(").count(), 6); // 1 definition + 5 call sites
        assert_eq!(src.matches("wtot +=").count(), 4); // one per interior point
        assert_eq!(src.matches('{').count(), src.matches('}').count());
    }

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

    /// A palette is several hues, measured as the widest gap among the 16 mesh colours' hue angles.
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
            // Graphite and Opal are deliberately near-neutral.
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

    /// `oled` is genuinely black: pure-black corners, mean under every other field, ground lifts to nothing.
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
            // Accent tints glass of the opposite polarity: dark on white frost, bright on dark glass.
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
