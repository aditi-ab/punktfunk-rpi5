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

use super::*;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
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

/// Off-thread GDI cursor poller. Samples `GetCursorInfo` every [`Self::INTERVAL`] (4 ms, ~250 Hz —
/// see that constant for why 16 ms was the bug), rasterises the `HCURSOR` when its handle value
/// changes and when [`Self::EXTENT_PROBE`] catches a resize under a STABLE handle, and publishes a
/// ready [`pf_frame::CursorOverlay`] snapshot; the
/// capture thread's per-tick cost is one uncontended mutex read + an `Arc` clone
/// (same split as [`DescriptorPoller`], and for the same reason: user32/gdi32 calls have no place
/// on the capture/encode thread).
pub(super) struct CursorPoller {
    slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>>,
    stop: Arc<AtomicBool>,
    /// The input desktop is a SECURE desktop (Winlogon — UAC consent / lock / logon). Classified
    /// on every reattach; the capturer polls it to stand the IddCx hardware-cursor declare down
    /// while the secure desktop needs the software-cursor path to render (see
    /// `IddPushCapturer::poll_secure_desktop`).
    secure: Arc<AtomicBool>,
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
    /// 250 ms, not the original 2 s: the reattach now also feeds [`Self::secure_desktop`], which
    /// gates when the secure desktop becomes VISIBLE in the stream (the hardware-cursor
    /// stand-down) — a 2 s freeze at every UAC prompt is user-visible, ~4 `OpenInputDesktop`
    /// syscalls/s are not.
    const REATTACH: Duration = Duration::from_millis(250);
    /// Cadence of the same-handle extent re-probe (see the [`run`] loop). Display-scale changes are
    /// human/OS-timescale events and the probe reads dimensions only — no pixel copy — so 4 Hz is
    /// both ample and negligible, and a ≤250 ms lag on the pointer's size is imperceptible.
    const EXTENT_PROBE: Duration = Duration::from_millis(250);

    /// Spawn the poller for the virtual display `target_id`. `rect` SEEDS the target's desktop rect
    /// (`source_desktop_rect` order: x, y, w, h) — cursor positions are desktop-global; the
    /// overlay wants frame-relative, and a pointer outside the rect reports `visible: false`
    /// (per-output semantics, matching the driver shm path and the Linux portal).
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
        let secure = Arc::new(AtomicBool::new(false));
        let (slot_t, stop_t, secure_t) = (slot.clone(), stop.clone(), secure.clone());
        let thread = std::thread::Builder::new()
            .name("pf-cursor-poll".into())
            .spawn(move || run(ccd, rect, &slot_t, &stop_t, &secure_t))
            .ok();
        if thread.is_none() {
            tracing::warn!("cursor poller thread spawn failed — cursor falls back to driver shm");
        }
        Self {
            slot,
            stop,
            secure,
            thread,
        }
    }

    /// The latest overlay snapshot (`None` until the first successful shape rasterisation).
    pub(super) fn read(&self) -> Option<pf_frame::CursorOverlay> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Whether the input desktop is currently a SECURE desktop (UAC consent / Winlogon lock or
    /// logon). Latched by the poll thread on its reattach cadence (≤ [`Self::REATTACH`] stale).
    pub(super) fn secure_desktop(&self) -> bool {
        self.secure.load(Ordering::Relaxed)
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
    ccd: pf_win_display::win_display::CcdTargetKey,
    mut rect: (i32, i32, i32, i32),
    slot: &Mutex<Option<pf_frame::CursorOverlay>>,
    stop: &AtomicBool,
    secure: &AtomicBool,
) {
    // Physical-pixel coordinates on this thread regardless of the process's DPI awareness:
    // `rect` comes from CCD (always physical), and a DPI-virtualized `GetCursorInfo` position
    // would land in the wrong frame pixel on any scaled display. Thread-scoped, so the rest of
    // the host is untouched.
    // SAFETY: takes and returns only a by-value context handle; affects this thread only.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let mut desktop = DesktopBinding::default();
    // best-effort: already on winsta0\default if this fails
    publish_secure(secure, desktop.reattach());
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
            publish_secure(secure, desktop.reattach());
            // …and re-read the target's desktop rect on the same cadence: a mid-session resize (or
            // an HDR recreate, or the user moving this display in the desktop arrangement) changes
            // BOTH the origin the position is made relative to and the extent `in_rect` tests
            // against, and this poller outlives all of them. `None` keeps the last good value — a
            // transient CCD failure must not park the pointer at a `(0, 0, 0, 0)` rect, which would
            // report every position invisible.
            let fresh = pf_win_display::win_display::source_desktop_rect(ccd);
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
            // Desktop went away under us (secure-desktop switch mid-call) — rebind and retry
            // next tick; the slot keeps its last snapshot meanwhile.
            publish_secure(secure, desktop.reattach());
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

        // …but the handle alone CANNOT see a re-render. Windows rebuilds the system cursors at a
        // new size whenever the scale under the pointer changes — crossing to a differently-scaled
        // monitor, or a monitor's own scale settling after a mode change (a fresh virtual display
        // is created at the RECOMMENDED scale and gets the client's saved `PerMonitorSettings`
        // override a beat later) — while the SHARED handle stays put for the session's life (the
        // arrow is 0x10003 throughout). Keyed on the handle alone the cache latched whatever size
        // the pointer happened to have when the poller started and never let go: a session that
        // sampled inside that pre-settle window forwarded — and composited — a 96 px pointer over
        // a 100 % desktop until it ended, which is exactly the "cursor is 3× too big while
        // everything else is fine" report. Re-read the bitmap's EXTENT on a slow cadence
        // (dimensions only, no pixel copy) and drop the cache when it moved.
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
                // `handle != 0` is part of "visible", not just of "worth rasterising": `SetCursor(NULL)`
                // — how a game or a video player hides the pointer for its own window — leaves
                // `CURSOR_SHOWING` set with a NULL `hCursor`. Judging on the flags alone published
                // `visible: true` carrying the last shape we rasterised, so the composite path blended a
                // ghost arrow into a game that had hidden its cursor, and the forward path told the
                // client to draw one too. Every rasterise gate below already tests this; the published
                // verdict has to agree with them.
                visible: showing && in_rect && handle != 0,
            }
        });
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = overlay;
    }
}

/// Store a reattach's secure-desktop verdict (`None` = classification unavailable — keep the
/// previous state rather than flapping the capturer's hardware-cursor stand-down).
fn publish_secure(secure: &AtomicBool, verdict: Option<bool>) {
    if let Some(s) = verdict {
        secure.store(s, Ordering::Relaxed);
    }
}

/// The thread's owned input-desktop handle — the [`SendInputInjector`] reattach model
/// (`pf-inject` sendinput.rs): keep the current binding, swap on demand, close exactly once.
#[derive(Default)]
struct DesktopBinding(Option<HDESK>);

impl DesktopBinding {
    /// Rebind to the CURRENT input desktop. Returns whether that desktop is a SECURE one
    /// (`UOI_NAME` != "Default": "Winlogon" during UAC consent / lock / logon) — `None` when the
    /// input desktop could not be opened, in which case the binding (and the caller's secure
    /// state) stays put.
    fn reattach(&mut self) -> Option<bool> {
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
                    let secure = desktop_is_secure(h);
                    if SetThreadDesktop(h).is_ok() {
                        if let Some(old) = self.0.replace(h) {
                            let _ = CloseDesktop(old);
                        }
                    } else {
                        let _ = CloseDesktop(h);
                    }
                    Some(secure)
                }
                Err(_) => None, // not privileged for this desktop; stay put
            }
        }
    }
}

/// `UOI_NAME` of `h` != "Default" — i.e. the input desktop is Winlogon (UAC consent / lock /
/// logon) or a screen-saver desktop, both of which need the OS's software-cursor render path.
/// Unnameable desktops read as NOT secure: the only in-contract failure is a too-small buffer,
/// and misreading secure-as-normal merely keeps today's behavior for a beat.
fn desktop_is_secure(h: HDESK) -> bool {
    let mut name = [0u16; 64]; // "Default"/"Winlogon"/"Screen-saver" all fit with room to spare
    let mut needed = 0u32;
    // SAFETY: `h` is the live desktop handle the caller just opened; `name`/`needed` are live
    // out-params sized exactly as passed; the call writes at most `nlength` bytes.
    let ok = unsafe {
        GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(h.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
    };
    if ok.is_err() {
        return false;
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    let name = String::from_utf16_lossy(&name[..len]);
    !name.eq_ignore_ascii_case("Default")
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

/// The CURRENT bitmap extent of `hcursor` — exactly the `(w, h)` [`convert`] would derive, without
/// the pixel read. Feeds the poll loop's staleness check (a re-render keeps the handle, see there).
/// `None` on any failure, which the caller reads as "no verdict" and keeps its cached shape.
fn cursor_extent(hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> Option<(u32, u32)> {
    // CopyIcon first, for the reason `rasterize` does it: the owning process can destroy its
    // HCURSOR between GetCursorInfo and the reads below; the copy is ours.
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

/// A GDI bitmap's dimensions, under [`read_bitmap_32`]'s sanity caps so the two agree on what a
/// plausible cursor is — a bitmap `rasterize` would reject must not read here as a size CHANGE.
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

/// Convert the ICONINFO bitmaps to straight RGBA. Two families:
/// - color (`hbmColor` set): 32bpp BGRA; if the alpha channel is entirely empty (old-style
///   masked-color cursors, including Windows 11's coloured I-beam) the AND mask is NOT just
///   transparency — it is the same four-state table as monochrome, with the colour bitmap
///   standing in for the XOR plane. Treating AND=1 as "always transparent" drops invert
///   pixels, and the text I-beam is almost entirely invert, so the client would install a
///   fully-transparent pointer over every text field.
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
            if alpha_is_empty(&rgba) {
                // Alpha-less color cursor: AND + colour-as-XOR, including invert.
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

/// Whether a 32bpp RGBA buffer's alpha channel is entirely zero — the "old-style cursor with no
/// alpha" test, whose transparency (and invert) live in the AND mask instead
/// ([`masked_color_to_rgba`]).
fn alpha_is_empty(rgba: &[u8]) -> bool {
    rgba.chunks_exact(4).all(|p| p[3] == 0)
}

/// Take alpha from an expanded AND mask: mask WHITE (AND bit 1) means transparent, black opaque.
/// `mask_bgra` is the 32bpp expansion `GetDIBits` produces from the 1bpp mask, so any non-zero
/// channel byte is "set".
///
/// Test-only: the simple AND-as-alpha helper (no invert). [`convert`] uses
/// [`masked_color_to_rgba`] instead — AND=1 plus a non-zero colour pixel is invert, not
/// transparent, and that is the I-beam.
#[cfg(test)]
fn apply_and_mask_alpha(rgba: &mut [u8], mask_bgra: &[u8]) {
    for (px, m) in rgba.chunks_exact_mut(4).zip(mask_bgra.chunks_exact(4)) {
        px[3] = if m[0] != 0 { 0 } else { 0xFF };
    }
}

/// Alpha-less colour cursor: the AND mask plus the colour bitmap as XOR, same four states as
/// [`mono_planes_to_rgba`]. A non-zero RGB with AND=1 is invert — which treating AND=1 as
/// "always transparent" would have dropped, vanishing the I-beam.
///
/// `(false, true)` keeps the colour (a painted glyph), unlike the monochrome table which can
/// only emit white.
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
            (true, false) => {} // transparent (already zeroed)
            (true, true) => {
                px.copy_from_slice(&[0, 0, 0, 0xFF]);
                invert[i] = true;
            }
        }
    }
    grow_invert_outline(&mut rgba, &invert, w, h);
    rgba
}

/// The monochrome-cursor truth table, plus the white outline that makes an INVERT region legible.
///
/// A monochrome `HCURSOR` has no colour bitmap: `hbmMask` is DOUBLE height — the AND plane over the
/// XOR plane — and the pair encodes four states (the WebRTC/Chromium table):
///
/// | AND | XOR | meaning     | straight-alpha result                    |
/// |-----|-----|-------------|------------------------------------------|
/// | 0   | 0   | black       | opaque black                             |
/// | 0   | 1   | white       | opaque white                             |
/// | 1   | 0   | transparent | fully transparent                        |
/// | 1   | 1   | INVERT dst  | opaque black + a grown white outline     |
///
/// INVERT is unrepresentable in straight alpha (it is a per-pixel XOR against whatever is behind
/// it), so it becomes opaque black and every TRANSPARENT 8-neighbour of an invert pixel is turned
/// opaque white. That outline is what keeps the text I-beam — which is almost entirely invert
/// pixels — legible over dark content; the earlier translucent-grey stand-in did not.
///
/// Extracted from `convert`'s GDI plumbing (sweep Phase 6.6) so the table is testable: the caller
/// needs a live `HCURSOR` and a screen DC, this needs two byte slices.
fn mono_planes_to_rgba(and_plane: &[u8], xor_plane: &[u8], w: usize, h: usize) -> Vec<u8> {
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
    grow_invert_outline(&mut rgba, &invert, w, h);
    rgba
}

/// Turn every TRANSPARENT 8-neighbour of an invert pixel opaque white. Invert itself stays
/// opaque black. Shared by the monochrome and masked-color converters.
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

    /// Expand a 1-bit-per-pixel plane (as `GetDIBits` does) into the 32bpp form the converters read:
    /// any non-zero channel byte means "bit set".
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

    /// All four AND/XOR states, in one 4×1 row — the table `mono_planes_to_rgba` documents.
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

    /// Transparency survives when there is no invert pixel next to it.
    #[test]
    fn transparent_pixels_stay_transparent_without_an_invert_neighbour() {
        let and = plane(&[1, 1, 1, 1]);
        let xor = plane(&[0, 0, 0, 0]);
        let out = mono_planes_to_rgba(&and, &xor, 4, 1);
        for i in 0..4 {
            assert_eq!(px(&out, i), TRANSPARENT, "pixel {i}");
        }
    }

    /// The outline grows into all eight neighbours, and only into TRANSPARENT ones — it must not
    /// repaint a black or white shape pixel.
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

    /// The outline must clip at the bitmap edges rather than wrap to the opposite side.
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

    // ---- the alpha-less colour path ---------------------------------------------------------

    #[test]
    fn an_empty_alpha_channel_is_detected() {
        assert!(alpha_is_empty(&[1, 2, 3, 0, 4, 5, 6, 0]));
        assert!(!alpha_is_empty(&[1, 2, 3, 0, 4, 5, 6, 1]));
        assert!(alpha_is_empty(&[]), "no pixels ⇒ vacuously empty");
    }

    /// Mask WHITE (AND bit 1) = transparent, black = opaque — and the colour bytes are untouched.
    #[test]
    fn the_and_mask_supplies_alpha_for_an_alpha_less_cursor() {
        let mut rgba = vec![
            10, 20, 30, 0, // pixel 0
            40, 50, 60, 0, // pixel 1
        ];
        let mask = plane(&[1, 0]); // pixel 0 masked out, pixel 1 kept
        apply_and_mask_alpha(&mut rgba, &mask);
        assert_eq!(px(&rgba, 0), [10, 20, 30, 0], "masked ⇒ transparent");
        assert_eq!(px(&rgba, 1), [40, 50, 60, 0xFF], "unmasked ⇒ opaque");
    }

    /// A mask with FEWER pixels than the colour bitmap must not panic — `zip` stops at the shorter
    /// side, leaving the tail at whatever alpha it had (the caller has already required
    /// `mask.h >= color.h`, so this is the belt).
    #[test]
    fn a_short_mask_does_not_panic() {
        let mut rgba = vec![1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0];
        apply_and_mask_alpha(&mut rgba, &plane(&[0]));
        assert_eq!(px(&rgba, 0), [1, 2, 3, 0xFF]);
        assert_eq!(px(&rgba, 1), [4, 5, 6, 0]);
    }

    // ---- masked-color (colour bitmap as XOR) ------------------------------------------------

    /// The four-state table, with colour standing in for XOR. Pixel 3 is the I-beam case:
    /// AND=1 and a non-zero colour pixel is invert, not transparent — `apply_and_mask_alpha`
    /// would have dropped it, which is how the text cursor vanished on a Windows host.
    #[test]
    fn a_masked_color_invert_pixel_is_not_transparent() {
        //          (0,0) black  (0,1) red    (1,0) transparent  (1,1) invert
        let color = vec![
            0, 0, 0, 0, //
            0xCC, 0, 0, 0, //
            0, 0, 0, 0, //
            0xFF, 0xFF, 0xFF, 0, //
        ];
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

    /// AND=1 and a zero colour pixel stays transparent when nothing invert-neighbours it.
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
