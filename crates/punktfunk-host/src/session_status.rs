//! Plane-neutral live-session status for the management `/status` (web-console Dashboard) view.
//!
//! The GameStream media pipeline records its session in `AppState.{launch, stream, streaming}`
//! (consumed by RTSP/media), but the native punktfunk/1 plane never touches `AppState` — by design
//! it is handed only the shared stats recorder ([`crate::native::serve`]). So a native session,
//! which is the DEFAULT plane (GameStream is opt-in, `--gamestream`), was invisible on the Dashboard:
//! `GET /status` reported `video_streaming: false` and no session/stream card while a client was
//! actively streaming (the Stats page worked because it shares the recorder — hence the confusing
//! "stats move but the dashboard says idle").
//!
//! This module is the small shared surface the native video loop ([`crate::native::virtual_stream`])
//! publishes a live snapshot to, keyed per session so CONCURRENT native sessions each get an entry
//! (the native server admits up to `max_sessions`, unbounded by default). The loop registers on
//! stream start and the returned [`LiveSessionGuard`] removes the entry on ANY scope exit (return,
//! `?`, panic — RAII). `/status` reads [`snapshot`]/[`count`]; the Dashboard's session-control
//! buttons reach a native session through [`stop_all`] (stop) and [`force_idr_all`] (request IDR),
//! so surfacing the session doesn't leave those buttons dead.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::encode::Codec;

/// A single live native session. Holds the SAME live Arc handles the video loop already maintains,
/// so a mid-stream mode switch / adaptive-bitrate change is reflected on the Dashboard with no
/// second update path — plus the flags the mgmt API flips to control the session.
struct LiveSession {
    id: u64,
    /// Packed `w:16|h:16|hz:16` ([`crate::native::pack_mode`]); updated on a mode switch.
    mode: Arc<AtomicU64>,
    /// Live encoder target (kbps); updated on an adaptive-bitrate change.
    bitrate_kbps: Arc<AtomicU32>,
    codec: Codec,
    /// The session's teardown flag ([`stop_all`] → mgmt `DELETE /session`).
    stop: Arc<AtomicBool>,
    /// The session's *deliberate*-stop flag ([`stop_all_quit`]). Distinct from `stop`: it marks the
    /// teardown as intended rather than a drop, which is what skips the display's keep-alive linger
    /// and what the end-game-on-session-end policy keys off.
    quit: Arc<AtomicBool>,
    /// One-shot force-keyframe flag ([`force_idr_all`] → mgmt `POST /session/idr`); the encode loop
    /// drains it alongside a client's decode-recovery keyframe request.
    force_idr: Arc<AtomicBool>,
    /// Short client label (cert-fingerprint prefix / peer IP) — carried on the lifecycle events.
    client: String,
    /// The client's display name (trust-store name, else its sanitized Hello name) — what the
    /// local summary's connect toast shows. `None` for a nameless knock (old client / Android).
    client_name: Option<String>,
    /// Whether the session negotiated HDR — carried on the lifecycle events.
    hdr: bool,
    /// Completed bring-up total (hello → first packet), ms; 0 until the first packet left. Written
    /// once by the session's [`crate::bringup::Trace`] (latency plan P0.1).
    ttff_ms: Arc<AtomicU32>,
    /// Most recent completed mid-stream resize (reconfigure → pipeline rebuilt), ms; 0 = none yet.
    last_resize_ms: Arc<AtomicU32>,
    /// The launched game's lease, when this session launched a title — so `/status` can report what
    /// is actually running and the console can show it (design/session-game-lifetime.md §8a).
    game: Option<Arc<crate::gamelease::LeaseShared>>,
}

/// A resolved read of one live session, for the `/status` view.
#[derive(Clone)]
pub struct SessionSnapshot {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// The client's display name (trust-store name, else its sanitized Hello name); `None` for a
    /// nameless client.
    pub client_name: Option<String>,
    /// Bring-up total (hello → first packet), ms; 0 while still bringing up (latency plan P0.1).
    pub time_to_first_frame_ms: u32,
    /// Most recent mid-stream resize total, ms; 0 = no resize this session.
    pub last_resize_ms: u32,
}

fn registry() -> &'static Mutex<Vec<LiveSession>> {
    static REG: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_id() -> u64 {
    static ID: AtomicU64 = AtomicU64::new(1);
    ID.fetch_add(1, Ordering::Relaxed)
}

/// Resolves one session's [`crate::events::SessionRef`] (mode read live) for the lifecycle events.
fn session_ref(s: &LiveSession) -> crate::events::SessionRef {
    let (width, height, fps) = crate::native::unpack_mode(s.mode.load(Ordering::Relaxed));
    crate::events::SessionRef {
        id: s.id,
        client: s.client.clone(),
        mode: crate::events::mode_str(width, height, fps),
        hdr: s.hdr,
    }
}

/// What [`register`] needs to publish a session. A named struct rather than a positional argument
/// list: half of these are same-typed `Arc<Atomic…>` handles, so a transposed pair would compile
/// cleanly and quietly report the wrong number on the Dashboard.
pub struct Registration {
    /// Packed `w:16|h:16|hz:16` ([`crate::native::pack_mode`]); updated on a mode switch.
    pub mode: Arc<AtomicU64>,
    /// Live encoder target (kbps); updated on an adaptive-bitrate change.
    pub bitrate_kbps: Arc<AtomicU32>,
    pub codec: Codec,
    /// Teardown flag ([`stop_all`]).
    pub stop: Arc<AtomicBool>,
    /// Deliberate-stop flag ([`stop_all_quit`]).
    pub quit: Arc<AtomicBool>,
    /// One-shot force-keyframe flag ([`force_idr_all`]).
    pub force_idr: Arc<AtomicBool>,
    /// Short client label (cert-fingerprint prefix / peer IP).
    pub client: String,
    /// The client's display name, when it has one (trust-store name, else sanitized Hello name).
    pub client_name: Option<String>,
    pub hdr: bool,
    /// Bring-up total slot (hello → first packet), ms.
    pub ttff_ms: Arc<AtomicU32>,
    /// Most recent mid-stream resize total, ms.
    pub last_resize_ms: Arc<AtomicU32>,
    /// The launched game's lease, when this session launched a title.
    pub game: Option<Arc<crate::gamelease::LeaseShared>>,
}

/// Registers a live native session; the returned guard removes it on drop (session end).
/// Emits the `session.started` lifecycle event; the guard's drop emits `session.ended` — RAII,
/// so every exit path (return, `?`, panic-unwind) pairs them.
pub fn register(reg: Registration) -> LiveSessionGuard {
    let Registration {
        mode,
        bitrate_kbps,
        codec,
        stop,
        quit,
        force_idr,
        client,
        client_name,
        hdr,
        ttff_ms,
        last_resize_ms,
        game,
    } = reg;
    let id = next_id();
    let session = LiveSession {
        id,
        mode,
        bitrate_kbps,
        codec,
        stop,
        quit,
        force_idr,
        client,
        client_name,
        hdr,
        ttff_ms,
        last_resize_ms,
        game,
    };
    crate::events::emit(crate::events::EventKind::SessionStarted {
        session: session_ref(&session),
    });
    registry().lock().unwrap().push(session);
    LiveSessionGuard {
        id,
        _sleep: crate::sleep_inhibit::hold(),
    }
}

/// Removes its session from the registry when dropped (any scope exit of the native video loop).
pub struct LiveSessionGuard {
    id: u64,
    /// While any native session lives, the box must not auto-suspend under a passive viewer
    /// ([`crate::sleep_inhibit`]) — refcounted, released with the guard.
    _sleep: crate::sleep_inhibit::StreamHold,
}

impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        let mut reg = registry().lock().unwrap();
        if let Some(pos) = reg.iter().position(|s| s.id == self.id) {
            let session = reg.remove(pos);
            drop(reg); // emit outside the registry lock — the bus takes its own
            crate::events::emit(crate::events::EventKind::SessionEnded {
                session: session_ref(&session),
            });
        }
    }
}

/// The number of live native sessions.
pub fn count() -> usize {
    registry().lock().unwrap().len()
}

/// A resolved snapshot of every live native session (mode/bitrate read live), newest last.
pub fn snapshot() -> Vec<SessionSnapshot> {
    registry()
        .lock()
        .unwrap()
        .iter()
        .map(|s| {
            let (width, height, fps) = crate::native::unpack_mode(s.mode.load(Ordering::Relaxed));
            SessionSnapshot {
                width,
                height,
                fps,
                bitrate_kbps: s.bitrate_kbps.load(Ordering::Relaxed),
                codec: s.codec,
                client_name: s.client_name.clone(),
                time_to_first_frame_ms: s.ttff_ms.load(Ordering::Relaxed),
                last_resize_ms: s.last_resize_ms.load(Ordering::Relaxed),
            }
        })
        .collect()
}

/// One launched game, as `/status` reports it.
pub struct GameSnapshot {
    /// The session streaming it, or `None` for a game whose session is gone and which is waiting out
    /// its reconnect window.
    pub session_id: Option<u64>,
    pub client: String,
    pub app_id: Option<String>,
    pub title: String,
    pub store: Option<String>,
    pub plane: crate::events::Plane,
    /// `launching` / `running` / `exited`, or `grace` for a game on its reconnect window.
    pub state: &'static str,
    /// Seconds left before the game is ended, for a `grace` row.
    pub grace_remaining_s: Option<u64>,
}

/// The compat plane's launched game, while it has one.
///
/// GameStream sessions are not in the live-session registry above — that registry holds the native
/// loop's own `Arc` handles (mode, bitrate, the stop/quit flags), none of which the compat plane has.
/// It is single-session by construction (one `AppState.launch`), so one slot is the whole story, and
/// this is what keeps a Moonlight client's game visible on the Dashboard alongside a native one.
fn gs_game() -> &'static Mutex<Option<Arc<crate::gamelease::LeaseShared>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<crate::gamelease::LeaseShared>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Publish the compat plane's game for the status surface; the returned guard retracts it when the
/// stream loop exits by any path (the plane's counterpart to [`LiveSessionGuard`]).
pub fn publish_gamestream_game(shared: Arc<crate::gamelease::LeaseShared>) -> GamestreamGameGuard {
    *gs_game().lock().unwrap_or_else(|e| e.into_inner()) = Some(shared);
    GamestreamGameGuard
}

/// Clears the compat plane's published game on drop.
pub struct GamestreamGameGuard;

impl Drop for GamestreamGameGuard {
    fn drop(&mut self) {
        *gs_game().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Every launched game the host currently knows about: the live sessions' games first, then the
/// compat plane's, then any game whose session has ended and which is waiting out its reconnect
/// window before being ended.
///
/// The sources are deliberately separate — a grace-pending game has no session to attribute it
/// to, and hiding it would make "the host is about to close my game" invisible in the UI.
pub fn games() -> Vec<GameSnapshot> {
    let mut out: Vec<GameSnapshot> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|s| {
            let g = s.game.as_ref()?;
            Some(GameSnapshot {
                session_id: Some(s.id),
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: g.state().as_str(),
                grace_remaining_s: None,
            })
        })
        .collect();
    out.extend(
        gs_game()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|g| GameSnapshot {
                // The compat plane has no session id to attribute it to; the console tells it from a
                // grace row by the state, which is never `grace` while a session is streaming it.
                session_id: None,
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: g.state().as_str(),
                grace_remaining_s: None,
            }),
    );
    out.extend(
        crate::gamelease::pending_snapshot()
            .into_iter()
            .map(|(g, remaining)| GameSnapshot {
                session_id: None,
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: "grace",
                grace_remaining_s: Some(remaining),
            }),
    );
    out
}

/// Signals every live native session to tear down. Best-effort: the video + send loops observe the
/// flag and exit, ending the stream; the guard then clears the entry.
///
/// This is the *drop*-flavored stop — the session ends, but nothing treats it as intended. Prefer
/// [`stop_all_quit`] for an operator action.
pub fn stop_all() {
    for s in registry().lock().unwrap().iter() {
        s.stop.store(true, Ordering::SeqCst);
    }
}

/// Signals every live native session to tear down **deliberately** (mgmt `DELETE /session`, the
/// console's and a plugin's Stop).
///
/// An operator pressing Stop is a decision, and the host now says so: `quit` before `stop` makes the
/// teardown look exactly like a client's own Stop, so the display skips its keep-alive linger and the
/// end-game-on-session-end policy sees an intent rather than a network drop. (Before this, a
/// management stop was indistinguishable from a client vanishing, which left the display lingering
/// for a session nobody was coming back to.)
pub fn stop_all_quit() {
    for s in registry().lock().unwrap().iter() {
        s.quit.store(true, Ordering::SeqCst);
        s.stop.store(true, Ordering::SeqCst);
    }
}

/// Requests a forced keyframe on every live native session (mgmt `POST /session/idr`). The encode
/// loop drains the flag exactly like a client decode-recovery request.
pub fn force_idr_all() {
    for s in registry().lock().unwrap().iter() {
        s.force_idr.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Moonlight client's game has no live-session entry to hang off, so without the compat-plane
    /// slot it would be missing from `/status` entirely — the Dashboard would show a stream with no
    /// game while one was plainly running. Publishing must also be strictly scoped to the stream: the
    /// row has to vanish with the guard, or a finished session leaves a ghost game on the console.
    #[test]
    fn a_gamestream_game_is_visible_only_while_its_stream_runs() {
        let id = "steam:1701";
        let mine = || {
            games()
                .into_iter()
                .find(|g| g.app_id.as_deref() == Some(id))
        };

        let lease = crate::gamelease::open(
            crate::gamelease::LeaseRequest {
                game: crate::gamelease::GameRef {
                    id: Some(id.to_string()),
                    store: Some("steam".into()),
                    title: "Test Title".into(),
                },
                client: "192.0.2.7".into(),
                plane: crate::events::Plane::Gamestream,
                // No signals: an inert lease, so no watcher thread races this test's assertions.
                spec: crate::library::DetectSpec::default(),
                nested: false,
                launcher: false,
                child: None,
                launch_stamp: None,
            },
            Box::new(|| {}),
        );
        assert!(mine().is_none(), "not published yet");

        {
            let _pub = publish_gamestream_game(lease.shared());
            let row = mine().expect("the compat plane's game is reported");
            assert_eq!(
                row.session_id, None,
                "no live-session entry to attribute it to"
            );
            assert_eq!(row.plane, crate::events::Plane::Gamestream);
            assert_eq!(row.client, "192.0.2.7");
            assert_eq!(row.title, "Test Title");
            // Never `grace` while the stream is up — that state is what the console keys its
            // countdown and "End now" off.
            assert_ne!(row.state, "grace");
        }
        assert!(mine().is_none(), "the row goes with the stream");
    }
}
