//! `GET /api/v1/events` — the host lifecycle event stream (scripting-and-hooks RFC §5, M1).
//!
//! Server-Sent Events over the existing HTTPS serve loop: each event goes out as one SSE frame
//! with `id:` = the event's `seq`, `event:` = its kind (`stream.started`, …), and `data:` = the
//! [`crate::events::HostEvent`] JSON. Consumers resume with the standard `Last-Event-ID` header
//! (or its `?since=` query twin); a consumer that fell off the catch-up ring gets a synthetic
//! `event: dropped` frame first and should resync via the REST snapshots (`/status`, `/clients`,
//! …). `?kinds=` filters server-side (exact kinds or `domain.*` prefixes, comma-separated).
//!
//! Bounds (RFC §9.6): at most [`MAX_EVENT_STREAMS`] concurrent streams (503 beyond — the
//! consumer retries; SSE clients reconnect by themselves), and a consumer too slow for the
//! live tail (broadcast lag) is **disconnected**, never buffered unboundedly — its reconnect
//! resumes from the ring via `Last-Event-ID`.
//!
//! Auth: deliberately NOT on the mTLS read-only allowlist (`auth::cert_may_access`) — the
//! stream is part of the loopback + bearer admin lane in v1 (RFC §5; revisit when paired
//! clients want an activity feed).

use super::shared::*;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::broadcast;

/// Concurrent SSE connection cap. Operators run a handful of consumers (console feed, a couple
/// of scripts/plugins); 32 is generous headroom while bounding a runaway reconnect loop.
const MAX_EVENT_STREAMS: usize = 32;

/// SSE keep-alive comment interval — detects a dead peer and keeps middleboxes from idling
/// the connection out between (low-rate) lifecycle events.
const KEEP_ALIVE: Duration = Duration::from_secs(15);

static LIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// RAII slot in the connection cap: taken before the stream starts, released when the SSE body
/// is dropped (client disconnect, slow-consumer cut, server shutdown). `pub(crate)` only for
/// the [`test_support`] saturation helper's return type.
pub(crate) struct StreamSlot;

fn try_acquire_slot() -> Option<StreamSlot> {
    LIVE_STREAMS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < MAX_EVENT_STREAMS).then_some(n + 1)
        })
        .ok()
        .map(|_| StreamSlot)
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        LIVE_STREAMS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `?kinds=stream.*,pairing.pending` — exact kind names or `domain.*` prefixes. `None` = all.
struct KindFilter(Option<Vec<String>>);

impl KindFilter {
    fn parse(kinds: Option<&str>) -> Self {
        let pats: Option<Vec<String>> = kinds.map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        });
        KindFilter(pats.filter(|p| !p.is_empty()))
    }

    fn matches(&self, kind: &str) -> bool {
        match &self.0 {
            None => true,
            Some(pats) => pats.iter().any(|p| crate::events::kind_matches(p, kind)),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct EventsQuery {
    /// Resume cursor: stream events with `seq > since`. The `Last-Event-ID` header (what an SSE
    /// client sends on auto-reconnect) takes precedence when both are present.
    since: Option<u64>,
    /// Comma-separated kind filter (`stream.*,pairing.pending`).
    kinds: Option<String>,
}

/// One [`crate::events::HostEvent`] as an SSE frame: `id:` = seq (drives `Last-Event-ID`),
/// `event:` = kind, `data:` = the full event JSON (kind included — the frame is self-contained
/// for consumers that read `data` only).
fn sse_event(ev: &crate::events::HostEvent) -> Event {
    Event::default()
        .id(ev.seq.to_string())
        .event(ev.kind.name())
        .data(serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string()))
}

/// Everything the poll loop owns; dropping it (the response body going away) releases the
/// connection-cap slot and the broadcast receiver.
struct StreamState {
    /// The dropped marker (when applicable) + the filtered catch-up, served before the live tail.
    pending: VecDeque<Event>,
    rx: broadcast::Receiver<crate::events::HostEvent>,
    filter: KindFilter,
    _slot: StreamSlot,
}

/// Stream host lifecycle events (SSE)
///
/// Server-Sent Events stream of the host's lifecycle events: client connect/disconnect, session
/// and stream start/end, pairing decisions, display create/release, library changes, host
/// start/stop — both protocol planes. Frames carry `id:` = the event's monotonic `seq`,
/// `event:` = its kind, and `data:` = the event JSON (schema-versioned, additive-only).
///
/// Resume: standard `Last-Event-ID` (or `?since=`) replays from the in-memory ring; a consumer
/// that fell off the ring receives an `event: dropped` frame first and should resync via the
/// REST snapshots. Keep-alive comments are sent every 15 s.
#[utoipa::path(
    get,
    path = "/events",
    tag = "events",
    operation_id = "streamEvents",
    params(
        ("since" = Option<u64>, Query, description = "Resume cursor: only events with `seq` greater than this are sent (the ring keeps the newest ~1024). `Last-Event-ID` takes precedence."),
        ("kinds" = Option<String>, Query, description = "Comma-separated server-side kind filter: exact kinds (`pairing.pending`) or `domain.*` prefixes (`stream.*`)."),
        ("Last-Event-ID" = Option<u64>, Header, description = "SSE auto-reconnect cursor — the `id:` of the last received frame."),
    ),
    responses(
        (status = OK, description = "SSE stream; each frame's `data:` is one HostEvent", body = crate::events::HostEvent, content_type = "text/event-stream"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Concurrent event-stream cap reached — retry shortly", body = ApiError),
    )
)]
pub(crate) async fn stream_events(Query(q): Query<EventsQuery>, headers: HeaderMap) -> Response {
    let Some(slot) = try_acquire_slot() else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "event-stream connection cap reached — close an existing stream or retry",
        );
    };
    // The header is what an SSE client re-sends on auto-reconnect, so it is always the newer
    // cursor when both it and the original URL's `?since=` are present.
    let since = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(q.since)
        .unwrap_or(0);
    let filter = KindFilter::parse(q.kinds.as_deref());
    let sub = crate::events::bus().subscribe(since);

    let mut pending = VecDeque::new();
    if sub.dropped {
        // The consumer's cursor precedes the ring: tell it so (the `LogPage.dropped` contract)
        // — its move is a REST resync, not trust in a complete replay.
        pending.push_back(
            Event::default()
                .event("dropped")
                .data(r#"{"dropped":true}"#),
        );
    }
    pending.extend(
        sub.catch_up
            .iter()
            .filter(|ev| filter.matches(ev.kind.name()))
            .map(sse_event),
    );

    let state = StreamState {
        pending,
        rx: sub.rx,
        filter,
        _slot: slot,
    };
    let stream = futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(ev) = st.pending.pop_front() {
                return Some((Ok::<_, std::convert::Infallible>(ev), st));
            }
            match st.rx.recv().await {
                Ok(ev) => {
                    if st.filter.matches(ev.kind.name()) {
                        return Some((Ok(sse_event(&ev)), st));
                    }
                }
                // Lagged = this consumer is too slow for the live tail: cut it loose (bounded
                // memory, RFC §9.6) — its auto-reconnect resumes from the ring, which flags
                // `dropped` if it also fell off that. Closed = host shutdown.
                Err(broadcast::error::RecvError::Lagged(_))
                | Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

#[cfg(test)]
pub(crate) mod test_support {
    /// Fill the connection cap and hand the slots to a test ([`Drop`] frees them). The events
    /// tests serialize on a lock so the full cap can't 503 an unrelated concurrent test.
    pub(crate) fn saturate_slots() -> Vec<super::StreamSlot> {
        std::iter::from_fn(super::try_acquire_slot).collect()
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn kind_filter_semantics() {
        let all = KindFilter::parse(None);
        assert!(all.matches("stream.started"));

        let f = KindFilter::parse(Some("stream.*, pairing.pending"));
        assert!(f.matches("stream.started"));
        assert!(f.matches("stream.stopped"));
        assert!(f.matches("pairing.pending"));
        assert!(!f.matches("pairing.completed"));
        assert!(!f.matches("client.connected"));
        // A prefix pattern must match on the dot boundary, not raw text.
        assert!(!f.matches("streamx.started"));

        // Empty/blank filter strings mean "no filter", not "nothing matches".
        assert!(KindFilter::parse(Some("")).matches("host.started"));
        assert!(KindFilter::parse(Some(" , ")).matches("host.started"));
    }
}
