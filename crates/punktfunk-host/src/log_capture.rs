//! In-memory capture of the host's own log stream for the web console.
//!
//! A `tracing` layer tees every event at DEBUG and above — independent of the `RUST_LOG` filter
//! that gates stderr/file output — into a bounded in-process ring, and the management API serves
//! it as `GET /api/v1/logs` (see `mgmt.rs`). That gives an operator the host's recent logs from
//! the web console without shell access to the box, which is where gamepad-driver / capture /
//! encoder failures otherwise go to die ("it just doesn't work" bug reports).
//!
//! The ring keeps the *newest* [`CAPACITY`] entries (a log tail — unlike the stats recorder,
//! which keeps the head of a capture). Readers poll with an `after` sequence cursor.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

/// Ring capacity — bounds memory at a few MB worst case ([`MAX_MSG`]-sized entries).
const CAPACITY: usize = 4096;
/// Per-entry message cap; log lines are short, anything longer is a payload dump we truncate.
const MAX_MSG: usize = 2048;
/// Hard cap on entries returned per poll (the client immediately re-polls to drain a backlog).
pub const MAX_PAGE: usize = 1000;

/// One captured log event.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct LogEntry {
    /// Monotonic sequence number (1-based) — pass the last one back as the `after` cursor.
    pub seq: u64,
    /// Unix timestamp in milliseconds.
    pub ts_ms: u64,
    /// `ERROR` | `WARN` | `INFO` | `DEBUG` | `TRACE`.
    pub level: String,
    /// The emitting module path (tracing target).
    pub target: String,
    /// The formatted message, structured fields appended as `key=value`.
    pub msg: String,
}

/// One poll's worth of log entries.
#[derive(Serialize, Deserialize, ToSchema, Debug)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    /// Cursor for the next poll (the last returned seq, or the request's `after` when empty).
    pub next: u64,
    /// True when entries between `after` and the first returned one were already evicted.
    pub dropped: bool,
}

/// The process-wide log ring (see [`ring`]).
pub struct LogRing {
    inner: Mutex<Inner>,
}

struct Inner {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
}

impl LogRing {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: VecDeque::with_capacity(CAPACITY),
                next_seq: 1,
            }),
        }
    }

    /// `pub(crate)` for the mgmt handler tests; production entries only come from [`RingLayer`].
    pub(crate) fn push(&self, level: &tracing::Level, target: &str, msg: String) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() == CAPACITY {
            inner.entries.pop_front();
        }
        inner.entries.push_back(LogEntry {
            seq,
            ts_ms,
            level: level.to_string(),
            target: target.to_string(),
            msg,
        });
    }

    /// Entries with `seq > after`, oldest first, capped at `limit` (≤ [`MAX_PAGE`]).
    pub fn since(&self, after: u64, limit: usize) -> LogPage {
        let limit = limit.clamp(1, MAX_PAGE);
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Entries are seq-ordered and contiguous: index of the first wanted one is derivable.
        let first_seq = inner.entries.front().map_or(inner.next_seq, |e| e.seq);
        let dropped = after != 0 && after + 1 < first_seq;
        let skip = after
            .saturating_sub(first_seq)
            .saturating_add(u64::from(after >= first_seq)) as usize;
        let entries: Vec<LogEntry> = inner
            .entries
            .iter()
            .skip(skip)
            .take(limit)
            .cloned()
            .collect();
        let next = entries.last().map_or(after, |e| e.seq);
        LogPage {
            entries,
            next,
            dropped,
        }
    }
}

/// The process-wide ring — a `OnceLock` singleton so the tracing layer (installed in `main()`
/// before any host state exists) and the mgmt handler share it without threading an `Arc`.
pub fn ring() -> &'static LogRing {
    static RING: OnceLock<LogRing> = OnceLock::new();
    RING.get_or_init(LogRing::new)
}

/// The tee: a `tracing_subscriber` layer pushing every event into [`ring`]. Install with a
/// per-layer `LevelFilter::DEBUG` so the ring sees DEBUG even when `RUST_LOG` keeps stderr at
/// `info` (remote debugging must not require a restart with a different env).
pub struct RingLayer;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let meta = event.metadata();
        let mut fields = FieldFmt::default();
        event.record(&mut fields);
        ring().push(meta.level(), meta.target(), fields.finish());
    }
}

/// Formats an event's fields like the default fmt layer: the `message` field first, every other
/// field appended as ` key=value`.
#[derive(Default)]
struct FieldFmt {
    msg: String,
    fields: String,
}

impl tracing::field::Visit for FieldFmt {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            let _ = write!(self.msg, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.msg.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }
}

impl FieldFmt {
    fn finish(mut self) -> String {
        if self.msg.is_empty() {
            self.msg = self.fields.trim_start().to_string();
        } else {
            self.msg.push_str(&self.fields);
        }
        if self.msg.len() > MAX_MSG {
            let mut end = MAX_MSG;
            while !self.msg.is_char_boundary(end) {
                end -= 1;
            }
            self.msg.truncate(end);
            self.msg.push('…');
        }
        self.msg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_n(ring: &LogRing, n: usize) {
        for i in 0..n {
            ring.push(&tracing::Level::INFO, "test", format!("m{i}"));
        }
    }

    #[test]
    fn cursor_pagination_and_eviction() {
        let ring = LogRing::new();
        push_n(&ring, 10);

        // Full backfill from 0.
        let page = ring.since(0, 100);
        assert_eq!(page.entries.len(), 10);
        assert_eq!(page.next, 10);
        assert!(!page.dropped);

        // Incremental: nothing new.
        let page = ring.since(10, 100);
        assert!(page.entries.is_empty());
        assert_eq!(page.next, 10);

        // Incremental: partial.
        let page = ring.since(4, 3);
        assert_eq!(
            page.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![5, 6, 7]
        );
        assert_eq!(page.next, 7);
        assert!(!page.dropped);
    }

    #[test]
    fn eviction_reports_dropped() {
        let ring = LogRing::new();
        push_n(&ring, CAPACITY + 50);
        // Seqs 1..=50 were evicted; a cursor inside the gap must flag it.
        let page = ring.since(10, 5);
        assert!(page.dropped);
        assert_eq!(page.entries.first().map(|e| e.seq), Some(51));
        // A cursor at the ring head is not a gap.
        let head = ring.since(page.next, 5);
        assert!(!head.dropped);
        assert_eq!(head.entries.first().map(|e| e.seq), Some(page.next + 1));
    }

    #[test]
    fn layer_captures_events_into_the_singleton_ring() {
        use tracing_subscriber::layer::SubscriberExt;

        // The singleton ring is process-wide — find its current tail first (parallel tests may
        // interleave, so only assert on OUR event appearing after it).
        let mut cur = 0;
        loop {
            let page = ring().since(cur, MAX_PAGE);
            if page.entries.is_empty() {
                break;
            }
            cur = page.next;
        }

        let subscriber = tracing_subscriber::registry().with(RingLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(answer = 42, "ring layer test message");
        });

        let page = ring().since(cur, MAX_PAGE);
        let hit = page
            .entries
            .iter()
            .find(|e| e.msg.contains("ring layer test message"))
            .expect("event captured");
        assert_eq!(hit.level, "WARN");
        assert!(
            hit.msg.contains("answer=42"),
            "fields appended: {}",
            hit.msg
        );
        assert!(hit.target.contains("log_capture"), "target: {}", hit.target);
        assert!(hit.ts_ms > 0);
    }

    #[test]
    fn message_truncation_keeps_char_boundary() {
        let f = FieldFmt {
            msg: "ä".repeat(MAX_MSG), // 2 bytes each — exceeds the cap at a multi-byte boundary
            ..Default::default()
        };
        let out = f.finish();
        assert!(out.ends_with('…'));
        assert!(out.len() <= MAX_MSG + '…'.len_utf8());
    }
}
