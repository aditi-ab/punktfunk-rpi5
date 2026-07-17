//! The off-thread injector service (plan §W4, carved out of the inject facade): a host-lifetime
//! pointer/keyboard injector pinned to its OWN thread and fed over a clonable `Send` channel, plus
//! the pre-injection [`coalesce`] pass. The backend owns non-`Send` compositor state (a Wayland
//! connection / xkb / EIS socket), so it must live on one thread; both the GameStream control plane
//! and the native punktfunk/1 plane forward decoded input here instead of injecting inline.

use super::*;

/// Host-lifetime pointer/keyboard injector running on its OWN thread, fed over a clonable `Send`
/// channel. The injector backend owns non-`Send` compositor state (a Wayland connection / xkb / EIS
/// socket), so it must live on a single thread; both the GameStream control plane and the native
/// punktfunk/1 plane forward their decoded keyboard/mouse events here instead of injecting inline, so
/// a slow inject (a portal stall, a desktop switch) never head-blocks the network thread's
/// keepalive/retransmit servicing.
pub struct InjectorService {
    tx: std::sync::mpsc::Sender<InputEvent>,
}

impl InjectorService {
    pub fn start() -> InjectorService {
        // Windows: make sure the process-wide resident virtual HID mouse exists (idempotent).
        // Without a pointing device present, win32k reports no cursor and DWM composites none
        // into the IDD frame — SendInput injection alone moves an invisible pointer.
        #[cfg(target_os = "windows")]
        super::mouse_windows::ensure_resident();

        let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-injector".into())
            .spawn(move || injector_service_thread(rx))
        {
            tracing::error!(error = %e, "injector service thread spawn failed — pointer/keyboard input disabled");
        }
        InjectorService { tx }
    }

    /// A sender a session/plane forwards its pointer/keyboard events to. Cloned per caller; dropping a
    /// clone does NOT stop the service (it runs while any sender — incl. the service's own — lives).
    pub fn sender(&self) -> std::sync::mpsc::Sender<InputEvent> {
        self.tx.clone()
    }
}

/// Backoff between reopen attempts after the injector backend fails to open or its worker dies, so a
/// persistently-unavailable portal isn't hammered once per event.
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// The host-lifetime injector worker: lazily open the pointer/keyboard backend, then inject every
/// forwarded event. Reopen (after [`INJECTOR_REOPEN_BACKOFF`]) on open failure, on a backend change
/// (input follows the active session), or if the backend's worker dies mid-stream. Exits only when
/// every sender has dropped (host shutdown), which drops the injector and closes its portal session.
///
/// Each wake drains the whole backlog and [`coalesce`]s redundant motion before injecting, so a slow
/// backend never builds up a queue of stale relative-mouse/scroll events (latency) — while button,
/// key, and absolute-move ordering is preserved exactly.
fn injector_service_thread(rx: std::sync::mpsc::Receiver<InputEvent>) {
    let mut injector: Option<Box<dyn InputInjector>> = None;
    let mut open_backend: Option<Backend> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    while let Ok(first) = rx.recv() {
        // Drain everything already queued behind `first` so we coalesce a whole burst at once.
        let mut batch = vec![first];
        while let Ok(ev) = rx.try_recv() {
            batch.push(ev);
        }

        // The resolved input backend (PUNKTFUNK_INPUT_BACKEND, set per connect / mid-stream session
        // switch) may have changed since we opened. Reopen against it so input FOLLOWS the active
        // session instead of injecting into a stale, still-warm backend (e.g. the managed gamescope's
        // EIS socket after the user switched to the KDE desktop).
        let want = default_backend();
        if injector.is_some() && open_backend != Some(want) {
            tracing::info!(
                ?open_backend,
                ?want,
                "input: backend changed — reopening injector for the active session"
            );
            injector = None;
            last_failed = None; // re-resolve immediately
        }
        if injector.is_none() {
            // Open on the first event; after a failure wait out the backoff before retrying (a few
            // events drop during setup — acceptable, input is lossy).
            let ready = last_failed.is_none_or(|t| t.elapsed() >= INJECTOR_REOPEN_BACKOFF);
            if ready {
                match open(want) {
                    Ok(i) => {
                        tracing::info!(backend = ?want, "input injector ready (host-lifetime)");
                        injector = Some(i);
                        open_backend = Some(want);
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
            for ev in coalesce(batch) {
                if let Err(e) = inj.inject(&ev) {
                    // The backend's worker (portal session / EIS socket) died — drop it and reopen on
                    // a later event (covers a gamescope EIS socket that respawns with its session).
                    tracing::warn!(error = %format!("{e:#}"), "inject failed — reopening injector");
                    injector = None;
                    open_backend = None;
                    last_failed = Some(std::time::Instant::now());
                    break; // abandon the rest of this batch; the next one reopens
                }
            }
        }
    }
    tracing::debug!("injector service stopped (host shutting down)");
}

/// Coalesce a drained burst: sum consecutive relative-mouse deltas and consecutive same-axis scroll
/// deltas (identical net effect, far fewer injects), passing buttons, keys, absolute moves, and any
/// type change through untouched and in order. Only *adjacent* same-type events merge, so a button
/// or key between two moves flushes the accumulated motion first — ordering is never reshuffled.
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
                    && last.code == ev.code =>
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
    use punktfunk_core::input::{InputEvent, InputKind};

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
            mk(InputKind::MouseMove, 0, 3, -1), // → summed with the previous move
            mk(InputKind::KeyDown, 30, 0, 0),   // flushes the move, passes through verbatim
            mk(InputKind::MouseMove, 0, 5, 5),  // a NEW run after the key (not merged across it)
            mk(InputKind::MouseScroll, 0, 1, 0),
            mk(InputKind::MouseScroll, 0, 2, 0), // same axis (code 0) → summed
            mk(InputKind::MouseScroll, 1, 1, 0), // different axis (code 1) → separate
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

    #[test]
    fn coalesce_handles_empty_and_singleton() {
        assert!(coalesce(vec![]).is_empty());
        assert_eq!(coalesce(vec![mk(InputKind::MouseMove, 0, 7, 8)]).len(), 1);
    }
}
