//! Audio: playback (decoded PCM → a WASAPI shared-mode render stream) and the microphone
//! uplink (WASAPI capture → Opus → 0xCB datagrams, the inverse of the host's virtual mic).
//!
//! The WASAPI twin of `audio.rs` (PipeWire) — same public surface (`AudioPlayer::spawn`/
//! `take_buffer`/`push`, `MicStreamer::spawn`), swapped in by lib.rs's `#[path]` so the
//! session pump compiles against one `crate::audio` on both OSes. It began as a copy of the
//! WinUI shell's own audio path; that shell's built-in streaming path has since been deleted,
//! so this is now the only WASAPI client ring.
//!
//! Playback: the session pump pushes one decoded frame per network arrival; the WASAPI render
//! thread pulls whole event-driven quanta on the device clock. The depth policy between them is
//! the SHARED `punktfunk_core::audio::JitterPolicy` (`JitterTuning::WASAPI`) — target in
//! milliseconds, crossfaded drift correction, de-prime hysteresis — so all four clients behave
//! the same way and none of them can ratchet latency upward.
//!
//! The endpoint is opened at the format the session NEGOTIATED ([`PlaybackFormat`]), not at a
//! constant: 48 kHz Opus frames of 5 ms on the `0xC9` plane, or 48/96 kHz lossless PCM frames of
//! 1–5 ms on `0xD3` (`design/hi-res-audio.md`). ⚠ Shared-mode `autoconvert` means an over-rate
//! stream is DOWNSAMPLED on arrival with no error — see [`can_render_at`], which is what keeps
//! the capability advertisement honest, and the engine-rate reading in the render thread.
//!
//! WASAPI objects are COM-apartment-bound and not `Send`, so they live on a dedicated
//! thread (the same discipline as the host's `wasapi_cap`); only the channels + stop flag
//! + join handle cross the boundary.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;
use wasapi::{
    AudioClientProperties, DeviceEnumerator, Direction, SampleType, StreamCategory, StreamMode,
    WaveFormat,
};

/// The protocol's default rate — and, now that render takes its rate from the `Welcome`
/// ([`PlaybackFormat`]), the MIC uplink's rate and nothing else. Voice is Opus, and libopus is
/// 48 kHz by construction, so the uplink has no reason to move and no way to.
const SAMPLE_RATE: usize = 48_000;
/// Mic capture requests STEREO from WASAPI (autoconvert matrixes any endpoint layout down to
/// it — the proven path; `read_from_device_to_deque` then delivers our requested format) and
/// downmixes to MONO in code before the encoder: voice is mono at the source, the host accepts
/// any Opus channel layout (its stereo decoder upmixes), and half the samples halve the
/// encode + wire cost. The render path is multichannel — its channel count + block align are
/// runtime, driven by the host-resolved layout.
const CAPT_CHANNELS: usize = 2;
/// Mic frames are 10 ms (480 mono samples) — any size ≤ 120 ms is fine host-side; 10 ms
/// halves the frame-fill share of mouth-to-ear latency vs the old 20 ms.
const MIC_FRAME: usize = 480;

/// This backend's de-jitter tuning. Named once so the decode thread can read the same numbers the
/// render loop runs on — its drought concealment is bounded by this preset's de-prime fuse, and
/// the two drifting apart is exactly how one platform quietly ends up with a third of another's
/// slack.
pub(crate) const TUNING: punktfunk_core::audio::JitterTuning =
    punktfunk_core::audio::JitterTuning::WASAPI;

/// A selectable WASAPI endpoint for the settings pickers.
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// The `IMMDevice` endpoint id (`{0.0.0.00000000}.{…}`) — the stable key the render and
    /// capture threads resolve via [`DeviceEnumerator::get_device`]. (The PipeWire twin
    /// stores `node.name` here; both are "the stable key", so the Settings fields and env
    /// contract stay OS-agnostic.)
    pub name: String,
    /// The endpoint's friendly name ("Speakers (Realtek …)") — what the picker shows.
    pub description: String,
}

/// Enumerate active audio endpoints: `(sinks, sources)` — the WASAPI twin of the PipeWire
/// probe (same tuple shape; no devices → the caller simply shows no pickers). Runs on its
/// own short-lived MTA thread: the caller is typically a UI thread whose COM apartment is
/// STA, where a direct `CoInitializeEx(MTA)` would fail with `RPC_E_CHANGED_MODE`.
pub fn devices() -> Result<(Vec<AudioDevice>, Vec<AudioDevice>)> {
    std::thread::Builder::new()
        .name("pf-audio-enum".into())
        .spawn(|| -> Result<(Vec<AudioDevice>, Vec<AudioDevice>)> {
            wasapi::initialize_mta()
                .ok()
                .context("CoInitializeEx (MTA)")?;
            let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
            let mut out = (Vec::new(), Vec::new());
            for (direction, list) in [
                (Direction::Render, &mut out.0),
                (Direction::Capture, &mut out.1),
            ] {
                let coll = enumerator
                    .get_device_collection(&direction)
                    .context("device collection")?;
                for i in 0..coll.get_nbr_devices().context("device count")? {
                    // One broken endpoint (driver limbo) must not hide the rest.
                    let Ok(dev) = coll.get_device_at_index(i) else {
                        continue;
                    };
                    let (Ok(id), Ok(name)) = (dev.get_id(), dev.get_friendlyname()) else {
                        continue;
                    };
                    list.push(AudioDevice {
                        name: id,
                        description: name,
                    });
                }
            }
            Ok(out)
        })
        .context("spawn audio enumeration thread")?
        .join()
        .map_err(|_| anyhow!("audio enumeration thread panicked"))?
}

/// The endpoint an env pick names (`PUNKTFUNK_AUDIO_SINK`/`SOURCE` — endpoint ids, the
/// Settings device pickers via session main), or the OS default. A picked device that's
/// gone (unplugged USB DAC, remote session) falls back to the default with a warning —
/// audio keeps working, like the PipeWire twin's `target.object` behavior.
/// Resolve an active endpoint by id WITHOUT `DeviceEnumerator::get_device`.
///
/// Through `wasapi 0.23` that helper built its argument as
/// `PCWSTR::from_raw(HSTRING::from(id).as_ptr())` — the `HSTRING` was a temporary, dropped at the
/// end of that statement, so `GetDevice` read freed memory and missed ids that are perfectly valid.
/// `wasapi 0.24` fixed that upstream. Scanning the active collection touches only safe crate APIs,
/// so it cannot regress the same way, and it additionally filters to ACTIVE endpoints — which is
/// why it stays. (`punktfunk-host` routes around the same bug with raw COM instead; this crate
/// cannot, because it pins a different `windows` revision than `wasapi` does, making the two
/// `IMMDevice` types incompatible.)
pub(crate) fn device_by_id(
    enumerator: &DeviceEnumerator,
    direction: &Direction,
    id: &str,
) -> Result<wasapi::Device> {
    let devices = enumerator
        .get_device_collection(direction)
        .map_err(|e| anyhow!("enumerate {direction:?} endpoints: {e}"))?;
    let count = devices
        .get_nbr_devices()
        .map_err(|e| anyhow!("endpoint count: {e}"))?;
    for i in 0..count {
        let dev = devices
            .get_device_at_index(i)
            .map_err(|e| anyhow!("endpoint {i}: {e}"))?;
        if dev.get_id().is_ok_and(|got| got == id) {
            return Ok(dev);
        }
    }
    anyhow::bail!("no active {direction:?} endpoint with id {id}")
}

fn pick_device(
    enumerator: &DeviceEnumerator,
    direction: &Direction,
    var: &str,
) -> Result<wasapi::Device> {
    if let Some(id) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
        match device_by_id(enumerator, direction, &id) {
            Ok(d) => {
                tracing::info!(
                    var,
                    endpoint = %d.get_friendlyname().unwrap_or_else(|_| id.clone()),
                    "using the picked audio endpoint"
                );
                return Ok(d);
            }
            Err(e) => tracing::warn!(
                var,
                endpoint_id = %id,
                error = %e,
                "picked audio endpoint not found — using the default"
            ),
        }
    }
    enumerator
        .get_default_device(direction)
        .context("default endpoint")
}

/// The playback format a session RESOLVED, straight off the `Welcome` — never what the client
/// asked for. Passed as one value rather than three positional `u32`s because all three are `u32`
/// and transposing them would open the endpoint at a plausible-looking wrong format.
///
/// (Declared in both audio backends rather than shared: `audio.rs` and `audio_wasapi.rs` are twins
/// by design — same public surface, picked by `lib.rs`'s `#[path]` — and every other item on that
/// surface is already spelled out in each.)
#[derive(Clone, Copy, Debug)]
pub struct PlaybackFormat {
    /// Interleaved channel count (2/6/8), canonical wire order FL FR FC LFE RL RR SL SR.
    pub channels: u32,
    /// The negotiated sample rate: 48 000 on every Opus session, 48 000 or 96 000 on a lossless
    /// one (`design/hi-res-audio.md` §3 — 44.1 kHz and its multiples are deferred, because they
    /// truncate `JitterPolicy`'s integer samples-per-millisecond arithmetic).
    pub rate_hz: u32,
    /// One protocol frame in microseconds: 5 000 on the Opus plane, and whatever the lossless
    /// plane negotiated from the path MTU (§4.2 — 4 ms at 48/24, 2 ms at 96/24 by default). It
    /// feeds the policy's shed/floor arithmetic, which is denominated in frames.
    pub frame_us: u32,
}

/// The render endpoint's own engine rate, or `None` when nothing readable answered.
///
/// ⚠ **This is the client-side twin of the capture trap in `design/hi-res-audio.md` §4.3, and it
/// is the reason this function exists at all.** The render client below initialises with
/// `autoconvert: true`, and in shared mode the ENGINE's mix format is authoritative: autoconvert
/// exists to reconcile our format with the engine's, in whichever direction is needed. So handing
/// a 48 kHz engine a 96 kHz stream does not fail — it succeeds, returns no error, and plays
/// interpolated-back-down samples, while the session spends 3–4 Mbps carrying detail that is
/// discarded on arrival. Both ends would audit clean and the content would be wrong, which is
/// exactly the shape of bug this project has been burned by before.
///
/// Runs on a short-lived MTA thread, like [`devices`]: the caller is the session pump, whose COM
/// apartment is not ours to claim for the rest of the process.
fn render_engine_rate_hz() -> Option<u32> {
    std::thread::Builder::new()
        .name("pf-audio-engine".into())
        .spawn(|| -> Option<u32> {
            if wasapi::initialize_mta().ok().is_err() {
                return None;
            }
            let enumerator = DeviceEnumerator::new().ok()?;
            // The endpoint the render thread WILL pick, not the default — a picked USB DAC and
            // the system default routinely run at different rates.
            let device =
                pick_device(&enumerator, &Direction::Render, "PUNKTFUNK_AUDIO_SINK").ok()?;
            let client = device.get_iaudioclient().ok()?;
            client.get_mixformat().ok().map(|f| f.get_samplespersec())
        })
        .ok()?
        .join()
        .ok()?
}

/// Can this client render a `rate_hz` stream? — the gate on advertising `CLIENT_CAP_AUDIO_HIRES`,
/// which means *capable **and** the user turned it on* (`design/hi-res-audio.md` §7). A client
/// that advertised it without being able to render it would spend bandwidth off the top of a link
/// ABR can neither see nor reclaim, to play interpolation.
///
/// Answered from the endpoint's own mix format — never assumed, never padded. An engine below
/// the asked-for rate, or an endpoint that will not say what it runs at, both DECLINE: refusing
/// here costs a hi-res session and can never cost a working 48 kHz one, which is the same trade
/// the host makes at the other end (§8.2). The operator's lever is Windows' own device
/// properties — set the endpoint's rate there and this sees it. Driving the engine format from the
/// client would fight the OS and every other application on the box.
///
/// BLOCKS on COM while it asks (the [`devices`] discipline — a few ms against a healthy audio
/// service), and runs on the connect path, so the early return below matters: only a request
/// ABOVE the legacy rate ever touches the endpoint. Every ordinary session, and every 48 kHz
/// lossless one, answers without opening anything.
pub fn can_render_at(rate_hz: u32) -> bool {
    if rate_hz <= SAMPLE_RATE as u32 {
        return true; // the baseline claim every session already makes
    }
    match render_engine_rate_hz() {
        Some(hz) if hz >= rate_hz => true,
        Some(hz) => {
            tracing::warn!(
                engine_hz = hz,
                requested = rate_hz,
                "the render endpoint's audio engine runs below the requested rate — not asking \
                 for lossless audio, because WASAPI's shared-mode autoconvert would downsample it \
                 on arrival and the bandwidth would buy nothing (raise the rate in Windows' Sound \
                 → Device properties → Advanced to change this)"
            );
            false
        }
        None => {
            tracing::warn!(
                requested = rate_hz,
                "the render endpoint would not report its engine mix format — not asking for \
                 lossless audio, because there is no way to tell whether it would be downsampled \
                 on arrival"
            );
            false
        }
    }
}

pub struct AudioPlayer {
    pcm_tx: SyncSender<Vec<f32>>,
    /// Drained chunk Vecs coming back from the render thread for reuse (the pool half of
    /// the pcm channel — see [`AudioPlayer::take_buffer`]).
    recycle_rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// A/V sync hand-off with the render thread: it publishes the ring depth, the decode thread
    /// posts the depth the sync loop wants. See [`punktfunk_core::audio::AudioSyncCell`].
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    /// The render loop's vitals, logged by the decode thread — the same surface the PipeWire
    /// twin exposes, so `session.rs` prints one line shape on both platforms
    /// (see [`crate::audio_vitals`]).
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
}

impl AudioPlayer {
    /// Spawn the WASAPI render thread at the session's RESOLVED format. Failure (no render
    /// endpoint on this box) is survivable — the caller streams video-only.
    pub fn spawn(fmt: PlaybackFormat) -> Result<AudioPlayer> {
        // 64 queued chunks of slack between the pump and the WASAPI loop — 320 ms at the Opus
        // plane's 5 ms frame, proportionally less on a lossless session's shorter one (128 ms at
        // 2 ms), which is still far above anything the de-jitter policy targets. Left as a chunk
        // COUNT rather than scaled to the negotiated frame, matching core's own `AUDIO_QUEUE`,
        // whose comment records the same trade.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        // Return path: the render thread sends each drained Vec back for reuse, so
        // steady-state playback stops allocating (~200 chunks/s otherwise). Same capacity
        // as the data channel; a full pool just drops the Vec (plain deallocation).
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        // The engine rate the render thread read, so the line below reports what this stream is
        // actually up against rather than a constant. `None` = the endpoint said nothing.
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<Option<u32>>>(1);
        let stop_t = stop.clone();
        let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
        let sync_t = sync.clone();
        let vitals: Arc<crate::audio_vitals::PlaybackVitals> = Arc::default();
        let vitals_t = vitals.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-audio".into())
            .spawn(move || {
                if let Err(e) =
                    render_thread(pcm_rx, recycle_tx, stop_t, ready_tx, fmt, sync_t, vitals_t)
                {
                    tracing::warn!(error = %format!("{e:#}"), "audio playback thread ended");
                }
            })
            .context("spawn audio thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(engine_hz)) => {
                // Default endpoint unless PUNKTFUNK_AUDIO_SINK picked one (logged there). Every
                // number here is the one this stream really opened with — the line used to read
                // "48 kHz f32" from a constant, which on a 96 kHz session would have been the
                // label-right/content-wrong shape the whole hi-res design is written against.
                tracing::info!(
                    channels = fmt.channels,
                    rate_hz = fmt.rate_hz,
                    frame_us = fmt.frame_us,
                    engine_hz,
                    "WASAPI render: 32-bit float"
                );
                Ok(AudioPlayer {
                    pcm_tx,
                    recycle_rx,
                    stop,
                    thread: Some(thread),
                    sync,
                    vitals,
                })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!(
                "wasapi render init timed out (no render endpoint?)"
            )),
        }
    }

    /// A recycled chunk Vec from the pool, empty but with its capacity intact — fill it
    /// and hand it back through [`push`](Self::push). Allocates only when the pool is dry
    /// (startup, or after the WASAPI side dropped chunks).
    pub fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    /// The A/V sync hand-off cell — the decode thread reads the ring depth from it and posts the
    /// depth the sync loop wants back through it.
    pub fn sync_cell(&self) -> Arc<punktfunk_core::audio::AudioSyncCell> {
        self.sync.clone()
    }

    /// The render loop's vitals — the decode thread logs them (see [`crate::audio_vitals`]).
    pub fn vitals(&self) -> Arc<crate::audio_vitals::PlaybackVitals> {
        self.vitals.clone()
    }

    /// Queue one interleaved f32 chunk (in the session's channel layout). Drops the chunk if the
    /// WASAPI side is wedged (the renderer conceals the gap; never block the session pump).
    pub fn push(&self, pcm: Vec<f32>) {
        if let Err(TrySendError::Disconnected(_)) = self.pcm_tx.try_send(pcm) {
            // Thread already dead — Drop will reap it; nothing to do per-chunk.
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn render_thread(
    pcm_rx: Receiver<Vec<f32>>,
    recycle_tx: SyncSender<Vec<f32>>,
    stop: Arc<AtomicBool>,
    ready: SyncSender<Result<Option<u32>>>,
    fmt: PlaybackFormat,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
    vitals: Arc<crate::audio_vitals::PlaybackVitals>,
) -> Result<()> {
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    // Event-driven at the endpoint's period on a plain thread until now: MMCSS "Pro Audio" +
    // THREAD_PRIORITY_HIGHEST, what every audio engine on the platform gives its render loop.
    // A missed period here is a click the ring cannot help with. Best-effort (`audio_rt`).
    crate::audio_rt::boost_and_log("wasapi-render");
    let res = (|| -> Result<Option<u32>> {
        let channels = fmt.channels.clamp(1, 8) as u8;
        // 32-bit float interleaved: channels × 4 bytes/sample, at EVERY rate and depth this client
        // plays — deliberately, and not an oversight left behind by the lossless plane. Core
        // decodes 16- and 24-bit PCM to f32 (`pcm::to_f32`) precisely so one render format serves
        // both planes; asking WASAPI for a 24-bit integer format instead would rewrite this whole
        // loop (block align, the ring, the crossfade helper, the policy's sample arithmetic) to
        // deliver bits that are already exact in the f32 they arrived in. Stereo is byte-identical
        // to the old fixed path (mask 0x3, block align 8).
        let block_align = channels as usize * 4;
        let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
        let device = pick_device(&enumerator, &Direction::Render, "PUNKTFUNK_AUDIO_SINK")
            .context("render endpoint")?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // The endpoint's ACTUAL engine mix format, read BEFORE we initialise — the client-side
        // twin of the capture reading in `design/hi-res-audio.md` §4.3/§8.2, and the one §9 asks
        // for by name. `autoconvert` below makes an over-rate stream succeed silently, so without
        // this line the session could carry 96 kHz, log 96 kHz, spend the bandwidth, and render
        // 48 kHz interpolation with nothing above 24 kHz in it.
        //
        // This is a REPORT, not a gate: by the time this thread runs the wire format is already
        // negotiated and the session is streaming, so declining would only mean silence. The gate
        // is `can_render_at`, which runs BEFORE the connect and is what keeps the capability bit
        // honest; reaching a mismatch here means the endpoint changed under us (a picked device
        // unplugged, a shared-mode rate changed mid-session), which is worth a loud line.
        let engine_hz = audio_client
            .get_mixformat()
            .ok()
            .map(|f| f.get_samplespersec())
            .filter(|&hz| hz > 0);
        if let Some(hz) = engine_hz {
            if hz < fmt.rate_hz {
                tracing::warn!(
                    engine_hz = hz,
                    stream_hz = fmt.rate_hz,
                    endpoint = %device.get_friendlyname().unwrap_or_default(),
                    "the render endpoint's audio engine runs BELOW this session's negotiated \
                     rate — WASAPI's shared-mode autoconvert is downsampling every frame on \
                     arrival, so the extra bandwidth is being spent for nothing (raise the rate \
                     in Windows' Sound → Device properties → Advanced, then reconnect)"
                );
            }
        } else if fmt.rate_hz != SAMPLE_RATE as u32 {
            tracing::warn!(
                stream_hz = fmt.rate_hz,
                "the render endpoint would not report its engine mix format — there is no way to \
                 tell whether this session's audio is being downsampled on arrival"
            );
        }
        // The explicit dwChannelMask is the wire order (FL FR FC LFE RL RR SL SR); 5.1 = 0x3F,
        // 7.1 = 0x63F. WASAPI delivers channels in ascending mask-bit order, which equals the wire
        // order, so the render mapping is the identity — no permute. `autoconvert` (below) lets the
        // audio engine downmix when the endpoint has fewer speakers.
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            fmt.rate_hz as usize,
            channels as usize,
            Some(punktfunk_core::audio::wasapi_channel_mask(channels)),
        );
        let (default_period, _min_period) =
            audio_client.get_device_period().context("device period")?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: default_period,
        };
        audio_client
            .initialize_client(&desired, &Direction::Render, &mode)
            .context("initialize render client")?;
        let h_event = audio_client.set_get_eventhandle().context("event handle")?;
        let render_client = audio_client
            .get_audiorenderclient()
            .context("IAudioRenderClient")?;
        audio_client.start_stream().context("start render stream")?;
        let _ = ready.send(Ok(engine_hz));

        // De-jitter ring, in interleaved f32 SAMPLES (it used to be raw bytes, which made the
        // depth arithmetic byte-vs-sample and kept it from sharing the policy and the crossfade
        // helper with the other three clients).
        let mut ring: VecDeque<f32> = VecDeque::new();
        // Shared ms-denominated policy: prime depth, crossfaded drift correction so latency
        // returns to target instead of ratcheting, and de-prime hysteresis — the last replacing
        // the old `if ring.is_empty()`, where a single transient drain manufactured a whole
        // target's worth of fresh silence.
        //
        // Both at the RESOLVED format: `new_at_rate` denominates every depth/target/shed figure —
        // and the `buffer_ms`/`target_ms` this client reports — in the right samples-per-
        // millisecond, and `set_frame_us` tells the two frame-denominated decisions (the floor
        // under the effective target, and the one-frame smooth shed) how long a frame is here.
        // Left at the defaults, a 96 kHz session would shed 2.5 frames at a time and crossfade
        // across a whole one.
        let mut policy =
            punktfunk_core::audio::JitterPolicy::new_at_rate(TUNING, channels, fmt.rate_hz);
        policy.set_frame_us(fmt.frame_us);
        let mut out = Vec::new(); // per-quantum scratch, reused across iterations

        while !stop.load(Ordering::Relaxed) {
            if h_event.wait_for_event(100).is_err() {
                continue;
            }
            // Drain everything the pump has queued into the ring, returning each drained
            // Vec to the pool (a full/closed pool drops it).
            while let Ok(mut chunk) = pcm_rx.try_recv() {
                ring.extend(chunk.iter().copied());
                chunk.clear();
                let _ = recycle_tx.try_send(chunk);
            }
            let avail_frames = audio_client
                .get_available_space_in_frames()
                .context("available space")? as usize;
            if avail_frames == 0 {
                continue;
            }
            let want = avail_frames * channels as usize;
            // Once per stream: the engine's period as we first see it — same field meanings as
            // the PipeWire twin's line, printed by the decode thread.
            if !vitals.quantum_known() {
                vitals.note_quantum(
                    avail_frames as u32,
                    avail_frames as u32,
                    avail_frames as u32,
                );
            }

            // A/V sync: same contract as the PipeWire ring — take the decode thread's request,
            // publish where the ring actually is. The policy clamps the request against its own
            // underrun floor, so continuity always outranks alignment.
            policy.set_sync_target(sync.target());
            sync.publish_depth(ring.len());

            let step = policy.step(ring.len(), want);
            if step.drop_front > 0 {
                punktfunk_core::audio::crossfade_drop(&mut ring, step.drop_front, step.crossfade);
            }
            // The mirror: the sync loop asked for a DEEPER ring, answered with one duplicated,
            // crossfaded frame instead of a de-prime (see `JitterStep::insert_front`).
            if step.insert_front > 0 {
                punktfunk_core::audio::crossfade_insert(
                    &mut ring,
                    step.insert_front,
                    step.crossfade,
                );
            }

            out.clear();
            out.resize(avail_frames * block_align, 0);
            let mut ran_short = false;
            if !step.silence {
                // `out` is exactly `want` f32s wide (avail_frames × channels × 4 bytes).
                for dst in out.chunks_exact_mut(4) {
                    let s = ring.pop_front().unwrap_or_else(|| {
                        ran_short = true;
                        0.0
                    });
                    dst.copy_from_slice(&s.to_le_bytes());
                }
            }
            // No-op while un-primed (the policy ignores it), so a deliberate priming silence is
            // never miscounted as an underrun.
            policy.note_read(ran_short);
            // The 10 s `audio playback` line is printed by the decode thread from these.
            vitals.note_callback(
                ran_short,
                step.drop_front > 0,
                step.insert_front > 0,
                policy.avg_depth_ms(),
                policy.target_ms(),
            );
            render_client
                .write_to_device(avail_frames, &out, None)
                .context("write_to_device")?;
        }
        audio_client.stop_stream().ok();
        Ok(engine_hz)
    })();
    if let Err(ref e) = res {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    res.map(|_| ())
}

/// The microphone uplink: capture the default input device, Opus-encode 10 ms mono chunks,
/// ship them as 0xCB datagrams into the host's virtual mic source.
pub struct MicStreamer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicStreamer {
    /// `muted` is the in-stream mute (B4), shared live with the capture loop: set, the loop
    /// keeps reading the endpoint and discarding whole frames but sends nothing. Muting by
    /// STOPPING the client was rejected — an `IAudioClient` stop/start re-primes the endpoint
    /// buffers and re-runs the category negotiation below on every unmute.
    ///
    /// `echo_cancel` is the Settings toggle; `PUNKTFUNK_NO_AEC=1` overrides it off.
    pub fn spawn(
        connector: Arc<NativeClient>,
        muted: Arc<AtomicBool>,
        echo_cancel: bool,
    ) -> Result<MicStreamer> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-mic".into())
            .spawn(move || {
                if let Err(e) = mic_thread(&connector, stop_t, muted, echo_cancel) {
                    tracing::warn!(error = %format!("{e:#}"), "mic uplink thread ended");
                }
            })
            .context("spawn mic thread")?;
        Ok(MicStreamer {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for MicStreamer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Whether the mic echo-cancellation hooks run this session: the `echo_cancel` setting, with
/// `PUNKTFUNK_NO_AEC=1` as a one-way override OFF. The env var wins — it is the escape hatch
/// for a box whose canceller misbehaves, and it predates the setting; nothing turns AEC back
/// on once it is set. Here the hook is the Communications stream category below; the PipeWire
/// twin gates its echo-cancelled-source preference the same way.
fn aec_enabled(echo_cancel: bool) -> bool {
    echo_cancel && !std::env::var("PUNKTFUNK_NO_AEC").is_ok_and(|v| !v.is_empty() && v != "0")
}

fn mic_thread(
    connector: &Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    muted: Arc<AtomicBool>,
    echo_cancel: bool,
) -> Result<()> {
    wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")?;
    // Same treatment for the capture loop: capture, encode and send all run here.
    crate::audio_rt::boost_and_log("wasapi-mic");

    let mut encoder = opus::Encoder::new(
        SAMPLE_RATE as u32,
        opus::Channels::Mono,
        opus::Application::Voip,
    )
    .map_err(|e| anyhow!("opus encoder: {e}"))?;
    // Voice tuning: 48 kbps mono is transparent for speech; in-band FEC + an assumed 10 %
    // loss let the host's decoder rebuild a lost 0xCB datagram from its successor instead
    // of concealing (datagrams are fire-and-forget — this FEC is the only redundancy).
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(48_000));
    let _ = encoder.set_inband_fec(true);
    let _ = encoder.set_packet_loss_perc(10);

    let enumerator = DeviceEnumerator::new().context("DeviceEnumerator")?;
    let device = pick_device(&enumerator, &Direction::Capture, "PUNKTFUNK_AUDIO_SOURCE")
        .context("capture endpoint (no microphone?)")?;
    let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
    // Communications category → the endpoint's communications signal-processing chain. A
    // driver/APO stack with an echo canceller only engages it for communications-category
    // streams; the default (Other) category never did, so the downlink audio playing on
    // this box fed straight back into the host's virtual mic. Must precede Initialize
    // (SetClientProperties is a pre-init call; the wasapi crate QIs IAudioClient2 inside).
    // Best-effort: an endpoint without IAudioClient2 just keeps the default category.
    // The "Echo cancellation" setting opts out, and PUNKTFUNK_NO_AEC=1 overrides that off
    // (same lever as the Linux echo-cancel-source preference) — see `aec_enabled`.
    if aec_enabled(echo_cancel) {
        if let Err(e) = audio_client.set_properties(
            AudioClientProperties::new().set_category(StreamCategory::Communications),
        ) {
            tracing::debug!(error = %e, "mic capture: Communications category not set");
        }
    }
    let desired = WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, CAPT_CHANNELS, None);
    let (default_period, _min_period) =
        audio_client.get_device_period().context("device period")?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: default_period,
    };
    audio_client
        .initialize_client(&desired, &Direction::Capture, &mode)
        .context("initialize capture client")?;
    let h_event = audio_client.set_get_eventhandle().context("event handle")?;
    let capture_client = audio_client
        .get_audiocaptureclient()
        .context("IAudioCaptureClient")?;
    audio_client
        .start_stream()
        .context("start capture stream")?;

    let mut bytes: VecDeque<u8> = VecDeque::new();
    let mut ring: VecDeque<f32> = VecDeque::new();
    let mut out = vec![0u8; 4000];
    let mut seq = 0u32;

    while !stop.load(Ordering::Relaxed) {
        if h_event.wait_for_event(100).is_err() {
            continue;
        }
        loop {
            match capture_client.get_next_packet_size() {
                Ok(Some(0)) | Ok(None) => break,
                Ok(Some(_n)) => {
                    capture_client
                        .read_from_device_to_deque(&mut bytes)
                        .context("read capture")?;
                }
                Err(e) => return Err(anyhow!("get_next_packet_size: {e}")),
            }
        }
        // One stereo capture frame (8 bytes) → one mono sample: average L/R. Autoconvert
        // already matrixed the endpoint's real layout (mono/stereo/array mic) into the
        // stereo stream we initialized, so this is the only downmix left to do.
        let stereo_frame = 4 * CAPT_CHANNELS;
        let whole = (bytes.len() / stereo_frame) * stereo_frame;
        for c in bytes
            .drain(..whole)
            .collect::<Vec<u8>>()
            .chunks_exact(stereo_frame)
        {
            let l = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            let r = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            ring.push_back((l + r) * 0.5);
        }
        // Muted (B4): the capture client stays started and keeps its primed buffers — only
        // the sending stops. Whole frames are discarded so the ring can't grow, and `seq`
        // deliberately does NOT advance: the host sees one continuous sequence with a silent
        // pause in the middle rather than a gap the size of the mute, which its de-jitter
        // would try to conceal frame by frame.
        if muted.load(Ordering::Relaxed) {
            let drop_n = (ring.len() / MIC_FRAME) * MIC_FRAME;
            ring.drain(..drop_n);
            continue;
        }
        // Ship every complete 10 ms mono frame.
        while ring.len() >= MIC_FRAME {
            let pcm: Vec<f32> = ring.drain(..MIC_FRAME).collect();
            match encoder.encode_float(&pcm, &mut out) {
                Ok(len) => {
                    let pts = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_nanos() as u64)
                        .unwrap_or(0);
                    let _ = connector.send_mic(seq, pts, out[..len].to_vec());
                    seq = seq.wrapping_add(1);
                }
                Err(e) => tracing::debug!(error = %e, "opus mic encode"),
            }
        }
    }
    audio_client.stop_stream().ok();
    Ok(())
}
