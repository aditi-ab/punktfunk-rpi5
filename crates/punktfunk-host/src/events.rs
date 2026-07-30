//! Host lifecycle event bus (scripting-and-hooks RFC §4, phase 0).
//!
//! A process-wide broadcast bus + bounded catch-up ring for **lifecycle events**: client
//! connect/disconnect, session and stream start/end, pairing decisions, virtual-display
//! create/release, library mutations, host start/stop. Fire sites on BOTH planes call
//! [`emit`]; consumers ([`EventBus::subscribe`]) get a ring-backed catch-up plus a live
//! tail — the shape `GET /api/v1/events` (SSE, phase 1) and the hook runner (phase 2)
//! consume. Until those land, the bus is host-internal.
//!
//! Design notes (mirrors [`crate::log_capture`], the shipped ring precedent):
//! - Events carry a monotonically increasing `seq` (1-based) and a wall-clock `ts_ms`.
//!   A consumer resumes with `since = last seen seq`; one that fell off the ring gets
//!   `dropped = true` and should resync via the REST snapshots.
//! - The wire shape is **versioned and additive-only** within a major ([`SCHEMA_VERSION`]):
//!   fields and kinds may be added, never removed or renamed. The JSON snapshot tests below
//!   are the review gate — a failing snapshot IS a schema change.
//! - **Payload hygiene: events never carry secrets** — no PINs, no tokens, no key material.
//!   Client names and fingerprints are fine (already exposed via the management API).
//! - Emission is fire-and-forget and cheap (a mutex push + a non-blocking broadcast send);
//!   nothing here sits in a streaming hot path. Slow consumers lag (`RecvError::Lagged`)
//!   rather than buffering unboundedly; the SSE layer disconnects them.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use utoipa::ToSchema;

/// Wire-shape major version, carried on every event. Additive-only within a major; removing
/// or renaming a field is a new major (and, at the API layer, a new endpoint negotiation).
pub const SCHEMA_VERSION: u32 = 1;

/// Catch-up ring capacity. Events are small (a few hundred bytes) and low-rate (lifecycle,
/// not per-frame), so 1024 spans hours of ordinary host activity.
const RING_CAPACITY: usize = 1024;
/// Live-tail channel depth per subscriber before a slow consumer starts lagging.
const BROADCAST_CAPACITY: usize = 256;

/// One host lifecycle event, as it will appear on the wire (`data:` of one SSE frame).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct HostEvent {
    /// Monotonic sequence number (1-based) — a consumer resumes with `since = last seen`.
    pub seq: u64,
    /// Unix timestamp in milliseconds (the [`crate::log_capture::LogEntry`] convention).
    pub ts_ms: u64,
    /// Wire-shape version ([`SCHEMA_VERSION`]).
    pub schema: u32,
    /// The event kind + payload, flattened: `"kind": "stream.started", …payload…`.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Which protocol plane an event originated from. Hooks and scripts filter on it — a hook
/// that fires for native clients but not Moonlight clients is a bug, not a v2 feature.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Plane {
    /// The native punktfunk/1 plane (QUIC).
    Native,
    /// The GameStream/Moonlight compat plane (`--gamestream`).
    Gamestream,
}

/// Why a client went away. `Quit` is a deliberate user "stop" (the typed close code);
/// `Timeout` is a transport idle timeout (the client vanished); `Error` is everything else.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DisconnectReason {
    Quit,
    Timeout,
    Error,
}

/// The connecting/disconnecting client's identity.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ClientRef {
    /// Client-supplied device name; may be empty (an anonymous or compat-plane client).
    pub name: String,
    /// Hex SHA-256 certificate fingerprint, when the client presented one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub plane: Plane,
}

/// A live A/V session (the plane-neutral notion the Dashboard shows).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct SessionRef {
    /// Host-local session id (unique within this host process).
    pub id: u64,
    /// Short client label (cert-fingerprint prefix, or peer IP for an anonymous client).
    pub client: String,
    /// Negotiated mode, `WxH@Hz` (e.g. `"3840x2160@120"`).
    pub mode: String,
    pub hdr: bool,
}

/// A live video stream (what the stream marker file reflects).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StreamRef {
    /// Negotiated mode, `WxH@Hz`.
    pub mode: String,
    pub hdr: bool,
    /// Client-supplied device name; may be empty.
    pub client: String,
    /// The launched app/title for this stream, when one was requested (store-qualified id on
    /// the native plane, app title on the GameStream plane).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub plane: Plane,
}

/// A launched game, as the `game.*` events see it.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct GameRefPayload {
    /// Store-qualified library id (`steam:570`). Absent for an operator-typed GameStream
    /// `apps.json` command, which has no library entry behind it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Display title.
    pub title: String,
    /// Which store surfaced it (`steam`, `heroic`, `custom`, …), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    /// Client-supplied device name of the session that launched it; may be empty.
    pub client: String,
    pub plane: Plane,
}

/// Why a launched game is no longer running.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GameEndReason {
    /// The player quit it (or it crashed) — the host did not ask.
    Exited,
    /// The host ended it, per the session⇄game lifetime policy.
    Terminated,
}

/// A device in the pairing flow.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct DeviceRef {
    /// Sanitized device name (the pairing store's copy).
    pub name: String,
    /// Hex certificate fingerprint.
    pub fingerprint: String,
    pub plane: Plane,
}

/// The event catalog (RFC §4). Serialized internally tagged as `"kind": "<domain>.<verb>"`,
/// flattened into [`HostEvent`]. **Additive-only** within [`SCHEMA_VERSION`].
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
#[serde(tag = "kind")]
pub enum EventKind {
    #[serde(rename = "client.connected")]
    ClientConnected { client: ClientRef },
    #[serde(rename = "client.disconnected")]
    ClientDisconnected {
        client: ClientRef,
        reason: DisconnectReason,
    },
    #[serde(rename = "session.started")]
    SessionStarted { session: SessionRef },
    #[serde(rename = "session.ended")]
    SessionEnded { session: SessionRef },
    #[serde(rename = "stream.started")]
    StreamStarted { stream: StreamRef },
    #[serde(rename = "stream.stopped")]
    StreamStopped { stream: StreamRef },
    /// A launched game was confirmed running — fires once per launch, after the host has actually
    /// seen the game's process (not merely spawned its launcher).
    #[serde(rename = "game.running")]
    GameRunning { game: GameRefPayload },
    /// A launched game is gone. `reason` distinguishes the player quitting from the host ending it
    /// per the lifetime policy.
    #[serde(rename = "game.exited")]
    GameExited {
        game: GameRefPayload,
        reason: GameEndReason,
    },
    #[serde(rename = "pairing.pending")]
    PairingPending { device: DeviceRef },
    #[serde(rename = "pairing.completed")]
    PairingCompleted { device: DeviceRef },
    #[serde(rename = "pairing.denied")]
    PairingDenied { device: DeviceRef },
    #[serde(rename = "display.created")]
    DisplayCreated {
        /// The virtual-display backend that minted it (`VirtualDisplay::name`).
        backend: String,
        /// `WxH@Hz`.
        mode: String,
    },
    #[serde(rename = "display.released")]
    DisplayReleased {
        /// How many kept displays this release retired.
        count: u32,
    },
    #[serde(rename = "library.changed")]
    LibraryChanged {
        /// What mutated the library: `"manual"` today; a provider id once the provider
        /// API (RFC §8) lands.
        source: String,
    },
    /// A verified update manifest announced a release newer than the running host. Emitted
    /// once per discovered version (a steady-state "newer exists" doesn't re-fire on every
    /// refresh).
    #[serde(rename = "update.available")]
    UpdateAvailable {
        /// The newer release's version string.
        version: String,
        /// The channel it was announced on (`stable` | `canary`).
        channel: String,
        /// This host's install kind (`apt`, `windows-installer`, …) — lets a hook or the
        /// tray render the right "how to update" hint without a second call.
        install_kind: String,
    },
    /// A host update completed: emitted by boot-time reconciliation, i.e. by the NEW binary's
    /// first start after a successful apply.
    #[serde(rename = "update.applied")]
    UpdateApplied { from: String, to: String },
    #[serde(rename = "plugins.changed")]
    PluginsChanged {
        /// The plugin whose registration changed (registered, restarted, deregistered, or
        /// lease-expired). A consumer re-reads `GET /api/v1/plugins` for the new set.
        id: String,
    },
    #[serde(rename = "store.changed")]
    /// The set of installed plugins, or what the store knows about them, changed — an install or
    /// uninstall finished, or a catalog refresh brought in new rows. A consumer re-reads
    /// `GET /api/v1/store/catalog` / `…/installed`. Deliberately payload-free: the store's answer
    /// is a join over several sources of truth, so "go look again" is the only honest signal.
    StoreChanged,
    #[serde(rename = "host.started")]
    HostStarted {
        version: String,
        /// Whether the GameStream/Moonlight compat plane is enabled.
        gamestream: bool,
    },
    #[serde(rename = "host.stopping")]
    HostStopping,
}

impl EventKind {
    /// The wire kind string (`"stream.started"`, …) — for filters and log lines.
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::ClientConnected { .. } => "client.connected",
            EventKind::ClientDisconnected { .. } => "client.disconnected",
            EventKind::SessionStarted { .. } => "session.started",
            EventKind::SessionEnded { .. } => "session.ended",
            EventKind::StreamStarted { .. } => "stream.started",
            EventKind::StreamStopped { .. } => "stream.stopped",
            EventKind::GameRunning { .. } => "game.running",
            EventKind::GameExited { .. } => "game.exited",
            EventKind::PairingPending { .. } => "pairing.pending",
            EventKind::PairingCompleted { .. } => "pairing.completed",
            EventKind::PairingDenied { .. } => "pairing.denied",
            EventKind::DisplayCreated { .. } => "display.created",
            EventKind::DisplayReleased { .. } => "display.released",
            EventKind::LibraryChanged { .. } => "library.changed",
            EventKind::UpdateAvailable { .. } => "update.available",
            EventKind::UpdateApplied { .. } => "update.applied",
            EventKind::PluginsChanged { .. } => "plugins.changed",
            EventKind::StoreChanged => "store.changed",
            EventKind::HostStarted { .. } => "host.started",
            EventKind::HostStopping => "host.stopping",
        }
    }
}

impl EventKind {
    /// The client/device name this event carries, if any — the `filter.client` axis of hooks
    /// and scripts. (For `session.*` this is the short client *label* the Dashboard shows —
    /// cert-fingerprint prefix or peer IP — since that is what the event carries.)
    pub fn client_name(&self) -> Option<&str> {
        match self {
            EventKind::ClientConnected { client }
            | EventKind::ClientDisconnected { client, .. } => Some(&client.name),
            EventKind::SessionStarted { session } | EventKind::SessionEnded { session } => {
                Some(&session.client)
            }
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                Some(&stream.client)
            }
            EventKind::GameRunning { game } | EventKind::GameExited { game, .. } => {
                Some(&game.client)
            }
            EventKind::PairingPending { device }
            | EventKind::PairingCompleted { device }
            | EventKind::PairingDenied { device } => Some(&device.name),
            _ => None,
        }
    }

    /// The certificate fingerprint this event carries, if any.
    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            EventKind::ClientConnected { client }
            | EventKind::ClientDisconnected { client, .. } => client.fingerprint.as_deref(),
            EventKind::PairingPending { device }
            | EventKind::PairingCompleted { device }
            | EventKind::PairingDenied { device } => Some(&device.fingerprint),
            _ => None,
        }
    }

    /// The protocol plane this event carries, if any.
    pub fn plane(&self) -> Option<Plane> {
        match self {
            EventKind::ClientConnected { client }
            | EventKind::ClientDisconnected { client, .. } => Some(client.plane),
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                Some(stream.plane)
            }
            EventKind::GameRunning { game } | EventKind::GameExited { game, .. } => {
                Some(game.plane)
            }
            EventKind::PairingPending { device }
            | EventKind::PairingCompleted { device }
            | EventKind::PairingDenied { device } => Some(device.plane),
            _ => None,
        }
    }

    /// The launched app id/title this event carries, if any.
    pub fn app(&self) -> Option<&str> {
        match self {
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                stream.app.as_deref()
            }
            // A `game.*` event without a library id came from an operator-typed command; its title
            // is the only handle a hook filter has, so fall back to it rather than matching nothing.
            EventKind::GameRunning { game } | EventKind::GameExited { game, .. } => {
                game.app.as_deref().or(Some(&game.title))
            }
            _ => None,
        }
    }
}

/// Does `pattern` select `kind`? Exact kind names (`stream.started`) or `domain.*` prefixes
/// matched on the dot boundary (`stream.*` matches `stream.started`, never `streamx.started`).
/// One vocabulary for the SSE `?kinds=` filter and the hooks `on:` field.
pub fn kind_matches(pattern: &str, kind: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(prefix) => kind
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.')),
        None => pattern == kind,
    }
}

/// Formats a mode as the wire's `WxH@Hz` string.
pub fn mode_str(width: u32, height: u32, hz: u32) -> String {
    format!("{width}x{height}@{hz}")
}

/// One consumer's view: the ring-backed catch-up plus a live-tail receiver, taken atomically
/// (no event can fall between `catch_up` and the first `rx.recv()`, and none is in both).
pub struct Subscription {
    /// Events with `seq > since`, oldest first.
    pub catch_up: Vec<HostEvent>,
    /// True when events between `since` and the first caught-up one were already evicted —
    /// the consumer should resync via the REST snapshots (the `LogPage.dropped` contract).
    pub dropped: bool,
    /// The live tail. A consumer that can't keep up sees `RecvError::Lagged`.
    pub rx: broadcast::Receiver<HostEvent>,
}

/// The process-wide event bus: a bounded seq-numbered ring (catch-up) + a broadcast channel
/// (live tail).
pub struct EventBus {
    inner: Mutex<Ring>,
    tx: broadcast::Sender<HostEvent>,
}

struct Ring {
    events: VecDeque<HostEvent>,
    next_seq: u64,
}

impl EventBus {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Mutex::new(Ring {
                events: VecDeque::with_capacity(RING_CAPACITY),
                next_seq: 1,
            }),
            tx,
        }
    }

    /// Stamp, ring-buffer, and broadcast one event. Fire-and-forget: no receivers is fine
    /// (the ring still records it for a later subscriber's catch-up).
    pub fn emit(&self, kind: EventKind) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut ring = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ev = HostEvent {
            seq: ring.next_seq,
            ts_ms,
            schema: SCHEMA_VERSION,
            kind,
        };
        ring.next_seq += 1;
        if ring.events.len() == RING_CAPACITY {
            ring.events.pop_front();
        }
        ring.events.push_back(ev.clone());
        // Send while still holding the ring lock: it serializes with `subscribe` (which also
        // takes the lock), so an event lands either in a subscriber's catch-up or on its live
        // tail — never both, never neither. `send` is non-blocking, the hold is trivial.
        let _ = self.tx.send(ev);
    }

    /// A live-tail-only subscription (no catch-up, no cursor) — for host-internal consumers
    /// like the hook runner that only care about events from now on.
    pub fn subscribe_live(&self) -> broadcast::Receiver<HostEvent> {
        self.tx.subscribe()
    }

    /// Subscribe with a resume cursor: events with `seq > since` come back as catch-up, the
    /// returned receiver carries everything after. `since = 0` means "from the ring start".
    pub fn subscribe(&self, since: u64) -> Subscription {
        let ring = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        let first_seq = ring.events.front().map_or(ring.next_seq, |e| e.seq);
        let dropped = since != 0 && since.saturating_add(1) < first_seq;
        let catch_up = ring
            .events
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();
        Subscription {
            catch_up,
            dropped,
            rx,
        }
    }
}

/// The process-wide bus — a `OnceLock` singleton (the [`crate::log_capture::ring`] shape) so
/// fire sites across both planes and the API layer share it without threading an `Arc`.
pub fn bus() -> &'static EventBus {
    static BUS: OnceLock<EventBus> = OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

/// Emit one lifecycle event on the process-wide bus. Cheap and non-blocking — safe from any
/// thread, including RAII `Drop` paths.
pub fn emit(kind: EventKind) {
    bus().emit(kind);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str) -> EventKind {
        EventKind::LibraryChanged {
            source: name.to_string(),
        }
    }

    #[test]
    fn seq_is_monotonic_and_catch_up_resumes() {
        let bus = EventBus::new();
        for i in 0..5 {
            bus.emit(ev(&format!("m{i}")));
        }
        let sub = bus.subscribe(0);
        assert_eq!(
            sub.catch_up.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(!sub.dropped);
        assert!(sub.catch_up.iter().all(|e| e.schema == SCHEMA_VERSION));

        // Resume from a cursor mid-ring.
        let sub = bus.subscribe(3);
        assert_eq!(
            sub.catch_up.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![4, 5]
        );
        assert!(!sub.dropped);

        // Cursor at the tip: empty catch-up, not a gap.
        let sub = bus.subscribe(5);
        assert!(sub.catch_up.is_empty());
        assert!(!sub.dropped);
    }

    #[test]
    fn eviction_reports_dropped() {
        let bus = EventBus::new();
        for i in 0..(RING_CAPACITY + 50) {
            bus.emit(ev(&format!("m{i}")));
        }
        // Seqs 1..=50 were evicted; a cursor inside the gap must flag it.
        let sub = bus.subscribe(10);
        assert!(sub.dropped);
        assert_eq!(sub.catch_up.first().map(|e| e.seq), Some(51));
        // A fresh consumer (since = 0) is a backfill, not a gap.
        let sub = bus.subscribe(0);
        assert!(!sub.dropped);
        assert_eq!(sub.catch_up.len(), RING_CAPACITY);
    }

    #[tokio::test]
    async fn live_tail_continues_exactly_after_catch_up() {
        let bus = EventBus::new();
        bus.emit(ev("before-1"));
        bus.emit(ev("before-2"));
        let mut sub = bus.subscribe(0);
        assert_eq!(sub.catch_up.len(), 2);
        // Emitted after subscribe → on the live tail only, starting at exactly seq 3.
        bus.emit(ev("after"));
        let live = sub.rx.recv().await.expect("live event");
        assert_eq!(live.seq, 3);
        assert_eq!(live.kind.name(), "library.changed");
        // Nothing duplicated: the tail holds only what wasn't in the catch-up.
        assert!(sub.rx.try_recv().is_err());
    }

    /// The wire shape IS the contract (additive-only, RFC §4): these snapshots are the review
    /// gate — if one fails, the change renames/removes a field and needs a schema-version bump,
    /// not a test update.
    #[test]
    fn wire_shape_snapshots() {
        let ev = HostEvent {
            seq: 4182,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::StreamStarted {
                stream: StreamRef {
                    mode: mode_str(3840, 2160, 120),
                    hdr: true,
                    client: "Living Room TV".into(),
                    app: Some("steam:570".into()),
                    plane: Plane::Native,
                },
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":4182,"ts_ms":1700000000000,"schema":1,"kind":"stream.started","stream":{"mode":"3840x2160@120","hdr":true,"client":"Living Room TV","app":"steam:570","plane":"native"}}"#
        );

        let ev = HostEvent {
            seq: 1,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::ClientDisconnected {
                client: ClientRef {
                    name: "Deck".into(),
                    fingerprint: Some("b1c2".into()),
                    plane: Plane::Gamestream,
                },
                reason: DisconnectReason::Timeout,
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":1,"ts_ms":1700000000000,"schema":1,"kind":"client.disconnected","client":{"name":"Deck","fingerprint":"b1c2","plane":"gamestream"},"reason":"timeout"}"#
        );

        let ev = HostEvent {
            seq: 2,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::HostStopping,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":2,"ts_ms":1700000000000,"schema":1,"kind":"host.stopping"}"#
        );

        let ev = HostEvent {
            seq: 3,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::PluginsChanged {
                id: "rom-manager".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":3,"ts_ms":1700000000000,"schema":1,"kind":"plugins.changed","id":"rom-manager"}"#
        );

        let ev = HostEvent {
            seq: 5,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::GameRunning {
                game: GameRefPayload {
                    app: Some("steam:570".into()),
                    title: "Dota 2".into(),
                    store: Some("steam".into()),
                    client: "Living Room TV".into(),
                    plane: Plane::Native,
                },
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":5,"ts_ms":1700000000000,"schema":1,"kind":"game.running","game":{"app":"steam:570","title":"Dota 2","store":"steam","client":"Living Room TV","plane":"native"}}"#
        );

        // A game the host ended itself, and one with no library entry behind it (an operator-typed
        // GameStream command) — the optional ids are omitted, not nulled.
        let ev = HostEvent {
            seq: 6,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::GameExited {
                game: GameRefPayload {
                    app: None,
                    title: "Big Picture".into(),
                    store: None,
                    client: String::new(),
                    plane: Plane::Gamestream,
                },
                reason: GameEndReason::Terminated,
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":6,"ts_ms":1700000000000,"schema":1,"kind":"game.exited","game":{"title":"Big Picture","client":"","plane":"gamestream"},"reason":"terminated"}"#
        );
    }

    /// The `game.*` events must be reachable by the same hook/SSE filters as every other kind — a
    /// filterable event nobody can select is not a feature.
    #[test]
    fn game_events_are_filterable() {
        let running = EventKind::GameRunning {
            game: GameRefPayload {
                app: Some("steam:570".into()),
                title: "Dota 2".into(),
                store: Some("steam".into()),
                client: "Deck".into(),
                plane: Plane::Native,
            },
        };
        assert_eq!(running.name(), "game.running");
        assert!(kind_matches("game.*", running.name()));
        assert!(kind_matches("game.running", running.name()));
        assert!(!kind_matches("gamestream.*", running.name()));
        assert_eq!(running.client_name(), Some("Deck"));
        assert_eq!(running.plane(), Some(Plane::Native));
        assert_eq!(running.app(), Some("steam:570"));

        // With no library id, the title is the filterable handle — otherwise an `apps.json` launch
        // would be unmatchable by app.
        let exited = EventKind::GameExited {
            game: GameRefPayload {
                app: None,
                title: "Big Picture".into(),
                store: None,
                client: String::new(),
                plane: Plane::Gamestream,
            },
            reason: GameEndReason::Exited,
        };
        assert_eq!(exited.app(), Some("Big Picture"));
        assert_eq!(exited.fingerprint(), None);
    }

    #[test]
    fn wire_shape_roundtrips() {
        let ev = HostEvent {
            seq: 7,
            ts_ms: 3,
            schema: 1,
            kind: EventKind::PairingPending {
                device: DeviceRef {
                    name: "iPad Pro".into(),
                    fingerprint: "ab12".into(),
                    plane: Plane::Native,
                },
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: HostEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.kind.name(), "pairing.pending");
        match back.kind {
            EventKind::PairingPending { device } => {
                assert_eq!(device.name, "iPad Pro");
                assert_eq!(device.plane, Plane::Native);
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }
}
