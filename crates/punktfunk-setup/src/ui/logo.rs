//! The lens mark, computed rather than embedded.
//!
//! `web/public/favicon.svg` is pure geometry — two equal circles whose intersection is the
//! lens — so the mark is a point-in-circle test per pixel over the union's bounding box. No
//! bitmap, no build-time raster step, and it scales by changing one constant.
//!
//! Rendering is the half-block idiom: `▀` with foreground = the top pixel and background = the
//! bottom one gives two square pixels per cell, so `MARK_PX` square is that many columns by
//! half as many rows. Hard edges, no anti-aliasing: the pixelated look is the point, and the
//! terminal background is unknown, so there is nothing safe to blend toward — which is also
//! why an uncovered cell must actively put the background back rather than leave it inherited.
//!
//! The animation slides the two circles together along their diagonal and the lens lights up
//! as they meet — the brand story drawn. `design/installer-v2.md` D7 owns the skip rules.

use crate::ui::theme::{Caps, Colors, Layer, Rgb};

/// The mark is square, and must be even: two pixel rows share a cell. 20 px is 10 text rows —
/// big enough to read as the lens, small enough not to own the screen above a prompt.
pub const MARK_PX: usize = 20;
pub const MARK_COLS: u16 = MARK_PX as u16;

const LIGHT: Rgb = Rgb(0xa7, 0x9f, 0xf8);
const DEEP: Rgb = Rgb(0x6c, 0x5b, 0xf3);
const LENS: Rgb = Rgb(0xd2, 0xc9, 0xfb);

/// Circle centres and radius in the favicon's 1000×1000 viewBox, read off its two arcs.
const R: f32 = 194.41;
const LIGHT_C: (f32, f32) = (403.037, 597.262);
const DEEP_C: (f32, f32) = (597.808, 402.853);

/// How far apart the circles start, in viewBox units. About one radius, so at t=0 each is
/// mostly outside the frame and the mark reads as two endpoints arriving.
const SPREAD: f32 = 190.0;

/// 15 frames at 40 ms is ~600 ms — long enough to read as motion, short enough that nobody
/// waiting to install a package resents it.
pub const FRAMES: usize = 15;
pub const FRAME_MS: u64 = 40;

/// How much of the intro the terminal has earned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intro {
    Animated,
    Static,
    Plain,
}

/// D7's skip ladder. `--yes` keeps the mark but never animates: an unattended run has nobody
/// watching, and its transcript should not carry 15 repaints.
pub fn intro_level(caps: &Caps, yes: bool) -> Intro {
    if !caps.tty || caps.colors < Colors::Ansi256 || caps.width < MARK_COLS {
        return Intro::Plain;
    }
    if yes {
        Intro::Static
    } else {
        Intro::Animated
    }
}

/// The union of both circles at rest, which every frame is drawn inside.
fn bbox() -> (f32, f32, f32, f32) {
    let x0 = (LIGHT_C.0 - R).min(DEEP_C.0 - R);
    let y0 = (LIGHT_C.1 - R).min(DEEP_C.1 - R);
    let x1 = (LIGHT_C.0 + R).max(DEEP_C.0 + R);
    let y1 = (LIGHT_C.1 + R).max(DEEP_C.1 + R);
    (x0, y0, x1 - x0, y1 - y0)
}

/// Unit vector from the light circle toward the deep one — the diagonal they travel.
fn diagonal() -> (f32, f32) {
    let (dx, dy) = (DEEP_C.0 - LIGHT_C.0, DEEP_C.1 - LIGHT_C.1);
    let len = dx.hypot(dy);
    (dx / len, dy / len)
}

/// The pixel grid at `t`, where 0.0 is fully apart and 1.0 is the mark at rest.
///
/// `None` is a pixel the mark does not cover. The draw order is the SVG's: the deep circle
/// sits over the light one, and the lens over both.
pub fn frame(t: f32) -> Vec<Vec<Option<Rgb>>> {
    // Ease-out: most of the travel happens early, so the circles settle rather than arrive.
    let eased = 1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3);
    let gap = SPREAD * (1.0 - eased);
    let (dx, dy) = diagonal();
    let light = (LIGHT_C.0 - dx * gap, LIGHT_C.1 - dy * gap);
    let deep = (DEEP_C.0 + dx * gap, DEEP_C.1 + dy * gap);

    let (x0, y0, w, h) = bbox();
    let inside = |c: (f32, f32), x: f32, y: f32| (x - c.0).hypot(y - c.1) <= R;
    (0..MARK_PX)
        .map(|row| {
            let y = y0 + (row as f32 + 0.5) * h / MARK_PX as f32;
            (0..MARK_PX)
                .map(|col| {
                    let x = x0 + (col as f32 + 0.5) * w / MARK_PX as f32;
                    match (inside(light, x, y), inside(deep, x, y)) {
                        (true, true) => Some(LENS),
                        (_, true) => Some(DEEP),
                        (true, _) => Some(LIGHT),
                        _ => None,
                    }
                })
                .collect()
        })
        .collect()
}

/// One frame as text: two pixel rows per line, `indent` columns in.
pub fn render(grid: &[Vec<Option<Rgb>>], caps: &Caps, indent: usize) -> String {
    let mut out = String::new();
    for pair in grid.chunks(2) {
        out.push_str(&" ".repeat(indent));
        let (top, bottom) = (&pair[0], pair.get(1));
        for col in 0..top.len() {
            let up = top[col];
            let down = bottom.and_then(|row| row[col]);
            match (up, down) {
                // A space still paints the background it inherits, so an uncovered cell has to
                // put it back to default. Skipping that smeared the previous cell's colour
                // across the rest of the line.
                (None, None) => {
                    out.push_str(&caps.clear(Layer::Bg));
                    out.push(' ');
                }
                // A half-covered cell paints one layer and leaves the other alone, so the
                // mark sits on the user's own background instead of a violet rectangle.
                (Some(c), None) => {
                    out.push_str(&caps.paint(c, Layer::Fg));
                    out.push_str(&caps.clear(Layer::Bg));
                    out.push('▀');
                }
                (None, Some(c)) => {
                    out.push_str(&caps.paint(c, Layer::Fg));
                    out.push_str(&caps.clear(Layer::Bg));
                    out.push('▄');
                }
                (Some(a), Some(b)) => {
                    out.push_str(&caps.paint(a, Layer::Fg));
                    out.push_str(&caps.paint(b, Layer::Bg));
                    out.push('▀');
                }
            }
        }
        out.push_str(&caps.reset());
        out.push('\n');
    }
    out
}

/// The mark as it settles, for a caller that will not animate.
pub fn still(caps: &Caps, indent: usize) -> String {
    render(&frame(1.0), caps, indent)
}

/// The grid as one character per pixel, for tests and for eyeballing the geometry.
pub fn ascii(grid: &[Vec<Option<Rgb>>]) -> String {
    let mut out = String::new();
    for row in grid {
        for cell in row {
            out.push(match *cell {
                None => '.',
                Some(LENS) => '#',
                Some(DEEP) => 'o',
                _ => '+',
            });
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::Env;

    fn caps(colors: Colors, width: u16, tty: bool) -> Caps {
        Caps { tty, colors, width }
    }

    #[test]
    fn the_skip_ladder_matches_d7() {
        let good = caps(Colors::Truecolor, 80, true);
        assert_eq!(intro_level(&good, false), Intro::Animated);
        assert_eq!(
            intro_level(&good, true),
            Intro::Static,
            "--yes keeps the mark, not the motion"
        );
        assert_eq!(
            intro_level(&caps(Colors::Truecolor, 80, false), false),
            Intro::Plain
        );
        assert_eq!(
            intro_level(&caps(Colors::Ansi16, 80, true), false),
            Intro::Plain
        );
        assert_eq!(
            intro_level(&caps(Colors::None, 80, true), false),
            Intro::Plain
        );
        assert_eq!(
            intro_level(&caps(Colors::Truecolor, 12, true), false),
            Intro::Plain
        );
    }

    #[test]
    fn a_no_color_terminal_never_reaches_the_mark() {
        let env = Env::of(&[("NO_COLOR", "1"), ("TERM", "xterm-256color")]);
        let caps = Caps::detect(&env, true, 120);
        assert_eq!(intro_level(&caps, false), Intro::Plain);
    }

    /// The settled mark is deterministic geometry, so it is worth pinning exactly.
    #[test]
    fn the_settled_mark_is_the_expected_pixel_grid() {
        let art = ascii(&frame(1.0));
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/mark.txt");
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::write(&path, &art).expect("write mark golden");
            return;
        }
        let want = std::fs::read_to_string(&path).expect("mark golden — UPDATE_GOLDEN=1");
        assert_eq!(art, want);
    }

    #[test]
    fn the_mark_is_square_and_the_grid_is_the_declared_size() {
        let grid = frame(1.0);
        assert_eq!(grid.len(), MARK_PX);
        assert!(grid.iter().all(|row| row.len() == MARK_PX));
    }

    /// The lens only exists where the circles overlap, so it must grow as they arrive.
    #[test]
    fn the_lens_lights_up_as_the_circles_meet() {
        let lens_pixels = |t: f32| {
            frame(t)
                .iter()
                .flatten()
                .filter(|c| **c == Some(LENS))
                .count()
        };
        assert_eq!(
            lens_pixels(0.0),
            0,
            "fully apart, there is no overlap to light"
        );
        assert!(lens_pixels(1.0) > 0, "at rest the lens is the whole point");
        assert!(lens_pixels(0.6) < lens_pixels(1.0));
    }

    /// Half-covered cells must not paint a background, or the mark sits in a violet box.
    #[test]
    fn an_edge_cell_leaves_the_terminal_background_alone() {
        let caps = caps(Colors::Truecolor, 80, true);
        let text = render(&frame(1.0), &caps, 2);
        assert!(
            text.contains("\x1b[49m"),
            "no cell returned the background to default"
        );
        assert!(
            text.lines().all(|l| l.starts_with("  ")),
            "the indent is not applied"
        );
    }

    /// A space paints whatever background is in effect. Emitting one without clearing it first
    /// smeared the previous cell's colour across the rest of the line — which the goldens, being
    /// colourless, could not see, and a real terminal showed immediately.
    #[test]
    fn no_blank_cell_is_painted_with_a_leftover_background() {
        let caps = caps(Colors::Truecolor, 80, true);
        for t in [0.0, 0.3, 0.7, 1.0] {
            let text = render(&frame(t), &caps, 2);
            let mut bg = false;
            let mut chars = text.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    let mut seq = String::new();
                    for c in chars.by_ref() {
                        seq.push(c);
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    if seq.starts_with("[48;") {
                        bg = true;
                    } else if seq == "[49m" || seq == "[0m" {
                        bg = false;
                    }
                } else {
                    assert!(
                        !(c == ' ' && bg),
                        "t={t}: a blank cell carried a background"
                    );
                }
            }
        }
    }

    #[test]
    fn every_two_pixel_rows_share_one_text_row() {
        let caps = caps(Colors::Truecolor, 80, true);
        assert_eq!(
            MARK_PX % 2,
            0,
            "an odd mark would leave a half-filled last row"
        );
        assert_eq!(still(&caps, 0).lines().count(), MARK_PX / 2);
    }
}
