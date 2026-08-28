//! `punktfunk-host ctl watch` — the host's SSE event stream bridged to **line-JSON on stdout**,
//! one object per line, flushed per line. That shape is the whole point: a Quickshell `Process`, a
//! waybar `custom/` module and `while read -r line` all consume it without an SSE parser, and none
//! of them ever sees a token (I3 — the plugin holds no credential because it holds no HTTP client).
//!
//! Reconnection is the interesting part. `mgmt/events.rs` keeps a ~1024-event ring and resumes
//! from `Last-Event-ID`; a consumer whose cursor has fallen off the ring gets a synthetic
//! `event: dropped` frame first and is expected to re-snapshot. We surface both facts to the
//! consumer as ordinary lines:
//!
//! - `{"v":1,"kind":"ctl.resync"}` — emitted **once** after a `dropped` frame, and also after any
//!   reconnect that could not resume exactly (no cursor yet). A widget that sees it re-runs
//!   `ctl status` / `ctl pending` rather than trusting its incremental state.
//! - `{"v":1,"kind":"ctl.disconnected","data":{"error":"…"}}` — the stream dropped and we are
//!   backing off. Purely informational; the reconnect is automatic.
//! - `{"v":1,"kind":"ctl.heartbeat"}` — the host's SSE keep-alive, roughly every 15 s. A consumer
//!   can use it as a liveness signal, and it is also what lets *us* notice that our own consumer
//!   has gone away (see [`emit`]).
//!
//! The cursor advances on every frame with an `id:`, so a host restart mid-watch resumes from the
//! last event actually delivered. Backoff is capped and jittered only by the cap: an operator's
//! plugin reconnecting in a tight loop against a host that is down would otherwise be the thing
//! that keeps hitting the SSE connection cap.
//!
//! The connection cap (`MAX_EVENT_STREAMS` = 32) is shared with the console; the plugin is
//! specified to hold exactly one stream. Exhausting it is a 503, which arrives here as an ordinary
//! API failure with the host's own message.

use std::io::{BufRead, BufReader, Write};
use std::time::Duration;

use super::client::{Client, Failure, Result, SCHEMA_VERSION};

/// Reconnect backoff: quick enough that a host restart is invisible to a widget, slow enough that
/// a host that is genuinely down doesn't get hammered.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub fn run(kinds: Option<&str>, since: Option<u64>) -> Result<()> {
    let mut cursor = since;
    let mut backoff = BACKOFF_MIN;
    // First connect is the only one allowed to fail the process: a bad pin, a missing token or a
    // host that has never run are all conditions a retry cannot fix, and a `watch` that silently
    // spins forever on them is worse than an exit code the caller can see.
    let mut client = Client::connect(None)?;
    loop {
        match pump(&client, kinds, &mut cursor) {
            // The stream ended cleanly (host shutdown) — reconnect like any other drop.
            Ok(()) => emit_control("ctl.disconnected", Some("stream closed by the host")),
            Err(e) if e.code == super::client::EXIT_PIN => return Err(e),
            Err(e) => emit_control("ctl.disconnected", Some(&e.message)),
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
        // Rebuild the client on every reconnect rather than reusing it: that re-reads
        // `native-cert.pem`, so a host that regenerated its identity while we were disconnected
        // is picked up instead of pinning us out forever (risk register #1). A pin that is now
        // genuinely wrong still exits 4 on the next attempt, which is the intended signal.
        match Client::connect(None) {
            Ok(c) => {
                client = c;
                backoff = BACKOFF_MIN;
            }
            Err(e) if e.code == super::client::EXIT_PIN => return Err(e),
            Err(e) => emit_control("ctl.disconnected", Some(&e.message)),
        }
    }
}

/// One connection's worth of frames. Returns `Ok(())` when the server closed the stream.
fn pump(client: &Client, kinds: Option<&str>, cursor: &mut Option<u64>) -> Result<()> {
    let mut path = String::from("/api/v1/events");
    let mut sep = '?';
    if let Some(k) = kinds {
        path.push_str(&format!("{sep}kinds={}", urlencode(k)));
        sep = '&';
    }
    if let Some(c) = *cursor {
        path.push_str(&format!("{sep}since={c}"));
    }
    let reader = BufReader::new(client.stream(&path)?);

    // One SSE frame = `id:`/`event:`/`data:` lines terminated by a blank line. Keep-alive comments
    // (`:` prefix) are skipped; they exist to detect a dead peer, not to be forwarded.
    let mut id: Option<u64> = None;
    let mut kind: Option<String> = None;
    let mut data = String::new();
    for line in reader.lines() {
        let line =
            line.map_err(|e| Failure::unreachable(format!("event stream read failed: {e}")))?;
        if line.is_empty() {
            if let Some(k) = kind.take() {
                dispatch(&k, &data, id, cursor);
            }
            id = None;
            data.clear();
            continue;
        }
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            // A comment (`: keep-alive`) splits with an empty field name. The host sends one every
            // 15 s, and we turn it into the one line that proves BOTH directions are alive.
            //
            // The write is the point. Our consumer is a shell widget, and when it dies its end of
            // our stdout pipe closes — but a stream that is only ever READ never notices, so an
            // idle host leaves `ctl watch` running forever against the server's connection cap.
            // Measured on an Omarchy box: six orphaned watchers after three shell restarts, all on
            // a host with no events at all. Writing here turns the next keep-alive into an EPIPE,
            // and [`emit`] exits on it.
            "" => emit(serde_json::json!({ "v": SCHEMA_VERSION, "kind": "ctl.heartbeat" })),
            "id" => id = value.parse().ok(),
            "event" => kind = Some(value.to_string()),
            "data" => {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Turn one decoded frame into a line of stdout, advancing the resume cursor.
fn dispatch(kind: &str, data: &str, id: Option<u64>, cursor: &mut Option<u64>) {
    if let Some(seq) = id {
        *cursor = Some(seq);
    }
    if kind == "dropped" {
        // We fell off the catch-up ring: whatever the consumer believes about pending devices or
        // live sessions may be stale, and no amount of further events will repair it.
        emit_control("ctl.resync", None);
        return;
    }
    let payload = serde_json::from_str::<serde_json::Value>(data)
        .unwrap_or_else(|_| serde_json::Value::String(data.to_string()));
    emit(serde_json::json!({
        "v": SCHEMA_VERSION,
        "kind": kind,
        "seq": id,
        "data": payload,
    }));
}

fn emit_control(kind: &str, error: Option<&str>) {
    let mut line = serde_json::json!({ "v": SCHEMA_VERSION, "kind": kind });
    if let Some(e) = error {
        line["data"] = serde_json::json!({ "error": e });
    }
    emit(line);
}

/// One line, flushed. A widget reading incrementally must not wait on an 8 KiB stdio buffer to
/// fill before it learns a device is knocking.
///
/// **A failed write ends the process**, rather than being ignored as it was: the only reason a
/// write to our own stdout fails is that the consumer is gone, and carrying on would hold an SSE
/// stream open against the host's connection cap for as long as the box stays up. Exit 0 — the
/// consumer going away is a normal end to a `watch`, not an error anyone needs to see.
fn emit(line: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    if writeln!(out, "{line}").is_err() || out.flush().is_err() {
        std::process::exit(0);
    }
}

/// Minimal percent-encoding for the `kinds` query value. The grammar the host accepts is
/// `[a-z0-9_.*,-]`, so this only ever has to escape what a typo could introduce.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'*' | b',' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_survive_encoding_and_typos_are_escaped() {
        assert_eq!(
            urlencode("stream.*,pairing.pending"),
            "stream.*,pairing.pending"
        );
        assert_eq!(urlencode("a b"), "a%20b");
    }

    #[test]
    fn a_dropped_frame_advances_the_cursor_and_asks_for_a_resync() {
        // The cursor must advance even for `dropped`: resuming from before it would replay the
        // same fell-off-the-ring condition on every reconnect.
        let mut cursor = None;
        dispatch("dropped", r#"{"dropped":true}"#, Some(7), &mut cursor);
        assert_eq!(cursor, Some(7));
    }

    #[test]
    fn an_ordinary_frame_advances_the_cursor() {
        let mut cursor = Some(3);
        dispatch(
            "pairing.pending",
            r#"{"kind":"pairing.pending"}"#,
            Some(9),
            &mut cursor,
        );
        assert_eq!(cursor, Some(9));
        // A frame with no id (the synthetic ones) must not rewind it.
        dispatch("stream.started", "{}", None, &mut cursor);
        assert_eq!(cursor, Some(9));
    }
}
