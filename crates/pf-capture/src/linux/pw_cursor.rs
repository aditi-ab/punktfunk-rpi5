//! Cursor-as-metadata: the `SPA_META_Cursor` parser and the CPU-path composite blits.
//!
//! Split out of `linux/pipewire.rs` (sweep Phase 5.2). Both halves are producer-driven and
//! bounds-critical: [`update_cursor_meta`] reads a bitmap at offsets the COMPOSITOR chose (its own
//! SAFETY proof notes that a missing bound SIGSEGVs inside the PipeWire `.process` callback, where
//! `catch_unwind` cannot help), and the `composite_cursor*` blits clip a caller-positioned bitmap
//! into a frame buffer. Separating them from the stream machinery is what makes them testable
//! without a compositor.

use super::PixelFormat;
use pipewire as pw;
use pw::spa;
use std::sync::Arc;

/// Latest cursor state parsed from `SPA_META_Cursor` (cursor-as-metadata mode). Position is
/// refreshed every buffer that carries the meta (including Mutter's cursor-only "corrupted"
/// buffers we otherwise skip for their stale frame); the RGBA bitmap is cached and only
/// replaced when the compositor sends a fresh one (`bitmap_offset != 0`).
#[derive(Default)]
pub(super) struct CursorState {
    /// True when the compositor reports a visible pointer (`spa_meta_cursor.id != 0`).
    visible: bool,
    /// Top-left where the bitmap is drawn = reported position − hotspot.
    x: i32,
    y: i32,
    /// Cached straight-alpha RGBA pixels (`bw*bh*4`, bytes R,G,B,A). `Arc` so the overlay handed
    /// to each GPU frame is a refcount bump, not a copy. Empty until the first bitmap arrives.
    rgba: Arc<Vec<u8>>,
    bw: u32,
    bh: u32,
    /// Bumps whenever the bitmap (`rgba`/`bw`/`bh`) changes — stable across position-only moves,
    /// so the GPU encoder re-uploads its cursor texture only on change.
    serial: u64,
    /// The compositor-reported hotspot — carried on the overlay for the cursor-forward
    /// channel (the blend path uses the pre-adjusted `x`/`y` and never reads it).
    hot_x: i32,
    hot_y: i32,
}

impl CursorState {
    /// A shareable overlay for the encode/forward paths, or `None` before the first bitmap
    /// arrived. A HIDDEN pointer still yields `Some` (with `visible: false`): the
    /// cursor-forward channel needs "known but hidden" — an app grabbed the pointer, the
    /// client's relative-mode hint (M3) — which is a different fact from "no cursor yet".
    /// The encode loop strips invisible overlays before any blend path sees the frame.
    /// Cheap: clones an `Arc` + a few scalars.
    pub(super) fn overlay(&self) -> Option<pf_frame::CursorOverlay> {
        if self.rgba.is_empty() {
            return None;
        }
        Some(pf_frame::CursorOverlay {
            x: self.x,
            y: self.y,
            w: self.bw,
            h: self.bh,
            rgba: self.rgba.clone(),
            serial: self.serial,
            hot_x: self.hot_x.max(0) as u32,
            hot_y: self.hot_y.max(0) as u32,
            visible: self.visible,
        })
    }
}

/// Extract straight (R,G,B,A) from one 4-byte cursor-bitmap pixel, honoring the bitmap's SPA
/// video format (portals emit RGBA or BGRA; ARGB/ABGR handled for completeness). Unknown
/// 4-byte formats are read as RGBA.
pub(super) fn decode_bitmap_pixel(vfmt: u32, s: &[u8]) -> (u8, u8, u8, u8) {
    match vfmt {
        x if x == spa::sys::SPA_VIDEO_FORMAT_RGBA => (s[0], s[1], s[2], s[3]),
        x if x == spa::sys::SPA_VIDEO_FORMAT_BGRA => (s[2], s[1], s[0], s[3]),
        x if x == spa::sys::SPA_VIDEO_FORMAT_ARGB => (s[1], s[2], s[3], s[0]),
        x if x == spa::sys::SPA_VIDEO_FORMAT_ABGR => (s[3], s[2], s[1], s[0]),
        _ => (s[0], s[1], s[2], s[3]),
    }
}

/// Update `cursor` from the newest buffer's `SPA_META_Cursor` (no-op when the buffer carries no
/// cursor meta — producer doesn't support it, or the portal isn't in Metadata cursor mode).
/// Called for EVERY dequeued buffer, before the stale-frame skip, so pointer-only movements
/// (which Mutter delivers as metadata-only "corrupted" buffers) still refresh the position.
pub(super) fn update_cursor_meta(cursor: &mut CursorState, spa_buf: *mut spa::sys::spa_buffer) {
    // SAFETY: `spa_buf` is the live buffer we still hold (dequeued, not yet requeued).
    // `spa_buffer_find_meta` returns the `spa_meta` (type + byte `size` + `data` pointer) for
    // `SPA_META_Cursor`, or null. We take `find_meta` rather than `find_meta_data` specifically
    // to obtain the region's real `size`: the bitmap offset, pixel offset and stride read below
    // are ALL producer-written, and without a bound against the actual region they drive
    // out-of-bounds pointer arithmetic and an oversized `slice::from_raw_parts` — an OOB read
    // that SIGSEGVs inside the PipeWire `.process` callback (a segfault `catch_unwind` cannot
    // catch). Every offset below is validated against `region_size` with checked arithmetic,
    // mirroring the fd-length guard the main frame path already applies to xdg-desktop-portal-wlr.
    let meta = unsafe { spa::sys::spa_buffer_find_meta(spa_buf, spa::sys::SPA_META_Cursor) };
    if meta.is_null() {
        return;
    }
    // SAFETY: `meta` is non-null and points into the held buffer's metadata array.
    let (region_size, data) = unsafe { ((*meta).size as usize, (*meta).data as *const u8) };
    if data.is_null() || region_size < std::mem::size_of::<spa::sys::spa_meta_cursor>() {
        return;
    }
    let cur = data as *const spa::sys::spa_meta_cursor;
    // SAFETY: `region_size >= size_of::<spa_meta_cursor>()` checked above, so every field is in bounds.
    let (id, pos_x, pos_y, hot_x, hot_y, bmp_off) = unsafe {
        (
            (*cur).id,
            (*cur).position.x,
            (*cur).position.y,
            (*cur).hotspot.x,
            (*cur).hotspot.y,
            (*cur).bitmap_offset,
        )
    };
    if id == 0 {
        // SPA contract: id 0 = "no cursor information", NOT "cursor hidden". Mutter only
        // REWRITES a buffer's meta region when the cursor changed, so recycled buffers
        // between damage frames carry a stale id-0 meta — treating that as hidden flickered
        // the cursor off between hovers (on-glass round 5). Keep the last-known state; a
        // pointer that really left/hid simply stops producing updates. (The M3 hidden hint
        // loses its Mutter signal — Windows has its own CURSOR_SUPPRESSED source.)
        return;
    }
    cursor.visible = true;
    cursor.x = pos_x - hot_x;
    cursor.y = pos_y - hot_y;
    cursor.hot_x = hot_x;
    cursor.hot_y = hot_y;
    if bmp_off == 0 {
        // Position-only update — keep the cached bitmap.
        return;
    }
    let bmp_off = bmp_off as usize;
    // The `spa_meta_bitmap` header must fit entirely inside the region before we read it —
    // `bitmap_offset` is producer-controlled and otherwise reads past the metadata.
    match bmp_off.checked_add(std::mem::size_of::<spa::sys::spa_meta_bitmap>()) {
        Some(end) if end <= region_size => {}
        _ => return,
    }
    // SAFETY: `bmp_off + size_of::<spa_meta_bitmap>() <= region_size` (checked directly above),
    // so the header is fully in bounds for a read of that many bytes. `read_unaligned` is
    // REQUIRED, not defensive: `bmp_off` is producer-written and nothing in the SPA contract or
    // in this function establishes that `data + bmp_off` meets `spa_meta_bitmap`'s alignment —
    // the previous field reads through an aligned `*const` asserted an invariant the code never
    // proved. The struct is `Copy` POD, so one unaligned read yields an owned, aligned local.
    let bmp = unsafe { (data.add(bmp_off) as *const spa::sys::spa_meta_bitmap).read_unaligned() };
    let (vfmt, bw, bh, stride, pix_off) = (
        bmp.format,
        bmp.size.width,
        bmp.size.height,
        bmp.stride.max(0) as usize,
        bmp.offset as usize,
    );
    // Ignore empty or implausibly large bitmaps (the meta-size request covers <= 1024×1024;
    // real cursors are ≤96px — the cursor channel downscales >120px for the wire anyway).
    if bw == 0 || bh == 0 || bw > 1024 || bh > 1024 {
        return;
    }
    let row = bw as usize * 4;
    let stride = if stride < row { row } else { stride };
    let Some(extent) = bitmap_extent(bmp_off, pix_off, stride, row, bh as usize, region_size)
    else {
        return;
    };
    // SAFETY: `bitmap_extent` returned `Some`, which means (see its contract) the whole range
    // `[bmp_off + pix_off, +len)` lies inside `region_size` and `len` is EXACTLY the extent the
    // strided loop below reads. `data` is the producer's meta-region base, live for this callback.
    let src = unsafe { std::slice::from_raw_parts(data.add(extent.start), extent.len()) };
    let mut rgba = vec![0u8; bw as usize * bh as usize * 4];
    for y in 0..bh as usize {
        for x in 0..bw as usize {
            let so = y * stride + x * 4;
            let (r, g, b, a) = decode_bitmap_pixel(vfmt, &src[so..so + 4]);
            let d = (y * bw as usize + x) * 4;
            rgba[d] = r;
            rgba[d + 1] = g;
            rgba[d + 2] = b;
            rgba[d + 3] = a;
        }
    }
    cursor.rgba = Arc::new(rgba);
    cursor.bw = bw;
    cursor.bh = bh;
    cursor.serial = cursor.serial.wrapping_add(1);
}

/// The byte range inside the producer's cursor-meta region that a `bh`-row, `row`-wide,
/// `stride`-strided bitmap at `bmp_off + pix_off` occupies — or `None` when it does not fit, or when
/// any of the arithmetic overflows.
///
/// THE bound on `update_cursor_meta`. Every input except `region_size` is producer-written, and
/// `region_size` is the real byte size of the meta region libspa handed us (which is why the caller
/// takes `spa_buffer_find_meta` rather than `find_meta_data`). Without this check the offsets drive
/// out-of-bounds pointer arithmetic and an oversized `slice::from_raw_parts` — an OOB read that
/// SIGSEGVs inside the PipeWire `.process` callback, where `catch_unwind` cannot help. A stride near
/// `i32::MAX` is enough to overflow the multiply on its own, so every step is checked.
///
/// `len()` of the returned range is EXACTLY `stride·(bh−1) + row`: the last row contributes only its
/// `row` visible bytes, not a full stride, so a bitmap that ends flush against the region's end is
/// accepted rather than rejected by a padding byte that is never read.
///
/// Extracted (sweep Phase 6.1) purely so it can be tested — the caller is unreachable without a live
/// compositor.
fn bitmap_extent(
    bmp_off: usize,
    pix_off: usize,
    stride: usize,
    row: usize,
    bh: usize,
    region_size: usize,
) -> Option<std::ops::Range<usize>> {
    if bh == 0 || row == 0 || stride < row {
        return None;
    }
    let span = stride.checked_mul(bh - 1)?.checked_add(row)?;
    let start = bmp_off.checked_add(pix_off)?;
    let end = start.checked_add(span)?;
    (end <= region_size).then_some(start..end)
}

/// Destination channel byte offsets (R,G,B) and bytes-per-pixel for a packed-RGB `PixelFormat`,
/// or `None` for a layout the CPU cursor blit doesn't handle (YUV/10-bit — those never reach
/// the CPU de-pad path anyway).
pub(super) fn dst_offsets(fmt: PixelFormat) -> Option<(usize, usize, usize, usize)> {
    Some(match fmt {
        PixelFormat::Bgrx | PixelFormat::Bgra => (2, 1, 0, 4),
        PixelFormat::Rgbx | PixelFormat::Rgba => (0, 1, 2, 4),
        PixelFormat::Rgb => (0, 1, 2, 3),
        PixelFormat::Bgr => (2, 1, 0, 3),
        _ => return None,
    })
}

/// Alpha-blend the cached cursor bitmap into a packed 10-bit (`X2Rgb10`/`X2Bgr10`) CPU frame:
/// unpack each u32, blend the 8-bit cursor channels scaled to 10 bits (`v<<2 | v>>6`), repack.
/// The frame samples are PQ-encoded, so like the 8-bit gamma-space blend this is a display-
/// referred approximation — fine for a cursor. `r_shift` is the R channel's bit offset (20 for
/// x:R:G:B, 0 for x:B:G:R); G is always at 10 and B mirrors R.
pub(super) fn composite_cursor_rgb10(
    tight: &mut [u8],
    w: usize,
    h: usize,
    r_shift: u32,
    cursor: &CursorState,
) {
    let b_shift = 20 - r_shift; // 0 or 20 — the opposite end from R
    let (bw, bh) = (cursor.bw as i32, cursor.bh as i32);
    for cy in 0..bh {
        let dy = cursor.y + cy;
        if dy < 0 || dy as usize >= h {
            continue;
        }
        for cx in 0..bw {
            let dx = cursor.x + cx;
            if dx < 0 || dx as usize >= w {
                continue;
            }
            let s = ((cy * bw + cx) as usize) * 4;
            let a = cursor.rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            // 8-bit cursor channel → 10-bit (replicate the top bits into the bottom).
            let up10 = |v: u8| ((v as u32) << 2) | ((v as u32) >> 6);
            let (sr, sg, sb) = (
                up10(cursor.rgba[s]),
                up10(cursor.rgba[s + 1]),
                up10(cursor.rgba[s + 2]),
            );
            let di = (dy as usize * w + dx as usize) * 4;
            let px = u32::from_le_bytes(tight[di..di + 4].try_into().unwrap());
            let blend = |dst: u32, src: u32| (src * a + dst * (255 - a)) / 255;
            let dr = blend((px >> r_shift) & 0x3ff, sr);
            let dg = blend((px >> 10) & 0x3ff, sg);
            let db = blend((px >> b_shift) & 0x3ff, sb);
            let out = (px & 0xc000_0000) | (dr << r_shift) | (dg << 10) | (db << b_shift);
            tight[di..di + 4].copy_from_slice(&out.to_le_bytes());
        }
    }
}

/// Alpha-blend the cached cursor bitmap into the tightly-packed CPU frame at its latched
/// position. Cheap: a straight-alpha blit over at most ~256×256 pixels, clipped to the frame —
/// the whole point of cursor-as-metadata (no forced full-frame composite on the producer).
pub(super) fn composite_cursor(
    tight: &mut [u8],
    w: usize,
    h: usize,
    fmt: PixelFormat,
    cursor: &CursorState,
) {
    if !cursor.visible || cursor.rgba.is_empty() {
        return;
    }
    // The packed 10-bit HDR layouts blend via bit unpack/repack, not byte offsets.
    match fmt {
        PixelFormat::X2Rgb10 => return composite_cursor_rgb10(tight, w, h, 20, cursor),
        PixelFormat::X2Bgr10 => return composite_cursor_rgb10(tight, w, h, 0, cursor),
        _ => {}
    }
    let Some((ri, gi, bi, bpp)) = dst_offsets(fmt) else {
        return;
    };
    let (bw, bh) = (cursor.bw as i32, cursor.bh as i32);
    for cy in 0..bh {
        let dy = cursor.y + cy;
        if dy < 0 || dy as usize >= h {
            continue;
        }
        for cx in 0..bw {
            let dx = cursor.x + cx;
            if dx < 0 || dx as usize >= w {
                continue;
            }
            let s = ((cy * bw + cx) as usize) * 4;
            let a = cursor.rgba[s + 3] as u32;
            if a == 0 {
                continue;
            }
            let (sr, sg, sb) = (
                cursor.rgba[s] as u32,
                cursor.rgba[s + 1] as u32,
                cursor.rgba[s + 2] as u32,
            );
            let di = (dy as usize * w + dx as usize) * bpp;
            let blend = |dst: u8, src: u32| ((src * a + dst as u32 * (255 - a)) / 255) as u8;
            tight[di + ri] = blend(tight[di + ri], sr);
            tight[di + gi] = blend(tight[di + gi], sg);
            tight[di + bi] = blend(tight[di + bi], sb);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A solid-colour cursor state at `(x, y)`, `w`×`h`, alpha `a`.
    fn cursor(x: i32, y: i32, w: u32, h: u32, rgb: (u8, u8, u8), a: u8) -> CursorState {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            px.extend_from_slice(&[rgb.0, rgb.1, rgb.2, a]);
        }
        CursorState {
            visible: true,
            x,
            y,
            rgba: Arc::new(px),
            bw: w,
            bh: h,
            serial: 1,
            hot_x: 0,
            hot_y: 0,
        }
    }

    // ---- bitmap_extent: the guard whose absence SIGSEGVs uncatchably -------------------------

    #[test]
    fn bitmap_extent_accepts_a_bitmap_that_fits() {
        // 4×2 RGBA, tightly packed: 32 bytes at offset 0.
        assert_eq!(bitmap_extent(0, 0, 16, 16, 2, 32), Some(0..32));
        // …and the same bitmap behind a header + pixel offset.
        assert_eq!(bitmap_extent(24, 8, 16, 16, 2, 64), Some(32..64));
    }

    #[test]
    fn bitmap_extent_charges_the_last_row_only_its_visible_bytes() {
        // stride 32, row 16, 3 rows ⇒ 32*2 + 16 = 80, NOT 96. A bitmap ending flush against the
        // region must be accepted: the trailing stride padding is never read.
        assert_eq!(bitmap_extent(0, 0, 32, 16, 3, 80), Some(0..80));
        assert_eq!(bitmap_extent(0, 0, 32, 16, 3, 79), None, "one byte short");
    }

    #[test]
    fn bitmap_extent_rejects_anything_past_the_region() {
        assert_eq!(bitmap_extent(0, 0, 16, 16, 2, 31), None);
        // An offset alone can push it out.
        assert_eq!(bitmap_extent(1, 0, 16, 16, 2, 32), None);
        assert_eq!(bitmap_extent(0, 1, 16, 16, 2, 32), None);
        // A region of zero accepts nothing.
        assert_eq!(bitmap_extent(0, 0, 16, 16, 1, 0), None);
    }

    /// The producer picks `stride` and both offsets, so each is an overflow vector on its own.
    #[test]
    fn bitmap_extent_survives_hostile_arithmetic() {
        // stride × (bh-1) overflows.
        assert_eq!(bitmap_extent(0, 0, usize::MAX, 16, 3, usize::MAX), None);
        // span + row overflows. Needs ≥2 rows so `stride·(bh−1)` is already at the ceiling: with a
        // SINGLE row `stride·0 == 0`, and even a `usize::MAX`-wide row is then arithmetically in
        // range — which is correct rather than a miss, since the caller has already capped `bw` at
        // 1024 and `row` is therefore ≤ 4096.
        assert_eq!(
            bitmap_extent(0, 0, usize::MAX, usize::MAX, 2, usize::MAX),
            None
        );
        // bmp_off + pix_off overflows.
        assert_eq!(bitmap_extent(usize::MAX, 1, 16, 16, 1, usize::MAX), None);
        // start + span overflows.
        assert_eq!(
            bitmap_extent(usize::MAX - 8, 0, 16, 16, 1, usize::MAX),
            None
        );
        // A near-i32::MAX stride — the case the SAFETY comment calls out — must not wrap.
        assert_eq!(bitmap_extent(0, 0, i32::MAX as usize, 16, 1024, 4096), None);
    }

    #[test]
    fn bitmap_extent_rejects_degenerate_geometry() {
        assert_eq!(bitmap_extent(0, 0, 16, 16, 0, 4096), None, "zero rows");
        assert_eq!(bitmap_extent(0, 0, 16, 0, 2, 4096), None, "zero-width row");
        assert_eq!(bitmap_extent(0, 0, 8, 16, 2, 4096), None, "stride < row");
    }

    // ---- composite_cursor: clipping, alpha, and every layout --------------------------------

    /// Read pixel `(x, y)`'s (R, G, B) out of a packed frame, honouring the layout's byte order.
    fn px_rgb(buf: &[u8], w: usize, x: usize, y: usize, fmt: PixelFormat) -> (u8, u8, u8) {
        let (ri, gi, bi, bpp) = dst_offsets(fmt).expect("packed layout");
        let i = (y * w + x) * bpp;
        (buf[i + ri], buf[i + gi], buf[i + bi])
    }

    #[test]
    fn every_packed_layout_lands_the_colour_in_its_own_channels() {
        for fmt in [
            PixelFormat::Bgrx,
            PixelFormat::Bgra,
            PixelFormat::Rgbx,
            PixelFormat::Rgba,
            PixelFormat::Rgb,
            PixelFormat::Bgr,
        ] {
            let bpp = dst_offsets(fmt).unwrap().3;
            let (w, h) = (4usize, 4usize);
            let mut buf = vec![0u8; w * h * bpp];
            // Opaque pure red at (1, 1).
            composite_cursor(&mut buf, w, h, fmt, &cursor(1, 1, 1, 1, (255, 0, 0), 255));
            assert_eq!(px_rgb(&buf, w, 1, 1, fmt), (255, 0, 0), "{fmt:?}");
            // Nothing else moved.
            assert_eq!(px_rgb(&buf, w, 0, 0, fmt), (0, 0, 0), "{fmt:?}");
            assert_eq!(px_rgb(&buf, w, 2, 1, fmt), (0, 0, 0), "{fmt:?}");
        }
    }

    #[test]
    fn a_cursor_hanging_off_every_edge_is_clipped_not_wrapped() {
        let (w, h, fmt) = (4usize, 4usize, PixelFormat::Bgrx);
        // Top-left: only the bottom-right quarter of a 2×2 lands, at (0, 0).
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(
            &mut buf,
            w,
            h,
            fmt,
            &cursor(-1, -1, 2, 2, (10, 20, 30), 255),
        );
        assert_eq!(px_rgb(&buf, w, 0, 0, fmt), (10, 20, 30));
        assert_eq!(px_rgb(&buf, w, 1, 0, fmt), (0, 0, 0));
        assert_eq!(px_rgb(&buf, w, 0, 1, fmt), (0, 0, 0));
        // Bottom-right: only the top-left quarter lands, at (3, 3).
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &cursor(3, 3, 2, 2, (10, 20, 30), 255));
        assert_eq!(px_rgb(&buf, w, 3, 3, fmt), (10, 20, 30));
        assert_eq!(px_rgb(&buf, w, 2, 3, fmt), (0, 0, 0));
        // Fully outside in each direction: the frame is untouched.
        for pos in [(-2, 0), (0, -2), (4, 0), (0, 4), (-9, -9), (99, 99)] {
            let mut buf = vec![0u8; w * h * 4];
            composite_cursor(
                &mut buf,
                w,
                h,
                fmt,
                &cursor(pos.0, pos.1, 2, 2, (255, 255, 255), 255),
            );
            assert!(buf.iter().all(|&b| b == 0), "drew something at {pos:?}");
        }
    }

    #[test]
    fn transparent_and_hidden_cursors_draw_nothing() {
        let (w, h, fmt) = (2usize, 2usize, PixelFormat::Bgrx);
        // Alpha 0 — every pixel skipped.
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &cursor(0, 0, 2, 2, (255, 255, 255), 0));
        assert!(buf.iter().all(|&b| b == 0));
        // `visible: false` — the whole blit is skipped.
        let mut c = cursor(0, 0, 2, 2, (255, 255, 255), 255);
        c.visible = false;
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &c);
        assert!(buf.iter().all(|&b| b == 0));
        // No bitmap yet — likewise.
        let mut c = cursor(0, 0, 2, 2, (255, 255, 255), 255);
        c.rgba = Arc::new(Vec::new());
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &c);
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn half_alpha_blends_toward_the_destination() {
        let (w, h, fmt) = (1usize, 1usize, PixelFormat::Bgrx);
        // dst = white, src = black at 50% ⇒ mid grey (integer blend: (0*128 + 255*127)/255 = 127).
        let mut buf = vec![255u8; w * h * 4];
        composite_cursor(&mut buf, w, h, fmt, &cursor(0, 0, 1, 1, (0, 0, 0), 128));
        assert_eq!(px_rgb(&buf, w, 0, 0, fmt), (127, 127, 127));
    }

    /// A layout the CPU blit cannot address must be declined, not mis-blitted.
    #[test]
    fn unsupported_layouts_are_declined() {
        assert!(dst_offsets(PixelFormat::Nv12).is_none());
        assert!(dst_offsets(PixelFormat::Yuv444).is_none());
        let (w, h) = (2usize, 2usize);
        let mut buf = vec![0u8; w * h * 4];
        composite_cursor(
            &mut buf,
            w,
            h,
            PixelFormat::Nv12,
            &cursor(0, 0, 2, 2, (255, 255, 255), 255),
        );
        assert!(buf.iter().all(|&b| b == 0), "NV12 must not be blitted");
    }

    // ---- composite_cursor_rgb10: the 10-bit unpack/repack round trip -------------------------

    /// Pack an `x:R:G:B` (`X2Rgb10`) pixel — R at bit 20, G at 10, B at 0 — the way a producer does.
    fn pack_x2rgb10(r: u32, g: u32, b: u32) -> [u8; 4] {
        (0xC000_0000 | (r << 20) | (g << 10) | b).to_le_bytes()
    }

    #[test]
    fn the_10bit_path_round_trips_an_untouched_pixel() {
        // Alpha 0 ⇒ the blend is skipped entirely, so the packed pixel must come back bit-identical
        // (including the top two bits, which are alpha and must survive the repack).
        for (r, g, b) in [(0, 0, 0), (1023, 1023, 1023), (940, 64, 512), (1, 2, 3)] {
            let src = pack_x2rgb10(r, g, b);
            let mut buf = src.to_vec();
            composite_cursor(
                &mut buf,
                1,
                1,
                PixelFormat::X2Rgb10,
                &cursor(0, 0, 1, 1, (255, 255, 255), 0),
            );
            assert_eq!(buf, src, "({r},{g},{b}) was modified by a zero-alpha blend");
        }
    }

    #[test]
    fn the_10bit_path_writes_the_right_channel_at_the_right_shift() {
        // Opaque pure red, 8-bit 255 → 10-bit 1023 (the `v<<2 | v>>6` expansion).
        let mut buf = pack_x2rgb10(0, 0, 0).to_vec();
        composite_cursor(
            &mut buf,
            1,
            1,
            PixelFormat::X2Rgb10,
            &cursor(0, 0, 1, 1, (255, 0, 0), 255),
        );
        let v = u32::from_le_bytes(buf[..4].try_into().unwrap());
        assert_eq!((v >> 20) & 0x3ff, 1023, "R");
        assert_eq!((v >> 10) & 0x3ff, 0, "G");
        assert_eq!(v & 0x3ff, 0, "B");
        assert_eq!(v & 0xc000_0000, 0xc000_0000, "alpha bits preserved");

        // X2Bgr10 puts R at bit 0 and B at 20 — the SAME cursor must land in the other end.
        let mut buf = pack_x2rgb10(0, 0, 0).to_vec();
        composite_cursor(
            &mut buf,
            1,
            1,
            PixelFormat::X2Bgr10,
            &cursor(0, 0, 1, 1, (255, 0, 0), 255),
        );
        let v = u32::from_le_bytes(buf[..4].try_into().unwrap());
        assert_eq!(v & 0x3ff, 1023, "R at bit 0 for x:B:G:R");
        assert_eq!((v >> 20) & 0x3ff, 0, "B untouched");
    }

    #[test]
    fn the_10bit_path_clips_like_the_8bit_one() {
        let (w, h) = (2usize, 2usize);
        let mut buf: Vec<u8> = (0..w * h).flat_map(|_| pack_x2rgb10(0, 0, 0)).collect();
        let before = buf.clone();
        // Entirely off-frame.
        composite_cursor(
            &mut buf,
            w,
            h,
            PixelFormat::X2Rgb10,
            &cursor(-5, -5, 2, 2, (255, 255, 255), 255),
        );
        assert_eq!(buf, before);
        // Straddling the top-left corner: only (0, 0) is written.
        composite_cursor(
            &mut buf,
            w,
            h,
            PixelFormat::X2Rgb10,
            &cursor(-1, -1, 2, 2, (255, 255, 255), 255),
        );
        let p0 = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let p1 = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        assert_eq!((p0 >> 20) & 0x3ff, 1023);
        assert_eq!((p1 >> 20) & 0x3ff, 0);
    }

    // ---- decode_bitmap_pixel: the producer's byte order ------------------------------------

    #[test]
    fn each_bitmap_format_is_decoded_to_straight_rgba() {
        let s = [1u8, 2, 3, 4];
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_RGBA, &s),
            (1, 2, 3, 4)
        );
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_BGRA, &s),
            (3, 2, 1, 4)
        );
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_ARGB, &s),
            (2, 3, 4, 1)
        );
        assert_eq!(
            decode_bitmap_pixel(spa::sys::SPA_VIDEO_FORMAT_ABGR, &s),
            (4, 3, 2, 1)
        );
        // An unknown 4-byte format reads as RGBA rather than being rejected — documented behaviour.
        assert_eq!(decode_bitmap_pixel(0xdead_beef, &s), (1, 2, 3, 4));
    }
}
