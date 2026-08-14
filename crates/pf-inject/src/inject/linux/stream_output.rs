//! The compositor output absolute coordinates belong to, by NAME — the Linux counterpart of the
//! Windows `stream_target` slot, and what the wlroots virtual-pointer backend aims at.
//!
//! `MouseMoveAbs` carries its own reference extent (`w`/`h` — the client's letterboxed video rect
//! in ITS window, not the streamed mode), and the wlr protocol normalizes `x`/`y` against it and
//! maps the result onto whichever `wl_output` the virtual pointer was **created with**. So the
//! extent takes care of itself and the OUTPUT is the whole question. The injector used to pass the
//! first `wl_output` the registry advertised, which is the oldest global — on any multi-head box
//! the operator's physical head, never the per-session headless output the client is looking at.
//! On the EXTEND backends (Hyprland, wlroots/sway) the streamed head sits *beside* the operator's,
//! so absolute samples landed on a screen no session was streaming. Reported from the field as
//! "no cursor was visible in the session", and later as a cursor pinned near the left edge that
//! vanished part-way across.
//!
//! The host publishes the streamed output's compositor name at capture bring-up
//! ([`set_stream_output`]) — Hyprland's `PF-<pid>-<n>`, sway's `HEADLESS-N`, or a mirrored head's
//! connector — and the wlr backend re-creates its virtual pointer bound to the matching `wl_output`
//! (`wl_output.name`, protocol v4; the name is explicitly "the same for all clients", so the name
//! `hyprctl`/`swaymsg` minted is the name we can match here).
//!
//! **One slot per process**, exactly like the Windows original: the injector is host-lifetime and
//! every concurrent session's input flows through it, so with parallel sessions the LAST capture
//! bring-up wins for every session's absolute input. Per-session routing needs source-tagged input
//! events (the injector has to become session-aware first — see [`crate::set_absolute_anchor`]'s
//! note), and the single slot is never worse than what it replaces: today EVERY session's absolute
//! input lands on a head that no session is streaming.
//!
//! With nothing published — before the first bring-up, or on a compositor whose `wl_output` is
//! older than v4 and therefore nameless — the pointer is bound to NO output, which maps absolute
//! coordinates over the whole layout. On a single-output compositor that is identical to binding
//! that output; on a multi-head one it is at least *reachable*, unlike a pin to the wrong head.

use std::sync::RwLock;

/// The streamed output's compositor name, or `None` when nothing has been published yet.
static STREAM_OUTPUT: RwLock<Option<String>> = RwLock::new(None);

/// Publish the compositor output (by name) that absolute input maps into. The host calls this at
/// capture bring-up, and ONLY there: nothing clears it at teardown, because an output that goes
/// away simply stops resolving (the backend falls back to whole-layout mapping, and between
/// sessions nothing injects anyway). A later bring-up is what rewrites it — including to `None`,
/// which a backend that needs no named binding passes so a stale name cannot outlive its
/// compositor. See the module doc for the one-slot-per-process trade with parallel sessions.
pub fn set_stream_output(name: Option<String>) {
    let mut cur = STREAM_OUTPUT.write().unwrap_or_else(|e| e.into_inner());
    if *cur != name {
        tracing::info!(output = ?name, "absolute-input stream output set");
        *cur = name;
    }
}

/// The streamed output's compositor name, if one has been published.
pub fn stream_output() -> Option<String> {
    STREAM_OUTPUT
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ONE test on purpose, like the libei anchor's: the slot is process-wide and cargo runs
    /// tests on threads in one process, so splitting this into several would let them race.
    #[test]
    fn publishes_clears_and_round_trips() {
        set_stream_output(Some("PF-1643-1".into()));
        assert_eq!(stream_output().as_deref(), Some("PF-1643-1"));
        // Re-publishing the same name is a no-op, not a second "set" (the backend keys its
        // pointer re-creation off the resolved name, but the log line should not repeat).
        set_stream_output(Some("PF-1643-1".into()));
        assert_eq!(stream_output().as_deref(), Some("PF-1643-1"));
        set_stream_output(Some("HEADLESS-2".into()));
        assert_eq!(stream_output().as_deref(), Some("HEADLESS-2"));
        set_stream_output(None);
        assert_eq!(stream_output(), None);
    }
}
