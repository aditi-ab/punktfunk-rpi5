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
//!
//! `log`-crate events (arriving via the tracing-log bridge) are normalized to their real module
//! path, and known-chatty third-party targets ([`NOISY_DEBUG_TARGETS`]) are demoted to
//! INFO-and-up so ambient LAN noise can't evict the tail the ring exists to preserve.

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
        self.push_entry(level.to_string(), target.to_string(), msg, ts_ms);
    }

    /// Ingest a line that was produced in **another process** — the plugin/script runner, via
    /// `POST /plugins/logs` (see `mgmt::plugins::ingest_plugin_logs`).
    ///
    /// Plugins are not host child processes: the runner is a separate bun process that `import()`s
    /// each plugin in-process, so a plugin's output never passes through this process's `tracing`
    /// and [`RingLayer`] can't see it. Without this door the console's log page shows nothing about
    /// the plugins at all, and on Windows nothing else does either — the runner task writes no log
    /// file, so a failing plugin was diagnosable only by stopping the task and re-running it by
    /// hand (field report 2026-08-03, the VirtualHere plugin).
    ///
    /// The caller's `ts_ms` is kept — the line was stamped when it happened, and re-stamping it on
    /// arrival would collapse a whole batch onto the moment it was flushed. `seq` stays ours: it is
    /// the cursor for a single ring with several producers, so only the ring can mint it.
    pub fn push_remote(&self, level: &str, target: &str, msg: &str, ts_ms: u64) {
        self.push_entry(
            normalize_level(level).to_string(),
            target.to_string(),
            truncate_msg(msg.to_string()),
            ts_ms,
        );
    }

    fn push_entry(&self, level: String, target: String, msg: String, ts_ms: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() == CAPACITY {
            inner.entries.pop_front();
        }
        inner.entries.push_back(LogEntry {
            seq,
            ts_ms,
            level,
            target,
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

/// Coerce an externally-supplied level to the five the console's filter ranks. Anything else —
/// a plugin inventing `NOTICE`, a truncated line, empty — becomes `INFO` rather than being
/// rejected: an unfamiliar level is not a reason to drop the operator's diagnostics on the floor,
/// and an unranked string would sort as `0` in the console's `RANK` map and hide under every filter.
fn normalize_level(level: &str) -> &'static str {
    match level.trim().to_ascii_uppercase().as_str() {
        "ERROR" | "FATAL" | "SEVERE" => "ERROR",
        "WARN" | "WARNING" => "WARN",
        "DEBUG" => "DEBUG",
        "TRACE" | "VERBOSE" => "TRACE",
        _ => "INFO",
    }
}

/// Cap a message at [`MAX_MSG`], cutting on a char boundary and marking the elision.
fn truncate_msg(mut msg: String) -> String {
    if msg.len() > MAX_MSG {
        let mut end = MAX_MSG;
        while !msg.is_char_boundary(end) {
            end -= 1;
        }
        msg.truncate(end);
        msg.push('…');
    }
    msg
}

/// Targets whose DEBUG/TRACE output is steady-state chatter, not diagnostics — left in, they evict
/// the entire ring tail: `mdns_sd` DEBUG-logs every multicast packet it can't parse (one chatty
/// AirPlay/HomePod device on the LAN floods thousands of entries per hour), and `wasapi` DEBUG-logs
/// the default audio device once a second (the device-watchdog poll). The ring keeps their
/// INFO-and-up; the file/stderr filter caps them separately (see `main`'s EnvFilter directives).
/// Prefix-matched on module path boundaries.
const NOISY_DEBUG_TARGETS: &[&str] = &["mdns_sd", "wasapi"];

fn is_noisy_debug(target: &str) -> bool {
    NOISY_DEBUG_TARGETS.iter().any(|t| {
        target
            .strip_prefix(t)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    })
}

/// Init the `log`→`tracing` bridge and install `subscriber` as the global default. Replaces
/// `SubscriberInitExt::init()` (which auto-inits the bridge with no crate filtering) so we can
/// **drop the `wasapi` crate's records at the bridge**: it polls the default audio device ~1×/s
/// and `log::debug!`s it, and those bridged events carry the bridge shim target at *filter* time,
/// so a downstream level/target filter on the file layer can't catch them (the ring can, in
/// `on_event`, via `normalized_metadata` — but the fmt layer filters pre-event). `ignore_crate`
/// stops them at the source, before they ever become tracing events, so neither sink sees them.
/// The bridge max-level stays DEBUG so every *other* `log`-crate dependency still reaches the ring.
pub fn install_global<S>(subscriber: S)
where
    S: tracing::Subscriber + Send + Sync + 'static,
{
    let _ = tracing_log::LogTracer::builder()
        .with_max_level(log::LevelFilter::Debug)
        .ignore_crate("wasapi")
        .init();
    let _ = tracing::subscriber::set_global_default(subscriber);
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
        // Events from `log`-crate dependencies arrive through the tracing-log bridge under the
        // shim target "log"; normalize back to the record's real module path so the console's
        // target column and the noise gate below see `mdns_sd::…`.
        use tracing_log::NormalizeEvent;
        let normalized = event.normalized_metadata();
        let meta = normalized.as_ref().unwrap_or_else(|| event.metadata());
        if *meta.level() > tracing::Level::INFO && is_noisy_debug(meta.target()) {
            return;
        }
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
        } else if !field.name().starts_with("log.") {
            // `log.target`/`log.file`/… are tracing-log bridge bookkeeping (already surfaced via
            // the normalized target), same suppression as the stderr fmt layer.
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            self.msg.push_str(value);
        } else if !field.name().starts_with("log.") {
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
        truncate_msg(self.msg)
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

    /// The singleton ring is process-wide — tests find its current tail first (parallel tests
    /// may interleave, so they only assert on THEIR events appearing after it).
    fn tail_seq() -> u64 {
        let mut cur = 0;
        loop {
            let page = ring().since(cur, MAX_PAGE);
            if page.entries.is_empty() {
                return cur;
            }
            cur = page.next;
        }
    }

    #[test]
    fn layer_captures_events_into_the_singleton_ring() {
        use tracing_subscriber::layer::SubscriberExt;

        let cur = tail_seq();

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
    fn log_bridge_events_normalize_target_and_noisy_debug_is_dropped() {
        use tracing_subscriber::layer::SubscriberExt;

        // Route `log` records into tracing (what SubscriberInitExt::init does in main). Global,
        // so tolerate a prior install; max_level explicit so debug! records reach the bridge.
        let _ = tracing_log::LogTracer::init();
        log::set_max_level(log::LevelFilter::Trace);

        let cur = tail_seq();

        let subscriber = tracing_subscriber::registry().with(RingLayer);
        tracing::subscriber::with_default(subscriber, || {
            log::debug!(target: "mdns_sd::service_daemon", "Invalid incoming DNS message: flood");
            log::warn!(target: "mdns_sd::service_daemon", "a real mdns problem");
            log::debug!(target: "mdns_sdx", "not actually mdns-sd");
        });

        let page = ring().since(cur, MAX_PAGE);
        assert!(
            !page.entries.iter().any(|e| e.msg.contains("flood")),
            "noisy-target DEBUG must not reach the ring"
        );
        let warn = page
            .entries
            .iter()
            .find(|e| e.msg.contains("a real mdns problem"))
            .expect("noisy-target WARN kept");
        // Normalized off the bridge's "log" shim, and the log.* bookkeeping fields are hidden.
        assert_eq!(warn.target, "mdns_sd::service_daemon");
        assert!(!warn.msg.contains("log.target"), "msg: {}", warn.msg);
        // Prefix match respects module-path boundaries.
        assert!(page.entries.iter().any(|e| e.target == "mdns_sdx"));
    }

    #[test]
    fn remote_entries_keep_their_own_timestamp_and_share_the_cursor() {
        let ring = LogRing::new();
        ring.push(&tracing::Level::INFO, "punktfunk_host", "local".into());
        ring.push_remote("WARN", "plugin:virtualhere", "remote", 1_700_000_000_123);

        let page = ring.since(0, 10);
        assert_eq!(page.entries.len(), 2);
        // One sequence across both producers — the console's cursor cannot see two rings.
        assert_eq!(page.entries[0].seq, 1);
        assert_eq!(page.entries[1].seq, 2);
        let remote = &page.entries[1];
        assert_eq!(remote.level, "WARN");
        assert_eq!(remote.target, "plugin:virtualhere");
        assert_eq!(remote.msg, "remote");
        // Stamped when it happened, not when the batch arrived.
        assert_eq!(remote.ts_ms, 1_700_000_000_123);
    }

    #[test]
    fn remote_levels_are_coerced_not_rejected() {
        assert_eq!(normalize_level("error"), "ERROR");
        assert_eq!(normalize_level(" Warning "), "WARN");
        assert_eq!(normalize_level("TRACE"), "TRACE");
        // An unranked level would sort as 0 in the console's filter and hide under every setting.
        assert_eq!(normalize_level("NOTICE"), "INFO");
        assert_eq!(normalize_level(""), "INFO");
    }

    #[test]
    fn remote_messages_are_truncated_like_local_ones() {
        let ring = LogRing::new();
        ring.push_remote("INFO", "plugin:x", &"ä".repeat(MAX_MSG), 1);
        let page = ring.since(0, 10);
        let msg = &page.entries[0].msg;
        assert!(msg.ends_with('…'));
        assert!(msg.len() <= MAX_MSG + '…'.len_utf8());
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
