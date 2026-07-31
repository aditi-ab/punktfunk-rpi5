//! Host-lifetime virtual-microphone pump: one thread owns the [`VirtualMic`] backend plus an Opus
//! decoder, self-heals across backend deaths, and feeds decoded client-mic PCM into the source so
//! the client's microphone reaches host apps. Split out of the `audio` facade (§2.1 — a
//! self-contained stateful subsystem does not belong in the trait facade); the [`VirtualMic`]
//! trait, its factory ([`open_virtual_mic`](super::open_virtual_mic)) and the audio-plane sample
//! rate stay in `super`.

use super::mic_jitter::{Deliver, MicDejitter};
use super::{VirtualMic, SAMPLE_RATE};
use anyhow::Result;

/// Mic is 48 kHz stereo — matches the Opus stereo decoder and the host→client audio layout.
pub const MIC_CHANNELS: u32 = 2;

/// One client mic frame off the wire (`0xCB` — `punktfunk_core::quic::decode_mic_datagram`).
/// The datagram's seq + pts used to be decoded at ingest and thrown away; the pump's de-jitter
/// needs them (reorder window, loss concealment, cadence/drift), so they ride the queue with
/// the Opus payload.
pub struct MicFrame {
    pub seq: u32,
    pub pts_ns: u64,
    pub opus: Vec<u8>,
}

/// Bound for the shared mic frame queue (drop-newest at the producer when full): the
/// host-lifetime queue is shared across all concurrent sessions and must not grow without limit
/// under a near-line-rate flood (security-review 2026-06-28 S6). 12 × 5–20 ms frames ≈
/// 60–240 ms of slack — enough for a scheduling hiccup, and the consumer heals the rest (see
/// [`DRAIN_ABOVE`]). The old cap of 64 could park 1.28 s of standing latency here that only a
/// >600 ms silence gap ever flushed.
const MIC_QUEUE_CAP: usize = 12;
/// Consumer self-healing: a pump that wakes to a backlog deeper than this drops down to the
/// newest [`DRAIN_KEEP`] frames before decoding — the oldest frames are already late, and
/// playing them would turn a one-off stall into permanent mic delay.
const DRAIN_ABOVE: usize = 6;
const DRAIN_KEEP: usize = 4;

/// Cadence of the "mic uplink health" telemetry line (emitted only while frames flow).
const TELEMETRY_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// Creep trim: the backend ring must sit more than this over its prime target, for
/// [`TRIM_AFTER`], before the pump starts shedding — and then only near-silent frames, at most
/// one per [`TRIM_SPACING`] (a 20 ms frame per 300 ms ≈ 7 ms per 100 ms). Depth built by a
/// burst otherwise only ever comes back down at a full drain (the device consumes exactly what
/// it plays).
const TRIM_MARGIN_MS: usize = 15;
const TRIM_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
const TRIM_SPACING: std::time::Duration = std::time::Duration::from_millis(300);
/// Peak below which a decoded frame counts as a silence stretch (≈ −48 dBFS).
const TRIM_SILENCE_PEAK: f32 = 0.004;

/// Tuning for [`MicPump`]'s open/reopen/flush behaviour — parameterized so the tests can run the
/// real pump loop in milliseconds instead of seconds.
#[derive(Clone, Copy)]
struct PumpTuning {
    /// First-retry delay after a failed backend open; doubles per failure up to `backoff_cap`
    /// (a persistently-absent PipeWire session / audio endpoint isn't hammered), resets on
    /// success.
    backoff_start: std::time::Duration,
    backoff_cap: std::time::Duration,
    /// Idle liveness-probe interval: with no frames flowing, the pump still notices a dead
    /// backend this often and reopens — so the mic is healthy BEFORE the next session starts.
    heartbeat: std::time::Duration,
    /// An uplink gap longer than this discards the backend's buffered audio before pushing the
    /// next frame (a recorder must never hear a stale burst from before a mute/session end).
    stale_gap: std::time::Duration,
    /// A backend that dies before living this long counts as a FAILED open for backoff purposes
    /// (an open that succeeds but dies instantly — e.g. a flapping daemon — must not churn at
    /// heartbeat rate); one that lived longer resets the backoff.
    stable_after: std::time::Duration,
}

const PUMP_TUNING: PumpTuning = PumpTuning {
    backoff_start: std::time::Duration::from_secs(2),
    backoff_cap: std::time::Duration::from_secs(60),
    heartbeat: std::time::Duration::from_secs(1),
    stale_gap: std::time::Duration::from_millis(600),
    stable_after: std::time::Duration::from_secs(5),
};

/// Host-lifetime virtual-microphone pump: one thread owns the [`VirtualMic`] backend + an Opus
/// decoder; sessions forward the client's Opus mic frames (0xCB) over a clonable `Send` sender,
/// the thread decodes and feeds the backend.
///
/// The rock-solid properties live HERE, not in the backends:
/// - **Eager**: the backend opens at host start (retrying with backoff), NOT on the first mic
///   frame — so the virtual mic device already exists when host apps/games launch and bind
///   their capture device (most games never re-follow a default-device change mid-run).
/// - **Self-healing**: a dead backend (PipeWire restart, Windows endpoint churn) is detected on
///   every push and on an idle heartbeat, and reopened with backoff. Sessions keep their
///   senders; nothing upstream notices.
/// - **Stale-flush**: buffered audio is discarded after an uplink gap (see [`PumpTuning`]).
/// - **De-jittered**: frames pass a sequence-aware reorder window + loss concealment, and an
///   adaptive target-depth estimator drives the backend rings' priming — see
///   [`MicDejitter`](super::mic_jitter) (`PUNKTFUNK_MIC_LEGACY_BUFFER=1` restores the fixed
///   pre-adaptive buffering).
///
/// Per-frame Opus DECODE errors stay non-fatal (dropped frame): the mic is shared across every
/// concurrent session, so one paired client's junk frames must not deny everyone's mic
/// (security-review 2026-06-28 S2). The thread exits when every sender is dropped (host
/// shutdown), tearing the backend down.
pub struct MicPump {
    tx: std::sync::mpsc::SyncSender<MicFrame>,
}

impl MicPump {
    /// Start the host-lifetime pump (Linux/Windows). On platforms without a virtual-mic backend
    /// the thread just drains and drops frames (sessions still count the datagrams).
    pub fn start() -> MicPump {
        let (tx, rx) = std::sync::mpsc::sync_channel::<MicFrame>(MIC_QUEUE_CAP);
        let spawned = std::thread::Builder::new()
            .name("punktfunk-mic-pump".into())
            .spawn(move || {
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                pump_thread(rx, || super::open_virtual_mic(MIC_CHANNELS), PUMP_TUNING);
                #[cfg(not(any(target_os = "linux", target_os = "windows")))]
                {
                    tracing::warn!("mic passthrough unsupported on this platform — frames dropped");
                    for _ in rx {}
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "mic pump thread spawn failed — mic passthrough disabled");
        }
        MicPump { tx }
    }

    /// A sender a session forwards the client's Opus mic frames to (`try_send` — never block a
    /// datagram loop). Cloned per session; dropping a clone does NOT stop the pump (it holds
    /// the original sender for the host life).
    pub fn sender(&self) -> std::sync::mpsc::SyncSender<MicFrame> {
        self.tx.clone()
    }
}

/// Sleep for `dur` while draining (and dropping) queued frames, so a closed/reopening backend
/// never accumulates a stale backlog and senders never see a wedged queue. Returns `false` when
/// every sender is gone (host shutdown).
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
            Ok(_) => {}                                          // drop frames while closed
            Err(RecvTimeoutError::Timeout) => {}                 // keep waiting
            Err(RecvTimeoutError::Disconnected) => return false, // host shutdown
        }
    }
}

/// The pump loop. `opener` is injected so the tests can run the REAL loop against a mock
/// backend; production passes [`open_virtual_mic`](super::open_virtual_mic).
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
        // Open phase — eager, from thread start.
        let (mic, mut decoder) = loop {
            let opened = opener().and_then(|m| {
                let d = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Stereo)
                    .map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
                Ok((m, d))
            });
            match opened {
                Ok(pair) => break pair,
                Err(e) => {
                    // Throttle (1st, 2nd, 4th, 8th … failure): a box without a PipeWire session
                    // or virtual audio device would otherwise log every backoff forever.
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
        // Drop anything queued while (re)opening — it predates the backend. (The backoff does
        // NOT reset here: only an instance that proves stable resets it — see the death triage.)
        while rx.try_recv().is_ok() {}
        let opened_at = Instant::now();

        // Pump phase — runs until the backend dies (break) or the host shuts down (return).
        let legacy = super::mic_legacy_buffer();
        let mut decode_fails: u64 = 0;
        let mut drain_drops: u64 = 0;
        let mut trimmed: u64 = 0;
        let mut frames_seen: u64 = 0;
        let mut pcm = vec![0f32; 5760 * MIC_CHANNELS as usize]; // up to 120 ms scratch
        let mut last_push = Instant::now();
        let mut batch: Vec<MicFrame> = Vec::new();
        let mut deliveries: Vec<Deliver> = Vec::new();
        let mut jitter = MicDejitter::new();
        // Concealment must match the last real frame's duration — libopus sizes PLC from the
        // output slice. One 20 ms frame until something decodes.
        let mut plc_samples: usize = 960;
        let mut applied_target_ms: u32 = 0;
        let mut over_since: Option<Instant> = None;
        let mut last_trim = Instant::now();
        let mut last_log = Instant::now();
        'pump: loop {
            // Wake for the next frame, the heartbeat, or a parked reorder hold aging out —
            // whichever is soonest.
            let timeout = jitter
                .hold_deadline()
                .map(|d| d.saturating_duration_since(Instant::now()))
                .unwrap_or(tuning.heartbeat)
                .min(tuning.heartbeat);
            deliveries.clear();
            match rx.recv_timeout(timeout) {
                Ok(first) => {
                    // Take everything already queued in one gulp: normally that's just `first`,
                    // but after a stall the backlog IS standing mic latency — heal by jumping
                    // to the newest few frames instead of replaying the pile.
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
                    // A hold whose window expired while traffic kept flowing (only late
                    // duplicates arriving) must still flush — the timeout arm never fires then.
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
                            continue; // DTX silence — the source underruns to silence on its own
                        }
                        match decoder.decode_float(&frame.opus, &mut pcm, false) {
                            Ok(n) => {
                                plc_samples = n.max(120); // ≥ 2.5 ms keeps PLC well-formed
                                decode_fails = 0;
                                n
                            }
                            Err(e) => {
                                // Malformed/garbage frame: drop it, keep the shared mic +
                                // decoder (see the struct docs). Throttled log (1, 2, 4, …).
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
                        // A lost datagram: an empty-input decode synthesizes libopus PLC for
                        // exactly the slice's duration, so the backend ring doesn't starve
                        // into a silence + re-prime cycle over one missing frame.
                        let want = (plc_samples * MIC_CHANNELS as usize).min(pcm.len());
                        match decoder.decode_float(&[], &mut pcm[..want], false) {
                            Ok(n) => n,
                            Err(_) => continue, // nothing decoded yet — nothing to extend
                        }
                    }
                };
                let total = (samples_per_ch * MIC_CHANNELS as usize).min(pcm.len());
                // Creep trim: ring depth persistently over target sheds a near-silent frame at
                // a time (a few ms per 100 ms) — never speech, never a hard clear. Without it,
                // depth built by a burst only ever comes back down at a full drain.
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
                    last_push = Instant::now(); // the uplink is live — a trim is not a stale gap
                    continue;
                }
                if !mic.push(&pcm[..total]) {
                    tracing::warn!("virtual mic backend died — reopening");
                    break 'pump;
                }
                last_push = Instant::now();
            }

            // Drive the backend ring's prime target from the measured jitter. Legacy mode
            // never calls this, which keeps the backend on its fixed constants (see
            // `VirtualMic::set_target_depth`).
            if !legacy {
                let t = jitter.target_ms(Instant::now());
                if t != applied_target_ms {
                    applied_target_ms = t;
                    mic.set_target_depth(t as usize * SAMPLE_RATE as usize / 1000);
                }
            }

            // One structured health line per interval while the mic is live (reset-on-read).
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

        // Death triage: an instance that lived is a one-off (PipeWire/audio-engine restart) —
        // reopen immediately with the backoff reset. One that died right after opening is a
        // failed open in disguise (flapping daemon, endpoint racing away): back off like the
        // open loop, or the pump would churn open→die→reopen at heartbeat rate.
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

    /// Mock backend: records pushes/discards, dies on command.
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

    /// Run the REAL pump loop against mock backends; `fail_first` opens fail before the first
    /// success (exercises the eager retry/backoff path). `dead_on_arrival` opens every instance
    /// pre-killed (exercises the rapid-death churn guard). `stable_after` mirrors the tuning
    /// field (ZERO = every death counts as stable → immediate reopen, keeping tests fast).
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
        for _ in 0..200 {
            if cond() {
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

    /// A wire-shaped frame: `seq` with the matching 20 ms pts, payload from [`opus_frame`].
    fn mic_frame(seq: u32) -> MicFrame {
        MicFrame {
            seq,
            pts_ns: seq as u64 * 20_000_000,
            opus: opus_frame(),
        }
    }

    /// Eager: the backend opens (after transient failures) with NO frame ever sent.
    #[test]
    fn opens_eagerly_with_backoff() {
        let h = start(3);
        wait_until("eager open after 3 failures", || {
            h.opens.load(Ordering::SeqCst) >= 4 && h.alive.lock().unwrap().is_some()
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// Frames flow: opus in → PCM pushed to the backend.
    #[test]
    fn decodes_and_pushes() {
        let h = start(0);
        wait_until("open", || h.alive.lock().unwrap().is_some());
        h.tx.send(mic_frame(0)).unwrap();
        wait_until("pcm pushed", || h.pushed.load(Ordering::SeqCst) > 0);
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// A dead backend is noticed WHILE IDLE (heartbeat) and reopened without any traffic.
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
            .store(false, Ordering::Release); // kill it
        wait_until("reopen after idle death", || {
            h.opens.load(Ordering::SeqCst) >= 2
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// A death detected on push (frame flowing) also reopens, and the frame after reopen flows.
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
        h.tx.send(mic_frame(0)).unwrap(); // push sees death → reopen
        wait_until("reopen", || h.opens.load(Ordering::SeqCst) >= 2);
        h.tx.send(mic_frame(1)).unwrap();
        wait_until("pcm after reopen", || h.pushed.load(Ordering::SeqCst) > 0);
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// Instances that die immediately after opening must be retried with BACKOFF, not at
    /// heartbeat rate — a flapping backend (daemon up but dropping us instantly) would
    /// otherwise churn open→die→reopen every heartbeat forever.
    #[test]
    fn rapid_death_backs_off() {
        // Every instance is dead on arrival; stability threshold high so each death counts
        // as a failed open. Without the guard: ~1 reopen per heartbeat (20 ms) ≈ 25 opens in
        // 500 ms. With backoff 10→20→40 (cap): ≈ 7.
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

    /// A lost datagram is concealed: seq 0 then 2 (1 missing) must still push ~3 frames of PCM
    /// (decode, PLC, decode) once the reorder window expires — the backend ring never starves
    /// over a single missing frame.
    #[test]
    fn seq_gap_is_concealed() {
        let h = start(0);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.tx.send(mic_frame(0)).unwrap();
        h.tx.send(mic_frame(2)).unwrap();
        // 0 plays immediately; 2 is held ≤ 30 ms for 1, then the gap is concealed and 2 plays.
        wait_until("conceal + late frame pushed", || {
            h.pushed.load(Ordering::SeqCst) >= 3 * 960 * 2
        });
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// An uplink gap discards buffered-stale audio before the next frame plays.
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
