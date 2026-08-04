//! WASAPI loopback capture of the desktop mix (system output) — the Windows analogue of the
//! PipeWire sink-monitor backend. Delivers interleaved f32 PCM at 48 kHz in the requested
//! channel count (stereo / 5.1 / 7.1, canonical wire order FL FR FC LFE RL RR SL SR via the
//! explicit `dwChannelMask`), ready for the Opus path with NO resampling (WASAPI shared-mode
//! autoconvert does any SRC + up/downmix to the requested layout). WASAPI objects are
//! COM-apartment-bound and not `Send`, so they live on a dedicated thread (mirrors
//! `linux::PwAudioCapturer`); only the channel + stop flag + join handle are in the struct.
//!
//! **Which endpoint, and self-healing.** The capture thread opens the wiring plan's loopback
//! endpoint EXPLICITLY — never "whatever the default happens to be": binding to the default
//! raced the plan's own `IPolicyConfig` default change and captured duds when that change failed
//! or the operator's default sat on a silent endpoint ("CABLE In 16ch", Steam Streaming
//! Speakers) — the field-reported "no audio until I cycled output devices" failure. The plan
//! also parks the default playback device on the loopback endpoint (a silent sink by default —
//! client-only audio; see [`super::wiring_plan`]) so app streams migrate to it.
//!
//! The thread then self-heals for its whole life: a ~1 s watchdog notices the default render
//! device changing under us — the operator picked a different output mid-stream — and reacts:
//! a loopback-capturable choice is FOLLOWED (their explicit choice wins; audio then also plays
//! on the host), a known-dud choice (cable/Steam Speakers/the mic target) snaps back to the
//! plan. Device errors (endpoint invalidated, engine restart) reopen with a capped exponential
//! backoff that an endpoint-set change cuts short. A plan with NO loopback endpoint at all is
//! never retried: `wiring_plan::plan` is pure in the endpoint set, so that verdict holds until
//! the set changes — the thread says why once, then parks on a cheap fingerprint poll and
//! re-plans the instant the set moves (the 2026-08 field case hammered a full wiring pass —
//! IPolicyConfig writes included — every 2 s for 8+ minutes without ever being able to
//! succeed). On thread exit (capturer dropped at stream end) the parked default playback
//! device is restored.

use super::capture_policy::{CaptureStats, FightDamper, FIGHT_BACKOFF, STATS_EVERY};
use super::{audio_control, wiring_plan, AudioCapturer, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use wasapi::{Device, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

pub struct WasapiLoopbackCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WasapiLoopbackCapturer {
    pub fn open(channels: u32) -> Result<WasapiLoopbackCapturer> {
        anyhow::ensure!(
            matches!(channels, 2 | 6 | 8),
            "WASAPI loopback backend supports 2/6/8 channels (got {channels})"
        );
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        // Bring-up handshake: report open success/failure before returning, so a missing render
        // endpoint surfaces as Err (the native plane then keeps retrying the open with backoff)
        // rather than a silent dead thread.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let stop_t = stop.clone();
        let join = thread::Builder::new()
            .name("punktfunk-wasapi-audio".into())
            .spawn(move || {
                if let Err(e) = capture_thread(tx, stop_t, ready_tx, channels) {
                    tracing::error!(error = %format!("{e:#}"), "wasapi loopback thread failed");
                }
            })
            .context("spawn wasapi audio thread")?;
        // Generous handshake: the first open may auto-install the Steam Streaming pair (two
        // driver installs, ~5 s of settling each) before the endpoint exists.
        match ready_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(())) => {
                tracing::info!(channels, "WASAPI loopback capture: 48 kHz f32");
                Ok(WasapiLoopbackCapturer {
                    chunks: rx,
                    channels,
                    stop,
                    join: Some(join),
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // The thread outlived the handshake (stalled driver install / hung endpoint).
                // Tell it to stop — otherwise it would keep capturing detached for the process
                // lifetime WITH the playback default still parked (restore only runs on exit).
                stop.store(true, Ordering::SeqCst);
                Err(anyhow!("wasapi loopback init timed out"))
            }
        }
    }
}

impl Drop for WasapiLoopbackCapturer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl AudioCapturer for WasapiLoopbackCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // A quiet sink is NOT a failure — return an empty chunk so the caller keeps the capturer
            // alive. Only a dead capture thread is an Err (→ caller reopens). Matches the Linux path.
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("wasapi audio thread ended")),
        }
    }
    fn channels(&self) -> u32 {
        self.channels
    }
    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
    }
}

/// How one open chooses its capture endpoint.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetMode {
    /// Capture the wiring plan's loopback endpoint and park the default playback device on it
    /// (client-only audio when the plan found a silent sink). The initial and snap-back mode.
    Assert,
    /// Capture the CURRENT default render endpoint — the operator changed the default mid-stream
    /// to a capturable device and their choice wins (audio then also plays on the host). Also the
    /// resolution under `PUNKTFUNK_KEEP_DEFAULT`.
    Follow,
}

/// Why one open's inner loop ended.
enum Next {
    /// `stop` was set — the capturer is being dropped.
    Stopped,
    /// Reopen in the given mode (default-device change observed).
    Reopen(TargetMode),
}

/// Reopen backoff after a TRANSIENT capture failure: starts here and doubles per consecutive
/// failure up to [`REOPEN_BACKOFF_CAP`], resetting on success or on an endpoint-set change
/// (mirrors the mic pump's `PUMP_TUNING` shape). The predecessor was a FLAT 2 s retry whose
/// every attempt re-ran the full wiring pass, IPolicyConfig writes included — tolerable for a
/// genuinely transient error, an 8-minute hammer in the 2026-08 field case where the failure
/// was structural.
const REOPEN_BACKOFF_START: Duration = Duration::from_secs(2);
const REOPEN_BACKOFF_CAP: Duration = Duration::from_secs(60);
/// Endpoint-set poll cadence while waiting out a failure (both the transient backoff sleep and
/// the unsatisfiable-plan wait): one enumerate-and-hash per tick, nothing else. A fingerprint
/// change ends the wait immediately — a (re)arrived endpoint (the display coming back, plugged
/// headphones) is exactly the recovery moment — so recovery stays as fast as the old 2 s hammer
/// without its side effects.
const ENDPOINT_POLL_EVERY: Duration = Duration::from_secs(2);
/// Watchdog cadence for "did the default render device change under us?" checks.
const DEFAULT_CHECK_EVERY: Duration = Duration::from_secs(1);
/// Total attempts for the FIRST open before its failure surfaces through the `ready` handshake.
/// Session start is peak endpoint churn — the virtual-display attach and this module's own
/// IPolicyConfig default flips race the activate, which then fails transiently (0x80070002,
/// endpoint mid-re-registration) — so a couple of quick retries absorb it within the
/// handshake budget.
const FIRST_OPEN_ATTEMPTS: u32 = 3;
/// Pause between first-open attempts (endpoint churn settles in well under a second).
const FIRST_OPEN_RETRY_PAUSE: Duration = Duration::from_secs(1);

fn capture_thread(
    tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<()>>,
    channels: u32,
) -> Result<()> {
    // COM must be initialized on THIS thread (MTA), before any device call.
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Self-heal for the capturer's whole life: each `capture_once` is one endpoint open + inner
    // capture loop; it returns to reopen (default-device change) or errors (device invalidated,
    // engine restart). The FIRST open gets [`FIRST_OPEN_ATTEMPTS`] tries (session-start endpoint
    // churn — see the constant) before its failure surfaces as `open()`'s Err; the caller keeps
    // retrying the whole open with its own backoff after that, so a bad start delays audio
    // rather than ending it.
    let mut ready = Some(ready);
    let mut mode = TargetMode::Assert;
    let mut failures: u64 = 0;
    let mut first_attempts: u32 = 0;
    let mut backoff = REOPEN_BACKOFF_START;
    // Endpoint-set fingerprint under which an unsatisfiable plan was already error-logged: an
    // unchanged set means an unchanged verdict (`wiring_plan::plan` is pure), so the diagnosis
    // is said once per topology — the field log drowned in 256+ copies of the same line.
    let mut unsat_logged: Option<u64> = None;
    while !stop.load(Ordering::Relaxed) {
        match capture_once(&tx, &stop, &mut ready, channels, mode) {
            Ok(Next::Stopped) => break,
            Ok(Next::Reopen(m)) => {
                mode = m;
                failures = 0;
                backoff = REOPEN_BACKOFF_START;
                unsat_logged = None;
            }
            Err(e) if ready.is_some() => {
                // An unsatisfiable PLAN cannot improve within the handshake window — the
                // once-per-process Steam-pair install already ran inside `capture_once` — so
                // fail the open now with the full diagnosis instead of spending the transient
                // retry budget on a structural verdict. The native plane owns first-open
                // retries and backs off on its own.
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
                // Stop-responsive pause (same discipline as the reopen backoff below).
                let until = Instant::now() + FIRST_OPEN_RETRY_PAUSE;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
            Err(e) => {
                mode = TargetMode::Assert;
                if let Some(unsat) = e.downcast_ref::<PlanUnsatisfiable>() {
                    // Structural: retrying against the same endpoints repeats the same verdict,
                    // and every retry used to re-run the wiring pass — IPolicyConfig writes
                    // included, stomping any operator default-recording change within 2 s. Say
                    // why once per topology, then park on the cheap fingerprint poll; the set
                    // changing IS the recovery moment and re-plans immediately.
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
                    // Capped exponential backoff, cut short (and reset) the moment the
                    // endpoint set changes — a re-arrived device is the likeliest cure for
                    // whatever killed the capture, and it must not wait out a 60 s sleep.
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
    // Hand the default playback device back to the operator (no-op if we never parked it, or if
    // they changed it themselves mid-stream). COM is initialized on this thread.
    audio_control::restore_default_playback();
    Ok(())
}

/// A wiring plan with NO loopback endpoint, as a typed error: [`wiring_plan::plan`] is pure in
/// the enumerated endpoint set, so unlike every other capture error this one is PERMANENT until
/// the topology changes — retrying it is guaranteed futile (the 2026-08 field case retried it
/// flat-out for 8+ minutes, one full wiring pass per retry). Carries the set's fingerprint
/// (what the reopen loop waits on) and the full diagnosis: inventory, per-endpoint rejection
/// reasons, and only the remedies not already taken.
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

/// How a [`wait_endpoint_change`] ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointWait {
    /// `stop` was set — the capturer is being dropped.
    Stopped,
    /// The endpoint-set fingerprint moved — re-plan NOW (this is the recovery moment).
    Changed,
    /// The deadline passed without a change (backoff waits only; `deadline: None` never ends
    /// this way).
    Elapsed,
}

/// Stop-responsive wait that polls the endpoint-set fingerprint every [`ENDPOINT_POLL_EVERY`] —
/// an enumerate-and-hash, no wiring pass, no IPolicyConfig writes, no logs — until the set
/// changes, `deadline` passes, or `stop` is set. `deadline: None` waits indefinitely: used while
/// the plan is unsatisfiable, where ONLY a topology change can alter the verdict.
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

/// The current default render endpoint, with its id (`None` on any enumeration failure —
/// transient failures must not kill the capture).
fn default_render(en: &DeviceEnumerator) -> Option<(Device, String)> {
    let d = en.get_default_device(&Direction::Render).ok()?;
    let id = d.get_id().ok()?;
    Some((d, id))
}

/// One endpoint open + capture loop. Returns how to continue ([`Next`]) or an error (first open:
/// retried [`FIRST_OPEN_ATTEMPTS`] times, then fatal via the `ready` handshake; later: reopen
/// with capped backoff — or, for a typed [`PlanUnsatisfiable`], an endpoint-set wait).
fn capture_once(
    tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    ready: &mut Option<SyncSender<Result<()>>>,
    channels: u32,
    mode: TargetMode,
) -> Result<Next> {
    // Interleaved f32: channels * 4 bytes per frame.
    let block_align = channels as usize * 4;
    let keep_default = audio_control::keep_default_devices();
    // Assert-mode without KEEP_DEFAULT is the only shape that parks the playback default.
    let assert_plan = mode == TargetMode::Assert && !keep_default;
    let mut plan = audio_control::wire_now_full(assert_plan);

    // Client-only audio needs a silent-on-host sink with a working loopback (the Steam Streaming
    // Microphone's render side). If the plan had to settle for real hardware (or nothing), try —
    // once per process — to install the Steam pair (present when Steam is), then re-plan.
    if assert_plan && !audio_control::host_audio_requested() {
        let have_silent = |w: &wiring_plan::Wiring| {
            w.loopback_render
                .as_ref()
                .is_some_and(|(n, _)| wiring_plan::silent_sink(&n.to_lowercase()))
        };
        static INSTALL_TRIED: AtomicBool = AtomicBool::new(false);
        if !have_silent(&plan.wiring) && !INSTALL_TRIED.swap(true, Ordering::SeqCst) {
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
    // Only the Assert path can knowingly sit on the plan's LAST-RESORT endpoint: Follow captures
    // the operator's chosen default, and `judge_default` never routes Follow onto the Steam
    // Speakers (they are `excluded_from_loopback` — a Dud that snaps back to the plan).
    let last_resort = assert_plan && wiring.loopback_last_resort;
    let plan_fp = plan.fingerprint;

    let en = DeviceEnumerator::new().context("DeviceEnumerator")?;
    // Resolve the endpoint to capture. ECHO GUARD (Follow/KEEP_DEFAULT shapes): the wiring plan
    // reserves one endpoint for the virtual mic (`super::wasapi_mic` writes the client's voice
    // there) — capturing THAT endpoint would stream the client's own mic straight back to it, so
    // fall back to the plan's loopback endpoint, or refuse — no desktop audio beats an echo loop.
    let (device, dev_name, dev_id) = if assert_plan {
        let Some(ep) = wiring.loopback_render.clone() else {
            // Detected BEFORE any open attempt, and typed: the plan is a pure function of the
            // endpoint set, so this cannot resolve until the set changes — the reopen loop
            // waits on the fingerprint instead of retrying (the old untyped bail was retried
            // flat-out every 2 s, forever, in the 2026-08 field case).
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
                // Same inventory shape as the Assert bail, but NOT typed as unsatisfiable:
                // Follow's inputs include the DEFAULT device, which the operator can change
                // without a topology change (especially under PUNKTFUNK_KEEP_DEFAULT) — the
                // capped backoff must keep retrying rather than a fingerprint wait sleeping
                // through a default-only change.
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
    // 48 kHz f32 interleaved in the requested channel layout; autoconvert lets WASAPI's
    // shared-mode SRC match the engine mix format to ours (incl. up/downmix to the requested
    // channel count), so we never resample/remix in Rust. The explicit dwChannelMask pins the
    // wire order (FL FR FC LFE RL RR SL SR; 7.1 = 0x63F, not 0xFF). Loopback is implied by
    // capturing a RENDER device with Direction::Capture in shared mode (STREAMFLAGS_LOOPBACK).
    let mask = punktfunk_core::audio::wasapi_channel_mask(channels as u8);
    let desired = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        SAMPLE_RATE as usize,
        channels as usize,
        Some(mask),
    );
    // WP0.1 — the endpoint's ACTUAL engine mix format, read BEFORE we initialize. Everything the
    // old log printed ("48 kHz f32 channels=2") was our REQUEST; with `autoconvert` WASAPI
    // silently converts from whatever the endpoint really runs, so a voice-carrier endpoint
    // narrowing the desktop mix to mono or 24 kHz was invisible in a 3,600-line field log. This
    // line is what makes an audio-quality report triageable without a round trip.
    let engine = audio_client.get_mixformat().ok();
    // NB the plan's WP4.5 ("open the loopback at the MINIMUM device period, worth ~5–10 ms") is
    // deliberately NOT done here, because its premise is wrong: in shared mode
    // `IAudioClient::Initialize` cannot change the engine period at all — `hnsBufferDuration` sizes
    // the buffer, and the callback still fires at the engine's fixed default period. Lowering it
    // needs `IAudioClient3::InitializeSharedAudioStream`, which the `wasapi` crate does not wrap.
    // Passing `min_period` here would therefore be a no-op at best and a new Initialize failure
    // path at worst, on a device this tree cannot compile for, let alone test. Left as real work.
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
        // The endpoint's own format — NOT the one we asked for.
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

    // Watchdog seed: the default as it stands right after our open. In Assert mode the plan just
    // parked the default on our endpoint — if it did NOT stick (IPolicyConfig denied) converge
    // instead of churning: follow a capturable default (audio plays on both ends), warn once on a
    // dud. Afterwards only a CHANGE of the observed default id triggers a reaction, so a
    // permanently-denied default set can never reopen-loop.
    let mut seen_default = default_render(&en).map(|(_, id)| id);
    if assert_plan {
        if let Some(d) = seen_default.as_deref() {
            if d != dev_id {
                match judge_default(&en, wiring, d) {
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
    // Triage breadcrumb: a broken loopback (endpoint renders but its loopback tap delivers
    // nothing — the Steam Streaming Speakers failure shape) is indistinguishable from a simply
    // quiet desktop, so after 30 s with zero packets say so ONCE. Info, not warn — an idle host
    // is legitimately silent — EXCEPT on a last-resort endpoint, where the plan already knew
    // the loopback is silent and zero packets all but confirms the quality risk materialized.
    let opened_at = Instant::now();
    let mut saw_packets = false;
    let mut silence_noted = false;
    // WP0.2 — the audio plane's own vitals, logged periodically. Before this, a host log said
    // nothing whatsoever about audio between "capturing" and the session ending: no level, no
    // cadence, and in particular no sign of the SILENT, uncounted drop below, where a stalled
    // encode thread loses chunks and the encoder simply concatenates across the hole (a click,
    // and a permanent A/V offset, with nothing in any log).
    let mut stats = CaptureStats::default();
    let mut last_stats = Instant::now();
    // WP2.4 — damping for the default-playback tug-of-war.
    let mut fight = FightDamper::new(Instant::now());
    loop {
        if stop.load(Ordering::Relaxed) {
            audio_client.stop_stream().ok();
            return Ok(Next::Stopped);
        }
        // Loopback fires events only while audio renders; the finite timeout keeps `stop` (and
        // the watchdog) responsive.
        let _ = h_event.wait_for_event(100);
        loop {
            match capture_client.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_n)) => {
                    saw_packets = true;
                    capture_client
                        .read_from_device_to_deque(&mut bytes)
                        .context("read loopback")?;
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
            // Non-blocking, lossy — same discipline as PipeWire. Now COUNTED: a full channel
            // means the encode thread is not keeping up, and every dropped chunk is a click plus
            // a permanent shift of everything after it.
            if tx.try_send(samples).is_err() {
                stats.dropped_chunks += 1;
            }
        }
        if last_stats.elapsed() >= STATS_EVERY {
            let (peak_db, rms_db, delivered_pct) = stats.summary(last_stats.elapsed(), SAMPLE_RATE);
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
                dropped_chunks = stats.dropped_chunks,
                "desktop audio capture"
            );
            last_stats = Instant::now();
            stats = CaptureStats::default();
        }

        // Watchdog: react when the default render device CHANGES from what we last observed —
        // the operator picked a different output mid-stream (the old code never noticed and
        // captured the stale endpoint forever; "cycle your output devices" was the workaround).
        if last_check.elapsed() >= DEFAULT_CHECK_EVERY {
            last_check = Instant::now();
            if let Some((_, nid)) = default_render(&en) {
                if seen_default.as_deref() != Some(nid.as_str()) {
                    seen_default = Some(nid.clone());
                    if nid != dev_id {
                        // NB the stream is stopped per-branch below, NOT here: the WP2.4 Dud
                        // path deliberately keeps capturing, and stopping first would have made
                        // the "no teardown" fix silently useless.
                        if keep_default {
                            audio_client.stop_stream().ok();
                            tracing::info!(
                                "default render device changed (PUNKTFUNK_KEEP_DEFAULT) — \
                                 following it"
                            );
                            return Ok(Next::Reopen(TargetMode::Follow));
                        }
                        match judge_default(&en, wiring, &nid) {
                            DefaultKind::Capturable(name) => {
                                audio_client.stop_stream().ok();
                                tracing::info!(device = %name,
                                    "operator changed the output device mid-stream — following \
                                     it (audio now also plays on the host)");
                                return Ok(Next::Reopen(TargetMode::Follow));
                            }
                            // WP2.4 — a DUD default does not affect what we are capturing:
                            // Assert mode binds the capture to the plan's endpoint EXPLICITLY,
                            // not to whatever the default happens to be. Only where *apps*
                            // render has moved. So put the default back and KEEP THE STREAM —
                            // the old full reopen tore the capture down for nothing, and the
                            // 2026-08-03 field log shows what that cost: something re-set the
                            // default to CABLE Input every ~4 s and each round trip was a
                            // teardown, a re-plan with IPolicyConfig writes, and an audible
                            // dropout — seven of them in sixteen seconds, one ending in a 2 s
                            // error backoff.
                            DefaultKind::Dud(name) => {
                                if !assert_plan {
                                    // Follow/KEEP_DEFAULT shapes still need the old behaviour:
                                    // there the capture IS bound to the default.
                                    audio_client.stop_stream().ok();
                                    return Ok(Next::Reopen(TargetMode::Assert));
                                }
                                fight.observed_at(Instant::now());
                                if fight.should_reassert() {
                                    audio_control::reassert_default_playback(&dev_id);
                                    // Believe our own write: the next watchdog tick sees the
                                    // default back on our endpoint and stays quiet.
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

        // A LAST-RESORT capture is a stopgap, not a steady state: the plan chose the
        // known-silent Steam Speakers only because nothing better existed, so any endpoint-set
        // change — the display's audio endpoint re-arriving, headphones plugged in — may unlock
        // a real plan. Re-plan on the change; without this the session would ride the silent
        // loopback forever AFTER the real endpoint returned (the original field defect in a
        // quieter costume). Preferred endpoints don't get this watch: mid-stream re-routing
        // there is the default-device watchdog's job, on the operator's terms.
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

/// The watchdog's verdict on a newly-observed default render endpoint.
enum DefaultKind {
    /// Loopback-capturable — following it yields working audio (audible on the host too).
    Capturable(String),
    /// The mic target or a known-silent/echoing loopback (cable, Steam Streaming Speakers) —
    /// following it can only produce silence or an echo loop.
    Dud(String),
    /// Could not resolve the endpoint (transient churn).
    Unknown,
}

/// Resolves through [`super::pad_endpoint::open_wasapi_device`], NOT the `wasapi` crate's
/// `DeviceEnumerator::get_device` — that one hands `GetDevice` a freed string (see the helper's
/// docs), and a spurious miss here silently downgrades a capturable default to `Unknown`.
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
    if is_mic || wiring_plan::excluded_from_loopback(&ln) {
        DefaultKind::Dud(name)
    } else {
        DefaultKind::Capturable(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live loopback round trip — skipped unless `PUNKTFUNK_WASAPI_LIVE=1` and a render endpoint
    /// exists. Opens the capturer and pulls one chunk of interleaved f32.
    #[test]
    fn live_open_and_read() {
        if std::env::var("PUNKTFUNK_WASAPI_LIVE").is_err() {
            return;
        }
        let mut cap = match WasapiLoopbackCapturer::open(2) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no render endpoint on this box ({e:#}) — skipping");
                return;
            }
        };
        assert_eq!(cap.channels(), 2);
        match cap.next_chunk() {
            Ok(samples) => assert!(
                samples.len() % 2 == 0,
                "interleaved stereo => even sample count"
            ),
            Err(e) => eprintln!("no audio within timeout (silent system?): {e:#}"),
        }
    }
}
