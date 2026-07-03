//! The dedicated video render thread: decoded frames flow session pump → bounded channel → here →
//! `Presenter::present`. Presenting off the XAML thread means UI jank (layout, input, dialogs)
//! never stalls video, and a filled present queue never blocks the UI thread — the two failure
//! modes of the old present-from-`on_rendering` design.
//!
//! Pacing: block on the channel (the host paces the stream), then on the swapchain's
//! frame-latency waitable (≤1 queued present — see `present.rs`), then drain to the NEWEST frame
//! so a stream faster than the display drops backlog before any GPU work. The UI thread only
//! writes panel size/DPI into [`RenderShared`] atomics; the loop applies them before the next
//! draw (and redraws the held frame after a resize — fresh back buffers are blank).

use crate::present::Presenter;
use crate::session::{FrameRx, FrameTimes};
use crossbeam_channel::RecvTimeoutError;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The last 1-second render window, published for the HUD (one render thread at a time):
/// presents/s, frames skipped by the newest-wins drain, the end-to-end (capture→on-glass)
/// p50/p95 and the `display` stage (decoded→displayed) p50, all stamped post-`Present()`, in µs.
/// Zeroed when a render thread starts so a new session never shows the previous one's numbers.
static PRESENT_FPS: AtomicU32 = AtomicU32::new(0);
static PRESENT_SKIPPED: AtomicU32 = AtomicU32::new(0);
static E2E_P50_US: AtomicU64 = AtomicU64::new(0);
static E2E_P95_US: AtomicU64 = AtomicU64::new(0);
static DISPLAY_P50_US: AtomicU64 = AtomicU64::new(0);

/// The last render window's glass-side numbers (see the statics above) — the HUD's headline
/// (end-to-end) and trailing stage (display) come from here.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct PresentStats {
    /// Presents per second (includes resize redraws of a held frame).
    pub fps: u32,
    /// Frames dropped by the newest-wins drain this window (client-side pacing skips).
    pub skipped: u32,
    /// End-to-end capture→displayed p50, ms (host-clock corrected, measured directly).
    pub e2e_p50_ms: f32,
    /// End-to-end capture→displayed p95, ms.
    pub e2e_p95_ms: f32,
    /// `display` stage p50, ms: decoded → displayed, single-clock client-local.
    pub display_p50_ms: f32,
}

pub fn present_stats() -> PresentStats {
    PresentStats {
        fps: PRESENT_FPS.load(Ordering::Relaxed),
        skipped: PRESENT_SKIPPED.load(Ordering::Relaxed),
        e2e_p50_ms: E2E_P50_US.load(Ordering::Relaxed) as f32 / 1000.0,
        e2e_p95_ms: E2E_P95_US.load(Ordering::Relaxed) as f32 / 1000.0,
        display_p50_ms: DISPLAY_P50_US.load(Ordering::Relaxed) as f32 / 1000.0,
    }
}

/// UI-thread → render-thread state. Size is packed into ONE atomic (w<<32|h) so a resize never
/// tears into a (new-width, old-height) pair.
pub struct RenderShared {
    size_px: AtomicU64,
    dpi: AtomicU32,
    stop: AtomicBool,
}

impl RenderShared {
    pub fn new(width: u32, height: u32, dpi: u32) -> Arc<RenderShared> {
        Arc::new(RenderShared {
            size_px: AtomicU64::new(pack(width, height)),
            dpi: AtomicU32::new(dpi),
            stop: AtomicBool::new(false),
        })
    }

    pub fn set_size(&self, width: u32, height: u32) {
        self.size_px.store(pack(width, height), Ordering::Relaxed);
    }

    pub fn set_dpi(&self, dpi: u32) {
        self.dpi.store(dpi, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u32, u32, u32) {
        let s = self.size_px.load(Ordering::Relaxed);
        ((s >> 32) as u32, s as u32, self.dpi.load(Ordering::Relaxed))
    }
}

fn pack(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}

/// Handle owned by the stream page; stops + joins the thread on unmount (and on drop, so a
/// navigation away can't leak a presenting thread).
pub struct RenderThread {
    shared: Arc<RenderShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RenderThread {
    pub fn shared(&self) -> &Arc<RenderShared> {
        &self.shared
    }

    pub fn stop_and_join(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for RenderThread {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// Moves the presenter (COM interfaces, `!Send` by default) onto the render thread. Sound here:
/// the shared device + immediate context are multithread-protected (see `crate::gpu`), D3D/DXGI
/// objects are apartment-agile, and after this one handoff the swapchain/RTV/context calls happen
/// on exactly the render thread — the same single-owner discipline as `SharedDevice`.
struct SendPresenter(Presenter);
unsafe impl Send for SendPresenter {}

/// Spawn the render thread. `frames` carries `(frame, FrameTimes)`; `clock_offset_ns` maps our
/// wall clock onto the host's so the end-to-end (capture→on-glass) number is cross-machine valid
/// (same math as the pump's host+network stage).
pub fn spawn(
    presenter: Presenter,
    frames: FrameRx,
    shared: Arc<RenderShared>,
    clock_offset_ns: i64,
) -> RenderThread {
    let boxed = SendPresenter(presenter);
    let shared_w = shared.clone();
    let join = std::thread::Builder::new()
        .name("pf-render".into())
        .spawn(move || run(boxed, frames, shared_w, clock_offset_ns))
        .expect("spawn render thread");
    RenderThread {
        shared,
        join: Some(join),
    }
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The window DPI, polled ~1 Hz as belt-and-braces for a monitor move that changes DPI without a
/// `SizeChanged` (same DIP size on both screens). `None` when the window isn't up (headless).
fn poll_window_dpi() -> Option<u32> {
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::FindWindowW;
    unsafe {
        let hwnd = FindWindowW(None, windows::core::w!("Punktfunk")).ok()?;
        match GetDpiForWindow(hwnd) {
            0 => None,
            d => Some(d),
        }
    }
}

fn run(presenter: SendPresenter, frames: FrameRx, shared: Arc<RenderShared>, clock_offset_ns: i64) {
    let mut p = presenter.0;
    let mut applied = (0u32, 0u32, 0u32); // last (w, h, dpi) handed to the presenter
    let mut presented = 0u32;
    let mut dropped = 0u32;
    // 1 s tumbling windows: end-to-end (capture→displayed) and the display stage
    // (decoded→displayed), sampled post-Present. Percentiles only (spec: stats-unification.md).
    let mut e2e_us: Vec<u64> = Vec::with_capacity(256);
    let mut display_us: Vec<u64> = Vec::with_capacity(256);
    let mut window_start = Instant::now();
    let mut last_dpi_poll = Instant::now();
    PRESENT_FPS.store(0, Ordering::Relaxed);
    PRESENT_SKIPPED.store(0, Ordering::Relaxed);
    E2E_P50_US.store(0, Ordering::Relaxed);
    E2E_P95_US.store(0, Ordering::Relaxed);
    DISPLAY_P50_US.store(0, Ordering::Relaxed);

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let first = match frames.recv_timeout(Duration::from_millis(50)) {
            Ok(f) => Some(f),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        if last_dpi_poll.elapsed() >= Duration::from_secs(1) {
            last_dpi_poll = Instant::now();
            if let Some(dpi) = poll_window_dpi() {
                shared.set_dpi(dpi);
            }
        }
        let snap = shared.snapshot();
        let resized = snap != applied && snap.0 > 0 && snap.1 > 0;
        if resized {
            p.resize(snap.0, snap.1, snap.2);
            applied = snap;
        }
        if first.is_none() && !resized {
            continue; // nothing new to show — don't burn GPU re-presenting a static frame
        }

        // Throttle to the compositor: with ≤1 present outstanding this returns as DWM frees a
        // slot, and frames decoded meanwhile are drained below so the newest is what's drawn.
        if !p.wait_present_slot(1000) {
            tracing::debug!("frame-latency waitable timed out — presenting anyway");
        }
        let mut newest = first;
        while let Ok(f) = frames.try_recv() {
            if newest.is_some() {
                dropped += 1;
            }
            newest = Some(f);
        }

        // The session pump is the sole 0xCE consumer and stashes the latest here (rare updates).
        if let Some(meta) = *crate::present::LATEST_HDR_META.lock().unwrap() {
            p.set_hdr_metadata(meta);
        }

        let times: Option<FrameTimes> = newest.as_ref().map(|(_, t)| *t);
        p.present(newest.map(|(f, _)| f));
        presented += 1;
        if let Some(t) = times {
            // The `displayed` point: post-Present() on this thread (the honest best-effort
            // presentation instant on Windows — endpoint label `capture→on-glass`).
            let displayed_ns = now_ns();
            // End-to-end = capture → displayed, host-clock corrected, measured directly
            // (never the sum of stage percentiles). Clamped (0, 10 s).
            let e2e =
                (displayed_ns as i128 + clock_offset_ns as i128 - t.pts_ns as i128).max(0) as u64;
            if e2e > 0 && e2e < 10_000_000_000 {
                e2e_us.push(e2e / 1000);
            }
            // `display` stage = decoded → displayed, single-clock client-local.
            let disp = displayed_ns.saturating_sub(t.decoded_ns);
            if disp < 10_000_000_000 {
                display_us.push(disp / 1000);
            }
        }

        if window_start.elapsed() >= Duration::from_secs(1) {
            e2e_us.sort_unstable();
            display_us.sort_unstable();
            let p50 = |v: &[u64]| v.get(v.len() / 2).copied().unwrap_or(0);
            // p95 = sorted[min(len*95/100, len-1)] — the empty-window case falls to 0 via `get`.
            let p95 = |v: &[u64]| {
                v.get((v.len() * 95 / 100).min(v.len().saturating_sub(1)))
                    .copied()
                    .unwrap_or(0)
            };
            tracing::debug!(
                presented,
                dropped,
                e2e_p50_us = p50(&e2e_us),
                e2e_p95_us = p95(&e2e_us),
                display_p50_us = p50(&display_us),
                "render window"
            );
            PRESENT_FPS.store(presented, Ordering::Relaxed);
            PRESENT_SKIPPED.store(dropped, Ordering::Relaxed);
            E2E_P50_US.store(p50(&e2e_us), Ordering::Relaxed);
            E2E_P95_US.store(p95(&e2e_us), Ordering::Relaxed);
            DISPLAY_P50_US.store(p50(&display_us), Ordering::Relaxed);
            window_start = Instant::now();
            presented = 0;
            dropped = 0;
            e2e_us.clear();
            display_us.clear();
        }
    }
    tracing::info!("render thread exiting");
}
