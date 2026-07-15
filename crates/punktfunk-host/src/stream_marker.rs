//! Script-facing **"a stream is live"** marker file.
//!
//! While one or more clients are actively streaming, the host maintains a small, shell-sourceable
//! file at `$XDG_RUNTIME_DIR/punktfunk/stream`. It is created when the first session goes live and
//! removed when the last one ends, so a per-title launch script (a Steam launch-option wrapper, a
//! `.desktop` Exec, whatever) can branch on presence + read the negotiated mode:
//!
//! ```sh
//! #!/bin/sh
//! STATE="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/punktfunk/stream"
//! if [ -f "$STATE" ]; then
//!     . "$STATE"          # sets PF_STREAM_WIDTH / _HEIGHT / _REFRESH …
//!     exec "$@"           # session already at the stream mode → run the game directly
//! fi
//! exec gamescope -W 5760 -H 1080 -f -- "$@"   # not streaming → local triple-head
//! ```
//!
//! ## Contract (stable — scripts depend on it)
//!
//! * The file EXISTS iff at least one client is actively streaming. Absence is the "no stream"
//!   signal; a script must treat a missing file as "not streaming", never error on it.
//! * It is a POSIX-`sh`-sourceable list of `KEY=value` lines — safe to `. "$file"`. Every value is
//!   either a bare integer or a single-quoted string with quotes/controls stripped, so sourcing can
//!   never inject or run anything. Keys are namespaced `PF_STREAM_*` so sourcing won't clobber a
//!   caller's `WIDTH`/`HEIGHT`.
//! * Keys only get ADDED across versions, never repurposed; `PF_STREAM_SCHEMA` bumps if an existing
//!   key's meaning ever changes. Current keys:
//!     - `PF_STREAM=1`            — presence flag (redundant with the file existing; handy in `env`)
//!     - `PF_STREAM_SCHEMA=1`     — format version
//!     - `PF_STREAM_WIDTH`        — negotiated width  (px)
//!     - `PF_STREAM_HEIGHT`       — negotiated height (px)
//!     - `PF_STREAM_REFRESH`      — negotiated refresh (Hz, integer)
//!     - `PF_STREAM_HDR`          — `1` if the session negotiated HDR, else `0`
//!     - `PF_STREAM_SESSIONS`     — number of clients currently streaming
//!     - `PF_STREAM_CLIENT`       — the primary (oldest live) client's name, single-quoted
//! * Writes are atomic (temp + `rename`), so a script that sources mid-update always sees a complete
//!   prior version, never a torn file.
//!
//! ## Semantics
//!
//! The marker reflects the **primary** session — the oldest still-live one — which is the single
//! mode a single-user host is at. With several concurrent clients at different modes there is only
//! one file, so `PF_STREAM_SESSIONS>1` is the script's cue that width/height/refresh describe just
//! one of them. The mode published is the one negotiated at session start; a mid-stream resize is
//! not currently re-published (gamescope — the main script consumer — can't live-resize anyway).

/// What one live session contributes to the marker.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub hdr: bool,
    /// Client-supplied device name (may be empty); sanitized before it reaches the file.
    pub client: String,
}

/// RAII handle for one announced session. While it is alive the session counts toward the marker;
/// dropping it (session end, error return, panic-unwind) deregisters and rewrites/removes the file.
/// Created by [`announce`]; never constructed directly.
#[must_use = "dropping the guard immediately retracts the stream marker"]
pub struct Guard {
    #[cfg(unix)]
    id: u64,
}

/// Announce that a client has started streaming at `info`'s mode. Returns a [`Guard`] that must be
/// held for the streaming lifetime — drop it when the session ends.
pub fn announce(info: StreamInfo) -> Guard {
    #[cfg(unix)]
    {
        imp::announce(info)
    }
    #[cfg(not(unix))]
    {
        let _ = info;
        Guard {}
    }
}

#[cfg(not(unix))]
impl Drop for Guard {
    fn drop(&mut self) {}
}

#[cfg(unix)]
mod imp {
    use super::{Guard, StreamInfo};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// The live sessions, oldest first (so `[0]` is the primary), each with the id its `Guard` holds.
    struct Registry {
        next_id: u64,
        sessions: Vec<(u64, StreamInfo)>,
    }

    static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
        next_id: 1,
        sessions: Vec::new(),
    });

    impl Registry {
        fn insert(&mut self, info: StreamInfo) -> u64 {
            let id = self.next_id;
            self.next_id += 1;
            self.sessions.push((id, info));
            id
        }

        fn remove(&mut self, id: u64) {
            if let Some(pos) = self.sessions.iter().position(|(sid, _)| *sid == id) {
                self.sessions.remove(pos);
            }
        }
    }

    /// `$XDG_RUNTIME_DIR/punktfunk/` — per-user tmpfs (cleared on logout, so no marker survives a
    /// reboot to lie about a stream). Falls back to `/tmp` only if the runtime dir is unset, matching
    /// the gamescope-log convention already in this crate.
    fn dir() -> PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("punktfunk")
    }

    fn marker_path() -> PathBuf {
        dir().join("stream")
    }

    /// Make a client name safe to place inside single quotes in a sourceable file: drop the single
    /// quote itself and any control characters (newlines especially), and cap the length so a hostile
    /// or absurd name can't bloat the file. Empty stays empty.
    fn sanitize(name: &str) -> String {
        name.chars()
            .filter(|c| *c != '\'' && !c.is_control())
            .take(96)
            .collect()
    }

    pub(super) fn announce(info: StreamInfo) -> Guard {
        let mut reg = REGISTRY.lock().unwrap();
        let id = reg.insert(info);
        rewrite_to(&marker_path(), &reg);
        Guard { id }
    }

    /// Rewrite (or remove) the marker at `path` to match `reg`. Called under the registry lock.
    fn rewrite_to(path: &std::path::Path, reg: &Registry) {
        let Some((_, primary)) = reg.sessions.first() else {
            // Last session gone — remove the marker. Best-effort: a missing file is already the
            // desired end state.
            let _ = std::fs::remove_file(path);
            return;
        };

        let body = format!(
            "# punktfunk stream marker — auto-generated. Present only while a client is streaming.\n\
             # Sourceable from a launch script: `. \"$XDG_RUNTIME_DIR/punktfunk/stream\"`.\n\
             PF_STREAM=1\n\
             PF_STREAM_SCHEMA=1\n\
             PF_STREAM_WIDTH={w}\n\
             PF_STREAM_HEIGHT={h}\n\
             PF_STREAM_REFRESH={hz}\n\
             PF_STREAM_HDR={hdr}\n\
             PF_STREAM_SESSIONS={n}\n\
             PF_STREAM_CLIENT='{client}'\n",
            w = primary.width,
            h = primary.height,
            hz = primary.refresh_hz,
            hdr = u8::from(primary.hdr),
            n = reg.sessions.len(),
            client = sanitize(&primary.client),
        );

        if let Err(e) = write_atomic(path, body.as_bytes()) {
            tracing::debug!(error = %e, path = %path.display(), "could not write stream marker");
        }
    }

    /// Write `bytes` to `path` atomically: create the parent, write a sibling temp file, `rename` it
    /// over the target. A concurrent reader (`. "$file"`) therefore always sees either the old
    /// complete file or the new complete file, never a partial one. Serialized by the registry lock,
    /// so a fixed temp name is safe.
    fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let mut reg = REGISTRY.lock().unwrap();
            reg.remove(self.id);
            rewrite_to(&marker_path(), &reg);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sanitize_strips_quotes_and_controls() {
            assert_eq!(sanitize("Living Room TV"), "Living Room TV");
            assert_eq!(sanitize("evil';rm -rf ~\n"), "evil;rm -rf ~");
            assert_eq!(sanitize(&"x".repeat(200)).len(), 96);
            assert_eq!(sanitize(""), "");
        }

        // The marker file lifecycle is exercised against a real path so the atomic-write + remove
        // logic is covered end to end. It drives a LOCAL registry at an explicit temp path: the
        // process-global one is shared with the punktfunk1 integration tests (which announce real
        // sessions concurrently), and mutating XDG_RUNTIME_DIR mid-run would race them too.
        #[test]
        fn marker_appears_while_held_and_vanishes_after() {
            let dir = std::env::temp_dir().join(format!("pf-marker-test-{}", std::process::id()));
            let path = dir.join("stream");
            let _ = std::fs::remove_file(&path);
            let mut reg = Registry {
                next_id: 1,
                sessions: Vec::new(),
            };

            let g = reg.insert(StreamInfo {
                width: 2560,
                height: 1440,
                refresh_hz: 120,
                hdr: true,
                client: "Couch'TV".to_string(),
            });
            rewrite_to(&path, &reg);
            let text = std::fs::read_to_string(&path).expect("marker exists while streaming");
            assert!(text.contains("PF_STREAM_WIDTH=2560"));
            assert!(text.contains("PF_STREAM_HEIGHT=1440"));
            assert!(text.contains("PF_STREAM_REFRESH=120"));
            assert!(text.contains("PF_STREAM_HDR=1"));
            assert!(text.contains("PF_STREAM_SESSIONS=1"));
            // The apostrophe must have been stripped, leaving a well-formed single-quoted value.
            assert!(text.contains("PF_STREAM_CLIENT='CouchTV'"));

            // A second concurrent session bumps the count but keeps the primary's mode.
            let g2 = reg.insert(StreamInfo {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                hdr: false,
                client: "Phone".to_string(),
            });
            rewrite_to(&path, &reg);
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("PF_STREAM_SESSIONS=2"));
            assert!(
                text.contains("PF_STREAM_WIDTH=2560"),
                "primary mode is retained"
            );

            reg.remove(g2);
            rewrite_to(&path, &reg);
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains("PF_STREAM_SESSIONS=1"));

            reg.remove(g);
            rewrite_to(&path, &reg);
            assert!(!path.exists(), "marker removed once the last session ends");
            let _ = std::fs::remove_dir(&dir);
        }
    }
}
