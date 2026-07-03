//! Desktop audio capture for the GameStream audio stream. On Linux: a PipeWire stream that
//! records the default sink's monitor (i.e. everything playing out of the system), delivered
//! as interleaved `f32` PCM at 48 kHz in the requested channel count (stereo, 5.1 or 7.1 —
//! GameStream surround order FL FR FC LFE RL RR [SL SR]). The audio data plane
//! (`gamestream::audio`) reframes this into fixed Opus frames, encodes, and sends it.

use anyhow::Result;

/// Opus/GameStream audio is 48 kHz.
pub const SAMPLE_RATE: u32 = 48_000;
/// Stereo channel count — the default and the punktfunk/1 audio plane's fixed layout.
pub const CHANNELS: usize = 2;

/// Produces interleaved `f32` PCM at [`SAMPLE_RATE`] in the channel count it was opened
/// with. Lives on its own thread; never blocks the capture loop (drops if the consumer
/// falls behind).
pub trait AudioCapturer: Send {
    /// Block until the next chunk of interleaved samples is available (variable size). The
    /// caller reframes into fixed Opus frames. An **empty** chunk means "no samples right now"
    /// (e.g. a quiet sink that hit the internal idle timeout) — NOT an error: the caller keeps the
    /// capturer. `Err` is reserved for a genuinely dead capture thread, signalling the caller to
    /// reopen.
    fn next_chunk(&mut self) -> Result<Vec<f32>>;

    /// The interleaved channel count this capturer delivers (what it was opened with).
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }

    /// Discard any buffered chunks (called when a persistent capturer is reused for a new
    /// stream, so the client doesn't hear stale audio captured while idle). Default: no-op.
    fn drain(&mut self) {}
}

/// Open a live capturer for the default sink monitor (system output) via PipeWire, asking
/// for `channels` interleaved channels. If the sink has fewer channels than requested,
/// PipeWire's channel-mixer fills the missing positions with silence (zero upmix).
#[cfg(target_os = "linux")]
pub fn open_audio_capture(channels: u32) -> Result<Box<dyn AudioCapturer>> {
    linux::PwAudioCapturer::open(channels).map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(target_os = "windows")]
pub fn open_audio_capture(channels: u32) -> Result<Box<dyn AudioCapturer>> {
    // The capture thread runs the audio wiring plan itself (audio_control::wire_now) before
    // resolving its endpoint — a fresh plan per open, because Windows endpoints churn.
    wasapi_cap::WasapiLoopbackCapturer::open(channels)
        .map(|c| Box::new(c) as Box<dyn AudioCapturer>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open_audio_capture(_channels: u32) -> Result<Box<dyn AudioCapturer>> {
    anyhow::bail!("audio capture requires Linux + PipeWire or Windows + WASAPI")
}

/// The inverse of [`AudioCapturer`]: a virtual microphone the host *produces*. It registers a
/// PipeWire `Audio/Source` node that host apps can record from; the host [`push`](Self::push)es
/// decoded client-mic PCM (interleaved `f32` at [`SAMPLE_RATE`]) into it, and PipeWire delivers
/// it to whichever app records the source — silence when no input is flowing. This is how the
/// client's microphone reaches host applications (mic passthrough).
///
/// **Liveness contract.** Both backends run a worker thread that CAN die under the host's feet
/// (Linux: the PipeWire daemon restarts with the session; Windows: the audio endpoint is
/// invalidated/removed). A dead backend must be observable — [`push`](Self::push) returns `false`
/// and [`alive`](Self::alive) turns false — so the owning [`MicPump`] drops the instance and
/// reopens. Before this contract existed, a single backend death left `push` feeding a dead
/// queue for the rest of the host's life: the historical "mic passthrough works on no host" bug.
pub trait VirtualMic: Send {
    /// Push one chunk of interleaved `f32` PCM. Non-blocking — drops if the backend is behind
    /// (mic audio is lossy/real-time; a stale chunk is worse than a dropped one). Returns
    /// `false` iff the backend is DEAD (worker thread gone) — the caller must reopen; a merely
    /// congested backend drops the chunk and returns `true`.
    fn push(&self, pcm: &[f32]) -> bool;

    /// Backend liveness without pushing data — lets an idle pump notice a death between
    /// sessions, so the mic is already healthy again when the next client connects.
    fn alive(&self) -> bool;

    /// Drop any buffered-but-unplayed audio. Called after an uplink gap (client muted,
    /// session ended) so a recorder never hears a stale burst when audio resumes.
    fn discard(&self);

    /// The interleaved channel count the source was opened with.
    fn channels(&self) -> u32 {
        CHANNELS as u32
    }
}

/// Open a virtual microphone with `channels` interleaved channels (1 or 2). Linux: a PipeWire
/// `Audio/Source`. Windows: writes into an existing virtual audio device's render endpoint (whose
/// capture endpoint apps see as a mic) — see [`wasapi_mic`].
#[cfg(target_os = "linux")]
pub fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    linux::PwMicSource::open(channels).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

#[cfg(target_os = "windows")]
pub fn open_virtual_mic(channels: u32) -> Result<Box<dyn VirtualMic>> {
    // The render thread runs the wiring plan itself (audio_control::wire_now) to resolve — and,
    // via the plan's default-device changes, to RESERVE — its target endpoint.
    wasapi_mic::WasapiVirtualMic::open(channels).map(|m| Box::new(m) as Box<dyn VirtualMic>)
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open_virtual_mic(_channels: u32) -> Result<Box<dyn VirtualMic>> {
    anyhow::bail!("virtual mic requires Linux + PipeWire or Windows + a virtual audio device")
}

/// Mic is 48 kHz stereo — matches the Opus stereo decoder and the host→client audio layout.
pub const MIC_CHANNELS: u32 = 2;
/// Bound for the shared mic frame queue (drop-newest when full): the host-lifetime queue is
/// shared across all concurrent sessions and must not grow without limit under a near-line-rate
/// flood (security-review 2026-06-28 S6). 64 × 5–20 ms frames ≈ 0.3–1.3 s of slack.
const MIC_QUEUE_CAP: usize = 64;

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
}

const PUMP_TUNING: PumpTuning = PumpTuning {
    backoff_start: std::time::Duration::from_secs(2),
    backoff_cap: std::time::Duration::from_secs(60),
    heartbeat: std::time::Duration::from_secs(1),
    stale_gap: std::time::Duration::from_millis(600),
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
///
/// Per-frame Opus DECODE errors stay non-fatal (dropped frame): the mic is shared across every
/// concurrent session, so one paired client's junk frames must not deny everyone's mic
/// (security-review 2026-06-28 S2). The thread exits when every sender is dropped (host
/// shutdown), tearing the backend down.
pub struct MicPump {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}

impl MicPump {
    /// Start the host-lifetime pump (Linux/Windows). On platforms without a virtual-mic backend
    /// the thread just drains and drops frames (sessions still count the datagrams).
    pub fn start() -> MicPump {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(MIC_QUEUE_CAP);
        let spawned = std::thread::Builder::new()
            .name("punktfunk-mic-pump".into())
            .spawn(move || {
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                pump_thread(rx, || open_virtual_mic(MIC_CHANNELS), PUMP_TUNING);
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
    pub fn sender(&self) -> std::sync::mpsc::SyncSender<Vec<u8>> {
        self.tx.clone()
    }
}

/// The pump loop. `opener` is injected so the tests can run the REAL loop against a mock
/// backend; production passes [`open_virtual_mic`].
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
fn pump_thread<O>(rx: std::sync::mpsc::Receiver<Vec<u8>>, opener: O, tuning: PumpTuning)
where
    O: Fn() -> Result<Box<dyn VirtualMic>>,
{
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;

    let mut backoff = tuning.backoff_start;
    let mut open_fails: u64 = 0;
    'reopen: loop {
        // Open phase — eager, from thread start. While closed, keep draining the queue so a
        // reopen never replays a backlog of stale frames (and senders never see a wedged queue).
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
                    let deadline = Instant::now() + backoff;
                    loop {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            break;
                        }
                        match rx.recv_timeout(left.min(std::time::Duration::from_millis(250))) {
                            Ok(_) => {}                                    // drop frames while closed
                            Err(RecvTimeoutError::Timeout) => {}           // keep waiting
                            Err(RecvTimeoutError::Disconnected) => return, // host shutdown
                        }
                    }
                    backoff = (backoff * 2).min(tuning.backoff_cap);
                }
            }
        };
        backoff = tuning.backoff_start;
        open_fails = 0;
        tracing::info!("virtual mic ready (host-lifetime)");
        // Drop anything queued while (re)opening — it predates the backend.
        while rx.try_recv().is_ok() {}

        let mut decode_fails: u64 = 0;
        let mut pcm = vec![0f32; 5760 * MIC_CHANNELS as usize]; // up to 120 ms scratch
        let mut last_push = Instant::now();
        loop {
            match rx.recv_timeout(tuning.heartbeat) {
                Ok(frame) => {
                    if frame.is_empty() {
                        continue; // DTX silence — the source underruns to silence on its own
                    }
                    if last_push.elapsed() > tuning.stale_gap {
                        mic.discard();
                    }
                    match decoder.decode_float(&frame, &mut pcm, false) {
                        Ok(samples_per_ch) => {
                            let total = (samples_per_ch * MIC_CHANNELS as usize).min(pcm.len());
                            if !mic.push(&pcm[..total]) {
                                tracing::warn!("virtual mic backend died — reopening");
                                continue 'reopen;
                            }
                            last_push = Instant::now();
                            decode_fails = 0;
                        }
                        Err(e) => {
                            // Malformed/garbage frame: drop it, keep the shared mic + decoder
                            // (see the struct docs). Throttled log (1, 2, 4, … fails).
                            decode_fails += 1;
                            if decode_fails.is_power_of_two() {
                                tracing::warn!(error = %e, fails = decode_fails,
                                    "mic opus decode failed — dropping frame");
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if !mic.alive() {
                        tracing::warn!("virtual mic backend died while idle — reopening");
                        continue 'reopen;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::debug!("mic pump stopped (host shutting down)");
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "windows")]
#[path = "audio/windows/audio_control.rs"]
mod audio_control;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
#[path = "audio/windows/wasapi_cap.rs"]
mod wasapi_cap;
#[cfg(target_os = "windows")]
#[path = "audio/windows/wasapi_mic.rs"]
mod wasapi_mic;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[path = "audio/wiring_plan.rs"]
pub(crate) mod wiring_plan;

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
        tx: std::sync::mpsc::SyncSender<Vec<u8>>,
        opens: Arc<AtomicUsize>,
        alive: Arc<Mutex<Option<Arc<AtomicBool>>>>, // latest instance's kill switch
        pushed: Arc<AtomicUsize>,
        discards: Arc<AtomicUsize>,
        join: std::thread::JoinHandle<()>,
    }

    /// Run the REAL pump loop against mock backends; `fail_first` opens fail before the first
    /// success (exercises the eager retry/backoff path).
    fn start(fail_first: usize) -> Harness {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(MIC_QUEUE_CAP);
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
        };
        let join = std::thread::spawn(move || {
            pump_thread(
                rx,
                move || {
                    let n = opens2.fetch_add(1, Ordering::SeqCst);
                    if n < fail_first {
                        anyhow::bail!("backend not up yet (simulated)");
                    }
                    let a = Arc::new(AtomicBool::new(true));
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
        h.tx.send(opus_frame()).unwrap();
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
        h.tx.send(opus_frame()).unwrap(); // push sees death → reopen
        wait_until("reopen", || h.opens.load(Ordering::SeqCst) >= 2);
        h.tx.send(opus_frame()).unwrap();
        wait_until("pcm after reopen", || h.pushed.load(Ordering::SeqCst) > 0);
        drop(h.tx);
        h.join.join().unwrap();
    }

    /// An uplink gap discards buffered-stale audio before the next frame plays.
    #[test]
    fn discards_after_gap() {
        let h = start(0);
        wait_until("instance", || h.alive.lock().unwrap().is_some());
        h.tx.send(opus_frame()).unwrap();
        wait_until("first push", || h.pushed.load(Ordering::SeqCst) > 0);
        std::thread::sleep(Duration::from_millis(150)); // > stale_gap
        h.tx.send(opus_frame()).unwrap();
        wait_until("discard on gap", || h.discards.load(Ordering::SeqCst) >= 1);
        drop(h.tx);
        h.join.join().unwrap();
    }
}
