//! Named compositor output for absolute input — Linux counterpart of the Windows
//! `stream_target` slot. The wlroots virtual pointer maps `MouseMoveAbs` onto the
//! `wl_output` it was **created with**; `w`/`h` on the event is the client's
//! letterboxed video rect, not the streamed mode, so the extent is already in the
//! protocol and the OUTPUT is the only pin.
//!
//! The host publishes [`set_stream_output`] at capture bring-up (`wl_output.name`,
//! protocol v4, "the same for all clients": Hyprland `PF-<pid>-<n>`, sway
//! `HEADLESS-N`, KWin `Virtual-<name>`, or a mirrored connector). The wlr backend
//! re-creates the pointer bound to that name; KWin fake_input maps into that head.
//!
//! **One aim per process** ([`crate::aim`]). Last capture bring-up wins for every concurrent
//! session; per-session routing needs source-tagged events (see
//! [`crate::set_absolute_anchor`]). Unpublished, or a compositor without v4 names,
//! binds no output and maps over the whole layout — reachable, unlike a pin to the
//! first-advertised (oldest, operator) head.

/// Host-only, at capture bring-up: the name, then the aim generation, in one write. Never
/// cleared on teardown: a vanished output stops resolving and the backend maps the whole
/// layout; nothing injects between sessions. A later bring-up may pass `None` so a stale name
/// cannot outlive its compositor. Parallel sessions: last writer wins (module doc).
pub fn set_stream_output(name: Option<String>) {
    crate::publish_aim(true, |t| {
        if t.name != name {
            tracing::info!(output = ?name, "absolute-input stream output set");
            t.name = name;
        }
    });
}

pub fn stream_output() -> Option<String> {
    crate::aim().target.name
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One test: the aim is process-wide and cargo runs tests on threads in one process, so
    /// splitting this would race.
    #[test]
    fn publishes_clears_and_round_trips() {
        let before = crate::aim().generation;
        set_stream_output(Some("PF-1643-1".into()));
        assert_eq!(stream_output().as_deref(), Some("PF-1643-1"));
        // One snapshot: the head and the bring-up that published it.
        let aim = crate::aim();
        assert_eq!(aim.target.name.as_deref(), Some("PF-1643-1"));
        assert!(aim.generation > before);
        // Same name is a no-op for the log, but still a bring-up that owes a warp.
        set_stream_output(Some("PF-1643-1".into()));
        assert_eq!(stream_output().as_deref(), Some("PF-1643-1"));
        assert!(crate::aim().generation > aim.generation);
        set_stream_output(Some("HEADLESS-2".into()));
        assert_eq!(stream_output().as_deref(), Some("HEADLESS-2"));
        set_stream_output(None);
        assert_eq!(stream_output(), None);
    }
}
