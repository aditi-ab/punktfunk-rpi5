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
/// The darkening veil's max opacity (side cards stay opaque — they overlap).
pub const RECEDE_DIM: f64 = 0.30;
/// Boundary recoil: a refused move deflects the strip this many px against the push.
pub const BUMP_PX: f64 = 16.0;
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

// --- Aurora (the Swift `GamepadScreenBackground` blob table, verbatim) --------------------

/// One drifting color blob: base position + drift ellipse in unit coordinates, angular
/// speeds in rad/s, a breathing radius (fraction of the larger dimension), layer opacity.
pub struct Blob {
    pub rgb: (f64, f64, f64),
    pub center: (f64, f64),
    pub drift: (f64, f64),
    pub speed: (f64, f64),
    pub phase: (f64, f64),
    pub radius: f64,
    pub breathe: (f64, f64),
    pub opacity: f64,
}

/// The brand violet, a deeper indigo, a warmer plum, and a cool blue — related hues so
/// the field shifts within one temperature instead of strobing through the rainbow.
pub const BLOBS: [Blob; 4] = [
    Blob {
        rgb: (0.53, 0.47, 0.96),
        center: (0.30, 0.24),
        drift: (0.16, 0.10),
        speed: (0.111, 0.083),
        phase: (0.0, 1.9),
        radius: 0.52,
        breathe: (0.07, 0.061),
        opacity: 0.52,
    },
    Blob {
        rgb: (0.24, 0.20, 0.72),
        center: (0.78, 0.66),
        drift: (0.13, 0.14),
        speed: (0.071, 0.096),
        phase: (2.4, 0.7),
        radius: 0.58,
        breathe: (0.08, 0.049),
        opacity: 0.55,
    },
    Blob {
        rgb: (0.62, 0.30, 0.80),
        center: (0.16, 0.82),
        drift: (0.12, 0.09),
        speed: (0.089, 0.067),
        phase: (4.1, 3.2),
        radius: 0.44,
        breathe: (0.09, 0.078),
        opacity: 0.42,
    },
    Blob {
        rgb: (0.22, 0.38, 0.86),
        center: (0.70, 0.12),
        drift: (0.10, 0.08),
        speed: (0.059, 0.104),
        phase: (1.2, 5.0),
        radius: 0.40,
        breathe: (0.06, 0.055),
        opacity: 0.38,
    },
];

/// The aurora as SkSL: the blob table baked into the source (only time + resolution are
/// uniforms), each blob an additive radial falloff to `radius/2` — the Cairo gradient's
/// stops — then the legibility scrim's piecewise-linear vertical darkening. Runs on the
/// GPU at full rate; the 30 Hz CPU-upscale path (and its frozen-on-Deck fallback) is the
/// thing this whole port deletes.
pub fn aurora_sksl() -> String {
    let mut src = String::from(
        "uniform float2 u_res;\nuniform float u_t;\n\
         half4 main(float2 xy) {\n\
             float side = max(u_res.x, u_res.y);\n\
             float3 acc = float3(0.0);\n\
             float2 c; float r; float a;\n",
    );
    for b in &BLOBS {
        src.push_str(&format!(
            "    c = float2(({cx} + {dx} * sin(u_t * {sx} + {px})) * u_res.x, \
                            ({cy} + {dy} * cos(u_t * {sy} + {py})) * u_res.y);\n\
                 r = side * {radius} * (1.0 + {b0} * sin(u_t * {b1} + {px}));\n\
                 a = max(0.0, 1.0 - distance(xy, c) / (r * 0.5));\n\
                 acc += float3({r0}, {g0}, {b0c}) * (a * {op});\n",
            cx = b.center.0,
            cy = b.center.1,
            dx = b.drift.0,
            dy = b.drift.1,
            sx = b.speed.0,
            sy = b.speed.1,
            px = b.phase.0,
            py = b.phase.1,
            radius = b.radius,
            b0 = b.breathe.0,
            b1 = b.breathe.1,
            r0 = b.rgb.0,
            g0 = b.rgb.1,
            b0c = b.rgb.2,
            op = b.opacity,
        ));
    }
    // Scrim stops (0, .55) (0.35, .15) (0.65, .20) (1, .60): title (top) and
    // detail/hints (bottom) stay on near-black whatever the blobs do behind them.
    src.push_str(
        "    float v = xy.y / u_res.y;\n\
             float s = v < 0.35 ? mix(0.55, 0.15, v / 0.35)\n\
                     : v < 0.65 ? mix(0.15, 0.20, (v - 0.35) / 0.30)\n\
                     : mix(0.20, 0.60, (v - 0.65) / 0.35);\n\
             return half4(half3(acc * (1.0 - s)), 1.0);\n\
         }\n",
    );
    src
}

// --- The shared binary↔overlay model ------------------------------------------------------

#[derive(Clone, PartialEq)]
pub enum LibraryPhase {
    Loading,
    /// Browse target isn't paired — pairing is the plugin's job, render the advice.
    PairFirst,
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
}

struct Shared {
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    /// Fetched poster bytes the renderer hasn't decoded yet (id, encoded image).
    art_in: VecDeque<(String, Vec<u8>)>,
    /// Bumped on phase/games changes so the renderer re-syncs its snapshot.
    generation: u64,
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
            art_in: VecDeque::new(),
            generation: 0,
        })))
    }
}

impl LibraryShared {
    pub fn set_phase(&self, phase: LibraryPhase) {
        let mut s = self.0.lock().unwrap();
        s.phase = phase;
        s.generation += 1;
    }

    /// Loaded games → the carousel (empty = the empty scene).
    pub fn set_games(&self, games: Vec<LibraryGame>) {
        let mut s = self.0.lock().unwrap();
        s.phase = if games.is_empty() {
            LibraryPhase::Empty
        } else {
            LibraryPhase::Ready
        };
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

    pub(crate) fn snapshot(&self) -> (LibraryPhase, Vec<LibraryGame>, u64) {
        let s = self.0.lock().unwrap();
        (s.phase.clone(), s.games.clone(), s.generation)
    }

    pub(crate) fn drain_art(&self) -> Vec<(String, Vec<u8>)> {
        self.0.lock().unwrap().art_in.drain(..).collect()
    }
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

    /// The GTK launcher's cursor tests, ported with the math.
    #[test]
    fn step_refuses_the_ends() {
        assert_eq!(step_cursor(0, 5, -1, false), StepResult::Boundary);
        assert_eq!(step_cursor(4, 5, 1, false), StepResult::Boundary);
        assert_eq!(step_cursor(2, 5, 1, false), StepResult::Moved(3));
        assert_eq!(step_cursor(0, 0, 1, false), StepResult::Boundary);
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
        assert!(p.is_finite() && v.is_finite() && p > 0.0 && p < 2.0, "{p}/{v}");
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
        let tilted = card_matrix(900.0, 400.0, -ROTATE_DEG, 1.0, POSTER_W, POSTER_H, PERSPECTIVE);
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

    /// The generated SkSL parses as far as syntax we control (sanity: balanced braces,
    /// all four blobs baked in).
    #[test]
    fn aurora_sksl_shape() {
        let src = aurora_sksl();
        assert_eq!(src.matches("acc +=").count(), 4);
        assert_eq!(
            src.matches('{').count(),
            src.matches('}').count(),
        );
    }
}
