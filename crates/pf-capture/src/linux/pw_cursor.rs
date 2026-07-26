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
    // `span` is the exact byte extent the strided loop reads: `stride·(bh-1) + row`. Compute it
    // with checked arithmetic (a producer stride near `i32::MAX` would otherwise overflow) and
    // require the whole pixel block `[bmp_off + pix_off, +span)` to lie inside the region before
    // fabricating the slice — this is the check whose absence made the read go out of bounds.
    let span = match stride
        .checked_mul(bh as usize - 1)
        .and_then(|v| v.checked_add(row))
    {
        Some(s) => s,
        None => return,
    };
    match bmp_off
        .checked_add(pix_off)
        .and_then(|v| v.checked_add(span))
    {
        Some(end) if end <= region_size => {}
        _ => return,
    }
    // SAFETY: `bmp_off + pix_off + span <= region_size` (checked directly above), so the slice
    // is fully within the producer's meta region; `span` is exactly the strided loop's extent.
    let src = unsafe { std::slice::from_raw_parts(data.add(bmp_off + pix_off), span) };
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
