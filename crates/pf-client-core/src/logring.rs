//! The client's own recent-log ring + the "send logs to host" uploader.
//!
//! Why: on locked-down platforms (a Steam Deck in Gaming Mode, tvOS, webOS) the user cannot get
//! the client's log off the device, so field reports arrive host-log-only and the client half of
//! any stutter/latency story is invisible. The cure is inverted collection: the client keeps its
//! newest few thousand log lines here, and an explicit user action posts them to the PAIRED host
//! (`POST /api/v1/client-logs`, authenticated by the same mTLS identity the stream uses), where
//! the web console lists them next to the host's own logs.
//!
//! The ring is deliberately dependency-free (no `tracing-subscriber` in this crate): each shell
//! installs a thin `Layer` that formats events into [`note`] — see `punktfunk-session`'s
//! `ring_layer`. Bounded by lines AND bytes so a log-storm can't grow memory; the byte budget
//! stays under the host's 1 MiB upload cap so a full ring always uploads whole.

use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

/// Newest lines kept (matches the host's own ring depth).
pub const MAX_LINES: usize = 4096;

/// Byte budget for the ring — under the host's 1 MiB bundle cap with headroom for the header.
pub const MAX_BYTES: usize = 768 * 1024;

struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    dropped: u64,
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(|| {
    Mutex::new(Ring {
        lines: VecDeque::new(),
        bytes: 0,
        dropped: 0,
    })
});

/// Append one formatted log line (no trailing newline). Oversized lines are truncated to keep a
/// single event from evicting the whole ring.
pub fn note(mut line: String) {
    if line.len() > 2048 {
        line.truncate(2048);
        line.push('…');
    }
    let mut r = RING.lock().unwrap_or_else(|e| e.into_inner());
    r.bytes += line.len();
    r.lines.push_back(line);
    while r.lines.len() > MAX_LINES || r.bytes > MAX_BYTES {
        if let Some(evicted) = r.lines.pop_front() {
            r.bytes -= evicted.len();
            r.dropped += 1;
        } else {
            break;
        }
    }
}

/// The ring rendered as one text bundle, oldest first, prefixed by `header` (the shell's own
/// identity line — binary name, version, platform) and an eviction note when the ring wrapped.
pub fn render(header: &str) -> String {
    let r = RING.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = String::with_capacity(r.bytes + header.len() + 64);
    out.push_str(header);
    out.push('\n');
    if r.dropped > 0 {
        out.push_str(&format!(
            "… {} older lines evicted from the ring …\n",
            r.dropped
        ));
    }
    for line in &r.lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Upload the ring to the paired host `addr` and return the stored bundle id. Same transport +
/// trust as the library fetch: TLS client auth with the device identity, host pinned by
/// fingerprint. Errors reuse the library's classification (401/403 ⇒ `NotPaired`, a pin-verifier
/// rejection ⇒ `PinMismatch`), so the shell's existing error strings apply.
pub fn send_to_host(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    header: &str,
) -> Result<String, crate::library::LibraryError> {
    use crate::library::LibraryError;
    let body = render(header);
    let agent = crate::library::agent(identity, pin)?;
    let url = format!(
        "{}/api/v1/client-logs",
        crate::library::base_url(addr, mgmt_port)
    );
    match agent
        .post(&url)
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(body.as_bytes())
    {
        Ok(mut resp) => {
            let text = resp
                .body_mut()
                .read_to_string()
                .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?;
            let id = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                .unwrap_or_default();
            Ok(id)
        }
        Err(e) => Err(crate::library::classify(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_bounds_and_renders_with_eviction_note() {
        // The ring is process-global, so this single test owns the whole lifecycle (parallel
        // tests over one global would interleave).
        for i in 0..(MAX_LINES + 10) {
            note(format!("line {i}"));
        }
        let text = render("punktfunk-client test");
        assert!(text.starts_with("punktfunk-client test\n"));
        assert!(text.contains("older lines evicted"));
        assert!(
            !text.contains("\nline 0\n"),
            "oldest line survived eviction"
        );
        assert!(text.ends_with(&format!("line {}\n", MAX_LINES + 9)));
        // A pathological line is truncated, not ring-flushing.
        note("x".repeat(10_000));
        let text = render("h");
        assert!(text.contains('…'));
    }
}
