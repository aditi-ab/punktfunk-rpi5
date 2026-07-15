//! Plane-neutral live-session status for the management `/status` (web-console Dashboard) view.
//!
//! The GameStream media pipeline records its session in `AppState.{launch, stream, streaming}`
//! (consumed by RTSP/media), but the native punktfunk/1 plane never touches `AppState` — by design
//! it is handed only the shared stats recorder ([`crate::punktfunk1::serve`]). So a native session,
//! which is the DEFAULT plane (GameStream is opt-in, `--gamestream`), was invisible on the Dashboard:
//! `GET /status` reported `video_streaming: false` and no session/stream card while a client was
//! actively streaming (the Stats page worked because it shares the recorder — hence the confusing
//! "stats move but the dashboard says idle").
//!
//! This module is the small shared surface the native video loop ([`crate::punktfunk1::virtual_stream`])
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
    /// Packed `w:16|h:16|hz:16` ([`crate::punktfunk1::pack_mode`]); updated on a mode switch.
    mode: Arc<AtomicU64>,
    /// Live encoder target (kbps); updated on an adaptive-bitrate change.
    bitrate_kbps: Arc<AtomicU32>,
    codec: Codec,
    /// The session's teardown flag ([`stop_all`] → mgmt `DELETE /session`).
    stop: Arc<AtomicBool>,
    /// One-shot force-keyframe flag ([`force_idr_all`] → mgmt `POST /session/idr`); the encode loop
    /// drains it alongside a client's decode-recovery keyframe request.
    force_idr: Arc<AtomicBool>,
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

/// Registers a live native session; the returned guard removes it on drop (session end).
pub fn register(
    mode: Arc<AtomicU64>,
    bitrate_kbps: Arc<AtomicU32>,
    codec: Codec,
    stop: Arc<AtomicBool>,
    force_idr: Arc<AtomicBool>,
) -> LiveSessionGuard {
    let id = next_id();
    registry().lock().unwrap().push(LiveSession {
        id,
        mode,
        bitrate_kbps,
        codec,
        stop,
        force_idr,
    });
    LiveSessionGuard { id }
}

/// Removes its session from the registry when dropped (any scope exit of the native video loop).
pub struct LiveSessionGuard {
    id: u64,
}

impl Drop for LiveSessionGuard {
    fn drop(&mut self) {
        registry().lock().unwrap().retain(|s| s.id != self.id);
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
            let (width, height, fps) =
                crate::punktfunk1::unpack_mode(s.mode.load(Ordering::Relaxed));
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
