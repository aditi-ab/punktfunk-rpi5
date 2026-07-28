//! The streamed display every absolute coordinate maps into (design/pen-tablet-input.md field
//! fix). Pen, touch, and absolute-mouse positions arrive normalized to the STREAMED output's
//! frame, but the injectors historically mapped them over the whole virtual desktop — correct
//! only when the virtual display is the sole active display (Exclusive topology, normalized to
//! origin). In Extend — a physical monitor kept on beside the virtual output, or an Exclusive
//! isolate degraded to the keep-physicals fallback — the streamed output sits at a non-zero
//! origin, so every sample landed shifted and mis-scaled (the pen exposed it first: a stylus is
//! strictly absolute, with no closed-loop correction onto the target like a cursor).
//!
//! The host publishes the streamed output's CCD target id at capture bring-up
//! ([`set_stream_target`]); the mapping sites resolve its CURRENT desktop rect through
//! [`pf_win_display::win_display::source_desktop_rect`] — the same resolver the cursor-readback
//! poller maps frames with, so the two directions always agree — TTL-cached because a
//! group-layout re-arrange moves a live output's origin mid-session. With no target set, or none
//! resolved yet, mapping falls back to the whole virtual desktop: the historical behavior, still
//! correct for Exclusive topology and the client-less devtest paths.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// `(x, y, w, h)` in desktop coordinates, physical pixels (`source_desktop_rect` order).
type Rect = (i32, i32, i32, i32);

/// How long a resolved rect stays fresh: long enough that the CCD query cost vanishes at input
/// rates (pen samples + the 40 ms refresh threads), short enough that a mid-session layout move
/// (a parallel session joining the auto-row) is picked up within a blink.
const RECT_TTL: Duration = Duration::from_millis(250);

struct State {
    target_id: Option<u32>,
    rect: Option<Rect>,
    queried: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    target_id: None,
    rect: None,
    queried: None,
});

/// Publish the streamed output (its CCD target id) that absolute input maps into. The host calls
/// this at capture bring-up; it is never cleared at teardown — a deactivated target simply stops
/// resolving (the last-known rect is kept, and nothing injects between sessions), and the next
/// session's bring-up re-targets. One slot per process: with parallel sessions the LAST bring-up
/// wins for every session's absolute input — per-session routing needs source-tagged input
/// events (parallel-displays plan) and the single slot is never worse than the historical
/// whole-desktop mapping.
pub fn set_stream_target(target_id: Option<u32>) {
    let mut st = STATE.lock().unwrap();
    if st.target_id != target_id {
        tracing::info!(?target_id, "absolute-input stream target set");
        st.target_id = target_id;
        st.rect = None;
        st.queried = None;
    }
}

/// The streamed output's current desktop rect, TTL-cached. `None` = no target set / never
/// resolved (callers fall back to the whole virtual desktop).
fn stream_rect() -> Option<Rect> {
    let mut st = STATE.lock().unwrap();
    let target_id = st.target_id?;
    let fresh = st.queried.is_some_and(|at| at.elapsed() < RECT_TTL);
    if !fresh {
        st.queried = Some(Instant::now());
        match pf_win_display::win_display::source_desktop_rect(target_id) {
            Some(r) => {
                if st.rect != Some(r) {
                    tracing::info!(target_id, rect = ?r, "stream-target desktop rect resolved");
                }
                st.rect = Some(r);
            }
            // Not an active path right now (teardown, or a topology commit in flight): keep the
            // last-known rect — snapping mid-stroke to the whole-desktop mapping would visibly
            // jump, and after teardown nothing injects until the next session re-targets.
            None => {
                if st.rect.is_some() {
                    tracing::debug!(
                        target_id,
                        "stream target not an active path — keeping last rect"
                    );
                }
            }
        }
    }
    st.rect
}

/// Desktop-space pixel for a normalized `[0,1]²` coordinate over the streamed output's rect,
/// falling back to the whole virtual desktop when no stream target is live.
pub(crate) fn map_normalized(nx: f64, ny: f64) -> (i32, i32) {
    map_into(stream_rect().unwrap_or_else(virtual_desktop_rect), nx, ny)
}

/// Pure mapping: `[0,1]²` over `(x, y, w, h)`, inclusive edges (1.0 lands on the last pixel).
fn map_into((x, y, w, h): Rect, nx: f64, ny: f64) -> (i32, i32) {
    (
        x + (nx.clamp(0.0, 1.0) * (w - 1).max(0) as f64).round() as i32,
        y + (ny.clamp(0.0, 1.0) * (h - 1).max(0) as f64).round() as i32,
    )
}

/// The virtual-desktop bounds `(x, y, w, h)` — the mapping fallback, and the surface
/// `MOUSEEVENTF_VIRTUALDESK` absolute coordinates normalize over.
pub(crate) fn virtual_desktop_rect() -> Rect {
    // SAFETY: each `GetSystemMetrics` takes a single by-value `SYSTEM_METRICS_INDEX` constant and
    // returns an `i32`; it dereferences no pointer and has no side effects — FFI-`unsafe` only.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

/// A desktop-space pixel as the `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` 0..65535
/// coordinate pair `SendInput` wants.
pub(crate) fn desktop_px_to_virtualdesk(px: (i32, i32)) -> (i32, i32) {
    px_to_abs(virtual_desktop_rect(), px)
}

/// SendInput absolute coordinates span 0..65535 over the chosen surface.
const ABS_MAX: f64 = 65535.0;

/// Pure normalization: a desktop pixel inside `(x, y, w, h)` → 0..65535 over that surface.
fn px_to_abs((vx, vy, vw, vh): Rect, (px, py): (i32, i32)) -> (i32, i32) {
    (
        ((px - vx) as f64 * ABS_MAX / (vw - 1).max(1) as f64).round() as i32,
        ((py - vy) as f64 * ABS_MAX / (vh - 1).max(1) as f64).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Extend-topology field bug: physical 1920x1080 at (0,0), streamed virtual 2560x1440
    /// beside it at (1920,0) — samples must land inside the VIRTUAL output, not at the desktop
    /// origin.
    #[test]
    fn maps_over_the_streamed_rect_not_the_desktop() {
        let r = (1920, 0, 2560, 1440);
        assert_eq!(map_into(r, 0.0, 0.0), (1920, 0));
        assert_eq!(map_into(r, 1.0, 1.0), (1920 + 2559, 1439));
        assert_eq!(map_into(r, 0.5, 0.5), (1920 + 1280, 720));
    }

    #[test]
    fn clamps_out_of_range_and_handles_negative_origins() {
        // An output placed LEFT of / ABOVE the primary has a negative desktop origin.
        let r = (-2560, -100, 2560, 1440);
        assert_eq!(map_into(r, 0.0, 0.0), (-2560, -100));
        assert_eq!(map_into(r, 2.0, -1.0), (-2560 + 2559, -100));
    }

    #[test]
    fn degenerate_rect_pins_to_its_origin() {
        assert_eq!(map_into((10, 20, 0, 0), 0.7, 0.7), (10, 20));
    }

    /// The VIRTUALDESK round trip: win32k maps an absolute coordinate back to a pixel roughly as
    /// `px = ax * vw / 65536` (floor) — edge pixels and the streamed output's origin must survive.
    #[test]
    fn virtualdesk_normalization_round_trips() {
        let v = (0, 0, 4480, 1080);
        assert_eq!(px_to_abs(v, (0, 0)), (0, 0));
        assert_eq!(px_to_abs(v, (4479, 1079)), (65535, 65535));
        let (ax, _) = px_to_abs(v, (1920, 0));
        assert_eq!((ax as i64 * 4480 / 65536) as i32, 1920);
        // Negative-origin desktops (a monitor left of the primary) still normalize from 0.
        let v = (-2560, 0, 4480, 1440);
        assert_eq!(px_to_abs(v, (-2560, 0)), (0, 0));
    }
}
