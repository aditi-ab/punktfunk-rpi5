//! GDI cursor poller — the Windows cursor-SHAPE source for the cursor-forward channel
//! (design/remote-desktop-sweep.md §8, the M2c redesign).
//!
//! Why not the IddCx hardware-cursor query (the v5 `CursorShm` path, now the fallback): it is
//! alpha-only BY DESIGN — `IDDCX_CURSOR_SHAPE_TYPE` has no monochrome value, the OS pre-converts
//! monochrome to masked-color (IDDCX_CURSOR_CAPS docs), and masked-color delivery is dead code on
//! modern builds (proven on-glass at every `ColorXorCursorSupport` level; no public evidence of a
//! MASKED_COLOR delivery anywhere). The driver keeps its hardware cursor declared at XOR FULL
//! purely so DWM EXCLUDES every cursor type from the IDD frame.
//!
//! Why not DXGI Desktop Duplication `GetFramePointerShape`: its `PointerPosition.Visible` goes
//! stale when the cursor moves only via injected input on current Win11 (Sunshine #5293 — exactly
//! a Punktfunk session's topology), it burns one of the session's four duplication slots, and its
//! per-output metadata on IDD monitors has conflicting field reports. The GDI path below is the
//! metadata-forwarding-remote-desktop pattern (RustDesk, WebRTC/Chrome Remote Desktop, OBS): the
//! cursor is per-session global state in win32k, readable cross-process, and `CURSOR_SHOWING` is
//! the logical visibility — immune to all of the above.
//!
//! Works because the capture host runs as SYSTEM *inside the interactive session* on
//! `winsta0\default` (the service supervisor retargets the token — `windows/service.rs`
//! `spawn_host`), so the poller thread sees the session's cursor directly; no helper process.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

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

/// `CURSORINFO.flags` bits (WindowsAndMessaging): the pointer is logically shown /
/// touch-or-pen-suppressed. Named locally so the visibility rule below reads as the docs do.
const CURSOR_SHOWING: u32 = 0x1;
const CURSOR_SUPPRESSED: u32 = 0x2;

/// A converted shape: the cache the per-tick overlay is assembled from. `rgba` is `Arc` so the
/// slot publish (and every downstream frame attach) is a refcount bump.
struct Shape {
    rgba: std::sync::Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    serial: u64,
}

/// Off-thread GDI cursor poller. Samples `GetCursorInfo` at ~60 Hz, rasterises the `HCURSOR` only
/// when its handle value changes, and publishes a ready [`pf_frame::CursorOverlay`] snapshot; the
/// capture thread's per-tick cost is one uncontended mutex read + an `Arc` clone
/// (same split as [`DescriptorPoller`], and for the same reason: user32/gdi32 calls have no place
/// on the capture/encode thread).
pub(super) struct CursorPoller {
    slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CursorPoller {
    /// ~250 Hz: the polled position is ALSO the composite-blend position (capture model), so
    /// it must out-pace the fastest session — at 16 ms a 240 fps stream re-used a stale
    /// position for ~4 consecutive frames and the composited pointer visibly stuttered
    /// against the video. A tick is one `GetCursorInfo` syscall (rasterisation only on shape
    /// change), so 250 Hz is still negligible CPU.
    const INTERVAL: Duration = Duration::from_millis(4);
    /// Unconditional input-desktop reattach cadence — catches secure-desktop (UAC/lock) switches
    /// without a failure signal (`GetCursorInfo` on a stale desktop *succeeds* with stale data).
    const REATTACH: Duration = Duration::from_secs(2);

    /// Spawn the poller for the virtual display `target_id`. `rect` = the target's desktop rect
    /// (`source_desktop_rect` order: x, y, w, h) — cursor positions are desktop-global; the
    /// overlay wants frame-relative, and a pointer outside the rect reports `visible: false`
    /// (per-output semantics, matching the driver shm path and the Linux portal).
    pub(super) fn spawn(target_id: u32, rect: (i32, i32, i32, i32)) -> Self {
        let slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (slot_t, stop_t) = (slot.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("pf-cursor-poll".into())
            .spawn(move || run(target_id, rect, &slot_t, &stop_t))
            .ok();
        if thread.is_none() {
            tracing::warn!("cursor poller thread spawn failed — cursor falls back to driver shm");
        }
        Self { slot, stop, thread }
    }

    /// The latest overlay snapshot (`None` until the first successful shape rasterisation).
    pub(super) fn read(&self) -> Option<pf_frame::CursorOverlay> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Whether the worker thread is (still) alive — `false` degrades the capturer to the shm read.
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

/// The poll loop. Owns the thread's input-desktop binding and the shape cache.
fn run(
    target_id: u32,
    rect: (i32, i32, i32, i32),
    slot: &Mutex<Option<pf_frame::CursorOverlay>>,
    stop: &AtomicBool,
) {
    // Physical-pixel coordinates on this thread regardless of the process's DPI awareness:
    // `rect` comes from CCD (always physical), and a DPI-virtualized `GetCursorInfo` position
    // would land in the wrong frame pixel on any scaled display. Thread-scoped, so the rest of
    // the host is untouched.
    // SAFETY: takes and returns only a by-value context handle; affects this thread only.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let mut desktop = DesktopBinding::default();
    desktop.reattach(); // best-effort: already on winsta0\default if this fails
    let mut last_attach = Instant::now();

    let mut shape: Option<Shape> = None;
    let mut cached_handle: isize = 0;
    let mut failed_handle: isize = 0; // don't re-rasterise a failing handle every tick
    let mut serial: u64 = 0;
    let mut logged_live = false;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(CursorPoller::INTERVAL);
        if last_attach.elapsed() >= CursorPoller::REATTACH {
            last_attach = Instant::now();
            desktop.reattach();
        }

        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `ci` is a live, correctly-sized out-param for this synchronous call; no pointer
        // escapes it.
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            // Desktop went away under us (secure-desktop switch mid-call) — rebind and retry
            // next tick; the slot keeps its last snapshot meanwhile.
            desktop.reattach();
            last_attach = Instant::now();
            continue;
        }

        let flags = ci.flags.0;
        let showing = flags & CURSOR_SHOWING != 0 && flags & CURSOR_SUPPRESSED == 0;

        // Rasterise on handle change only (position-only ticks are a header update). Hidden
        // cursors keep the cached shape — the forwarder's hidden-but-known contract needs the
        // bitmap to have been seen. v1: animated cursors publish their first frame (the OBS
        // behavior); frame cycling via DrawIconEx istep is a known follow-up.
        let handle = ci.hCursor.0 as isize;
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
                            target_id,
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
                visible: showing && in_rect,
            }
        });
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = overlay;
    }
}

/// The thread's owned input-desktop handle — the [`SendInputInjector`] reattach model
/// (`pf-inject` sendinput.rs): keep the current binding, swap on demand, close exactly once.
#[derive(Default)]
struct DesktopBinding(Option<HDESK>);

impl DesktopBinding {
    fn reattach(&mut self) {
        const GENERIC_ALL: u32 = 0x1000_0000;
        // SAFETY: `OpenInputDesktop`/`SetThreadDesktop`/`CloseDesktop` take only by-value args.
        // `OpenInputDesktop` yields an owned `HDESK` only on `Ok`; it is either installed (and the
        // previously-owned handle closed exactly once) or closed on failure — no handle is leaked
        // or used after close. `SetThreadDesktop` rebinds only this calling thread (which owns
        // no windows/hooks, so the rebind cannot fail on that account).
        unsafe {
            match OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            ) {
                Ok(h) => {
                    if SetThreadDesktop(h).is_ok() {
                        if let Some(old) = self.0.replace(h) {
                            let _ = CloseDesktop(old);
                        }
                    } else {
                        let _ = CloseDesktop(h);
                    }
                }
                Err(_) => { /* not privileged for this desktop; stay put */ }
            }
        }
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

/// Rasterise `hcursor` to straight-alpha RGBA: `(rgba, w, h, hot_x, hot_y)`. `None` on any
/// failure (caller keeps the previous shape).
fn rasterize(hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> RasterOut {
    // CopyIcon first: the owning process can destroy its HCURSOR between GetCursorInfo and the
    // reads below; the copy is ours (the OBS/WebRTC guard).
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

/// Convert the ICONINFO bitmaps to straight RGBA. Two families:
/// - color (`hbmColor` set): 32bpp BGRA; if the alpha channel is entirely empty (old-style
///   cursors) the AND mask supplies it (mask bit 1 = transparent).
/// - monochrome (`hbmColor` null): `hbmMask` is DOUBLE height — AND plane over XOR plane, the
///   WebRTC truth table: (0,0) black, (0,1) white, (1,0) transparent, (1,1) invert. Invert
///   pixels — unrepresentable in straight alpha — become opaque black with a white outline
///   grown into adjacent transparency (the WebRTC approximation; keeps the I-beam legible on
///   any background, which the old translucent-gray stand-in did not).
fn convert(ii: &ICONINFO) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: GetDC(None) yields the screen DC, released below on every path; it is only used
    // as the GetDIBits reference DC.
    let dc = unsafe { GetDC(None) };
    let result = (|| {
        if !ii.hbmColor.is_invalid() {
            let color = read_bitmap_32(dc, ii.hbmColor)?;
            let (w, h) = (color.w as u32, color.h as u32);
            let mut rgba = bgra_to_rgba(&color.bgra);
            if rgba.chunks_exact(4).all(|p| p[3] == 0) {
                // Alpha-less color cursor: transparency lives in the AND mask.
                let mask = read_bitmap_32(dc, ii.hbmMask)?;
                if mask.w != color.w || mask.h < color.h {
                    return None;
                }
                for (px, m) in rgba.chunks_exact_mut(4).zip(mask.bgra.chunks_exact(4)) {
                    px[3] = if m[0] != 0 { 0 } else { 0xFF }; // mask white (AND=1) = transparent
                }
            }
            Some((rgba, w, h))
        } else {
            let mask = read_bitmap_32(dc, ii.hbmMask)?;
            if mask.h < 2 || mask.h % 2 != 0 {
                return None;
            }
            let (w, h) = (mask.w as usize, (mask.h / 2) as usize);
            let row = w * 4;
            let (and_plane, xor_plane) = mask.bgra.split_at(h * row);
            let mut rgba = vec![0u8; w * h * 4];
            let mut invert = vec![false; w * h];
            for i in 0..w * h {
                let (a, x) = (and_plane[i * 4] != 0, xor_plane[i * 4] != 0);
                let px = &mut rgba[i * 4..i * 4 + 4];
                match (a, x) {
                    (false, false) => px.copy_from_slice(&[0, 0, 0, 0xFF]),
                    (false, true) => px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]),
                    (true, false) => {} // transparent (already zeroed)
                    (true, true) => {
                        px.copy_from_slice(&[0, 0, 0, 0xFF]);
                        invert[i] = true;
                    }
                }
            }
            // White outline around invert regions so the (now black) shape survives dark
            // backgrounds: any transparent 8-neighbor of an invert pixel turns opaque white.
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

/// Read any GDI bitmap as 32bpp top-down via `GetDIBits` (which performs the 1bpp→32bpp
/// expansion for the mask planes).
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
