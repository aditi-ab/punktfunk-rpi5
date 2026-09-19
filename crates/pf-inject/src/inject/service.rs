//! Off-thread pointer/keyboard injector plus the pre-injection [`coalesce`] pass.
//!
//! The backend owns non-`Send` compositor state (Wayland / xkb / EIS), so it lives on one thread
//! and is fed over a clonable `Send` channel. GameStream and native punktfunk/1 both forward
//! decoded input here instead of injecting inline.

use super::*;

/// Host-lifetime injector on its own thread. A slow inject (portal stall, desktop switch) must
/// not head-block the network thread's keepalive/retransmit. Backend is non-`Send`.
pub struct InjectorService {
    tx: std::sync::mpsc::Sender<InputEvent>,
}

impl InjectorService {
    pub fn start() -> InjectorService {
        // Without a pointing device, win32k reports no cursor and DWM composites none into the
        // IDD frame — SendInput then moves an invisible pointer. Idempotent.
        #[cfg(target_os = "windows")]
        super::mouse_windows::ensure_resident();

        Self::start_inner(None)
    }

    /// Session-lifetime injector pinned to one gamescope EIS relay (`design/gamescope-multiuser.md`).
    /// Never follows the published session backend. Dropping the service (and every sender clone)
    /// ends the thread and closes the EIS connection.
    #[cfg(target_os = "linux")]
    pub fn start_at(relay: std::path::PathBuf) -> InjectorService {
        Self::start_inner(Some(relay))
    }

    fn start_inner(pin: Option<std::path::PathBuf>) -> InjectorService {
        let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-injector".into())
            .spawn(move || injector_service_thread(rx, pin))
        {
            tracing::error!(error = %e, "injector service thread spawn failed — pointer/keyboard input disabled");
        }
        InjectorService { tx }
    }

    /// Cloned per caller. Dropping a clone does not stop the service; it runs while any sender lives.
    pub fn sender(&self) -> std::sync::mpsc::Sender<InputEvent> {
        self.tx.clone()
    }
}

/// 2 s between reopen attempts after open/worker death, so a dead portal is not hit once per event.
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Lazy-open worker. Reopen after [`INJECTOR_REOPEN_BACKOFF`] on open failure, on an unpinned
/// backend change, or if the worker dies. Exits when every sender drops (host shutdown, or
/// session end for a pin). `pin` is the gamescope relay ([`InjectorService::start_at`]); `None`
/// follows [`default_backend`]. Each wake drains the backlog and [`coalesce`]s motion so a slow
/// backend cannot queue stale relative-mouse/scroll; buttons, keys, and absolute moves stay ordered.
fn injector_service_thread(
    rx: std::sync::mpsc::Receiver<InputEvent>,
    pin: Option<std::path::PathBuf>,
) {
    let mut injector: Option<Box<dyn InputInjector>> = None;
    let mut open_backend: Option<Backend> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    let mut warped_gen = crate::aim_gen();
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }

        // Unpinned: reopen if the published backend changed, so input follows the active session
        // instead of a stale EIS socket. Read the `RwLock`, not `getenv` — `setenv` on connect
        // raced this hot path. A pin never follows; its target can only die (reopen arm below).
        let want = if pin.is_none() {
            let want = default_backend();
            if injector.is_some() && open_backend != Some(want) {
                tracing::info!(
                    ?open_backend,
                    ?want,
                    "input: backend changed — reopening injector for the active session"
                );
                injector = None;
                last_failed = None; // skip backoff; resolve now
            }
            Some(want)
        } else {
            None
        };
        if injector.is_none() {
            // First event opens; after failure wait the backoff (a few events drop; input is lossy).
            // Lazy open also covers pin ordering: the service is created before gamescope, and the
            // relay exists by the first event (the libei worker polls it).
            let ready = last_failed.is_none_or(|t| t.elapsed() >= INJECTOR_REOPEN_BACKOFF);
            if ready {
                let opened = match (&pin, want) {
                    #[cfg(target_os = "linux")]
                    (Some(relay), _) => crate::open_gamescope_at(relay.clone()),
                    #[cfg(not(target_os = "linux"))]
                    (Some(_), _) => unreachable!("pinned injector is Linux-only (start_at)"),
                    (None, Some(want)) => open(want),
                    (None, None) => unreachable!("unpinned resolve always yields a backend"),
                };
                match opened {
                    Ok(i) => {
                        match &pin {
                            Some(relay) => tracing::info!(relay = %relay.display(),
                                "input injector ready (session-pinned gamescope)"),
                            None => {
                                tracing::info!(backend = ?want, "input injector ready (host-lifetime)")
                            }
                        }
                        injector = Some(i);
                        open_backend = want;
                        last_failed = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "pointer/keyboard injection unavailable — will retry");
                        last_failed = Some(std::time::Instant::now());
                    }
                }
            }
        }
        if let Some(inj) = injector.as_mut() {
            for ev in warp_onto_stream_head(
                coalesce(batch),
                crate::aim_gen(),
                &mut warped_gen,
                crate::stream_extent(),
            ) {
                if let Err(e) = inj.inject(&ev) {
                    // Portal / EIS worker died. Drop and reopen on a later event (gamescope respawns).
                    tracing::warn!(error = %format!("{e:#}"), "inject failed — reopening injector");
                    injector = None;
                    open_backend = None;
                    last_failed = Some(std::time::Instant::now());
                    break; // rest of this batch is stale; the next recv reopens
                }
            }
        }
    }
    tracing::debug!("injector service stopped (host shutting down)");
}

/// Centre of the streamed head: a sample at half of `extent`, which is the head's own mode when
/// [`crate::stream_extent`] knows it — libei resolves its region by that size — and `2×2`
/// otherwise, since every other backend normalizes the position and takes any extent.
fn head_centre(extent: Option<(u16, u16)>) -> InputEvent {
    let (w, h) = extent.filter(|(w, h)| *w > 1 && *h > 1).unwrap_or((2, 2));
    InputEvent {
        kind: InputKind::MouseMoveAbs,
        _pad: [0; 3],
        code: 0,
        x: i32::from(w / 2),
        y: i32::from(h / 2),
        flags: (u32::from(w) << 16) | u32::from(h),
    }
}

/// Start a session's first pointer motion on the streamed head. The host pointer sits wherever
/// the operator left it — on an extended desktop that is another monitor, and a relative delta
/// has no way to cross onto the streamed one. Absolute motion already lands there and only
/// clears the debt. One warp per capture bring-up ([`crate::aim_gen`]), and only once the client
/// actually moves, so an idle session never takes the operator's pointer.
fn warp_onto_stream_head(
    events: Vec<InputEvent>,
    aim: u64,
    warped_gen: &mut u64,
    extent: Option<(u16, u16)>,
) -> Vec<InputEvent> {
    if aim == *warped_gen {
        return events;
    }
    let Some(at) = events
        .iter()
        .position(|e| matches!(e.kind, InputKind::MouseMove | InputKind::MouseMoveAbs))
    else {
        return events;
    };
    *warped_gen = aim;
    if events[at].kind == InputKind::MouseMoveAbs {
        return events;
    }
    let mut out = events;
    out.insert(at, head_centre(extent));
    out
}

/// Sum adjacent relative-mouse and same-axis, same-precision scroll. Buttons, keys, moves, and type
/// changes pass through in order: a key between two moves flushes the accumulated motion first.
fn coalesce(events: Vec<InputEvent>) -> Vec<InputEvent> {
    let mut out: Vec<InputEvent> = Vec::with_capacity(events.len());
    for ev in events {
        match out.last_mut() {
            Some(last) if last.kind == InputKind::MouseMove && ev.kind == InputKind::MouseMove => {
                last.x = last.x.saturating_add(ev.x);
                last.y = last.y.saturating_add(ev.y);
            }
            Some(last)
                if last.kind == InputKind::MouseScroll
                    && ev.kind == InputKind::MouseScroll
                    && last.code == ev.code
                    && last.flags == ev.flags =>
            {
                last.x = last.x.saturating_add(ev.x);
            }
            _ => out.push(ev),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::{InputEvent, InputKind, SCROLL_FLAG_PRECISE};

    fn mk(kind: InputKind, code: u32, x: i32, y: i32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y,
            flags: 0,
        }
    }

    #[test]
    fn coalesce_sums_adjacent_motion_and_preserves_order() {
        let events = vec![
            mk(InputKind::MouseMove, 0, 1, 2),
            mk(InputKind::MouseMove, 0, 3, -1),
            mk(InputKind::KeyDown, 30, 0, 0),
            mk(InputKind::MouseMove, 0, 5, 5),
            mk(InputKind::MouseScroll, 0, 1, 0),
            mk(InputKind::MouseScroll, 0, 2, 0),
            mk(InputKind::MouseScroll, 1, 1, 0),
        ];
        let out = coalesce(events);
        assert_eq!(out.len(), 5);
        assert_eq!(
            (out[0].kind, out[0].x, out[0].y),
            (InputKind::MouseMove, 4, 1)
        );
        assert_eq!(out[1].kind, InputKind::KeyDown);
        assert_eq!(
            (out[2].kind, out[2].x, out[2].y),
            (InputKind::MouseMove, 5, 5)
        );
        assert_eq!(
            (out[3].kind, out[3].code, out[3].x),
            (InputKind::MouseScroll, 0, 3)
        );
        assert_eq!(
            (out[4].kind, out[4].code, out[4].x),
            (InputKind::MouseScroll, 1, 1)
        );
    }

    /// Normalized scroll carries gesture phases a sum would erase: adjacent
    /// `Scroll` events never merge, even same-axis.
    #[test]
    fn coalesce_never_merges_normalized_scroll() {
        let events = vec![
            mk(InputKind::Scroll, 0, 100, 0),
            mk(InputKind::Scroll, 0, 200, 0),
        ];
        assert_eq!(coalesce(events).len(), 2);
    }

    fn warp(events: Vec<InputEvent>, aim: u64, warped: &mut u64) -> Vec<InputEvent> {
        warp_onto_stream_head(events, aim, warped, Some((3840, 2160)))
    }

    /// The operator's pointer is on another monitor and a delta cannot cross to the stream, so
    /// the session's first move starts at the streamed head's centre — and only the first.
    #[test]
    fn the_first_relative_move_of_a_session_warps() {
        let mut warped = 0;
        let out = warp(vec![mk(InputKind::MouseMove, 0, 5, 5)], 1, &mut warped);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, InputKind::MouseMoveAbs);
        // Centre at the head's own mode, so a region resolved by size still matches.
        assert_eq!(
            (out[0].x, out[0].y, out[0].flags),
            (1920, 1080, (3840 << 16) | 2160)
        );
        assert_eq!(out[1].kind, InputKind::MouseMove);
        let out = warp(vec![mk(InputKind::MouseMove, 0, 5, 5)], 1, &mut warped);
        assert_eq!(out.len(), 1);
        // The next bring-up owes its own warp.
        let out = warp(vec![mk(InputKind::MouseMove, 0, 5, 5)], 2, &mut warped);
        assert_eq!(out.len(), 2);
    }

    /// An unpublished mode still warps: every backend but libei normalizes the position.
    #[test]
    fn an_unknown_mode_warps_at_the_neutral_extent() {
        let mut warped = 0;
        let out = warp_onto_stream_head(
            vec![mk(InputKind::MouseMove, 0, 5, 5)],
            1,
            &mut warped,
            None,
        );
        assert_eq!((out[0].x, out[0].y, out[0].flags), (1, 1, (2 << 16) | 2));
    }

    /// An absolute sample (TV remote, touch, pen) already lands on the streamed head. Warping
    /// first would drag the pointer through the centre on every session's first sample.
    #[test]
    fn an_absolute_move_pays_the_debt_without_a_warp() {
        let mut warped = 0;
        let out = warp(vec![mk(InputKind::MouseMoveAbs, 0, 7, 7)], 1, &mut warped);
        assert_eq!(out.len(), 1);
        let out = warp(vec![mk(InputKind::MouseMove, 0, 5, 5)], 1, &mut warped);
        assert_eq!(out.len(), 1);
    }

    /// Keys and buttons must not spend the warp: the pointer has not moved yet.
    #[test]
    fn a_keystroke_leaves_the_warp_owed() {
        let mut warped = 0;
        let out = warp(vec![mk(InputKind::KeyDown, 30, 0, 0)], 1, &mut warped);
        assert_eq!(out.len(), 1);
        let out = warp(vec![mk(InputKind::MouseMove, 0, 5, 5)], 1, &mut warped);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn coalesce_handles_empty_and_singleton() {
        assert!(coalesce(vec![]).is_empty());
        assert_eq!(coalesce(vec![mk(InputKind::MouseMove, 0, 7, 8)]).len(), 1);
    }

    /// A trackpad delta and a wheel detent mean different distances, so summing one into the
    /// other would inject the pair at whichever precision happened to arrive first.
    #[test]
    fn coalesce_keeps_precise_scroll_apart_from_wheel() {
        let wheel = mk(InputKind::MouseScroll, 0, 120, 0);
        let mut precise = mk(InputKind::MouseScroll, 0, 12, 0);
        precise.flags = SCROLL_FLAG_PRECISE;
        let out = coalesce(vec![precise, precise, wheel, wheel]);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].x, out[0].flags), (24, SCROLL_FLAG_PRECISE));
        assert_eq!((out[1].x, out[1].flags), (240, 0));
    }
}
