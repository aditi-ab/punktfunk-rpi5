//! Audio: playback (decoded PCM → a WASAPI shared-mode render stream) and the microphone
//! uplink (WASAPI capture → Opus → 0xCB datagrams, the inverse of the host's virtual mic).
//!
//! The WASAPI twin of `audio.rs` (PipeWire) — same public surface (`AudioPlayer::spawn`/
//! `take_buffer`/`push`, `MicStreamer::spawn`), swapped in by lib.rs's `#[path]` so the
//! session pump compiles against one `crate::audio` on both OSes. Adapted from
//! `clients/windows/src/audio.rs` (which remains the WinUI shell's own copy until its
//! built-in streaming path is deleted).
//!
//! Playback mirrors the host's virtual-mic producer's adaptive jitter buffer: the session
//! pump pushes 5 ms Opus-decoded chunks on the network clock; the WASAPI render thread
//! pulls whole event-driven quanta on the device clock. Prime to ~3 quanta before
//! producing, cap the ring so latency stays bounded, re-prime after a real drain.
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

const SAMPLE_RATE: usize = 48_000;
/// Mic capture requests STEREO from WASAPI (autoconvert matrixes any endpoint layout down to
/// it — the proven path; `read_from_device_to_deque` then delivers our requested format) and
/// downmixes to MONO in code before the encoder: voice is mono at the source, the host accepts
/// any Opus channel layout (its stereo decoder upmixes), and half the samples halve the encode
/// + wire cost. The render path is multichannel — its channel count + block align are runtime,
/// driven by the host-resolved layout.
const CAPT_CHANNELS: usize = 2;
/// Mic frames are 10 ms (480 mono samples) — any size ≤ 120 ms is fine host-side; 10 ms
/// halves the frame-fill share of mouth-to-ear latency vs the old 20 ms.
const MIC_FRAME: usize = 480;

pub struct AudioPlayer {
    pcm_tx: SyncSender<Vec<f32>>,
    /// Drained chunk Vecs coming back from the render thread for reuse (the pool half of
    /// the pcm channel — see [`AudioPlayer::take_buffer`]).
    recycle_rx: Receiver<Vec<f32>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlayer {
    /// Spawn the WASAPI render thread for `channels` (2/6/8, canonical wire order
    /// FL FR FC LFE RL RR SL SR). Failure (no render endpoint on this box) is survivable — the
    /// caller streams video-only.
    pub fn spawn(channels: u32) -> Result<AudioPlayer> {
        // 64 × 5 ms = 320 ms of slack between the pump and the WASAPI loop.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        // Return path: the render thread sends each drained Vec back for reuse, so
        // steady-state playback stops allocating (~200 chunks/s otherwise). Same capacity
        // as the data channel; a full pool just drops the Vec (plain deallocation).
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-audio".into())
            .spawn(move || {
                if let Err(e) = render_thread(pcm_rx, recycle_tx, stop_t, ready_tx, channels as u8)
                {
                    tracing::warn!(error = %format!("{e:#}"), "audio playback thread ended");
                }
            })
            .context("spawn audio thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(3)) {
            Ok(Ok(())) => {
                tracing::info!(channels, "WASAPI render: 48 kHz f32 (default endpoint)");
                Ok(AudioPlayer {
                    pcm_tx,
                    recycle_rx,
                    stop,
                    thread: Some(thread),
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
    ready: SyncSender<Result<()>>,
    channels: u8,
) -> Result<()> {
    if let Err(e) = wasapi::initialize_mta()
        .ok()
        .context("CoInitializeEx (MTA)")
    {
        let _ = ready.send(Err(e));
        return Ok(());
    }
    let res = (|| -> Result<()> {
        // F32LE interleaved: channels × 4 bytes/sample. Stereo (channels == 2) is byte-identical
        // to the old fixed path (mask 0x3, block align 8).
        let block_align = channels as usize * 4;
        let device = DeviceEnumerator::new()
            .context("DeviceEnumerator")?
            .get_default_device(&Direction::Render)
            .context("default render endpoint")?;
        let mut audio_client = device.get_iaudioclient().context("IAudioClient")?;
        // The explicit dwChannelMask is the wire order (FL FR FC LFE RL RR SL SR); 5.1 = 0x3F,
        // 7.1 = 0x63F. WASAPI delivers channels in ascending mask-bit order, which equals the wire
        // order, so the render mapping is the identity — no permute. `autoconvert` (below) lets the
        // audio engine downmix when the endpoint has fewer speakers.
        let desired = WaveFormat::new(
            32,
            32,
            &SampleType::Float,
            SAMPLE_RATE,
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
        let _ = ready.send(Ok(()));

        // Adaptive jitter buffer, in f32-byte units (same shape as the host's virtual mic).
        let mut ring: VecDeque<u8> = VecDeque::new();
        let mut primed = false;
        let mut out = Vec::new(); // per-quantum scratch, reused across iterations

        while !stop.load(Ordering::Relaxed) {
            if h_event.wait_for_event(100).is_err() {
                continue;
            }
            // Drain everything the pump has queued into the ring, returning each drained
            // Vec to the pool (a full/closed pool drops it).
            while let Ok(mut chunk) = pcm_rx.try_recv() {
                for s in chunk.iter() {
                    ring.extend(s.to_le_bytes());
                }
                chunk.clear();
                let _ = recycle_tx.try_send(chunk);
            }
            let avail_frames = audio_client
                .get_available_space_in_frames()
                .context("available space")? as usize;
            if avail_frames == 0 {
                continue;
            }
            let want_bytes = avail_frames * block_align;

            // Prime to ~3 quanta; cap at ~1 quantum of slack beyond that; re-prime on drain.
            let target = (3 * want_bytes).clamp(720 * block_align, 9600 * block_align);
            let cap = target.max(want_bytes) + want_bytes;
            if ring.len() > cap {
                ring.drain(..ring.len() - cap);
            }
            if !primed && ring.len() >= target {
                primed = true;
            }

            out.clear();
            out.resize(want_bytes, 0);
            if primed {
                let n = ring.len().min(want_bytes);
                for (dst, b) in out.iter_mut().zip(ring.drain(..n)) {
                    *dst = b;
                }
            }
            if ring.is_empty() {
                primed = false;
            }
            render_client
                .write_to_device(avail_frames, &out, None)
                .context("write_to_device")?;
        }
        audio_client.stop_stream().ok();
        Ok(())
    })();
    if let Err(ref e) = res {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    res
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

    let device = DeviceEnumerator::new()
        .context("DeviceEnumerator")?
        .get_default_device(&Direction::Capture)
        .context("default capture endpoint (no microphone?)")?;
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
