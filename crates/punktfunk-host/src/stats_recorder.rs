//! Shared streaming-stats recorder (`design/stats-capture-plan.md`). One
//! [`StatsRecorder`] is created in `gamestream::serve` and shared with
//! [`crate::mgmt`] and the native / GameStream encode loops.
//!
//! Captures persist as JSON under the captures dir and survive a host restart.
//! [`StatsRecorder::is_armed`] is a `Relaxed` load per frame; samples are built
//! only at the loops' ~2 s / ~1 s aggregation boundary, never per frame.
//! Memory is bounded ([`MAX_SAMPLES`]). The on-disk write is temp + rename.
//! Capture ids are charset-gated so `dir.join` cannot leave the captures dir.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use utoipa::ToSchema;

/// ≈ 3 h at one sample / 2 s. Overflow stops appending (oldest kept) so a
/// recording keeps its start and never grows unbounded.
const MAX_SAMPLES: usize = 5400;

/// One stage's p50/p99 in an aggregation window (microseconds).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StageTiming {
    /// `"capture" | "submit" | "encode" | "packetize" | "send"` (path-dependent).
    pub name: String,
    pub p50_us: f32,
    pub p99_us: f32,
}

/// One aggregated sample (~2 s native, ~1 s GameStream).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StatsSample {
    /// Milliseconds since capture start (monotonic; stamped by [`StatsRecorder::push_sample`]).
    pub t_ms: u64,
    /// Distinguishes concurrent sessions (usually constant for one loop).
    pub session_id: u32,
    pub stages: Vec<StageTiming>,
    /// Genuine new frames/s from the source (not including repeats).
    pub fps: f32,
    /// Re-encoded holds/s — source starvation, not new frames.
    pub repeat_fps: f32,
    /// Attempted sealed wire Mb/s at seal time (AU + shard framing + FEC, including
    /// datagram-aligned zero-pad). Not goodput; socket send drops do not reduce it.
    pub mbps: f32,
    pub bitrate_kbps: u32,
    /// Frames dropped this window (delta, not cumulative).
    pub frames_dropped: u32,
    /// Packets dropped this window (receiver / reassembler, when known).
    pub packets_dropped: u32,
    /// Host send-buffer overflow / EAGAIN this window (delta).
    pub send_dropped: u32,
    /// FEC shards recovered this window (delta).
    pub fec_recovered: u32,
}

/// Filename stem plus negotiated mode/codec/client. On-disk head;
/// [`StatsRecorder::list`] returns this without the sample body.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct CaptureMeta {
    /// Filename stem, e.g. `2026-06-26T20-14-03Z_5120x1440`.
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
    /// Fingerprint prefix, or `""` if unknown.
    pub client: String,
    pub sample_count: u32,
    /// Backend that actually opened (`"nvenc"`, `"vaapi"`, `"vulkan"`, `"amf"`,
    /// `"qsv"`, `"software"`, …), from `pf_gpu::active()`. Stage timings are
    /// unreadable without it. `""` if nothing was streaming at registration.
    #[serde(default)]
    pub encoder_backend: String,
    /// GPU name from `pf_gpu::active()`, or `""`.
    #[serde(default)]
    pub gpu: String,
}

/// Wire and on-disk shape: summary plus sample time-series.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct Capture {
    pub meta: CaptureMeta,
    pub samples: Vec<StatsSample>,
}

/// In-progress capture, as the management API reports it.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StatsStatus {
    pub armed: bool,
    pub sample_count: u32,
    /// Unix start of the in-progress capture (`0` if idle).
    pub started_unix_ms: u64,
    /// Host monotonic elapsed ms (`0` if idle). Do not subtract `started_unix_ms`
    /// from the console's wall clock — that clock may be skewed.
    pub elapsed_ms: u64,
    /// `"native" | "gamestream"`, or `""` if idle.
    pub kind: String,
}

/// Mode/codec/client from the first [`StatsRecorder::register_session`] of a capture.
#[derive(Clone)]
struct MetaSeed {
    kind: String,
    width: u32,
    height: u32,
    fps: u32,
    codec: String,
    client: String,
    encoder_backend: String,
    gpu: String,
}

/// In-progress capture (present iff armed).
struct Live {
    /// Monotonic origin for sample `t_ms`.
    started: Instant,
    started_unix_ms: u64,
    /// Seeded once, on the first session registration.
    meta: Option<MetaSeed>,
    samples: Vec<StatsSample>,
    /// Sample cap was hit; further samples are dropped.
    truncated: bool,
}

pub struct StatsRecorder {
    dir: PathBuf,
    /// Hot-path gate: `Relaxed` load per frame; never blocks the frame thread.
    armed: AtomicBool,
    /// In-progress capture. Poison recovers (`into_inner`) so a stats panic
    /// cannot kill a healthy stream.
    live: Mutex<Option<Live>>,
    next_sid: AtomicU32,
}

/// `~/.config/punktfunk/captures/`, via the same config-dir helper as `cert.pem`.
pub fn default_dir() -> PathBuf {
    pf_paths::config_dir().join("captures")
}

/// Charset `^[A-Za-z0-9._-]+$` (what [`capture_id`] emits; dashes not colons
/// so the stem is a Windows filename). Also reject `.` / `..` — the charset
/// allows bare dots. `/` and `\` are already excluded, so `dir.join` is one child.
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

/// Filesystem-safe id from start time + resolution, e.g.
/// `2026-06-26T20-14-03Z_5120x1440`. Dashes, not colons, so Windows accepts it.
fn capture_id(unix_ms: u64, width: u32, height: u32) -> String {
    let secs = (unix_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}-{mi:02}-{s:02}Z_{width}x{height}")
}

/// Civil (Y, M, D) from a count of days since the Unix epoch (Howard Hinnant's `civil_from_days`).
/// `pub(crate)`: `client_logs::bundle_id` builds its timestamp stem the same way.
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
    /// Create `dir` owner-private (best-effort) if missing.
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

    /// Per-frame `Relaxed` load: whether this frame should measure.
    pub fn is_armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    /// Arm a new capture. No-op if already armed (does not wipe; returns status).
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
            // Publish after `live` exists so a frame that sees `armed` can always push.
            self.armed.store(true, Ordering::Relaxed);
        }
        status_of(guard.as_ref())
    }

    /// First registration while armed seeds `CaptureMeta`; later ones are ignored.
    /// Returns a session id to stamp on this loop's samples.
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
        // `pf_gpu::active()` takes its own lock — read it outside `live`, once per capture.
        let (encoder_backend, gpu) = pf_gpu::active()
            .map(|(g, _)| (g.backend.to_string(), g.name))
            .unwrap_or_default();
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
                    encoder_backend,
                    gpu,
                });
            }
        }
        sid
    }

    /// Append one aggregated sample. Restamps `t_ms` from the monotonic start
    /// (callers may leave it `0`). Stops appending at [`MAX_SAMPLES`] (oldest kept).
    /// No-op if unarmed (a `stop()` raced the frame boundary).
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

    /// Disarm, write `<dir>/<id>.json` (temp + rename), return meta. `Ok(None)` if idle.
    pub fn stop(&self) -> std::io::Result<Option<CaptureMeta>> {
        // Clear the gate first so frame threads stop building samples immediately.
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
        // Sibling temp then rename: a crash mid-write cannot leave a half file.
        // `id` is generated (`valid_id`), so this names a child of `dir`.
        let path = self.dir.join(format!("{}.json", meta.id));
        let tmp = self.dir.join(format!("{}.json.tmp", meta.id));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Some(meta))
    }

    /// In-progress status (idle = `armed: false`, zeroed fields).
    pub fn status(&self) -> StatsStatus {
        status_of(self.live.lock().unwrap_or_else(|e| e.into_inner()).as_ref())
    }

    /// Clone of the in-progress capture (`None` when idle).
    pub fn live_snapshot(&self) -> Option<Capture> {
        let guard = self.live.lock().unwrap_or_else(|e| e.into_inner());
        let live = guard.as_ref()?;
        Some(Capture {
            meta: meta_of(live),
            samples: live.samples.clone(),
        })
    }

    /// Saved recordings, newest first. Parses each file's `meta` head only.
    pub fn list(&self) -> Vec<CaptureMeta> {
        /// `meta` only — serde skips the large `samples` array.
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

    /// Load by id. Path-unsafe id and missing file are both `NotFound`.
    pub fn load(&self, id: &str) -> std::io::Result<Capture> {
        let path = self.recording_path(id)?;
        let bytes = std::fs::read(&path)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Delete by id. Path-unsafe id and missing file are both `NotFound`.
    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        let path = self.recording_path(id)?;
        std::fs::remove_file(&path)
    }

    /// `dir/<id>.json` after [`valid_id`]. Rejected id is `NotFound` so `join` never leaves `dir`.
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

/// `CaptureMeta` for a live or finalizing capture. Id from start + mode;
/// duration from the monotonic clock.
fn meta_of(live: &Live) -> CaptureMeta {
    let (kind, width, height, fps, codec, client, encoder_backend, gpu) = match &live.meta {
        Some(m) => (
            m.kind.clone(),
            m.width,
            m.height,
            m.fps,
            m.codec.clone(),
            m.client.clone(),
            m.encoder_backend.clone(),
            m.gpu.clone(),
        ),
        None => Default::default(),
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
        encoder_backend,
        gpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        // Process-wide counter, not a timestamp: parallel tests in the same
        // millisecond would share a dir and one cleanup would wipe the other.
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
        assert!(rec.stop().unwrap().is_none());

        let list = rec.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, meta.id);
        let loaded = rec.load(&meta.id).unwrap();
        assert_eq!(loaded.samples.len(), 2);
        assert_eq!(loaded.meta.codec, "hevc");

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
        let st = rec.start();
        assert!(st.armed);
        assert_eq!(st.sample_count, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
