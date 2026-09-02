//! Host-lifetime virtual-microphone pump.
//!
//! One thread owns the [`VirtualMic`] backend and an Opus decoder. Sessions
//! `try_send` client `0xCB` frames onto a clonable sender; the thread de-jitters,
//! decodes, and feeds PCM so host apps hear the client's mic. Opens at host
//! start (games bind capture once and never re-follow), reopens a dead backend
//! with backoff, and discards buffered audio after an uplink gap. Decode errors
//! drop that frame only. The thread exits when every sender is dropped.
//!
//! Pin via [`MicPump::start`] / [`MicPump::start_named`]. Evidence: `pump_tests`.
//! [`VirtualMic`], [`open_virtual_mic`](super::open_virtual_mic), and
//! [`SAMPLE_RATE`](super::SAMPLE_RATE) stay in `super`.

use super::mic_jitter::{Deliver, MicDejitter};
use super::{VirtualMic, SAMPLE_RATE};
use anyhow::Result;

/// Stereo: the Opus decoder and the host→client layout are both 2ch.
pub const MIC_CHANNELS: u32 = 2;

/// One `0xCB` uplink frame (`punktfunk_core::quic::decode_mic_datagram`).
/// `seq`/`pts_ns` ride with the Opus payload so de-jitter can reorder, conceal, and track cadence.
pub struct MicFrame {
    pub seq: u32,
    pub pts_ns: u64,
    pub opus: Vec<u8>,
}

/// Drop-newest bound on the host-lifetime queue: 12 × 5–20 ms ≈ 60–240 ms of slack.
/// Shared across sessions; [`DRAIN_ABOVE`] heals anything deeper.
const MIC_QUEUE_CAP: usize = 12;
/// Wake deeper than this: keep the newest [`DRAIN_KEEP`]. Replaying late frames turns a stall into standing delay.
const DRAIN_ABOVE: usize = 6;
const DRAIN_KEEP: usize = 4;

const TELEMETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Shed only when depth stays > target by this many ms for [`TRIM_AFTER`], then at most
/// one silent frame per [`TRIM_SPACING`] (20 ms / 300 ms ≈ 7 ms per 100 ms). A burst otherwise
/// only comes back down on a full drain.
const TRIM_MARGIN_MS: usize = 15;
const TRIM_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
const TRIM_SPACING: std::time::Duration = std::time::Duration::from_millis(300);
/// ≈ −48 dBFS; only near-silent frames may be shed.
const TRIM_SILENCE_PEAK: f32 = 0.004;

/// Open/reopen/flush delays; tests pass millisecond values so the real loop still runs.
#[derive(Clone, Copy)]
struct PumpTuning {
    /// First retry after a failed open; doubles up to `backoff_cap` so a missing endpoint is not hammered.
    backoff_start: std::time::Duration,
    backoff_cap: std::time::Duration,
    /// Idle liveness probe: a dead backend reopens before the next session starts.
    heartbeat: std::time::Duration,
    /// Uplink gap longer than this: discard buffered audio so a recorder never hears a mute-era burst.
    stale_gap: std::time::Duration,
    /// Died before this: treat as a failed open (flapping daemon must not churn at heartbeat rate). Lived longer: reset backoff.
    stable_after: std::time::Duration,
}

const PUMP_TUNING: PumpTuning = PumpTuning {
    backoff_start: std::time::Duration::from_secs(2),
    backoff_cap: std::time::Duration::from_secs(60),
    heartbeat: std::time::Duration::from_secs(1),
    stale_gap: std::time::Duration::from_millis(600),
    stable_after: std::time::Duration::from_secs(5),
};

/// One thread owns [`VirtualMic`] plus an Opus decoder; sessions clone a `Send` sender for `0xCB`.
///
/// Opens at host start (games bind capture once). Reopens on push-fail or idle heartbeat
/// without invalidating senders. Discards buffered audio after an uplink gap. De-jitters via
/// [`MicDejitter`](super::mic_jitter) (`PUNKTFUNK_MIC_LEGACY_BUFFER=1` pins the old fixed prime).
/// Per-frame decode errors drop that frame. Exits when every sender is dropped.
pub struct MicPump {
    tx: std::sync::mpsc::SyncSender<MicFrame>,
}

impl MicPump {
    /// Host-lifetime pump. Linux/Windows open a backend; other platforms drain and drop (sessions still count datagrams).
    pub fn start() -> MicPump {
        Self::start_named(None)
    }

    /// [`start`](Self::start) with a `node.name`. `Some` is a session-lifetime source for an
    /// isolated gamescope session (`design/gamescope-multiuser.md`): `punktfunk-mic-{id}`, fed
    /// only by that session, torn down when the owner and its sender clone drop. `None` is the
    /// shared `punktfunk-mic`.
    pub fn start_named(source_name: Option<String>) -> MicPump {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MicFrame>(MIC_QUEUE_CAP);
        let spawned = std::thread::Builder::new()
            .name("punktfunk-mic-pump".into())
            .spawn(move || {
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                pump_thread(
                    rx,
                    move || super::open_virtual_mic_named(MIC_CHANNELS, source_name.as_deref()),
                    PUMP_TUNING,
                );
                #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                {
                    let _ = source_name;
                    tracing::warn!("mic passthrough unsupported on this platform — frames dropped");
                    for _ in rx {}
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "mic pump thread spawn failed — mic passthrough disabled");
        }
        MicPump { tx }
    }

    /// Session clone: `try_send` so a datagram loop never blocks. Dropping a clone does not stop
    /// the pump — it holds the original sender for the host life.
    pub fn sender(&self) -> std::sync::mpsc::SyncSender<MicFrame> {
        self.tx.clone()
    }
}

/// Sleep `dur` while dropping queued frames so a closed backend cannot wedge senders or keep a stale backlog. `false` = every sender gone.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn drain_sleep(rx: &std::sync::mpsc::Receiver<MicFrame>, dur: std::time::Duration) -> bool {
    use std::sync::mpsc::RecvTimeoutError;
    let deadline = std::time::Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return true;
        }
        match rx.recv_timeout(left.min(std::time::Duration::from_millis(250))) {
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// Pump loop. `opener` is injected so tests run this loop against a mock; production uses [`open_virtual_mic`](super::open_virtual_mic).
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn pump_thread<O>(rx: std::sync::mpsc::Receiver<MicFrame>, opener: O, tuning: PumpTuning)
where
    O: Fn() -> Result<Box<dyn VirtualMic>>,
{
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;

    let mut backoff = tuning.backoff_start;
    let mut open_fails: u64 = 0;
    loop {
        let (mic, mut decoder) = loop {
            let opened = opener().and_then(|m| {
                let d = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                    .map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
                Ok((m, d))
            });
            match opened {
                Ok(pair) => break pair,
                Err(e) => {
                    // Power-of-two log: a missing endpoint would otherwise warn every backoff forever.
                    open_fails += 1;
                    if open_fails.is_power_of_two() {
                        tracing::warn!(error = %format!("{e:#}"), attempts = open_fails,
                            "virtual mic unavailable — retrying with backoff");
                    }
                    if !drain_sleep(&rx, backoff) {
                        return;
                    }
                    backoff = (backoff * 2).min(tuning.backoff_cap);
                }
            }
        };
        tracing::info!("virtual mic ready (host-lifetime)");
        // Queued frames predate this backend. Backoff resets only after a stable life (death triage below).
        while rx.try_recv().is_ok() {}
        let opened_at = Instant::now();

        let legacy = super::mic_legacy_buffer();
        let mut decode_fails: u64 = 0;
        let mut drain_drops: u64 = 0;
        let mut trimmed: u64 = 0;
        let mut frames_seen: u64 = 0;
        let mut pcm = vec![0f32; 5760 * MIC_CHANNELS as usize]; // 120 ms at 48 kHz
        let mut last_push = Instant::now();
        let mut batch: Vec<MicFrame> = Vec::new();
        let mut deliveries: Vec<Deliver> = Vec::new();
        let mut jitter = MicDejitter::new();
        // libopus sizes PLC from the output slice; 960 = 20 ms until a real frame sets it.
        let mut plc_samples: usize = 960;
        let mut applied_target_ms: u32 = 0;
        let mut over_since: Option<Instant> = None;
        let mut last_trim = Instant::now();
        let mut last_log = Instant::now();
        'pump: loop {
            // Soonest of heartbeat and a parked reorder hold aging out.
            let timeout = jitter
                .hold_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(tuning.heartbeat)
                .min(tuning.heartbeat);
            deliveries.clear();
            match rx.recv_timeout(timeout) {
                Ok(first) => {
                    // After a stall the backlog is standing latency; jump to the newest DRAIN_KEEP.
                    batch.clear();
                    batch.push(first);
                    while batch.len() <= MIC_QUEUE_CAP {
                        match rx.try_recv() {
                            Ok(f) => batch.push(f),
                            Err(_) => break,
                        }
                    }
                    if batch.len() > DRAIN_ABOVE {
                        let drop_n = batch.len() - DRAIN_KEEP;
                        drain_drops += drop_n as u64;
                        batch.drain(..drop_n);
                    }
                    frames_seen += batch.len() as u64;
                    if last_push.elapsed() > tuning.stale_gap {
                        mic.discard();
                        jitter.reset_stream();
                    }
                    let now = Instant::now();
                    for frame in batch.drain(..) {
                        jitter.ingest(now, frame, &mut deliveries);
                    }
                    // Traffic that is only late duplicates never hits the timeout arm; still flush an expired hold.
                    jitter.flush_expired_hold(now, &mut deliveries);
                }
                Err(RecvTimeoutError::Timeout) => {
                    jitter.flush_expired_hold(Instant::now(), &mut deliveries);
                    if !mic.alive() {
                        tracing::warn!("virtual mic backend died while idle — reopening");
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::debug!("mic pump stopped (host shutting down)");
                    return;
                }
            }

            for d in deliveries.drain(..) {
                let samples_per_ch = match d {
                    Deliver::Frame(frame) => {
                        if frame.opus.is_empty() {
                            continue; // DTX — the source underruns to silence on its own
                        }
                        match decoder.decode_float(&frame.opus, &mut pcm, false) {
                            Ok(n) => {
                                plc_samples = n.max(120); // ≥ 2.5 ms; shorter slices mis-size libopus PLC
                                decode_fails = 0;
                                n
                            }
                            Err(e) => {
                                decode_fails += 1;
                                if decode_fails.is_power_of_two() {
                                    tracing::warn!(error = %e, fails = decode_fails,
                                        "mic opus decode failed — dropping frame");
                                }
                                continue;
                            }
                        }
                    }
                    Deliver::Conceal => {
                        // Empty-input decode = libopus PLC for this slice, so one gap does not starve the ring into a re-prime.
                        let want = (plc_samples * MIC_CHANNELS as usize).min(pcm.len());
                        match decoder.decode_float(&[], &mut pcm[..want], false) {
                            Ok(n) => n,
                            Err(_) => continue, // nothing decoded yet — nothing to extend
                        }
                    }
                };
                let total = (samples_per_ch * MIC_CHANNELS as usize).min(pcm.len());
                // Persistently-over-target depth sheds one near-silent frame; never speech, never a hard clear.
                let mut shed = false;
                if !legacy {
                    match mic.depth() {
                        Some((buffered, target))
                            if buffered > target + TRIM_MARGIN_MS * SAMPLE_RATE as usize / 1000 =>
                        {
                            let since = *over_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= TRIM_AFTER
                                && last_trim.elapsed() >= TRIM_SPACING
                                && pcm[..total].iter().all(|s| s.abs() < TRIM_SILENCE_PEAK)
                            {
                                shed = true;
                                last_trim = Instant::now();
                                trimmed += 1;
                            }
                        }
                        _ => over_since = None,
                    }
                }
                if shed {
                    last_push = Instant::now(); // trim is not a stale gap
                    continue;
                }
                if !mic.push(&pcm[..total]) {
                    tracing::warn!("virtual mic backend died — reopening");
                    break 'pump;
                }
                last_push = Instant::now();
            }

            // Legacy mode leaves the backend on its fixed prime (`VirtualMic::set_target_depth`).
            if !legacy {
                let t = jitter.target_ms(Instant::now());
                if t != applied_target_ms {
                    applied_target_ms = t;
                    mic.set_target_depth(t as usize * SAMPLE_RATE as usize / 1000);
                }
            }

            // Reset-on-read; skip the line when the window saw no frames.
            if last_log.elapsed() >= TELEMETRY_EVERY {
                if frames_seen > 0 {
                    let js = jitter.take_stats();
                    let bs = mic.take_stats();
                    let (depth_ms, target_ms) = mic
                        .depth()
                        .map(|(d, t)| {
                            (
                                d * 1000 / SAMPLE_RATE as usize,
                                t * 1000 / SAMPLE_RATE as usize,
                            )
                        })
                        .unwrap_or((0, 0));
                    tracing::info!(
                        depth_ms,
                        target_ms,
                        cadence_ms = (js.cadence_ms * 10.0).round() / 10.0,
                        frame_ms = (js.frame_ms * 10.0).round() / 10.0,
                        frames = frames_seen,
                        gaps = js.seq_gaps,
                        concealed = js.concealed,
                        reorders = js.reorders,
                        late = js.late_drops,
                        drained = drain_drops,
                        trimmed,
                        reprimes = bs.reprimes,
                        overflow_ms = bs.overflow_dropped * 1000 / SAMPLE_RATE as u64,
                        "mic uplink health"
                    );
                }
                last_log = Instant::now();
                frames_seen = 0;
                drain_drops = 0;
                trimmed = 0;
            }
        }

        // Lived ≥ stable_after: one-off death, reset backoff. Died earlier: failed open — back off or the pump churns at heartbeat rate.
        if opened_at.elapsed() >= tuning.stable_after {
            backoff = tuning.backoff_start;
            open_fails = 0;
        } else {
            open_fails += 1;
            if !drain_sleep(&rx, backoff) {
                return;
            }
            backoff = (backoff * 2).min(tuning.backoff_cap);
        }
    }
}

#[cfg(test)]
mod pump_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct MockMic {
        alive: Arc<AtomicBool>,
        pushed: Arc<AtomicUsize>,
        discards: Arc<AtomicUsize>,
    }
    impl VirtualMic for MockMic {
        fn push(&self, pcm: &[f32]) -> bool {
            if !self.alive.load(Ordering::Acquire) {
                return false;
            }
            self.pushed.fetch_add(pcm.len(), Ordering::Relaxed);
            true
        }
        fn alive(&self) -> bool {
            self.alive.load(Ordering::Acquire)
        }
        fn discard(&self) {
            self.discards.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Harness {
        tx: std::sync::mpsc::SyncSender<MicFrame>,
        opens: Arc<AtomicUsize>,
        alive: Arc<Mutex<Option<Arc<AtomicBool>>>>, // latest instance's kill switch
        pushed: Arc<AtomicUsize>,
        discards: Arc<AtomicUsize>,
        join: std::thread::JoinHandle<()>,
    }

    /// Real loop vs mocks. `fail_first` = open failures before success. `dead_on_arrival` = every
    /// instance pre-killed. `stable_after = ZERO` treats every death as stable so tests stay fast.
    fn start_tuned(fail_first: usize, dead_on_arrival: bool, stable_after: Duration) -> Harness {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MicFrame>(MIC_QUEUE_CAP);
        let opens = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(Mutex::new(None::<Arc<AtomicBool>>));
        let pushed = Arc::new(AtomicUsize::new(0));
        let discards = Arc::new(AtomicUsize::new(0));
        let (opens2, alive2, pushed2, discards2) = (
            opens.clone(),
            alive.clone(),
            pushed.clone(),
            discards.clone(),
        );
        let tuning = PumpTuning {
            backoff_start: Duration::from_millis(10),
            backoff_cap: Duration::from_millis(40),
            heartbeat: Duration::from_millis(20),
            stale_gap: Duration::from_millis(80),
            stable_after,
        };
        let join = std::thread::spawn(move || {
            pump_thread(
                rx,
                move || {
                    let n = opens2.fetch_add(1, Ordering::SeqCst);
                    if n < fail_first {
                        anyhow::bail!("backend not up yet (simulated)");
                    }
                    let a = Arc::new(AtomicBool::new(!dead_on_arrival));
                    *alive2.lock().unwrap() = Some(a.clone());
                    Ok(Box::new(MockMic {
                        alive: a,
                        pushed: pushed2.clone(),
                        discards: discards2.clone(),
                    }) as Box<dyn VirtualMic>)
                },
                tuning,
            )
        });
        Harness {
            tx,
            opens,
            alive,
            pushed,
            discards,
            join,
        }
    }

    fn start(fail_first: usize) -> Harness {
        start_tuned(fail_first, false, Duration::ZERO)
    }

    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        for _ in 0..600 {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Keep sending until PCM hits the backend.
    ///
    /// One frame is not enough after reopen: the open path drains the queue (that audio predates
    /// the device), and `opens` ticks at the *start* of open, so a send on the counter bump lands
    /// in that drain. Seq must advance or de-jitter drops repeats as duplicates.
    fn wait_until_pushed(what: &str, h: &Harness, from_seq: u32) {
        let mut seq = from_seq;
        for _ in 0..600 {
            let _ = h.tx.try_send(mic_frame(seq));
            seq = seq.wrapping_add(1);
            if h.pushed.load(Ordering::SeqCst) > 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    fn opus_frame() -> Vec<u8> {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Voip)
            .expect("opus encoder");
        let pcm = [0.1f32; 960 * 2]; // 20 ms stereo
        let mut out = vec![0u8; 4000];
        let n = enc.encode_float(&pcm, &mut out).expect("encode");
        out.truncate(n);
        out
    }

    /// `pts_ns = seq * 20 ms`; payload from [`opus_frame`].
    fn mic_frame(seq: u32) -> MicFrame {
        MicFrame {
            seq,
            pts_ns: seq as u64 * 20_000_000,
            opus: opus_frame(),
        }
    }

    #[test]
    fn opens_eagerly_with_backoff() {
        let h = start(3);
        wait_until("eager open after 3 failures", || {
            h.opens.load(Ordering::SeqCst) >= 4 && h.alive.lock().unwrap().is_some()
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn decodes_and_pushes() {
        let h = start(0);
        wait_until("open", || h.alive.lock().unwrap().is_some());
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("pcm pushed", || h.pushed.load(Ordering::SeqCst) > 0);
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn reopens_after_idle_death() {
        let h = start(0);
        wait_until("first open", || h.opens.load(Ordering::SeqCst) >= 1);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.alive
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store(false, Ordering::Release);
        wait_until("reopen after idle death", || {
            h.opens.load(Ordering::SeqCst) >= 2
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn reopens_after_push_death() {
        let h = start(0);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.alive
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .store(false, Ordering::Release);
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("reopen", || h.opens.load(Ordering::SeqCst) >= 2);
        wait_until_pushed("pcm after reopen", &h, 1);
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn rapid_death_backs_off() {
        // Dead on arrival; high stable_after so each death is a failed open.
        // Unguarded: ~25 opens / 500 ms at 20 ms heartbeat. Backoff 10→20→40: ≈ 7.
        let h = start_tuned(0, true, Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(500));
        let opens = h.opens.load(Ordering::SeqCst);
        assert!(opens >= 2, "must keep retrying (got {opens})");
        assert!(
            opens <= 15,
            "must back off, not churn per heartbeat (got {opens})"
        );
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// seq 0 then 2 must push ~3 frames (decode, PLC, decode) once the reorder window expires.
    #[test]
    fn seq_gap_is_concealed() {
        let h = start(0);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.tx.send(mic_frame(0)).unwrap();
        h.tx.send(mic_frame(2)).unwrap();
        // 0 plays now; 2 is held ≤ 30 ms for 1, then conceal + play 2.
        wait_until("conceal + late frame pushed", || {
            h.pushed.load(Ordering::SeqCst) >= 3 * 960 * 2
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    #[test]
    fn discards_after_gap() {
        let h = start(0);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("first push", || h.pushed.load(Ordering::SeqCst) > 0);
        std::thread::sleep(Duration::from_millis(150)); // > stale_gap
        h.tx.send(mic_frame(1)).unwrap();
        wait_until("discard on gap", || h.discards.load(Ordering::SeqCst) >= 1);
        drop(h.tx);
        h.join.join().unwrap();
    }
}
