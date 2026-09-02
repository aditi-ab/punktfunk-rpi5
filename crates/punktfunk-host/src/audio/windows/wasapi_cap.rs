//! WASAPI loopback capture of the desktop mix (Windows analogue of the PipeWire sink-monitor).
//! Interleaved f32 PCM at the opened engine rate — never above it; see
//! [`WasapiLoopbackCapturer::opened_rate`] — in the requested layout (stereo / 5.1 / 7.1,
//! `dwChannelMask` FL FR FC LFE RL RR SL SR). Shared-mode autoconvert does SRC and up/downmix.
//! WASAPI objects are COM-apartment-bound and `!Send`, so they live on a dedicated thread;
//! the struct holds only the channel, stop flag, and join handle.
//!
//! Capture binds the wiring plan's loopback endpoint explicitly, never the current default
//! (that races the plan's own `IPolicyConfig` write). A 1 s watchdog follows a capturable
//! default change and snaps a known-dud back to the plan. Device errors reopen with capped
//! exponential backoff cut short by an endpoint-set change. A plan with no loopback endpoint
//! is never retried: [`wiring_plan::plan`](super::wiring_plan) is pure in the set, so the
//! thread parks on a fingerprint poll until the set moves. On drop, parked default playback
//! and recording devices are restored — both are session-scoped.
//!
//! Pin: `design/hi-res-audio.md`, [`super::wiring_plan`], [`super::audio_control`].

use super::capture_policy::{CaptureStats, FightDamper, FIGHT_BACKOFF, STATS_EVERY};
use super::{audio_control, wiring_plan, AudioCapturer, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wasapi::{Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub struct WasapiLoopbackCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Shared with the capture thread so drops mean "encode fell behind", not "nobody is reading".
    /// Native/gamestream planes park a capturer between sessions ([`idle`](AudioCapturer::idle))
    /// on a bounded channel; without this the thread fills it once and then warns that the
    /// stream will click with no stream to click.
    active: Arc<AtomicBool>,
    /// Rate the endpoint actually opened at. Written before the thread reports ready; read by
    /// [`AudioCapturer::sample_rate`]. Shared-mode autoconvert succeeds on an upward request
    /// with interpolated samples (`design/hi-res-audio.md`), so the open declines instead of
    /// padding and stores what it settled for.
    opened_rate: Arc<AtomicU32>,
}

impl WasapiLoopbackCapturer {
    pub fn open(channels: u32, rate_hz: u32) -> Result<WasapiLoopbackCapturer> {
        anyhow::ensure!(
            matches!(channels, 2 | 6 | 8),
            "WASAPI loopback backend supports 2/6/8 channels (got {channels})"
        );
        anyhow::ensure!(rate_hz > 0, "audio capture rate must be positive");
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        // Handshake: a missing render endpoint is Err (native plane retries), not a silent dead thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let stop_t = stop.clone();
        let active = Arc::new(AtomicBool::new(true));
        let active_t = active.clone();
        // Honest until the endpoint is read; also the final answer on the common 48 kHz path.
        let opened_rate = Arc::new(AtomicU32::new(rate_hz));
        let opened_rate_t = opened_rate.clone();
        let join = thread::Builder::new()
            .name("punktfunk-wasapi-audio".into())
            .spawn(move || {
                if let Err(e) = capture_thread(
                    tx,
                    stop_t,
                    ready_tx,
                    channels,
                    rate_hz,
                    active_t,
                    opened_rate_t,
                ) {
                    tracing::error!(error = %format!("{e:#}"), "wasapi loopback thread failed");
                }
            })
            .context("spawn wasapi audio thread")?;
        // 30 s: first open may auto-install the Steam Streaming pair (two driver installs, ~5 s each).
        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {
                // Settled rate, not `rate_hz` — a log must not print one rate while the stream carries another.
                tracing::info!(
                    channels,
                    rate_hz = opened_rate.load(Ordering::Relaxed),
                    "WASAPI loopback capture: f32"
                );
                Ok(WasapiLoopbackCapturer {
                    chunks: rx,
                    channels,
                    stop,
                    join: Some(join),
                    active,
                    opened_rate,
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Otherwise it captures for the process lifetime with the playback default still parked.
                stop.store(true, Ordering::SeqCst);
                Err(anyhow!("wasapi loopback init timed out"))
            }
        }
    }
}

impl Drop for WasapiLoopbackCapturer {
    fn drop(&mut self) {
        // Receiver dies with us; leftover pushes must not count as encode lag.
        self.active.store(false, Ordering::Relaxed);
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl AudioCapturer for WasapiLoopbackCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        self.next_chunk_within(Duration::from_secs(5))
    }
    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(budget) {
            Ok(c) => Ok(c),
            // Quiet sink is not a failure — empty chunk keeps the capturer. Dead thread is Err.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("wasapi audio thread ended")),
        }
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn sample_rate(&self) -> u32 {
        self.opened_rate.load(Ordering::Relaxed)
    }
    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
        // After the drain so the capture thread never counts a drop this call is emptying.
        self.active.store(true, Ordering::Relaxed);
    }
    fn idle(&mut self) {
        // Channel will fill and stay full; those drops are not encode lag. See `active`.
        self.active.store(false, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetMode {
    /// Plan's loopback endpoint; parks default playback on it (client-only when the sink is silent).
    Assert,
    /// Current default render — operator changed it mid-stream, or `PUNKTFUNK_KEEP_DEFAULT`.
    Follow,
}

enum Next {
    Stopped,
    Reopen(TargetMode),
}

/// First reopen wait after a transient failure. Doubles per miss up to [`REOPEN_BACKOFF_CAP`];
/// resets on success or an endpoint-set change. Do not retry flat at 2 s — each attempt re-runs
/// the wiring pass, IPolicyConfig included.
const REOPEN_BACKOFF_START: Duration = Duration::from_secs(2);
const REOPEN_BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Fingerprint poll while backing off or waiting out an unsatisfiable plan: enumerate-and-hash
/// only. A change ends the wait immediately so a re-arrived endpoint is not stuck behind the cap.
const ENDPOINT_POLL_EVERY: Duration = Duration::from_secs(2);
const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(1);
/// First-open tries before the handshake surfaces Err. Session start races virtual-display
/// attach and this module's own IPolicyConfig flips; activate then fails with 0x80070002
/// (endpoint mid-re-registration).
const FIRST_OPEN_ATTEMPTS: u32 = 3;
/// Endpoint churn settles in well under a second.
const FIRST_OPEN_RETRY_PAUSE: Duration = Duration::from_secs(1);
/// Packet-less stretch after which `DATA_DISCONTINUITY` is idle-resume, not a hole.
/// Classic loopback delivers nothing while nothing renders, then flags the resume packet;
/// scoring that flag always would charge every notification on a silent host. ~10 ms engine
/// period: 1 s is past anything this loop can still tell from a gap.
const LOOPBACK_IDLE_AFTER: Duration = Duration::from_secs(1);

fn capture_thread(
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
    channels: u32,
    rate_hz: u32,
    active: Arc<AtomicBool>,
    opened_rate: Arc<AtomicU32>,
) -> Result<()> {
    // COM is apartment-bound; MTA on this thread, before any device call.
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Must wake on the engine event every ~10 ms or the loopback buffer wraps. Same MMCSS +
    // `THREAD_PRIORITY_HIGHEST` boost as the paced sender; a no-op if refused.
    pf_frame::thread_qos::boost_thread_priority(true);
    // Each `capture_once` is one open + inner loop. First open gets [`FIRST_OPEN_ATTEMPTS`]
    // tries before `open()` surfaces Err; the native plane then retries the whole open.
    let mut ready = Some(ready);
    let mut mode = TargetMode::Assert;
    let mut failures: u64 = 0;
    let mut first_attempts: u32 = 0;
    let mut backoff = REOPEN_BACKOFF_START;
    // Plan is pure in the endpoint set: log the unsatisfiable diagnosis once per fingerprint.
    let mut unsat_logged: Option<u64> = None;
    while !stop.load(Ordering::Relaxed) {
        match capture_once(
            &tx,
            &stop,
            &mut ready,
            channels,
            rate_hz,
            mode,
            &active,
            &opened_rate,
        ) {
            Ok(Next::Stopped) => break,
            Ok(Next::Reopen(m)) => {
                mode = m;
                failures = 0;
                backoff = REOPEN_BACKOFF_START;
                unsat_logged = None;
            }
            Err(e) if ready.is_some() => {
                // Unsatisfiable plan cannot improve inside the handshake — fail now rather than
                // spending the transient retry budget. The native plane owns first-open retries.
                if e.downcast_ref::<PlanUnsatisfiable>().is_some() {
                    let _ = ready.take().unwrap().send(Err(anyhow!("{e:#}")));
                    break;
                }
                first_attempts += 1;
                if first_attempts >= FIRST_OPEN_ATTEMPTS || stop.load(Ordering::Relaxed) {
                    let _ = ready.take().unwrap().send(Err(anyhow!("{e:#}")));
                    break;
                }
                tracing::info!(error = %format!("{e:#}"), attempt = first_attempts,
                    "audio loopback first open failed — retrying");
                // Stop-responsive; same 100 ms slices as the reopen wait below.
                let until = Instant::now() + FIRST_OPEN_RETRY_PAUSE;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            Err(e) => {
                mode = TargetMode::Assert;
                if let Some(unsat) = e.downcast_ref::<PlanUnsatisfiable>() {
                    // Same endpoints → same verdict. Wait on the fingerprint; a wiring retry
                    // would IPolicyConfig-stomp an operator recording-default change.
                    failures = 0;
                    backoff = REOPEN_BACKOFF_START;
                    if unsat_logged != Some(unsat.fingerprint) {
                        unsat_logged = Some(unsat.fingerprint);
                        tracing::error!(
                            "desktop audio unavailable, and retrying cannot help until the \
                             audio endpoint set changes — waiting for that change. {unsat}"
                        );
                    }
                    if wait_endpoint_change(&stop, unsat.fingerprint, None) == EndpointWait::Stopped
                    {
                        break;
                    }
                } else {
                    unsat_logged = None;
                    failures += 1;
                    if failures.is_power_of_two() {
                        tracing::warn!(error = %format!("{e:#}"), count = failures,
                            backoff_secs = backoff.as_secs(),
                            "audio loopback capture failed — reopening after backoff");
                    }
                    // Cut short (and reset) when the set changes — a re-arrived device must not
                    // sit out the 60 s cap.
                    let fp = audio_control::endpoint_fingerprint();
                    match wait_endpoint_change(&stop, fp, Some(Instant::now() + backoff)) {
                        EndpointWait::Stopped => break,
                        EndpointWait::Changed => backoff = REOPEN_BACKOFF_START,
                        EndpointWait::Elapsed => backoff = (backoff * 2).min(REOPEN_BACKOFF_CAP),
                    }
                }
            }
        }
    }
    // Restore both parked defaults (no-op if never parked, or if the operator moved them).
    // Recording restore keeps the parked mic session-scoped; see `audio_control`.
    audio_control::restore_default_playback();
    audio_control::restore_default_recording();
    Ok(())
}

/// Wiring plan with no loopback endpoint. [`wiring_plan::plan`] is pure in the enumerated set,
/// so this is permanent until the topology changes — unlike every other capture error.
/// Carries the fingerprint the reopen loop waits on, plus the diagnosis.
#[derive(Debug)]
struct PlanUnsatisfiable {
    fingerprint: u64,
    detail: String,
}

impl PlanUnsatisfiable {
    fn from_plan(plan: &audio_control::WiredPlan) -> PlanUnsatisfiable {
        debug_assert!(plan.wiring.loopback_unsatisfiable());
        PlanUnsatisfiable {
            fingerprint: plan.fingerprint,
            detail: wiring_plan::describe_no_loopback(&plan.renders, &plan.wiring),
        }
    }
}

impl std::fmt::Display for PlanUnsatisfiable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for PlanUnsatisfiable {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointWait {
    Stopped,
    /// Fingerprint moved — re-plan; this is the recovery.
    Changed,
    /// Deadline passed with no change. `deadline: None` never returns this.
    Elapsed,
}

/// Poll the endpoint-set fingerprint every [`ENDPOINT_POLL_EVERY`] (enumerate-and-hash only —
/// no wiring, no IPolicyConfig) until the set changes, `deadline` passes, or `stop` is set.
/// `deadline: None` waits indefinitely: used while the plan is unsatisfiable.
fn wait_endpoint_change(
    stop: &AtomicBool,
    fingerprint: u64,
    deadline: Option<Instant>,
) -> EndpointWait {
    let mut next_poll = Instant::now() + ENDPOINT_POLL_EVERY;
    loop {
        if stop.load(Ordering::Relaxed) {
            return EndpointWait::Stopped;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return EndpointWait::Elapsed;
        }
        thread::sleep(Duration::from_millis(100));
        if Instant::now() >= next_poll {
            next_poll = Instant::now() + ENDPOINT_POLL_EVERY;
            if audio_control::endpoint_fingerprint() != fingerprint {
                return EndpointWait::Changed;
            }
        }
    }
}

/// Current default render endpoint (`None` on enumeration failure — a miss must not kill capture).
fn default_render(en: &DeviceEnumerator) -> Option<(Device, String)> {
    let d = en.get_default_device(&Direction::Render).ok()?;
    let id = d.get_id().ok()?;
    Some((d, id))
}

/// One endpoint open + capture loop. First open: [`FIRST_OPEN_ATTEMPTS`] then fatal via `ready`.
/// Later: capped backoff, or an endpoint-set wait for [`PlanUnsatisfiable`].
#[allow(clippy::too_many_arguments)]
fn capture_once(
    tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    ready: &mut Option<SyncSender<Result<()>>>,
    channels: u32,
    rate_hz: u32,
    mode: TargetMode,
    active: &AtomicBool,
    opened_rate: &AtomicU32,
) -> Result<Next> {
    // 4 bytes per f32 sample, interleaved.
    let block_align = channels as usize * 4;
    let keep_default = audio_control::keep_default_devices();
    // Only this shape parks the playback default.
    let assert_plan = mode == TargetMode::Assert && !keep_default;
    let mut plan = audio_control::wire_now_full(assert_plan);

    // Client-only audio wants a silent sink with working loopback. Latch is once per INF-STATE,
    // not once per process: an attempt while Steam was absent re-arms when the driver INFs
    // appear. Those files are invisible to the endpoint-set fingerprint, so nothing else retries.
    if assert_plan && !audio_control::host_audio_requested() {
        // Name-match plus minted-id: "Punktfunk Speakers" is silent but the name rule refuses
        // "Speakers", so without the id check a minted session re-attempts a Steam-pair install
        // it does not need.
        let have_silent = |w: &wiring_plan::Wiring| {
            w.loopback_render.as_ref().is_some_and(|(n, id)| {
                wiring_plan::silent_sink(&n.to_lowercase())
                    || super::minted::minted_ids().speakers_render.as_deref() == Some(id.as_str())
            })
        };
        static TRIED_WITH_INFS: Mutex<Option<bool>> = Mutex::new(None);
        let should_try = !have_silent(&plan.wiring) && {
            let infs = super::wasapi_mic::steam_infs_present();
            let mut tried = TRIED_WITH_INFS.lock().unwrap();
            let go = match *tried {
                None => true,
                Some(had_infs) => !had_infs && infs,
            };
            if go {
                *tried = Some(infs);
            }
            go
        };
        if should_try {
            if super::wasapi_mic::install_steam_audio_pair() {
                plan = audio_control::wire_now_full(true);
            }
            if !have_silent(&plan.wiring) {
                tracing::info!(
                    "no silent virtual sink for client-only audio — desktop audio will also play \
                     on the host (install Steam, whose Remote Play streaming drivers provide one)"
                );
            }
        }
    }
    let wiring = &plan.wiring;
    // Last-resort is Assert-only: Follow captures the operator default, and `judge_default`
    // never routes Follow onto Steam Speakers (`excluded_from_loopback`).
    let last_resort = assert_plan && wiring.loopback_last_resort;
    let plan_fp = plan.fingerprint;

    let en = DeviceEnumerator::new().context("DeviceEnumerator")?;
    // Echo guard: the plan reserves `mic_render` for the virtual mic. Capturing it streams
    // the client's voice back to them — fall back to the plan's loopback, or refuse.
    let (device, dev_name, dev_id) = if assert_plan {
        let Some(ep) = wiring.loopback_render.clone() else {
            // Typed: the plan is a pure function of the set, so wait on the fingerprint.
            return Err(PlanUnsatisfiable::from_plan(&plan).into());
        };
        let d = audio_control::open_endpoint(&ep)?;
        (d, ep.0, ep.1)
    } else {
        let (default, id) = default_render(&en)
            .context("default render endpoint (loopback needs a render device)")?;
        let default_is_mic = wiring
            .mic_render
            .as_ref()
            .is_some_and(|(_, mic_id)| *mic_id == id);
        if default_is_mic {
            let Some(lb) = wiring.loopback_render.clone() else {
                // Not [`PlanUnsatisfiable`]: Follow's inputs include the default, which the
                // operator can change without a topology change (esp. `PUNKTFUNK_KEEP_DEFAULT`).
                anyhow::bail!(
                    "the default render endpoint is reserved for the virtual mic (capturing it \
                     would echo the client's voice back) — {}",
                    wiring_plan::describe_no_loopback(&plan.renders, wiring)
                );
            };
            tracing::warn!(mic = %wiring.mic_render.as_ref().unwrap().0, loopback = %lb.0,
                "default render endpoint is the virtual-mic target — loopback-capturing the plan's \
                 endpoint instead");
            let d = audio_control::open_endpoint(&lb)?;
            (d, lb.0, lb.1)
        } else {
            let name = default.get_friendlyname().unwrap_or_default();
            (default, name, id)
        }
    };

    let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
    // Mix format is authoritative in shared mode. `AUTOCONVERTPCM` succeeds on an upward
    // request and returns interpolated samples — never pad; open at the engine rate.
    // Floor is [`SAMPLE_RATE`], not the engine: libopus takes 8/12/16/24/48 kHz only, so a
    // 44.1 kHz endpoint still opens at 48 kHz. `rate_hz > hz.max(SAMPLE_RATE)` is both rules.
    let engine = audio_client.get_mixformat().ok();
    let engine_hz = engine.as_ref().map(|f| f.get_samplespersec());
    let open_hz = match engine_hz {
        Some(hz) if hz > 0 && rate_hz > hz.max(SAMPLE_RATE) => {
            let settled = hz.max(SAMPLE_RATE);
            tracing::info!(
                device = %dev_name,
                engine_hz = hz,
                requested = rate_hz,
                opening_at = settled,
                "engine rate is below the requested capture rate — hi-res declined; opening at \
                 the engine rate rather than letting WASAPI autoconvert upsample it (set this \
                 endpoint's rate in Windows' device properties to raise it)"
            );
            settled
        }
        // Unreadable mix format: declining hi-res cannot cost a working 48 kHz session.
        None if rate_hz != SAMPLE_RATE => {
            tracing::info!(
                device = %dev_name,
                requested = rate_hz,
                "endpoint mix format unreadable — hi-res declined; opening at the legacy rate"
            );
            SAMPLE_RATE
        }
        _ => rate_hz,
    };
    // Before Initialize can fail — `sample_rate()` already has a decided answer.
    opened_rate.store(open_hz, Ordering::Relaxed);
    // Autoconvert matches the engine mix to this layout. `dwChannelMask` pins wire order
    // (FL FR FC LFE RL RR SL SR; 7.1 = 0x63F, not 0xFF). Loopback is implied by capturing a
    // RENDER device with `Direction::Capture` in shared mode.
    let mask = punktfunk_core::audio::wasapi_channel_mask(channels as u8);
    let desired = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        open_hz as usize,
        channels as usize,
        Some(mask),
    );
    // Do not pass `min_period`: shared-mode `Initialize` cannot change the engine period
    // (`hnsBufferDuration` sizes the buffer; the callback still fires at the default).
    // Lowering it needs `IAudioClient3::InitializeSharedAudioStream`, which `wasapi` does not wrap.
    let (default_period, min_period) = audio_client.get_device_period().context("device period")?;
    let stream_mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    };
    let used_period = default_period;
    audio_client
        .initialize_client(&desired, &Direction::Capture, &stream_mode)
        .context("initialize loopback client")?;
    let h_event = audio_client.set_get_eventhandle().context("event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("IAudioCaptureClient")?;
    audio_client
        .start_stream()
        .context("start loopback stream")?;
    if let Some(r) = ready.take() {
        let _ = r.send(Ok(()));
    }
    tracing::info!(device = %dev_name,
        follow = matches!(mode, TargetMode::Follow) || keep_default,
        last_resort,
        // Asked vs settled — they differ only when an upward request was declined.
        requested_hz = rate_hz,
        opened_hz = open_hz,
        // Endpoint mix format, not the request.
        engine_hz = engine.as_ref().map(|f| f.get_samplespersec()),
        engine_ch = engine.as_ref().map(|f| f.get_nchannels()),
        engine_bits = engine.as_ref().map(|f| f.get_bitspersample()),
        buffer_ms = used_period as f32 / 10_000.0,
        min_buffer_ms = min_period as f32 / 10_000.0,
        "audio loopback capturing");
    if let Some(why) = &wiring.loopback_narrowing {
        tracing::warn!(device = %dev_name,
            "capturing an endpoint that {why} — the stream cannot sound better than this source");
    }

    // Seed is the default right after open. If Assert's park did not stick, converge: follow a
    // capturable default, warn once on a dud. Only a later CHANGE of that id reacts, so a
    // permanently-denied default set cannot reopen-loop.
    let mut seen_default = default_render(&en).map(|(_, id)| id);
    if assert_plan {
        if let Some(d) = seen_default.as_deref() {
            if d != dev_id {
                match judge_default(wiring, d) {
                    DefaultKind::Capturable(name) => {
                        tracing::info!(default = %name, planned = %dev_name,
                            "could not park the default playback on the planned endpoint — \
                             capturing the actual default instead (audio audible on the host)");
                        return Ok(Next::Reopen(TargetMode::Follow));
                    }
                    DefaultKind::Dud(name) => tracing::warn!(default = %name, planned = %dev_name,
                        "default playback stayed on an endpoint whose loopback cannot work — \
                         capturing the planned endpoint; desktop audio may be silent"),
                    DefaultKind::Unknown => {}
                }
            }
        }
    }

    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut last_check = Instant::now();
    let mut last_fp_check = Instant::now();
    // 30 s with zero packets: a broken loopback looks like a quiet desktop. Info, not warn —
    // idle hosts are silent — except last-resort, where the plan already knew the tap is silent.
    let opened_at = Instant::now();
    let mut saw_packets = false;
    let mut silence_noted = false;
    // Periodic vitals. A stalled encode drops chunks with no other log line; the encoder
    // concatenates across the hole (click + permanent A/V offset).
    let mut stats = CaptureStats::default();
    let mut last_stats = Instant::now();
    // Gap source: this polling tap stops while the endpoint idles, so "time since last data"
    // would score every quiet as a hole. `DATA_DISCONTINUITY` plus `index` (next packet start
    // if nothing was lost) sizes missing audio in the device clock.
    let mut last_packet: Option<Instant> = None;
    let mut next_index: u64 = 0;
    let mut fight = FightDamper::new(Instant::now());
    loop {
        if stop.load(Ordering::Relaxed) {
            audio_client.stop_stream().ok();
            return Ok(Next::Stopped);
        }
        // Events fire only while audio renders; finite timeout keeps `stop` and the watchdog alive.
        let _ = h_event.wait_for_event(100);
        loop {
            match capture_client.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_n)) => {
                    saw_packets = true;
                    let before = bytes.len();
                    let info = capture_client
                        .read_from_device_to_deque(&mut bytes)
                        .context("read loopback")?;
                    let now = Instant::now();
                    // Before the stamp moves: discontinuity on the first packet after a quiet
                    // stretch is idle-resume, not a hole in anything that was playing.
                    let flowing =
                        last_packet.is_some_and(|t| now.duration_since(t) < LOOPBACK_IDLE_AFTER);
                    let frames = ((bytes.len() - before) / block_align) as u64;
                    if frames == 0 {
                        // Packet-ready then zero frames: a spinning tap looks like a quiet desktop.
                        stats.missed_dequeues += 1;
                    } else {
                        if info.flags.data_discontinuity && flowing {
                            let lost = info.index.saturating_sub(next_index);
                            stats.observe_gap(Duration::from_micros(
                                lost.saturating_mul(1_000_000) / open_hz.max(1) as u64,
                            ));
                        }
                        next_index = info.index.saturating_add(frames);
                        last_packet = Some(now);
                    }
                }
                Err(e) => return Err(anyhow!("get_next_packet_size: {e}")),
            }
        }
        if !saw_packets && !silence_noted && opened_at.elapsed() >= Duration::from_secs(30) {
            silence_noted = true;
            if last_resort {
                tracing::warn!(device = %dev_name,
                    "no audio captured in the first 30 s from the LAST-RESORT loopback — the \
                     Steam Streaming Speakers' loopback is known-silent, so desktop audio is \
                     most likely not reaching the client; attach any output device to give the \
                     plan a working endpoint (it re-plans on the change)");
            } else {
                tracing::info!(device = %dev_name,
                    "no audio captured in the first 30 s — fine if the host is quiet; if it \
                     should be playing audio, this endpoint's loopback may be broken (set \
                     PUNKTFUNK_HOST_AUDIO=1 to prefer real hardware)");
            }
        }
        let whole = (bytes.len() / block_align) * block_align;
        if whole > 0 {
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            let mut samples = Vec::with_capacity(whole / 4);
            for c in raw.chunks_exact(4) {
                samples.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            stats.observe(&samples, channels);
            // Lossy, non-blocking. Count only while a session is reading: a full channel under
            // a live consumer is encode lag (click + permanent shift). A parked capturer fills
            // once and then refuses everything ([`WasapiLoopbackCapturer::active`]).
            if tx.try_send(samples).is_err() && active.load(Ordering::Relaxed) {
                stats.dropped_chunks += 1;
            }
        }
        if last_stats.elapsed() >= STATS_EVERY {
            let (peak_db, rms_db, delivered_pct) = stats.summary(last_stats.elapsed(), open_hz);
            if stats.dropped_chunks > 0 {
                tracing::warn!(
                    device = %dev_name,
                    dropped_chunks = stats.dropped_chunks,
                    "the audio encode thread could not keep up — captured audio was DROPPED; the \
                     stream will click and everything after it shifts"
                );
            }
            tracing::info!(
                device = %dev_name,
                peak_db = format!("{peak_db:.1}"),
                rms_db = format!("{rms_db:.1}"),
                delivered_pct = format!("{delivered_pct:.0}"),
                // Shape of whatever `delivered_pct` is short by. From WASAPI discontinuity, not
                // callback cadence — an idle-then-resume endpoint is not a gap. See [`LOOPBACK_IDLE_AFTER`].
                gaps = stats.gaps,
                max_gap_ms = stats.max_gap_ms(),
                // Bucket counts (<20/50/100 ms, ≥100 ms) and total cost; same fields as Linux.
                gap_hist = %stats.gap_hist(),
                missing_ms = stats.missing_ms(),
                missed_dequeues = stats.missed_dequeues,
                dropped_chunks = stats.dropped_chunks,
                "desktop audio capture"
            );
            last_stats = Instant::now();
            stats = CaptureStats::default();
        }

        // Default render id changed — operator picked a different output mid-stream.
        if last_check.elapsed() >= DEFAULT_CHECK_EVERY {
            last_check = Instant::now();
            if let Some((_, nid)) = default_render(&en) {
                if seen_default.as_deref() != Some(nid.as_str()) {
                    seen_default = Some(nid.clone());
                    if nid != dev_id {
                        // Stop per-branch, not here: the Dud path keeps capturing, and a
                        // stop-first would make that keep-alive a no-op.
                        if keep_default {
                            audio_client.stop_stream().ok();
                            tracing::info!(
                                "default render device changed (PUNKTFUNK_KEEP_DEFAULT) — \
                                 following it"
                            );
                            return Ok(Next::Reopen(TargetMode::Follow));
                        }
                        match judge_default(wiring, &nid) {
                            DefaultKind::Capturable(name) => {
                                audio_client.stop_stream().ok();
                                tracing::info!(device = %name,
                                    "operator changed the output device mid-stream — following \
                                     it (audio now also plays on the host)");
                                return Ok(Next::Reopen(TargetMode::Follow));
                            }
                            // Assert binds capture to the plan's endpoint, not the default.
                            // Only where apps render has moved — put the default back, keep the
                            // stream. A full reopen is a dropout on every dud-default fight.
                            DefaultKind::Dud(name) => {
                                if !assert_plan {
                                    // Follow/KEEP_DEFAULT capture IS the default — reopen on Assert.
                                    audio_client.stop_stream().ok();
                                    return Ok(Next::Reopen(TargetMode::Assert));
                                }
                                fight.observed_at(Instant::now());
                                if fight.should_reassert() {
                                    audio_control::reassert_default_playback(&dev_id);
                                    // Next watchdog tick sees our endpoint and stays quiet.
                                    seen_default = Some(dev_id.clone());
                                    if fight.warn_now() {
                                        tracing::warn!(device = %name, planned = %dev_name,
                                            "something keeps moving the default playback to an \
                                             endpoint whose loopback cannot work — putting it \
                                             back (the capture is unaffected)");
                                    }
                                } else if fight.warn_giving_up() {
                                    tracing::warn!(device = %name, planned = %dev_name,
                                        backoff_s = FIGHT_BACKOFF.as_secs(),
                                        "another program is repeatedly taking the default \
                                         playback device — backing off rather than fighting it. \
                                         Desktop audio keeps streaming from the planned endpoint, \
                                         but apps rendering to the other device will not be heard");
                                }
                            }
                            DefaultKind::Unknown => {
                                audio_client.stop_stream().ok();
                                return Ok(Next::Reopen(TargetMode::Assert));
                            }
                        }
                    }
                }
            }
        }

        // Last-resort is a stopgap: any endpoint-set change may unlock a real plan. Preferred
        // endpoints don't watch this — mid-stream re-routing is the default-device watchdog.
        if last_resort && last_fp_check.elapsed() >= ENDPOINT_POLL_EVERY {
            last_fp_check = Instant::now();
            if audio_control::endpoint_fingerprint() != plan_fp {
                audio_client.stop_stream().ok();
                tracing::info!(
                    "endpoint set changed while capturing the last-resort loopback — re-planning"
                );
                return Ok(Next::Reopen(TargetMode::Assert));
            }
        }
    }
}

/// Watchdog verdict on a newly-observed default render endpoint.
enum DefaultKind {
    /// Following it yields working audio (audible on the host too).
    Capturable(String),
    /// Mic target, pad endpoint, or known-silent/echoing loopback. Following it is silence or echo.
    Dud(String),
    /// Enumeration miss (transient churn).
    Unknown,
}

/// Resolve via [`super::pad_endpoint::open_wasapi_device`], not `DeviceEnumerator::get_device`:
/// that handed `GetDevice` a freed string through 0.23, and a miss here silently downgrades a
/// capturable default to `Unknown`. Keep one resolution path — see the helper.
fn judge_default(wiring: &wiring_plan::Wiring, id: &str) -> DefaultKind {
    let Ok(dev) = super::pad_endpoint::open_wasapi_device(id) else {
        return DefaultKind::Unknown;
    };
    let name = dev.get_friendlyname().unwrap_or_default();
    let ln = name.to_lowercase();
    let is_mic = wiring
        .mic_render
        .as_ref()
        .is_some_and(|(_, mic_id)| mic_id == id);
    // Pad endpoints are stamped with the controller name so games treat them as the pad speaker;
    // `excluded_from_loopback` therefore passes them as Capturable. This classifier also drives
    // the watchdog and Follow, so a pad default would send the desktop mix to voice coils.
    // Identity, not name.
    let is_pad = super::pad_endpoint::is_pad_render_endpoint(id);
    if is_mic || is_pad || wiring_plan::excluded_from_loopback(&ln) {
        DefaultKind::Dud(name)
    } else {
        DefaultKind::Capturable(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live loopback round trip. Skipped unless `PUNKTFUNK_WASAPI_LIVE=1` and a render endpoint exists.
    #[test]
    fn live_open_and_read() {
        if std::env::var("PUNKTFUNK_WASAPI_LIVE").is_err() {
            return;
        }
        let mut cap = match WasapiLoopbackCapturer::open(2, SAMPLE_RATE) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no render endpoint on this box ({e:#}) — skipping");
                return;
            }
        };
        assert_eq!(cap.channels(), 2);
        // Legacy rate is never an upward request, so the settled rate must equal the ask.
        assert_eq!(cap.sample_rate(), SAMPLE_RATE);
        match cap.next_chunk() {
            Ok(samples) => assert!(
                samples.len() % 2 == 0,
                "interleaved stereo => even sample count"
            ),
            Err(e) => eprintln!("no audio within timeout (silent system?): {e:#}"),
        }
    }
}
