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
//! plan. Device errors (endpoint invalidated, engine restart) reopen with backoff instead of
//! killing audio for the rest of the session. On thread exit (capturer dropped at stream end)
//! the parked default playback device is restored.

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

/// Backoff between self-heal reopen attempts after a capture failure.
const REOPEN_BACKOFF: Duration = Duration::from_secs(2);
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
    while !stop.load(Ordering::Relaxed) {
        match capture_once(&tx, &stop, &mut ready, channels, mode) {
            Ok(Next::Stopped) => break,
            Ok(Next::Reopen(m)) => {
                mode = m;
                failures = 0;
            }
            Err(e) if ready.is_some() => {
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
                failures += 1;
                if failures.is_power_of_two() {
                    tracing::warn!(error = %format!("{e:#}"), count = failures,
                        "audio loopback capture failed — reopening");
                }
                mode = TargetMode::Assert;
                // Backoff in stop-responsive slices.
                let until = Instant::now() + REOPEN_BACKOFF;
                while Instant::now() < until && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    // Hand the default playback device back to the operator (no-op if we never parked it, or if
    // they changed it themselves mid-stream). COM is initialized on this thread.
    audio_control::restore_default_playback();
    Ok(())
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
/// with backoff).
fn capture_once(
    tx: &SyncSender<Vec<f32>>,
    stop: &AtomicBool,
    ready: &mut Option<SyncSender<Result<()>>>,
    channels: u32,
    mode: TargetMode,
) -> Result<Next> {
    // Interleaved f32: channels * 4 bytes per frame.
    let block_align = channels as usize * 4;
    let keep_default = std::env::var_os("PUNKTFUNK_KEEP_DEFAULT").is_some();
    // Assert-mode without KEEP_DEFAULT is the only shape that parks the playback default.
    let assert_plan = mode == TargetMode::Assert && !keep_default;
    let mut wiring = audio_control::wire_now(assert_plan);

    // Client-only audio needs a silent-on-host sink with a working loopback (the Steam Streaming
    // Microphone's render side). If the plan had to settle for real hardware (or nothing), try —
    // once per process — to install the Steam pair (present when Steam is), then re-plan.
    if assert_plan && !audio_control::host_audio_requested() {
        let have_silent = wiring
            .loopback_render
            .as_ref()
            .is_some_and(|(n, _)| wiring_plan::silent_sink(&n.to_lowercase()));
        static INSTALL_TRIED: AtomicBool = AtomicBool::new(false);
        if !have_silent && !INSTALL_TRIED.swap(true, Ordering::SeqCst) {
            // SAFETY: `install_steam_audio_pair` is `unsafe` only because it `LoadLibraryExW`s
            // `newdev.dll` and calls `DiInstallDriverW` through a `transmute`d function pointer;
            // calling it imposes no extra precondition here (it takes no args and aliases
            // nothing). Its internal contract holds: the `DiInstall` type matches the documented
            // `BOOL DiInstallDriverW(HWND, PCWSTR, DWORD, PBOOL)` ABI, and it passes a
            // NUL-terminated UTF-16 INF path with null/zero optional args.
            if unsafe { super::wasapi_mic::install_steam_audio_pair() } {
                wiring = audio_control::wire_now(true);
            }
            if !wiring
                .loopback_render
                .as_ref()
                .is_some_and(|(n, _)| wiring_plan::silent_sink(&n.to_lowercase()))
            {
                tracing::info!(
                    "no silent virtual sink for client-only audio — desktop audio will also play \
                     on the host (install Steam, whose Remote Play streaming drivers provide one)"
                );
            }
        }
    }

    let en = DeviceEnumerator::new().context("DeviceEnumerator")?;
    // Resolve the endpoint to capture. ECHO GUARD (Follow/KEEP_DEFAULT shapes): the wiring plan
    // reserves one endpoint for the virtual mic (`super::wasapi_mic` writes the client's voice
    // there) — capturing THAT endpoint would stream the client's own mic straight back to it, so
    // fall back to the plan's loopback endpoint, or refuse — no desktop audio beats an echo loop.
    let (device, dev_name, dev_id) = if assert_plan {
        let Some(ep) = wiring.loopback_render.clone() else {
            anyhow::bail!(
                "no loopback-capturable render endpoint (every usable endpoint is reserved for \
                 the virtual mic or has a silent loopback) — attach an output device or install \
                 the Steam Streaming pair to get desktop audio"
            );
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
                anyhow::bail!(
                    "the only render endpoint is reserved for the virtual mic (capturing it would \
                     echo the client's voice back) — attach another output device or install the \
                     Steam Streaming pair to get desktop audio"
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
    let (default_period, _min_period) =
        audio_client.get_device_period().context("device period")?;
    let stream_mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    };
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
        "audio loopback capturing");

    // Watchdog seed: the default as it stands right after our open. In Assert mode the plan just
    // parked the default on our endpoint — if it did NOT stick (IPolicyConfig denied) converge
    // instead of churning: follow a capturable default (audio plays on both ends), warn once on a
    // dud. Afterwards only a CHANGE of the observed default id triggers a reaction, so a
    // permanently-denied default set can never reopen-loop.
    let mut seen_default = default_render(&en).map(|(_, id)| id);
    if assert_plan {
        if let Some(d) = seen_default.as_deref() {
            if d != dev_id {
                match judge_default(&en, &wiring, d) {
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
    // Triage breadcrumb: a broken loopback (endpoint renders but its loopback tap delivers
    // nothing — the Steam Streaming Speakers failure shape) is indistinguishable from a simply
    // quiet desktop, so after 30 s with zero packets say so ONCE. Info, not warn: an idle host
    // is legitimately silent.
    let opened_at = Instant::now();
    let mut saw_packets = false;
    let mut silence_noted = false;
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
            tracing::info!(device = %dev_name,
                "no audio captured in the first 30 s — fine if the host is quiet; if it should \
                 be playing audio, this endpoint's loopback may be broken (set \
                 PUNKTFUNK_HOST_AUDIO=1 to prefer real hardware)");
        }
        let whole = (bytes.len() / block_align) * block_align;
        if whole > 0 {
            let raw: Vec<u8> = bytes.drain(..whole).collect();
            let mut samples = Vec::with_capacity(whole / 4);
            for c in raw.chunks_exact(4) {
                samples.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
            let _ = tx.try_send(samples); // non-blocking, lossy — same discipline as PipeWire
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
                        audio_client.stop_stream().ok();
                        if keep_default {
                            tracing::info!(
                                "default render device changed (PUNKTFUNK_KEEP_DEFAULT) — \
                                 following it"
                            );
                            return Ok(Next::Reopen(TargetMode::Follow));
                        }
                        return Ok(match judge_default(&en, &wiring, &nid) {
                            DefaultKind::Capturable(name) => {
                                tracing::info!(device = %name,
                                    "operator changed the output device mid-stream — following \
                                     it (audio now also plays on the host)");
                                Next::Reopen(TargetMode::Follow)
                            }
                            DefaultKind::Dud(name) => {
                                tracing::warn!(device = %name,
                                    "default playback moved to an endpoint whose loopback cannot \
                                     work — re-asserting the audio wiring plan");
                                Next::Reopen(TargetMode::Assert)
                            }
                            DefaultKind::Unknown => Next::Reopen(TargetMode::Assert),
                        });
                    }
                }
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

fn judge_default(en: &DeviceEnumerator, wiring: &wiring_plan::Wiring, id: &str) -> DefaultKind {
    let Ok(dev) = en.get_device(id) else {
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
