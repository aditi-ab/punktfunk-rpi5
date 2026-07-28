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
/// display only when something DIRTIES it — an idle desktop never does, so a freshly-attached ring
/// (session open, or a mid-session ring recreate) can sit at E_PENDING with no first frame even
/// though everything is healthy.
///
/// The PRIMARY first-frame mechanism is the driver's `FrameStash` (frame_transport.rs): the driver
/// retains the last composed frame and republishes it into every freshly-attached ring, so with a
/// stash-capable driver the first frame lands milliseconds after the channel delivery and this kick
/// never fires. It remains for pre-stash drivers and for the empty-stash cold start (a monitor that
/// has NEVER composed — normally the activation compose covers that). Synthetic input is inherently
/// unreliable — blocked on the secure desktop, defeated by a fullscreen game's ClipCursor, and
/// user-visible in the sibling-display case — which is exactly why it was demoted to fallback.
///
/// pf-vdisplay implements no hardware-cursor plane, so a cursor move is composited into
/// the frame — a guaranteed real present onto the IDD swap-chain (empirically what
/// `punktfunk-probe --input-test` always relied on).
///
/// The cursor only dirties the display it is ON — proven on-glass in the Stage-W3 two-display
/// validation: display B's session-open kicks wiggled the cursor on display A and B never composed
/// a first frame. So the kick is per-TARGET: when the cursor already sits inside `target_id`'s
/// desktop region (always true single-display), two net-zero 1 px relative moves (the historical
/// behavior, pointer ends exactly where it started); when it sits on a SIBLING display, jump the
/// cursor to the target's center and straight back (`SetCursorPos` ×2 — each absolute move dirties
/// the cursor layer of the display it lands on, so the target composes at least one frame).
/// Best-effort — injection can be unavailable on the secure desktop, where a fresh compose just
/// happened anyway.
///
/// **COST:** the sibling-display branch SLEEPS 35 ms on the calling thread between the two
/// `SetCursorPos`es. The dwell is load-bearing (see the comment at that branch: a sub-tick
/// jump-and-return never dirties anything), but the caller is the capture/encode thread, so a kick
/// on that branch costs ~2 frames of latency at 60 Hz. Every call site is a first-frame or
/// post-recreate recovery window where no frames are flowing anyway, and the global 50 ms throttle
/// plus the callers' own 600–800 ms schedules bound how often it can happen.
///
/// **HID-first**: when the host has registered [`HID_COMPOSE_KICK`] (the resident pf-mouse virtual
/// HID pointer), the kick goes through it INSTEAD of the `SendInput` paths below. A report from a
/// HID device is real input to win32k — delivered regardless of this process's session or the
/// active desktop, it wakes a powered-off display subsystem (lid-closed laptop / display idle-off /
/// modern standby) and counts as user presence — every condition under which `SendInput` is
/// silently impotent (wrong session → wrong input queue; secure desktop → blocked; display off →
/// nothing composes at all). That set is exactly the lid-closed field-report state.
pub(super) fn kick_dwm_compose(target_id: u32) {
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
    let rect = pf_win_display::win_display::source_desktop_rect(target_id);
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
