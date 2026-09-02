//! GDI cursor poller: the Windows cursor-SHAPE source for the cursor-forward
//! channel. Off-thread `GetCursorInfo` + `HCURSOR` rasterise, published as
//! [`pf_frame::CursorOverlay`].
//!
//! IddCx hardware-cursor query (`CursorShm`) is alpha-only: `IDDCX_CURSOR_SHAPE_TYPE`
//! has no monochrome, the OS pre-converts mono to masked-color, and MASKED_COLOR
//! delivery is dead on modern builds. The driver still declares XOR FULL so DWM
//! excludes every cursor type from the IDD frame; this poller is the shape that
//! then gets forwarded or host-composited. DXGI `GetFramePointerShape` is not
//! used: `PointerPosition.Visible` goes stale under injected input, and it burns
//! one of four duplication slots.
//!
//! The host runs as SYSTEM inside the interactive session on `winsta0\default`
//! (`windows/service.rs` `spawn_host`), so this thread reads the session cursor
//! directly. Pin via the overlay snapshot and `pf_win_display::secure_desktop`. Invert/mask
//! contracts are tested below; design in `design/remote-desktop-sweep.md`.

use super::*;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS,
    HDESK,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DestroyIcon, GetCursorInfo, GetIconInfo, CURSORINFO, HICON, ICONINFO,
};

const CURSOR_SHOWING: u32 = 0x1;
const CURSOR_SUPPRESSED: u32 = 0x2;

/// `rgba` is `Arc` so slot publish and every downstream attach is a refcount bump.
struct Shape {
    rgba: std::sync::Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    serial: u64,
}

/// Off-thread GDI cursor poller. User32/gdi32 stay off the capture/encode thread;
/// the capture tick is one uncontended mutex read plus an `Arc` clone.
pub(super) struct CursorPoller {
    slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CursorPoller {
    /// 4 ms ≈ 250 Hz. The polled position is also the composite-blend position, so
    /// it must out-pace a 240 fps session; 16 ms reused a stale spot for ~4 frames.
    const INTERVAL: Duration = Duration::from_millis(4);
    /// Unconditional input-desktop reattach. `GetCursorInfo` on a stale desktop
    /// *succeeds* with stale data, so there is no failure signal. Each reattach also
    /// refreshes `pf_win_display::secure_desktop`, the fallback for the
    /// desktop-switch WinEvent; 250 ms keeps a missed event's freeze short.
    const REATTACH: Duration = Duration::from_millis(250);
    /// Same-handle extent re-probe. Scale changes are human-timescale; the probe
    /// reads dimensions only, so 4 Hz is ample and a ≤250 ms size lag is invisible.
    const EXTENT_PROBE: Duration = Duration::from_millis(250);

    /// Spawn for virtual display `target_id`. `rect` seeds the desktop rect
    /// (`source_desktop_rect` order: x, y, w, h). Positions are desktop-global;
    /// the overlay is frame-relative, and a pointer outside the rect is
    /// `visible: false` (per-output, matching shm and the Linux portal).
    ///
    /// A SEED, not the value: the poll thread re-queries the rect on its [`Self::REATTACH`] cadence.
    /// It used to be captured once here and used forever for BOTH the desktop→frame offset and the
    /// `in_rect` test, while both mid-session mode-change paths (`resize_output` and
    /// `poll_display_hdr` → `recreate_ring`) keep the same poller — so after an in-place resize the
    /// pointer was clipped to the OLD rect and offset by a stale origin. Re-querying on the poll
    /// thread is what keeps the CCD call off the capture/encode thread, which is the whole reason
    /// this poller exists (see `DescriptorPoller`).
    pub(super) fn spawn(
        ccd: pf_win_display::win_display::CcdTargetKey,
        rect: (i32, i32, i32, i32),
    ) -> Self {
        let slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (slot_t, stop_t) = (slot.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("pf-cursor-poll".into())
            .spawn(move || run(ccd, rect, &slot_t, &stop_t))
            .ok();
        if thread.is_none() {
            tracing::warn!("cursor poller thread spawn failed — cursor falls back to driver shm");
        }
        Self { slot, stop, thread }
    }

    /// Latest overlay; `None` until the first successful rasterise.
    pub(super) fn read(&self) -> Option<pf_frame::CursorOverlay> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Worker still running; `false` degrades the capturer to the shm read.
    pub(super) fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for CursorPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join(); // worker sleeps ≤ INTERVAL — a bounded join
        }
    }
}

fn run(
    ccd: pf_win_display::win_display::CcdTargetKey,
    mut rect: (i32, i32, i32, i32),
    slot: &Mutex<Option<pf_frame::CursorOverlay>>,
    stop: &AtomicBool,
) {
    // Physical pixels on this thread: `rect` is CCD (always physical). A
    // DPI-virtualized `GetCursorInfo` would miss the frame pixel on a scaled display.
    // Thread-scoped; the rest of the host is untouched.

    // SAFETY: takes and returns only a by-value context handle; affects this thread only.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let mut desktop = DesktopBinding::default();
    // best-effort: already on winsta0\default if this fails
    desktop.reattach();
    let mut last_attach = Instant::now();

    let mut shape: Option<Shape> = None;
    let mut cached_handle: isize = 0;
    let mut failed_handle: isize = 0; // don't re-rasterise a failing handle every tick
    let mut serial: u64 = 0;
    let mut logged_live = false;
    let mut last_extent = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(CursorPoller::INTERVAL);
        if last_attach.elapsed() >= CursorPoller::REATTACH {
            last_attach = Instant::now();
            desktop.reattach();
            // …and re-read the target's desktop rect from the display actor's snapshot (no CCD
            // call here): a resize, an HDR recreate or the user moving this display changes BOTH
            // the origin positions are made relative to and the extent `in_rect` tests against,
            // and this poller outlives all of them. `None` keeps the last good value — a target
            // briefly absent must not park the pointer at a `(0, 0, 0, 0)` rect (all invisible).
            let fresh = pf_win_display::display_events::snapshot().source_rect(ccd);
            if let Some(fresh) = fresh {
                if fresh != rect {
                    tracing::info!(
                        target = %ccd,
                        from = ?rect,
                        to = ?fresh,
                        "cursor poller: target desktop rect changed — re-basing pointer positions"
                    );
                    rect = fresh;
                }
            }
        }

        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `ci` is a live, correctly-sized out-param for this synchronous call; no pointer
        // escapes it.
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            // Desktop gone (secure-desktop switch mid-call) — rebind next tick;
            // the slot keeps its last snapshot.
            desktop.reattach();
            last_attach = Instant::now();
            continue;
        }

        let flags = ci.flags.0;
        let showing = flags & CURSOR_SHOWING != 0 && flags & CURSOR_SUPPRESSED == 0;

        // Rasterise on handle change only. Hidden cursors keep the cached shape
        // (hidden-but-known needs a seen bitmap). Animated cursors publish frame 0.
        let handle = ci.hCursor.0 as isize;

        // Handle identity cannot see a re-render. Windows rebuilds system cursors
        // when the scale under the pointer changes, but the shared handle stays
        // put for the session (arrow is 0x10003 throughout). Re-read extent
        // (dimensions only) and drop the cache when it moved.
        if showing && handle != 0 && handle == cached_handle {
            if last_extent.elapsed() >= CursorPoller::EXTENT_PROBE {
                last_extent = Instant::now();
                if let (Some(now), Some(s)) = (cursor_extent(ci.hCursor), shape.as_ref()) {
                    if now != (s.w, s.h) {
                        tracing::info!(
                            target = %ccd,
                            "cursor: the pointer bitmap resized under a stable handle \
                             ({}x{} -> {}x{}) — re-rasterising (the scale under the pointer moved)",
                            s.w,
                            s.h,
                            now.0,
                            now.1
                        );
                        cached_handle = 0; // re-rasterise below, on this same tick
                    }
                }
            }
        } else {
            // A handle change re-rasterises on its own — hold the probe off so it can't fire on
            // the very next tick against a shape that is current by construction.
            last_extent = Instant::now();
        }

        if showing && handle != 0 && handle != cached_handle && handle != failed_handle {
            match rasterize(ci.hCursor) {
                Some((rgba, w, h, hot_x, hot_y)) => {
                    serial += 1;
                    shape = Some(Shape {
                        rgba: std::sync::Arc::new(rgba),
                        w,
                        h,
                        hot_x,
                        hot_y,
                        serial,
                    });
                    cached_handle = handle;
                    failed_handle = 0;
                    if !logged_live {
                        logged_live = true;
                        tracing::info!(
                            target = %ccd,
                            "cursor poller live — GDI shape source publishing (serial 1: {w}x{h})"
                        );
                    }
                }
                None => {
                    // The owning app may have destroyed the cursor mid-read; keep the previous
                    // shape and don't hammer this handle again until it changes.
                    failed_handle = handle;
                }
            }
        }

        let overlay = shape.as_ref().map(|s| {
            let (px, py) = (ci.ptScreenPos.x - rect.0, ci.ptScreenPos.y - rect.1);
            let in_rect = px >= 0 && py >= 0 && px < rect.2 && py < rect.3;
            pf_frame::CursorOverlay {
                // Overlay x/y = bitmap top-left (reported position − hotspot), frame pixels.
                x: px - s.hot_x as i32,
                y: py - s.hot_y as i32,
                w: s.w,
                h: s.h,
                rgba: s.rgba.clone(),
                serial: s.serial,
                hot_x: s.hot_x,
                hot_y: s.hot_y,
                // `handle != 0` is part of visible, not just of rasterise:
                // `SetCursor(NULL)` (game/video hide) leaves `CURSOR_SHOWING` set
                // with a NULL `hCursor`. Flags alone would publish the last shape.
                visible: showing && in_rect && handle != 0,
            }
        });
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = overlay;
    }
}

/// Owned input-desktop handle: keep the current binding, swap on demand, close
/// exactly once (same reattach model as [`SendInputInjector`] in `pf-inject`).
#[derive(Default)]
struct DesktopBinding(Option<HDESK>);

impl DesktopBinding {
    /// Rebind to the current input desktop (the binding stays put if it cannot be
    /// opened), then refresh the process-wide secure-desktop verdict.
    fn reattach(&mut self) {
        const GENERIC_ALL: u32 = 0x1000_0000;
        // SAFETY: `OpenInputDesktop`/`SetThreadDesktop`/`CloseDesktop` take only by-value args.
        // `OpenInputDesktop` yields an owned `HDESK` only on `Ok`; it is either installed (and the
        // previously-owned handle closed exactly once) or closed on failure — no handle is leaked
        // or used after close. `SetThreadDesktop` rebinds only this calling thread (which owns
        // no windows/hooks, so the rebind cannot fail on that account).
        unsafe {
            if let Ok(h) = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            ) {
                if SetThreadDesktop(h).is_ok() {
                    if let Some(old) = self.0.replace(h) {
                        let _ = CloseDesktop(old);
                    }
                } else {
                    let _ = CloseDesktop(h);
                }
            }
        }
        pf_win_display::refresh_secure_desktop();
    }
}

impl Drop for DesktopBinding {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // SAFETY: `h` is our owned desktop handle, closed exactly once here.
            let _ = unsafe { CloseDesktop(h) };
        }
    }
}

/// Rasterise `hcursor` to straight-alpha RGBA. `None` on any failure (caller
/// keeps the previous shape).
fn rasterize(hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> RasterOut {
    // CopyIcon first: the owner can destroy its HCURSOR between GetCursorInfo
    // and the reads below; the copy is ours.

    // SAFETY: `HICON(hcursor.0)` reinterprets the cursor handle as an icon handle (cursors ARE
    // icons in user32); CopyIcon yields an owned HICON we destroy below.
    let Ok(icon) = (unsafe { CopyIcon(HICON(hcursor.0)) }) else {
        return None;
    };
    let mut ii = ICONINFO::default();
    // SAFETY: `ii` is a live out-param. On Ok it hands us COPIES of the mask/color bitmaps —
    // both deleted below (GDI-handle leak otherwise).
    let got = unsafe { GetIconInfo(icon, &mut ii) };
    let out = if got.is_ok() { convert(&ii) } else { None };
    // SAFETY: deleting the two bitmap copies GetIconInfo returned (null-safe: DeleteObject on a
    // null HGDIOBJ fails harmlessly) and the icon copy — each exactly once.
    unsafe {
        let _ = DeleteObject(ii.hbmColor.into());
        let _ = DeleteObject(ii.hbmMask.into());
        let _ = DestroyIcon(icon);
    }
    out.map(|(rgba, w, h)| {
        let hot_x = ii.xHotspot.min(w.saturating_sub(1));
        let hot_y = ii.yHotspot.min(h.saturating_sub(1));
        (rgba, w, h, hot_x, hot_y)
    })
}

type RasterOut = Option<(Vec<u8>, u32, u32, u32, u32)>;

/// Bitmap extent of `hcursor` — the `(w, h)` [`convert`] would derive, no pixel
/// read. A re-render keeps the handle; `None` is "no verdict" (keep the cache).
fn cursor_extent(hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> Option<(u32, u32)> {
    // CopyIcon first, same reason as `rasterize`: the owner can destroy the
    // HCURSOR between GetCursorInfo and the reads below.

    // SAFETY: `HICON(hcursor.0)` reinterprets the cursor handle as an icon handle (cursors ARE
    // icons in user32); CopyIcon yields an owned HICON destroyed below.
    let icon = unsafe { CopyIcon(HICON(hcursor.0)) }.ok()?;
    let mut ii = ICONINFO::default();
    // SAFETY: `ii` is a live out-param. On Ok it hands us COPIES of the mask/color bitmaps — both
    // deleted below (GDI-handle leak otherwise).
    let got = unsafe { GetIconInfo(icon, &mut ii) };
    // Mirrors `convert`'s two families: a color cursor's extent is its color bitmap's; a
    // monochrome one's mask carries the AND plane OVER the XOR plane, so its height is doubled.
    let extent = got.is_ok().then_some(()).and_then(|()| {
        if !ii.hbmColor.is_invalid() {
            bitmap_extent(ii.hbmColor)
        } else {
            let (w, h) = bitmap_extent(ii.hbmMask)?;
            (h >= 2 && h % 2 == 0).then_some((w, h / 2))
        }
    });
    // SAFETY: deleting the two bitmap copies GetIconInfo returned (null-safe: DeleteObject on a
    // null HGDIOBJ fails harmlessly) and the icon copy — each exactly once.
    unsafe {
        let _ = DeleteObject(ii.hbmColor.into());
        let _ = DeleteObject(ii.hbmMask.into());
        let _ = DestroyIcon(icon);
    }
    extent
}

/// Dimensions under [`read_bitmap_32`]'s caps so the two agree: a bitmap
/// `rasterize` would reject must not read here as a size change.
fn bitmap_extent(hbm: HBITMAP) -> Option<(u32, u32)> {
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a live out-param sized exactly as passed; GetObjectW only writes into it.
    let n = unsafe {
        GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 || bm.bmWidth > 512 || bm.bmHeight > 1024 {
        return None;
    }
    Some((bm.bmWidth as u32, bm.bmHeight as u32))
}

/// Convert ICONINFO bitmaps to straight RGBA. Two families:
///
/// - color (`hbmColor` set): 32bpp BGRA. If alpha is entirely empty (old-style
///   masked-color, including Win11's coloured I-beam) the AND mask is the same
///   four-state table as monochrome, with the colour bitmap as XOR. Treating
///   AND=1 as "always transparent" drops invert pixels; the I-beam is almost
///   entirely invert.
/// - monochrome (`hbmColor` null): `hbmMask` is double height — AND over XOR.
///   (0,0) black, (0,1) white, (1,0) transparent, (1,1) invert. Invert is
///   unrepresentable in straight alpha, so it becomes opaque black with a white
///   outline grown into adjacent transparency (keeps the I-beam legible).
fn convert(ii: &ICONINFO) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: GetDC(None) yields the screen DC, released below on every path; it is only used
    // as the GetDIBits reference DC.
    let dc = unsafe { GetDC(None) };
    let result = (|| {
        if !ii.hbmColor.is_invalid() {
            let color = read_bitmap_32(dc, ii.hbmColor)?;
            let (w, h) = (color.w as u32, color.h as u32);
            let mut rgba = bgra_to_rgba(&color.bgra);
            if alpha_is_empty(&rgba) {
                let mask = read_bitmap_32(dc, ii.hbmMask)?;
                if mask.w != color.w || mask.h < color.h {
                    return None;
                }
                rgba = masked_color_to_rgba(&rgba, &mask.bgra, w as usize, h as usize);
            }
            Some((rgba, w, h))
        } else {
            let mask = read_bitmap_32(dc, ii.hbmMask)?;
            if mask.h < 2 || mask.h % 2 != 0 {
                return None;
            }
            let (w, h) = (mask.w as usize, (mask.h / 2) as usize);
            let (and_plane, xor_plane) = mask.bgra.split_at(h * w * 4);
            let rgba = mono_planes_to_rgba(and_plane, xor_plane, w, h);
            Some((rgba, w as u32, h as u32))
        }
    })();
    // SAFETY: releasing the screen DC obtained above, exactly once.
    unsafe {
        ReleaseDC(None, dc);
    }
    result
}

const NEIGHBORS: [(i32, i32); 8] = [
    (-1, -1),
    (0, -1),
    (1, -1),
    (-1, 0),
    (1, 0),
    (-1, 1),
    (0, 1),
    (1, 1),
];

struct RawBitmap {
    w: i32,
    h: i32,
    /// 32bpp top-down BGRA rows, `w*h*4` (monochrome sources arrive expanded: 0x00/0xFF channels).
    bgra: Vec<u8>,
}

/// 32bpp top-down via `GetDIBits` (1bpp→32bpp expansion for the mask planes).
fn read_bitmap_32(dc: HDC, hbm: HBITMAP) -> Option<RawBitmap> {
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a live out-param sized exactly as passed; GetObjectW only writes into it.
    let n = unsafe {
        GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 || bm.bmWidth > 512 || bm.bmHeight > 1024 {
        return None; // 512/1024: sanity caps (256² is the wire max; XL accessibility ≤ that)
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    // SAFETY: `buf` spans exactly `h` rows of `w` 32bpp pixels as described by `info`; both are
    // live locals for this synchronous call, `hbm` is a live bitmap not selected into any DC
    // (fresh GetIconInfo copies).
    let rows = unsafe {
        GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    (rows != 0).then_some(RawBitmap { w, h, bgra: buf })
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    out
}

/// Alpha channel entirely zero: old-style cursor whose transparency (and invert)
/// live in the AND mask ([`masked_color_to_rgba`]).
fn alpha_is_empty(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|p| p[3] == 0)
}

/// Test-only AND-as-alpha (no invert). `mask_bgra` is GetDIBits' 32bpp expansion
/// of the 1bpp mask, so any non-zero channel is "set"; white = transparent.
/// [`convert`] uses [`masked_color_to_rgba`]: AND=1 plus colour is invert (I-beam).
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
fn masked_color_to_rgba(color_rgba: &[u8], mask_bgra: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for i in 0..w * h {
        let and = mask_bgra.get(i * 4).is_some_and(|&b| b != 0);
        let c = color_rgba.get(i * 4..i * 4 + 3).unwrap_or(&[0, 0, 0]);
        let xor = c[0] != 0 || c[1] != 0 || c[2] != 0;
        let px = &mut rgba[i * 4..i * 4 + 4];
        match (and, xor) {
            (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
            (false, true) => px.copy_from_slice(&[c[0], c[1], c[2], 0xFF]),
            (true, false) => {}
            (true, true) => {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
                invert[i] = true;
            }
        }
    }
    grow_invert_outline(&mut rgba, &invert, w, h);
    rgba
}

/// Monochrome-cursor truth table, plus the white outline that makes invert legible.
///
/// A monochrome `HCURSOR` has no colour bitmap: `hbmMask` is double height — AND
/// over XOR — and the pair encodes four states:
///
/// | AND | XOR | meaning     | straight-alpha result                    |
/// |-----|-----|-------------|------------------------------------------|
/// | 0   | 0   | black       | opaque black                             |
/// | 0   | 1   | white       | opaque white                             |
/// | 1   | 0   | transparent | fully transparent                        |
/// | 1   | 1   | INVERT dst  | opaque black + a grown white outline     |
///
/// Invert is unrepresentable in straight alpha (per-pixel XOR of the destination),
/// so it becomes opaque black and every transparent 8-neighbour of an invert
/// pixel is turned opaque white. That outline keeps a text I-beam — almost
/// entirely invert — legible over dark content.
fn mono_planes_to_rgba(and_plane: &[u8], xor_plane: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; w * h * 4];
    let mut invert = vec![false; w * h];
    for i in 0..w * h {
        let (a, x) = (and_plane[i * 4] != 0, xor_plane[i * 4] != 0);
        let px = &mut rgba[i * 4..i * 4 + 4];
        match (a, x) {
            (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
            (false, true) => px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]),
            (true, false) => {}
            (true, true) => {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
                invert[i] = true;
            }
        }
    }
    grow_invert_outline(&mut rgba, &invert, w, h);
    rgba
}

/// Transparent 8-neighbours of an invert pixel become opaque white. Invert
/// itself stays opaque black. Shared by the monochrome and masked-color paths.
fn grow_invert_outline(rgba: &mut [u8], invert: &[bool], w: usize, h: usize) {
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            if !invert[(y * w as i32 + x) as usize] {
                continue;
            }
            for (dx, dy) in NEIGHBORS {
                let (nx, ny) = (x + dx, y + dy);
                if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                    continue;
                }
                let o = (ny * w as i32 + nx) as usize * 4;
                if rgba[o + 3] == 0 {
                    rgba[o..o + 4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn the_monochrome_truth_table_is_exact() {
        //          (0,0) black  (0,1) white  (1,0) transparent  (1,1) invert
        let and = plane(&[0, 0, 1, 1]);
        let xor = plane(&[0, 1, 0, 1]);
        let out = mono_planes_to_rgba(&and, &xor, 4, 1);
        assert_eq!(px(&out, 0), OPAQUE_BLACK, "AND=0 XOR=0 ⇒ black");
        assert_eq!(px(&out, 1), OPAQUE_WHITE, "AND=0 XOR=1 ⇒ white");
        // Pixel 2 is transparent by the table, but it is an 8-neighbour of the invert pixel at 3,
        // so the outline claims it — that IS the documented behaviour.
        assert_eq!(
            px(&out, 2),
            OPAQUE_WHITE,
            "outline grows into adjacent transparency"
        );
        assert_eq!(px(&out, 3), OPAQUE_BLACK, "AND=1 XOR=1 ⇒ black + outline");
    }

    #[test]
    fn transparent_pixels_stay_transparent_without_an_invert_neighbour() {
        let and = plane(&[1, 1, 1, 1]);
        let xor = plane(&[0, 0, 0, 0]);
        let out = mono_planes_to_rgba(&and, &xor, 4, 1);
        for i in 0..4 {
            assert_eq!(px(&out, i), TRANSPARENT, "pixel {i}");
        }
    }

    #[test]
    fn the_invert_outline_covers_eight_neighbours_and_overwrites_nothing() {
        // 3×3, invert at the centre, everything else transparent.
        let and = plane(&[1, 1, 1, 1, 1, 1, 1, 1, 1]);
        let mut xor = plane(&[0; 9]);
        for b in &mut xor[4 * 4..4 * 4 + 3] {
            *b = 0xFF; // centre pixel's XOR bit
        }
        let out = mono_planes_to_rgba(&and, &xor, 3, 3);
        assert_eq!(px(&out, 4), OPAQUE_BLACK, "the invert pixel itself");
        for i in [0, 1, 2, 3, 5, 6, 7, 8] {
            assert_eq!(px(&out, i), OPAQUE_WHITE, "neighbour {i} outlined");
        }

        // Now surround it with BLACK shape pixels (AND=0, XOR=0): the outline must leave them alone.
        let and = plane(&[0, 0, 0, 0, 1, 0, 0, 0, 0]);
        let out = mono_planes_to_rgba(&and, &xor, 3, 3);
        for i in [0, 1, 2, 3, 5, 6, 7, 8] {
            assert_eq!(
                px(&out, i),
                OPAQUE_BLACK,
                "neighbour {i} must not be repainted"
            );
        }
    }

    #[test]
    fn the_outline_clips_at_the_edges() {
        // 2×2 with the invert at (0, 0): only (1,0), (0,1) and (1,1) can be outlined.
        let and = plane(&[1, 1, 1, 1]);
        let mut xor = plane(&[0; 4]);
        for b in &mut xor[0..3] {
            *b = 0xFF;
        }
        let out = mono_planes_to_rgba(&and, &xor, 2, 2);
        assert_eq!(px(&out, 0), OPAQUE_BLACK);
        for i in [1, 2, 3] {
            assert_eq!(px(&out, i), OPAQUE_WHITE, "in-bounds neighbour {i}");
        }
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
    /// would have dropped it.
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
        // Pixel 2 is transparent by the table, but it is an 8-neighbour of the invert pixel at 3,
        // so the outline claims it — same as the monochrome table.
        assert_eq!(
            px(&out, 2),
            OPAQUE_WHITE,
            "outline grows into adjacent transparency"
        );
        assert_eq!(
            px(&out, 3),
            OPAQUE_BLACK,
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
