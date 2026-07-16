//! Shared streaming-stats recorder (`design/stats-capture-plan.md` §1). One
//! [`StatsRecorder`] handle is created once in the unified host entry
//! (`gamestream::serve`) alongside [`crate::native_pairing::NativePairing`], and shared with
//! **both** the management API ([`crate::mgmt`]) and the streaming loops (threaded through
//! [`crate::native::serve`] → `SessionContext` and into the GameStream encode loop). The
//! operator arms a capture from the web console, plays a session, stops, and reviews the
//! captured time-series as graphs; captures are saved to disk and survive a host restart.
//!
//! Hot-path discipline: [`StatsRecorder::is_armed`] is a cheap `Relaxed` atomic load (re-read
//! per frame); sample construction happens only at the loops' existing ~2 s / ~1 s aggregation
//! boundary, never per frame. Memory is bounded ([`MAX_SAMPLES`]); the on-disk write is atomic
//! (temp + rename); and capture ids are path-traversal-safe.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use utoipa::ToSchema;

/// Cap on samples kept in one capture: ≈ 3 h at one sample / 2 s. On overflow we stop appending
/// (keeping the oldest — a saved recording must keep its start), never dropping the front and never
/// growing unbounded.
const MAX_SAMPLES: usize = 5400;

/// One pipeline stage's latency in an aggregation window (microseconds).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StageTiming {
    /// `"capture" | "submit" | "encode" | "packetize" | "send"` (path-dependent).
    pub name: String,
    pub p50_us: f32,
    pub p99_us: f32,
}

/// One aggregated sample (~ every 2 s native, ~ every 1 s GameStream).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StatsSample {
    /// Milliseconds since capture start (monotonic; stamped by [`StatsRecorder::push_sample`]).
    pub t_ms: u64,
    /// Disambiguates concurrent sessions (usually constant).
    pub session_id: u32,
    /// Ordered pipeline stages for this path.
    pub stages: Vec<StageTiming>,
    /// Genuine NEW frames/s from the source.
    pub fps: f32,
    /// Re-encoded holds/s (source-starvation indicator).
    pub repeat_fps: f32,
    /// Transmit goodput (Mb/s).
    pub mbps: f32,
    /// Configured target bitrate.
    pub bitrate_kbps: u32,
    /// Frames dropped this window (delta).
    pub frames_dropped: u32,
    /// Packets dropped this window (receiver-side / reassembler, where known).
    pub packets_dropped: u32,
    /// Host send-buffer overflow / EAGAIN this window (delta).
    pub send_dropped: u32,
    /// FEC shards recovered this window (delta).
    pub fec_recovered: u32,
}

/// Capture summary — the filename stem plus the negotiated mode/codec/client. Stored at the head
/// of each on-disk recording and listed standalone (without the sample body) by
/// [`StatsRecorder::list`].
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct CaptureMeta {
    /// e.g. `"2026-06-26T20-14-03Z_5120x1440"` — also the filename stem.
    pub id: String,
    pub started_unix_ms: u64,
    pub duration_ms: u64,
    /// `"native" | "gamestream"`.
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `"h264" | "hevc" | "av1"`.
    pub codec: String,
    /// Short label / fingerprint prefix, or `""` if unknown.
    pub client: String,
    pub sample_count: u32,
}

/// A full capture: summary + the sample time-series. The wire + on-disk shape.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct Capture {
    pub meta: CaptureMeta,
    pub samples: Vec<StatsSample>,
}

/// Snapshot of the in-progress capture for the management API.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StatsStatus {
    /// Capture currently running.
    pub armed: bool,
    /// Samples in the in-progress capture.
    pub sample_count: u32,
    /// Unix start time of the in-progress capture (`0` if idle).
    pub started_unix_ms: u64,
    /// Host-measured elapsed time of the in-progress capture, in ms (`0` if idle). Computed from the
    /// host's MONOTONIC clock, so a console can show elapsed time without subtracting `started_unix_ms`
    /// from its own (possibly skewed) wall clock.
    pub elapsed_ms: u64,
    /// Path of the in-progress capture (`""` if idle).
    pub kind: String,
}

/// Mode/codec/client seeded on the first [`StatsRecorder::register_session`] of a capture.
#[derive(Clone)]
struct MetaSeed {
    kind: String,
    width: u32,
    height: u32,
    fps: u32,
    codec: String,
    client: String,
}

/// The in-progress capture (present iff armed).
struct Live {
    /// Monotonic clock origin for sample `t_ms`.
    started: Instant,
    started_unix_ms: u64,
    /// Seeded once, on the first session registration.
    meta: Option<MetaSeed>,
    samples: Vec<StatsSample>,
    /// Set once the sample cap was hit (further samples dropped). Read so it isn't dead.
    truncated: bool,
}

/// Shared streaming-stats recorder: an arm/disarm flag (the hot-path gate), the in-progress
/// capture, and the on-disk capture directory.
pub struct StatsRecorder {
    dir: PathBuf,
    /// The hot-path gate — a `Relaxed` load per frame; never blocks the frame thread.
    armed: AtomicBool,
    /// The in-progress capture. Locks recover a poisoned guard (`unwrap_or_else(|e| e.into_inner())`,
    /// as in `vdisplay::gamescope`) rather than `unwrap()`: a panic somewhere must never make stats
    /// recording crash an otherwise-healthy stream. The critical sections only push/clone/format, so
    /// poisoning is near-impossible anyway — this is belt-and-suspenders.
    live: Mutex<Option<Live>>,
    next_sid: AtomicU32,
}

/// The default captures directory: `~/.config/punktfunk/captures/` (next to `cert.pem`),
/// resolved via the same config-dir helper the rest of the host uses.
pub fn default_dir() -> PathBuf {
    pf_paths::config_dir().join("captures")
}

/// `id` charset gate, matching `^[A-Za-z0-9._-]+$` — the exact charset `capture_id` emits (which
/// deliberately uses dashes, not colons, so the stem is a valid Windows filename). We additionally
/// reject `.`/`..` so a path-component sneaks no parent reference even though the charset would allow
/// bare dots. The charset already excludes `/` and `\`, so `dir.join("<id>.json")` is always a single
/// child of `dir`. Defense in depth — the endpoints are bearer-authed.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A human-readable, filesystem-safe capture id from the start time + mode, e.g.
/// `2026-06-26T20-14-03Z_5120x1440`. Dashes (not colons) in the time so it's a valid Windows
/// filename; matches [`valid_id`].
fn capture_id(unix_ms: u64, width: u32, height: u32) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z_{width}x{height}")
}

/// Civil (Y, M, D) from a count of days since the Unix epoch (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d)
}

impl StatsRecorder {
    /// Create the recorder, creating `dir` (owner-private, best-effort) if missing.
    pub fn new(dir: PathBuf) -> Arc<Self> {
        if let Err(e) = pf_paths::create_private_dir(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "could not create stats captures dir");
        }
        Arc::new(StatsRecorder {
            dir,
            armed: AtomicBool::new(false),
            live: Mutex::new(None),
            next_sid: AtomicU32::new(0),
        })
    }

    /// The hot-path gate: cheap `Relaxed` load, called per frame to decide whether to measure.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Arm a new capture. No-op if already armed (returns the current status).
    pub fn start(&self) -> StatsStatus {
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(Live {
                started: Instant::now(),
                started_unix_ms: unix_ms_now(),
                meta: None,
                samples: Vec::new(),
                truncated: false,
            });
            // Publish AFTER the live capture exists, so a frame thread that observes `armed` always
            // finds a capture to push into.
            self.armed.store(true, Ordering::Relaxed);
        }
        status_of(guard.as_ref())
    }

    /// A streaming loop announces itself when it first records while armed. Seeds the capture's
    /// `CaptureMeta` (kind/w/h/fps/codec/client) on the FIRST registration; returns a session id
    /// to stamp on the loop's samples.
    pub fn register_session(
        &self,
        kind: &'static str,
        w: u32,
        h: u32,
        fps: u32,
        codec: &str,
        client: &str,
    ) -> u32 {
        let sid = self.next_sid.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(live) = guard.as_mut() {
            if live.meta.is_none() {
                live.meta = Some(MetaSeed {
                    kind: kind.to_string(),
                    width: w,
                    height: h,
                    fps,
                    codec: codec.to_string(),
                    client: client.to_string(),
                });
            }
        }
        sid
    }

    /// Append one aggregated sample (called from the loops' existing ~2 s / ~1 s boundary). The
    /// `t_ms` is (re)stamped here from the capture's monotonic start, so callers may leave it `0`.
    /// Bounded at [`MAX_SAMPLES`]: on overflow we stop appending (oldest kept) and flag truncation.
    /// A no-op when nothing is armed (e.g. a `stop()` raced the frame boundary).
    pub fn push_sample(&self, session_id: u32, mut sample: StatsSample) {
        let mut guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let Some(live) = guard.as_mut() else { return };
        if live.samples.len() >= MAX_SAMPLES {
            if !live.truncated {
                live.truncated = true;
                tracing::warn!(
                    max = MAX_SAMPLES,
                    "stats capture hit the sample cap — further samples dropped (oldest kept)"
                );
            }
            return;
        }
        sample.session_id = session_id;
        sample.t_ms = live.started.elapsed().as_millis() as u64;
        live.samples.push(sample);
    }

    /// Disarm + finalize: write `<dir>/<id>.json` atomically (temp + rename) and return its meta.
    /// `Ok(None)` if nothing was recording.
    pub fn stop(&self) -> std::io::Result<Option<CaptureMeta>> {
        // Clear the hot-path gate first so frame threads stop building samples immediately.
        self.armed.store(false, Ordering::Relaxed);
        let Some(live) = self.live.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return Ok(None);
        };
        let meta = meta_of(&live);
        let capture = Capture {
            meta: meta.clone(),
            samples: live.samples,
        };
        let bytes = serde_json::to_vec(&capture).map_err(std::io::Error::other)?;
        // Atomic replace: write a sibling temp then rename, so a crash mid-write can't leave a half
        // file. The id is generated (always `valid_id`), so this only ever names a child of `dir`.
        let path = self.dir.join(format!("{}.json", meta.id));
        let tmp = self.dir.join(format!("{}.json.tmp", meta.id));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Some(meta))
    }

    /// The in-progress capture status (idle = `armed: false`, zeroed fields).
    pub fn status(&self) -> StatsStatus {
        status_of(self.live.lock().unwrap_or_else(|e| e.into_inner()).as_ref())
    }

    /// A clone of the in-progress capture for live graphing (`None` when idle).
    pub fn live_snapshot(&self) -> Option<Capture> {
        let guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let live = guard.as_ref()?;
        Some(Capture {
            meta: meta_of(live),
            samples: live.samples.clone(),
        })
    }

    /// All saved recordings, newest first, parsing each file's `meta` head only (not the samples).
    pub fn list(&self) -> Vec<CaptureMeta> {
        /// Parse only the `meta` head — serde skips the (large) `samples` array.
        #[derive(Deserialize)]
        struct MetaOnly {
            meta: CaptureMeta,
        }
        let mut out: Vec<CaptureMeta> = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&path) {
                if let Ok(parsed) = serde_json::from_slice::<MetaOnly>(&bytes) {
                    out.push(parsed.meta);
                }
            }
        }
        out.sort_by_key(|m| std::cmp::Reverse(m.started_unix_ms));
        out
    }

    /// Load a saved recording by id. Rejects a path-unsafe id (and a missing file) as `NotFound`.
    pub fn load(&self, id: &str) -> std::io::Result<Capture> {
        let path = self.recording_path(id)?;
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Delete a saved recording by id. Rejects a path-unsafe id (and a missing file) as `NotFound`.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let path = self.recording_path(id)?;
        std::fs::remove_file(&path)
    }

    /// Resolve `dir/<id>.json` after validating `id`. A rejected id is `NotFound` (defense in
    /// depth: never let an attacker-shaped id escape `dir`).
    fn recording_path(&self, id: &str) -> std::io::Result<PathBuf> {
        if !valid_id(id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "invalid recording id",
            ));
        }
        Ok(self.dir.join(format!("{id}.json")))
    }
}

/// Build the live `StatsStatus` from the optional in-progress capture.
fn status_of(live: Option<&Live>) -> StatsStatus {
    match live {
        Some(l) => StatsStatus {
            armed: true,
            sample_count: l.samples.len() as u32,
            started_unix_ms: l.started_unix_ms,
            elapsed_ms: l.started.elapsed().as_millis() as u64,
            kind: l.meta.as_ref().map(|m| m.kind.clone()).unwrap_or_default(),
        },
        None => StatsStatus {
            armed: false,
            sample_count: 0,
            started_unix_ms: 0,
            elapsed_ms: 0,
            kind: String::new(),
        },
    }
}

/// Compute the `CaptureMeta` for an in-progress or finalizing capture (id derived from the start
/// time + negotiated mode; duration from the monotonic start).
fn meta_of(live: &Live) -> CaptureMeta {
    let (kind, width, height, fps, codec, client) = match &live.meta {
        Some(m) => (
            m.kind.clone(),
            m.width,
            m.height,
            m.fps,
            m.codec.clone(),
            m.client.clone(),
        ),
        None => (String::new(), 0, 0, 0, String::new(), String::new()),
    };
    CaptureMeta {
        id: capture_id(live.started_unix_ms, width, height),
        started_unix_ms: live.started_unix_ms,
        duration_ms: live.started.elapsed().as_millis() as u64,
        kind,
        width,
        height,
        fps,
        codec,
        client,
        sample_count: live.samples.len() as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        // A per-call unique dir: a process-wide counter (NOT a timestamp, which collides when tests
        // run in parallel within the same millisecond — one test's cleanup would then wipe another's
        // dir mid-run).
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("pf-stats-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn sample() -> StatsSample {
        StatsSample {
            t_ms: 0,
            session_id: 0,
            stages: vec![StageTiming {
                name: "capture".into(),
                p50_us: 100.0,
                p99_us: 200.0,
            }],
            fps: 60.0,
            repeat_fps: 0.0,
            mbps: 25.0,
            bitrate_kbps: 20_000,
            frames_dropped: 0,
            packets_dropped: 0,
            send_dropped: 0,
            fec_recovered: 0,
        }
    }

    #[test]
    fn arm_record_save_load_delete() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        assert!(!rec.is_armed());
        assert!(!rec.status().armed);
        // A push while idle is a no-op (no live capture).
        rec.push_sample(0, sample());

        let st = rec.start();
        assert!(st.armed);
        assert!(rec.is_armed());
        let sid = rec.register_session("native", 5120, 1440, 240, "hevc", "abcd");
        rec.push_sample(sid, sample());
        rec.push_sample(sid, sample());
        assert_eq!(rec.status().sample_count, 2);
        assert_eq!(rec.status().kind, "native");
        assert!(rec.live_snapshot().is_some());

        let meta = rec.stop().unwrap().expect("a capture was recording");
        assert_eq!(meta.sample_count, 2);
        assert_eq!(meta.kind, "native");
        assert_eq!(meta.width, 5120);
        assert!(meta.id.ends_with("_5120x1440"), "id was {}", meta.id);
        assert!(!rec.is_armed());
        assert!(rec.live_snapshot().is_none());
        // Stop with nothing recording → Ok(None).
        assert!(rec.stop().unwrap().is_none());

        // It is listed and loadable.
        let list = rec.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, meta.id);
        let loaded = rec.load(&meta.id).unwrap();
        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(loaded.meta.codec, "hevc");

        // Delete removes it; a second delete is NotFound.
        rec.delete(&meta.id).unwrap();
        assert!(rec.list().is_empty());
        assert_eq!(
            rec.delete(&meta.id).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_path_traversal_ids() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        for bad in [
            "../secret",
            "..",
            ".",
            "a/b",
            "a\\b",
            "",
            "/etc/passwd",
            "x/../../y",
        ] {
            assert_eq!(
                rec.load(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "load({bad:?}) must be rejected as NotFound"
            );
            assert_eq!(
                rec.delete(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "delete({bad:?}) must be rejected as NotFound"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn samples_are_bounded() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        rec.start();
        for _ in 0..(MAX_SAMPLES + 50) {
            rec.push_sample(0, sample());
        }
        assert_eq!(rec.status().sample_count as usize, MAX_SAMPLES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn start_is_idempotent_while_armed() {
        let dir = temp_dir();
        let rec = StatsRecorder::new(dir.clone());
        rec.start();
        rec.register_session("native", 1920, 1080, 60, "hevc", "");
        rec.push_sample(0, sample());
        // A second start must NOT wipe the in-progress capture.
        let st = rec.start();
        assert!(st.armed);
        assert_eq!(st.sample_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
