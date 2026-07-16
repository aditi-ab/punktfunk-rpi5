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
    /// One-shot force-keyframe flag ([`force_idr_all`] → mgmt `POST /session/idr`); the encode loop
    /// drains it alongside a client's decode-recovery keyframe request.
    force_idr: Arc<AtomicBool>,
    /// Short client label (cert-fingerprint prefix / peer IP) — carried on the lifecycle events.
    client: String,
    /// Whether the session negotiated HDR — carried on the lifecycle events.
    hdr: bool,
}

/// A resolved read of one live session, for the `/status` view.
#[derive(Clone, Copy)]
pub struct SessionSnapshot {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
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

/// Registers a live native session; the returned guard removes it on drop (session end).
/// Emits the `session.started` lifecycle event; the guard's drop emits `session.ended` — RAII,
/// so every exit path (return, `?`, panic-unwind) pairs them.
pub fn register(
    mode: Arc<AtomicU64>,
    bitrate_kbps: Arc<AtomicU32>,
    codec: Codec,
    stop: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
    client: String,
    hdr: bool,
) -> LiveSessionGuard {
    let id = next_id();
    let session = LiveSession {
        id,
        mode,
        bitrate_kbps,
        codec,
        stop,
        force_idr,
        client,
        hdr,
    };
    crate::events::emit(crate::events::EventKind::SessionStarted {
        session: session_ref(&session),
    });
    registry().lock().unwrap().push(session);
    LiveSessionGuard { id }
}

/// Removes its session from the registry when dropped (any scope exit of the native video loop).
pub struct LiveSessionGuard {
    id: u64,
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
            }
        })
        .collect()
}

/// Signals every live native session to tear down (mgmt `DELETE /session`). Best-effort: the video
/// + send loops observe the flag and exit, ending the stream; the guard then clears the entry.
pub fn stop_all() {
    for s in registry().lock().unwrap().iter() {
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
