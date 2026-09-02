//! Last-resort DWM compose kick: synthetic pointer input that dirties one virtual
//! display so DWM presents it.
//!
//! Primary first-frame path is the driver's `FrameStash` (`frame_transport.rs`). This
//! remains for pre-stash drivers and an empty-stash cold start. Synthetic input is
//! blocked on the secure desktop, defeated by a fullscreen `ClipCursor`, and
//! user-visible on a sibling display — which is why it is a fallback.
//!
//! A cursor move dirties only the display the pointer is on, so the kick is per-target.
//! HID-first: when [`crate::HID_COMPOSE_KICK`] is registered, the report is real win32k
//! input (any session/desktop, wakes a powered-off display). `SendInput` is the remaining
//! path.

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
    let mut pos = POINT::default();
    // SAFETY: `pos` is a valid out-param for this call.
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
            // Sibling: a wiggle there dirties the wrong display. Jump to the target center,
            // dwell one vsync, restore. DWM samples dirty state at the next tick, so a
            // sub-tick jump-and-return is invisible. 35 ms covers a 30 Hz tick with margin.
            // SAFETY: coordinates are ints; the second call restores the observed position.
            unsafe {
                let _ = SetCursorPos(x + w / 2, y + h / 2);
            }
            std::thread::sleep(Duration::from_millis(35));
            // SAFETY: restores the position observed above.
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
    // SAFETY: the slice is a fully-initialized local; `cbSize` is `size_of::<INPUT>()`.
    unsafe {
        let _ = SendInput(&[mk(1), mk(-1)], std::mem::size_of::<INPUT>() as i32);
    }
}
