//! The LAST-RESORT DWM compose kick — synthetic pointer input that dirties a specific virtual
//! display so DWM presents it.
//!
//! Split out of `idd_push.rs` in sweep Phase 5.4. It is self-contained (one function plus a
//! process-global throttle) and it is the one piece of the capture path that reaches for synthetic
//! INPUT, which is worth keeping visibly separate from the frame machinery: it is unreliable by
//! nature, user-visible in the sibling-display case, and only ever a fallback for the driver's own
//! `FrameStash` republish.

use super::*;

/// LAST-RESORT fallback: nudge DWM into composing THE TARGET virtual display. DWM presents a
/// display only when something DIRTIES it, so a freshly-attached ring over an idle desktop can
/// sit at E_PENDING forever. The PRIMARY first-frame mechanism is the driver's `FrameStash`
/// republish; this kick remains for pre-stash drivers and the never-composed cold start.
/// Synthetic input is inherently unreliable (secure desktop, ClipCursor, user-visible on a
/// sibling display), which is why it is the fallback.
///
/// The cursor only dirties the display it is ON (proven on-glass, Stage W3), so the kick is
/// per-TARGET: inside the target's desktop region, two net-zero 1 px relative moves; on a
/// SIBLING display, `SetCursorPos` to the target's center and back — each absolute move dirties
/// the display it lands on. Best-effort on the secure desktop, where a fresh compose just
/// happened anyway.
///
/// **COST:** the sibling-display branch SLEEPS 35 ms on the calling (capture/encode) thread —
/// the dwell is load-bearing (a sub-tick jump-and-return never dirties anything). Every call
/// site is a first-frame or recovery window with no frames flowing, and the global 50 ms
/// throttle plus the callers' 600–800 ms schedules bound the rate.
///
/// **HID-first**: a registered [`HID_COMPOSE_KICK`] (the pf-mouse virtual HID pointer) replaces
/// the `SendInput` paths — a HID report is real input to win32k regardless of session or active
/// desktop, wakes a powered-off display subsystem, and counts as user presence: every condition
/// under which `SendInput` is silently impotent (the lid-closed field-report state).
pub(super) fn kick_dwm_compose(ccd: pf_win_display::win_display::CcdTargetKey) {
    // Process-GLOBAL throttle (Stage W3): with N parallel capturers each nudging on its own
    // schedule, DWM needs only one dirty per composition window — and the nudge is synthetic INPUT
    // (global, user-visible pointer state), so it must not multiply with capturer count. 50 ms
    // covers every composition interval we ship (≥ 60 Hz) while staying far under the callers' own
    // 600–800 ms per-capturer schedules.
    static LAST_KICK: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST_KICK.lock().unwrap();
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(50)) {
            return;
        }
        *last = Some(now);
    }
    // Where is the cursor, and where does the target display live in desktop space?
    let mut pos = POINT::default();
    // SAFETY: plain FFI; `pos` is a valid out-param for this synchronous call.
    let have_pos = unsafe { GetCursorPos(&mut pos) }.is_ok();
    let rect = pf_win_display::win_display::source_desktop_rect(ccd);
    // HID-first (see the doc comment): the registered virtual-mouse kick works from any
    // session/desktop and wakes an off display. Both geometries come from CCD (global database),
    // NOT per-session GDI metrics, so the aim is right even from a non-console session. Fall
    // through to SendInput only when the hook isn't registered / the mouse isn't up.
    if let (Some(kick), Some(rect)) = (crate::HID_COMPOSE_KICK.get(), rect) {
        let bounds = pf_win_display::win_display::desktop_bounds();
        if let Some(bounds) = bounds {
            if kick(rect, bounds) {
                return;
            }
        }
    }
    if let (true, Some((x, y, w, h))) = (have_pos, rect) {
        let inside = pos.x >= x && pos.x < x + w.max(1) && pos.y >= y && pos.y < y + h.max(1);
        if !inside {
            // The cursor is on a sibling display — a wiggle there dirties the WRONG display. Jump
            // to the target's center, DWELL one composition interval, then restore. The dwell is
            // load-bearing (proven on-glass, Stage W3): DWM computes dirty state from the CURRENT
            // cursor position at the next vsync tick, so a sub-tick jump-and-return is invisible
            // and the target never composes — 35 ms covers a 30 Hz tick with margin. The cursor
            // visibly leaves the sibling display for those ~2 frames; kicks only fire during THIS
            // display's session-open / recovery windows (throttled), so the blip is rare and brief.
            // SAFETY: plain FFI; coordinates are plain ints, and the second call restores the
            // observed original position.
            unsafe {
                let _ = SetCursorPos(x + w / 2, y + h / 2);
            }
            std::thread::sleep(Duration::from_millis(35));
            // SAFETY: as above.
            unsafe {
                let _ = SetCursorPos(pos.x, pos.y);
            }
            return;
        }
    }
    let mk = |dx: i32| INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    // SAFETY: plain FFI; the input slice is valid, fully-initialized local data for this synchronous
    // call, and `cbsize` is the true element size.
    unsafe {
        let _ = SendInput(&[mk(1), mk(-1)], std::mem::size_of::<INPUT>() as i32);
    }
}
