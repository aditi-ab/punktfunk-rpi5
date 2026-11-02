//! The GDI cursor poller's pure half: a cursor's AND/XOR planes to straight-alpha RGBA, and a
//! rasterised shape to the overlay one poll publishes. Pure over `&[u8]`, so its tests run on
//! every target; `idd/cursor_poll.rs` owns the GDI reads.

// Off Windows only the tests read this module.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use pf_driver_proto::cursor::INVERT_RGBA;

/// How the OS draws a cursor. Monochrome cursors are masked on both sources: the OS converts
/// them for the driver's section, and the poller's truth table does the same.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ShapeClass {
    /// Straight alpha.
    Alpha,
    /// A mask with invert pixels.
    Masked,
}

/// `rgba` is `Arc` so slot publish and every downstream attach is a refcount bump.
pub(crate) struct Shape {
    pub(crate) rgba: std::sync::Arc<Vec<u8>>,
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) hot_x: u32,
    pub(crate) hot_y: u32,
    pub(crate) serial: u64,
    pub(crate) class: ShapeClass,
}

/// One poll's overlay. `pos` is the pointer relative to `rect`'s origin, and outside `rect`
/// the pointer is `visible: false`; `shown` is a drawn pointer with a cursor handle. A pointer
/// hidden before any shape was seen (a game that hid it before this session) is still a hide
/// the client must hear: an empty, invisible overlay.
pub(crate) fn compose_overlay(
    pos: (i32, i32),
    rect: (i32, i32, i32, i32),
    shown: bool,
    shape: Option<&Shape>,
) -> Option<pf_frame::CursorOverlay> {
    let (px, py) = pos;
    let Some(s) = shape else {
        return (!shown).then(|| pf_frame::CursorOverlay {
            x: px,
            y: py,
            w: 0,
            h: 0,
            rgba: std::sync::Arc::new(Vec::new()),
            serial: 0,
            hot_x: 0,
            hot_y: 0,
            visible: false,
        });
    };
    let in_rect = px >= 0 && py >= 0 && px < rect.2 && py < rect.3;
    Some(pf_frame::CursorOverlay {
        // Overlay x/y = bitmap top-left (reported position − hotspot), frame pixels.
        x: px - s.hot_x as i32,
        y: py - s.hot_y as i32,
        w: s.w,
        h: s.h,
        rgba: s.rgba.clone(),
        serial: s.serial,
        hot_x: s.hot_x,
        hot_y: s.hot_y,
        visible: shown && in_rect,
    })
}

/// The overlay the client draws: the poller's position and visibility carrying the shape
/// the OS rendered for this display, read from the driver's section. `GetIconInfo` hands
/// the poller the 96-DPI bitmap whatever the display's scale; the section holds the one
/// Windows draws. A masked cursor the section did not receive leaves it on the last alpha
/// shape, so a class mismatch keeps the poller's own.
pub(crate) fn with_section_shape(
    polled: pf_frame::CursorOverlay,
    polled_class: Option<ShapeClass>,
    section: Option<(pf_frame::CursorOverlay, ShapeClass)>,
) -> pf_frame::CursorOverlay {
    let Some((sec, class)) = section else {
        return polled;
    };
    if polled.w == 0 || sec.w == 0 || polled_class != Some(class) {
        return polled;
    }
    let (px, py) = (
        polled.x + polled.hot_x as i32,
        polled.y + polled.hot_y as i32,
    );
    pf_frame::CursorOverlay {
        x: px - sec.hot_x as i32,
        y: py - sec.hot_y as i32,
        w: sec.w,
        h: sec.h,
        rgba: sec.rgba,
        serial: sec.serial,
        hot_x: sec.hot_x,
        hot_y: sec.hot_y,
        visible: polled.visible,
    }
}

/// Alpha channel entirely zero: old-style cursor whose transparency (and invert)
/// live in the AND mask ([`masked_color_to_rgba`]).
pub(crate) fn alpha_is_empty(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|p| p[3] == 0)
}

/// Test-only AND-as-alpha (no invert). `mask_bgra` is GetDIBits' 32bpp expansion
/// of the 1bpp mask, so any non-zero channel is "set"; white = transparent.
/// The GDI `convert` uses [`masked_color_to_rgba`]: AND=1 plus colour is invert (I-beam).
#[cfg(test)]
fn apply_and_mask_alpha(rgba: &mut [u8], mask_bgra: &[u8]) {
    for (px, m) in rgba.chunks_exact_mut(4).zip(mask_bgra.chunks_exact(4)) {
        px[3] = if m[0] != 0 { 0 } else { 0xFF };
    }
}

/// Alpha-less colour cursor: AND plus colour-as-XOR, same four states as
/// [`mono_planes_to_rgba`]. Non-zero RGB with AND=1 is invert — treating AND=1
/// as always-transparent drops the I-beam. `(false, true)` keeps the colour
/// (a painted glyph); the monochrome table can only emit white.
pub(crate) fn masked_color_to_rgba(
    color_rgba: &[u8],
    mask_bgra: &[u8],
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    for i in 0..w * h {
        let and = mask_bgra.get(i * 4).is_some_and(|&b| b != 0);
        let c = color_rgba.get(i * 4..i * 4 + 3).unwrap_or(&[0, 0, 0]);
        let xor = c[0] != 0 || c[1] != 0 || c[2] != 0;
        let px = &mut rgba[i * 4..i * 4 + 4];
        match (and, xor) {
            (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
            (false, true) => px.copy_from_slice(&[c[0], c[1], c[2], 0xFF]),
            (true, false) => {}
            (true, true) => px.copy_from_slice(&INVERT_RGBA),
        }
    }
    rgba
}

/// Monochrome-cursor truth table.
///
/// A monochrome `HCURSOR` has no colour bitmap: `hbmMask` is double height — AND
/// over XOR — and the pair encodes four states:
///
/// | AND | XOR | meaning     | straight-alpha result                    |
/// |-----|-----|-------------|------------------------------------------|
/// | 0   | 0   | black       | opaque black                             |
/// | 0   | 1   | white       | opaque white                             |
/// | 1   | 0   | transparent | fully transparent                        |
/// | 1   | 1   | INVERT dst  | [`INVERT_RGBA`], translucent mid-gray    |
///
/// Invert is unrepresentable in straight alpha (per-pixel XOR of the destination), so it
/// draws as the driver's composite draws it.
pub(crate) fn mono_planes_to_rgba(
    and_plane: &[u8],
    xor_plane: &[u8],
    w: usize,
    h: usize,
) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    for i in 0..w * h {
        let (a, x) = (and_plane[i * 4] != 0, xor_plane[i * 4] != 0);
        let px = &mut rgba[i * 4..i * 4 + 4];
        match (a, x) {
            (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
            (false, true) => px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]),
            (true, false) => {}
            (true, true) => px.copy_from_slice(&INVERT_RGBA),
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrow() -> Shape {
        Shape {
            rgba: std::sync::Arc::new(vec![255; 2 * 2 * 4]),
            w: 2,
            h: 2,
            hot_x: 1,
            hot_y: 1,
            serial: 7,
            class: ShapeClass::Alpha,
        }
    }

    fn section(w: u32, hot: (u32, u32)) -> pf_frame::CursorOverlay {
        pf_frame::CursorOverlay {
            x: 0,
            y: 0,
            w,
            h: w,
            rgba: std::sync::Arc::new(vec![1; (w * w * 4) as usize]),
            serial: 5 | (1 << 31),
            hot_x: hot.0,
            hot_y: hot.1,
            visible: false,
        }
    }

    /// The section's shape rides the poller's hotspot position and visibility; a class
    /// mismatch, an empty section or a shapeless poll keep the poller's overlay.
    #[test]
    fn the_section_shape_rides_the_polled_position() {
        let rect = (0, 0, 100, 100);
        let polled = compose_overlay((10, 20), rect, true, Some(&arrow())).expect("overlay");
        let sec = section(48, (12, 14));
        let alpha = Some(ShapeClass::Alpha);
        let o = with_section_shape(
            polled.clone(),
            alpha,
            Some((sec.clone(), ShapeClass::Alpha)),
        );
        assert_eq!(
            (o.x, o.y, o.w, o.h, o.hot_x, o.hot_y),
            (-2, 6, 48, 48, 12, 14)
        );
        assert!(o.visible);
        assert_eq!(o.serial, 5 | (1 << 31));
        let masked = Some(ShapeClass::Masked);
        let kept = with_section_shape(
            polled.clone(),
            masked,
            Some((sec.clone(), ShapeClass::Alpha)),
        );
        assert_eq!((kept.w, kept.serial), (2, 7));
        assert_eq!(with_section_shape(polled.clone(), alpha, None).serial, 7);
        let empty = Some((section(0, (0, 0)), ShapeClass::Alpha));
        assert_eq!(with_section_shape(polled, alpha, empty).serial, 7);
        let hide = compose_overlay((1, 1), rect, false, None).expect("hide");
        assert_eq!(
            with_section_shape(hide, None, Some((sec, ShapeClass::Alpha))).w,
            0
        );
    }

    /// The overlay is the pointer less the hotspot, visible only inside the target's rect.
    #[test]
    fn the_overlay_is_visible_only_inside_the_rect() {
        let rect = (1920, 0, 1280, 720);
        let o = compose_overlay((10, 20), rect, true, Some(&arrow())).expect("overlay");
        assert_eq!((o.x, o.y, o.w, o.h, o.serial), (9, 19, 2, 2, 7));
        assert!(o.visible);
        let out = compose_overlay((1280, 20), rect, true, Some(&arrow())).expect("overlay");
        assert!(!out.visible, "past the rect's width");
        let hidden = compose_overlay((10, 20), rect, false, Some(&arrow())).expect("overlay");
        assert!(!hidden.visible, "a NULL handle or a hidden cursor");
    }

    /// Before any shape: a hide still reaches the client as an empty overlay; a shown pointer
    /// publishes nothing until it rasterises.
    #[test]
    fn a_hide_before_any_shape_is_an_empty_overlay() {
        let hidden = compose_overlay((3, 4), (0, 0, 640, 480), false, None).expect("a hide");
        assert_eq!((hidden.x, hidden.y, hidden.w, hidden.h), (3, 4, 0, 0));
        assert!(!hidden.visible && hidden.rgba.is_empty());
        assert!(compose_overlay((3, 4), (0, 0, 640, 480), true, None).is_none());
    }

    /// 1bpp plane → 32bpp as `GetDIBits` does: any non-zero channel means "bit set".
    fn plane(bits: &[u8]) -> Vec<u8> {
        bits.iter()
            .flat_map(|&b| {
                let v = if b != 0 { 0xFF } else { 0 };
                [v, v, v, 0]
            })
            .collect()
    }

    fn px(rgba: &[u8], i: usize) -> [u8; 4] {
        rgba[i * 4..i * 4 + 4].try_into().unwrap()
    }

    const OPAQUE_BLACK: [u8; 4] = [0, 0, 0, 0xFF];
    const OPAQUE_WHITE: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];
    const TRANSPARENT: [u8; 4] = [0, 0, 0, 0];

    /// Invert draws as the driver's gray and leaves its transparent neighbour alone.
    #[test]
    fn the_monochrome_truth_table_is_exact() {
        //          (0,0) black  (0,1) white  (1,0) transparent  (1,1) invert
        let and = plane(&[0, 0, 1, 1]);
        let xor = plane(&[0, 1, 0, 1]);
        let out = mono_planes_to_rgba(&and, &xor, 4, 1);
        assert_eq!(px(&out, 0), OPAQUE_BLACK, "AND=0 XOR=0 ⇒ black");
        assert_eq!(px(&out, 1), OPAQUE_WHITE, "AND=0 XOR=1 ⇒ white");
        assert_eq!(px(&out, 2), TRANSPARENT, "AND=1 XOR=0 ⇒ transparent");
        assert_eq!(px(&out, 3), INVERT_RGBA, "AND=1 XOR=1 ⇒ invert");
    }

    #[test]
    fn an_empty_alpha_channel_is_detected() {
        assert!(alpha_is_empty(&[1, 2, 3, 0, 4, 5, 6, 0]));
        assert!(!alpha_is_empty(&[1, 2, 3, 0, 4, 5, 6, 1]));
        assert!(alpha_is_empty(&[]), "no pixels ⇒ vacuously empty");
    }

    #[test]
    fn the_and_mask_supplies_alpha_for_an_alpha_less_cursor() {
        let mut rgba = vec![10, 20, 30, 0, 40, 50, 60, 0];
        let mask = plane(&[1, 0]); // pixel 0 masked out, pixel 1 kept
        apply_and_mask_alpha(&mut rgba, &mask);
        assert_eq!(px(&rgba, 0), [10, 20, 30, 0], "masked ⇒ transparent");
        assert_eq!(px(&rgba, 1), [40, 50, 60, 0xFF], "unmasked ⇒ opaque");
    }

    /// A short mask must not panic: `zip` stops at the shorter side. The caller
    /// already requires `mask.h >= color.h`; this is the belt.
    #[test]
    fn a_short_mask_does_not_panic() {
        let mut rgba = vec![1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0];
        apply_and_mask_alpha(&mut rgba, &plane(&[0]));
        assert_eq!(px(&rgba, 0), [1, 2, 3, 0xFF]);
        assert_eq!(px(&rgba, 1), [4, 5, 6, 0]);
    }

    /// Colour standing in for XOR. Pixel 3 is the I-beam case: AND=1 and a
    /// non-zero colour pixel is invert, not transparent — `apply_and_mask_alpha`
    /// would have dropped it. Invert is the driver's gray, as in the monochrome table.
    #[test]
    fn a_masked_color_invert_pixel_is_not_transparent() {
        //          (0,0) black  (0,1) red    (1,0) transparent  (1,1) invert
        let color = vec![0, 0, 0, 0, 0xCC, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0];
        let mask = plane(&[0, 0, 1, 1]);
        let out = masked_color_to_rgba(&color, &mask, 4, 1);
        assert_eq!(px(&out, 0), OPAQUE_BLACK, "AND=0 colour=0 ⇒ black");
        assert_eq!(
            px(&out, 1),
            [0xCC, 0, 0, 0xFF],
            "AND=0 colour ⇒ opaque colour"
        );
        assert_eq!(px(&out, 2), TRANSPARENT, "AND=1 colour=0 ⇒ transparent");
        assert_eq!(
            px(&out, 3),
            INVERT_RGBA,
            "AND=1 colour≠0 ⇒ invert, not drop"
        );
    }

    #[test]
    fn a_masked_color_transparent_pixel_stays_transparent() {
        let color = vec![0u8; 16];
        let mask = plane(&[1, 1, 1, 1]);
        let out = masked_color_to_rgba(&color, &mask, 4, 1);
        for i in 0..4 {
            assert_eq!(px(&out, i), TRANSPARENT, "pixel {i}");
        }
    }
}
