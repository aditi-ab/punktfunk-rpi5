//! Audio: playback (decoded PCM → a PipeWire playback stream) and the microphone uplink
//! (PipeWire capture → Opus → 0xCB datagrams, the inverse of the host's virtual mic).
//!
//! Playback mirrors the host's virtual-mic producer (`punktfunk-host::audio::linux`) with
//! the same adaptive jitter buffer: the session pump pushes 5 ms Opus-decoded chunks on
//! the network clock; PipeWire pulls whole quanta on the device clock. Prime to ~3
//! quanta before producing, cap the ring so latency stays bounded, re-prime after a real
//! drain.

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

const SAMPLE_RATE: u32 = 48_000;
/// Mic capture is MONO: voice is mono at the source, the host accepts any Opus channel
/// layout (its stereo decoder upmixes), and half the samples halve the encode + wire cost.
const MIC_CHANNELS: usize = 1;
/// Mic frames are 10 ms (480 mono samples) — any size ≤ 120 ms is fine host-side; 10 ms
/// halves the frame-fill share of mouth-to-ear latency vs the old 20 ms.
const MIC_FRAME: usize = 480;

struct Terminate;

/// A selectable PipeWire endpoint for the settings pickers.
#[derive(Clone, Debug)]
pub struct AudioDevice {
    /// `node.name` — the stable key the streams target via `target.object`.
    pub name: String,
    /// `node.description` — the human label the picker shows.
    pub description: String,
}

/// Enumerate audio endpoints: `(sinks, sources)`. One registry roundtrip on a private
/// mainloop (a few ms against a live PipeWire); no daemon errors out and the caller
/// simply shows no pickers.
pub fn devices() -> Result<(Vec<AudioDevice>, Vec<AudioDevice>)> {
    use pipewire as pw;
    use std::cell::RefCell;
    use std::rc::Rc;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;
    let registry = core.get_registry_rc().context("pw registry")?;

    let found: Rc<RefCell<(Vec<AudioDevice>, Vec<AudioDevice>)>> = Rc::default();
    let _reg_listener = registry
        .add_listener_local()
        .global({
            let found = found.clone();
            move |g| {
                let Some(props) = g.props else { return };
                let sink = match props.get("media.class") {
                    Some("Audio/Sink") => true,
                    Some("Audio/Source") => false,
                    _ => return,
                };
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .unwrap_or(name)
                    .to_string();
                let dev = AudioDevice {
                    name: name.to_string(),
                    description,
                };
                let mut f = found.borrow_mut();
                if sink { &mut f.0 } else { &mut f.1 }.push(dev);
            }
        })
        .register();

    // The registry replays existing globals asynchronously; one core sync marks the
    // point they've all been delivered — quit the loop there.
    let pending = core.sync(0).context("pw sync")?;
    let _core_listener = core
        .add_listener_local()
        .done({
            let mainloop = mainloop.clone();
            move |_, seq| {
                if seq == pending {
                    mainloop.quit();
                }
            }
        })
        .register();
    mainloop.run();

    let result = found.borrow().clone();
    Ok(result)
}

pub struct AudioPlayer {
    pcm_tx: SyncSender<Vec<f32>>,
    /// Drained chunk Vecs coming back from the PipeWire consumer for reuse (the pool half
    /// of the pcm channel — see [`AudioPlayer::take_buffer`]).
    recycle_rx: Receiver<Vec<f32>>,
    quit_tx: pipewire::channel::Sender<Terminate>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// A/V sync hand-off with the PipeWire callback: it publishes the ring depth, the decode
    /// thread posts the depth the sync loop wants. See [`punktfunk_core::audio::AudioSyncCell`].
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
}

impl AudioPlayer {
    /// Spawn the PipeWire playback thread for `channels` (2/6/8, canonical wire order
    /// FL FR FC LFE RL RR SL SR). Failure (no PipeWire in the session) is survivable — the
    /// caller streams video-only.
    pub fn spawn(channels: u32) -> Result<AudioPlayer> {
        // 64 × 5 ms = 320 ms of slack between the pump and the PipeWire loop.
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        // Return path: the process callback sends each drained Vec back for reuse, so
        // steady-state playback stops allocating (~200 chunks/s otherwise). Same capacity
        // as the data channel; a full pool just drops the Vec (plain deallocation).
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let sync: Arc<punktfunk_core::audio::AudioSyncCell> = Arc::default();
        let sync_cb = sync.clone();
        let thread = std::thread::Builder::new()
            .name("punktfunk-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(pcm_rx, recycle_tx, quit_rx, channels as usize, sync_cb) {
                    tracing::warn!(error = %e, "audio playback thread ended");
                }
            })
            .context("spawn audio thread")?;
        Ok(AudioPlayer {
            pcm_tx,
            recycle_rx,
            quit_tx,
            thread: Some(thread),
            sync,
        })
    }

    /// The A/V sync hand-off cell — the decode thread reads the ring depth from it and posts the
    /// depth the sync loop wants back through it.
    pub fn sync_cell(&self) -> Arc<punktfunk_core::audio::AudioSyncCell> {
        self.sync.clone()
    }

    /// A recycled chunk Vec from the pool, empty but with its capacity intact — fill it
    /// and hand it back through [`push`](Self::push). Allocates only when the pool is dry
    /// (startup, or after the PipeWire side dropped chunks).
    pub fn take_buffer(&self) -> Vec<f32> {
        self.recycle_rx.try_recv().unwrap_or_default()
    }

    /// Queue one interleaved f32 chunk (in the session's channel layout). Drops the chunk if the
    /// PipeWire side is wedged (the renderer conceals the gap; never block the session pump).
    pub fn push(&self, pcm: Vec<f32>) {
        if let Err(TrySendError::Disconnected(_)) = self.pcm_tx.try_send(pcm) {
            // Thread already dead — Drop will reap it; nothing to do per-chunk.
        }
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Producer-side state: incoming decoded PCM and the ring the process callback drains.
struct PlayerData {
    rx: Receiver<Vec<f32>>,
    /// Drained chunk Vecs go back here for the decode side to refill (allocation pool).
    recycle: SyncSender<Vec<f32>>,
    ring: VecDeque<f32>,
    /// Shared ms-denominated de-jitter policy: prime depth, drift correction, de-prime
    /// hysteresis. Replaces the old `3 × quantum` target, which meant 15 ms at a 5 ms graph
    /// quantum and a silent 64 ms at a 20 ms one, and the `if ring.is_empty()` re-prime, where
    /// one transient drain manufactured a whole target's worth of fresh silence.
    policy: punktfunk_core::audio::JitterPolicy,
    /// Interleaved channel count this stream was opened with (2/6/8).
    channels: usize,
    /// Diagnostics (WP0.3), logged ~every 10 s: the audio plane used to be entirely silent in a
    /// client log, so a latency or dropout report had nothing to go on.
    underruns: u64,
    sheds: u64,
    callbacks: u64,
    /// A/V sync hand-off with the decode thread (depth out, target in).
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
}

fn pw_thread(
    pcm_rx: Receiver<Vec<f32>>,
    recycle_tx: SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: usize,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Playback",
        *pw::keys::MEDIA_ROLE       => "Game",
        *pw::keys::NODE_NAME        => "punktfunk-client",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream",
        // ~5 ms quantum (one Opus frame) keeps the ring — and so the latency — small.
        *pw::keys::NODE_LATENCY     => "240/48000",
    };
    // The Settings speaker pick (session main maps `Settings::speaker_device` here);
    // unset/empty = PipeWire's default routing.
    if let Ok(target) = std::env::var("PUNKTFUNK_AUDIO_SINK") {
        if !target.is_empty() {
            // Raw key: the `keys::TARGET_OBJECT` constant is feature-gated on a newer
            // libpipewire than we require; the wire name is stable.
            props.insert("target.object", target);
        }
    }
    let stream =
        pw::stream::StreamBox::new(&core, "punktfunk-client", props).context("pw Stream")?;

    let ud = PlayerData {
        rx: pcm_rx,
        recycle: recycle_tx,
        ring: VecDeque::new(),
        policy: punktfunk_core::audio::JitterPolicy::new(
            punktfunk_core::audio::JitterTuning::PIPEWIRE,
            channels as u8,
        ),
        channels,
        underruns: 0,
        sheds: 0,
        callbacks: 0,
        sync,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(|_s, _ud, old, new| {
            tracing::debug!(?old, ?new, "pipewire playback stream state");
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                while let Ok(mut chunk) = ud.rx.try_recv() {
                    ud.ring.extend(chunk.iter().copied());
                    // Return the drained Vec to the pool; a full/closed pool drops it.
                    chunk.clear();
                    let _ = ud.recycle.try_send(chunk);
                }
                // The graph asks for `requested` frames this cycle (one quantum, after
                // rate-matching); the mapped buffer is sized for the WORST case — PipeWire's
                // `quantum-limit`, 8192 frames ≈ 170 ms — not for this cycle. Filling to
                // capacity queued ~170 ms per buffer downstream of the ring and, worse, taught
                // the jitter policy that the device drains 170 ms per callback, which lifted
                // the underrun floor (`want` + one frame) above any depth the A/V sync loop is
                // allowed to ask for: audio sat a stable ~270 ms late and, by the continuity
                // rule, sync was FORBIDDEN from draining it. Capacity is only the ceiling;
                // `requested == 0` (no adapter suggestion) falls back to it.
                let requested = usize::try_from(buffer.requested()).unwrap_or(0);
                let stride = 4 * ud.channels; // F32LE interleaved
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let max_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                let want_frames = if requested > 0 {
                    requested.min(max_frames)
                } else {
                    max_frames
                };
                let want = want_frames * ud.channels;
                // Once per stream, in the shape of the host's per-capture-open quantum log:
                // whether the graph's request or the buffer ceiling is sizing our writes is
                // exactly what an on-glass latency report needs to say.
                if ud.callbacks == 0 {
                    tracing::info!(
                        requested_frames = requested,
                        capacity_frames = max_frames,
                        write_frames = want_frames,
                        write_ms = want_frames / 48,
                        "audio playback quantum"
                    );
                }

                // A/V sync: take whatever depth the decode thread's sync loop last asked for, and
                // publish where the ring actually is so it can measure the result. The policy
                // clamps the request between its own underrun floor and the hard cap — continuity
                // outranks sync, always (see `JitterPolicy::set_sync_target`).
                ud.policy.set_sync_target(ud.sync.target());
                ud.sync.publish_depth(ud.ring.len());

                // Shared de-jitter policy: prime depth in MILLISECONDS, smooth drift correction
                // (a crossfaded 5 ms shed) so latency returns to target instead of ratcheting,
                // and a hard cap as the backstop.
                let step = ud.policy.step(ud.ring.len(), want);
                if step.drop_front > 0 {
                    ud.sheds += 1;
                    punktfunk_core::audio::crossfade_drop(
                        &mut ud.ring,
                        step.drop_front,
                        step.crossfade,
                    );
                }

                let mut ran_short = false;
                let n_frames = if let Some(slice) = data.data() {
                    for k in 0..want {
                        let s = if step.silence {
                            0.0
                        } else {
                            ud.ring.pop_front().unwrap_or_else(|| {
                                ran_short = true;
                                0.0
                            })
                        };
                        let off = k * 4;
                        slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                    }
                    want_frames
                } else {
                    0
                };
                // No-op while un-primed (the policy ignores it), so a deliberate priming silence
                // is never miscounted as an underrun.
                ud.policy.note_read(ran_short);
                ud.underruns += u64::from(ran_short);
                ud.callbacks += 1;
                // ~10 s at a 5 ms quantum; the exact cadence does not matter, only that the
                // plane stops being invisible.
                if ud.callbacks % 2_000 == 0 {
                    tracing::debug!(
                        buffer_ms = ud.policy.avg_depth_ms(),
                        target_ms = ud.policy.target_ms(),
                        underruns = ud.underruns,
                        drift_sheds = ud.sheds,
                        "audio playback"
                    );
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as _;
                *chunk.size_mut() = (stride * n_frames) as _;
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire playback callback");
            }
        })
        .register()
        .context("register playback listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(channels as u32);
    // Channel positions in canonical wire order (FL FR FC LFE RL RR SL SR) so PipeWire routes each
    // slot to the matching speaker (and downmixes when the sink has fewer). Identity, no permute.
    let order = punktfunk_core::audio::spa_positions(channels as u8);
    let mut positions = [0u32; 64];
    positions[..order.len()].copy_from_slice(order);
    info.set_position(positions);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire playback loop exited");
    Ok(())
}

/// The microphone uplink: capture the default input device (or the picked / echo-cancelled
/// source), Opus-encode 10 ms mono chunks, ship them as 0xCB datagrams into the host's
/// virtual PipeWire source.
pub struct MicStreamer {
    quit_tx: pipewire::channel::Sender<Terminate>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MicStreamer {
    /// `muted` is the in-stream mute (B4), shared live with the capture callback: set, the
    /// callback keeps pulling and discarding whole frames but sends nothing. Muting by
    /// STOPPING the stream was rejected — it re-primes the device buffers and re-runs the
    /// source selection below on every unmute, so the first second back is glitchy.
    ///
    /// `echo_cancel` is the Settings toggle; `PUNKTFUNK_NO_AEC=1` overrides it off.
    pub fn spawn(
        connector: Arc<NativeClient>,
        muted: Arc<std::sync::atomic::AtomicBool>,
        echo_cancel: bool,
    ) -> Result<MicStreamer> {
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let thread = std::thread::Builder::new()
            .name("punktfunk-mic".into())
            .spawn(move || {
                if let Err(e) = mic_thread(&connector, quit_rx, muted, echo_cancel) {
                    tracing::warn!(error = %e, "mic uplink thread ended");
                }
            })
            .context("spawn mic thread")?;
        Ok(MicStreamer {
            quit_tx,
            thread: Some(thread),
        })
    }
}

impl Drop for MicStreamer {
    fn drop(&mut self) {
        let _ = self.quit_tx.send(Terminate);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Capture-side state: accumulated PCM and the Opus encoder (encoding a 10 ms frame is
/// well under 100 µs — fine inside the process callback).
struct MicData {
    connector: Arc<NativeClient>,
    ring: VecDeque<f32>,
    encoder: opus::Encoder,
    seq: u32,
    out: Vec<u8>,
    /// The in-stream mute (B4), flipped by the session's chord. Read per callback.
    muted: Arc<std::sync::atomic::AtomicBool>,
}

/// Whether the mic echo-cancellation hooks run this session: the `echo_cancel` setting, with
/// `PUNKTFUNK_NO_AEC=1` as a one-way override OFF. The env var wins — it is the escape hatch
/// for a box whose canceller misbehaves, and it predates the setting; nothing turns AEC back
/// on once it is set. Here the hook is the echo-cancelled-source preference below; the WASAPI
/// twin gates its Communications stream category the same way.
fn aec_enabled(echo_cancel: bool) -> bool {
    echo_cancel && !std::env::var("PUNKTFUNK_NO_AEC").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// The capture stream's `target.object`, in preference order: the Settings microphone pick
/// (`Settings::mic_device` via session main's `PUNKTFUNK_AUDIO_SOURCE`) verbatim, else — so a
/// desktop that already runs `module-echo-cancel` stops feeding its own downlink audio back
/// into the host's virtual mic — the first echo-cancelled source in the graph. `None` = the
/// user picked nothing and no such source exists: PipeWire's default routing, as before.
///
/// Preference-only by design: loading `libpipewire-module-echo-cancel` ourselves needs
/// `pw_context_load_module`, which the pipewire crate (0.9) doesn't expose safely — until it
/// does, we only ever target processing the user (or their session) already set up.
fn mic_capture_target(echo_cancel: bool) -> Option<String> {
    if let Ok(target) = std::env::var("PUNKTFUNK_AUDIO_SOURCE") {
        if !target.is_empty() {
            return Some(target);
        }
    }
    if !aec_enabled(echo_cancel) {
        return None;
    }
    let name = echo_cancel_source()?;
    tracing::info!(
        source = %name,
        "mic capture targets the echo-cancelled source (Echo cancellation off, or \
         PUNKTFUNK_NO_AEC=1, disables this)"
    );
    Some(name)
}

/// Find an existing echo-cancelled capture node: the first `Audio/Source` whose `node.name`
/// or description says echo-cancel (`module-echo-cancel`'s convention — `echo-cancel-*`
/// nodes, "Echo-Cancel …" descriptions; PulseAudio-compat setups match too). One registry
/// roundtrip via [`devices`]; any failure reads as "none".
fn echo_cancel_source() -> Option<String> {
    let (_, sources) = devices().ok()?;
    sources.into_iter().find_map(|d| {
        let name = d.name.to_ascii_lowercase();
        let desc = d.description.to_ascii_lowercase();
        (name.contains("echo-cancel")
            || name.contains("echo_cancel")
            || desc.contains("echo-cancel")
            || desc.contains("echo cancel"))
        .then_some(d.name)
    })
}

fn mic_thread(
    connector: &Arc<NativeClient>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    muted: Arc<std::sync::atomic::AtomicBool>,
    echo_cancel: bool,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let mut encoder =
        opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)
            .map_err(|e| anyhow::anyhow!("opus encoder: {e}"))?;
    // Voice tuning: 48 kbps mono is transparent for speech; in-band FEC + an assumed 10 %
    // loss let the host's decoder rebuild a lost 0xCB datagram from its successor instead
    // of concealing (datagrams are fire-and-forget — this FEC is the only redundancy).
    let _ = encoder.set_bitrate(opus::Bitrate::Bits(48_000));
    let _ = encoder.set_inband_fec(true);
    let _ = encoder.set_packet_loss_perc(10);

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw mic MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw mic Context")?;
    let core = context
        .connect_rc(None)
        .context("pw mic connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Capture",
        *pw::keys::MEDIA_ROLE       => "Communication",
        *pw::keys::NODE_NAME        => "punktfunk-mic-capture",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Microphone",
        // ~10 ms quantum (one mic frame). Without it the capture stream inherits the graph
        // quantum — commonly 1024–2048 samples, so the mic arrived in 21–43 ms bursts that
        // sat ahead of the encoder as latency (the playback stream always asked for 5 ms).
        *pw::keys::NODE_LATENCY     => "480/48000",
    };
    if let Some(target) = mic_capture_target(echo_cancel) {
        // Raw key: the `keys::TARGET_OBJECT` constant is feature-gated on a newer
        // libpipewire than we require; the wire name is stable.
        props.insert("target.object", target);
    }
    let stream = pw::stream::StreamBox::new(&core, "punktfunk-mic-capture", props)
        .context("pw mic Stream")?;

    let ud = MicData {
        connector: connector.clone(),
        ring: VecDeque::new(),
        encoder,
        seq: 0,
        out: vec![0u8; 4000],
        muted,
    };

    let _listener = stream
        .add_local_listener_with_user_data(ud)
        .state_changed(|_s, _ud, old, new| {
            tracing::debug!(?old, ?new, "pipewire mic capture stream state");
        })
        .process(|stream, ud| {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let n = data.chunk().size() as usize;
                if let Some(slice) = data.data() {
                    for s in slice[..n.min(slice.len())].chunks_exact(4) {
                        ud.ring
                            .push_back(f32::from_le_bytes([s[0], s[1], s[2], s[3]]));
                    }
                }
                // Muted (B4): the stream stays open and the device keeps its primed buffers —
                // only the sending stops. Whole frames are discarded so the ring can't grow,
                // and `seq` deliberately does NOT advance: the host sees one continuous
                // sequence with a silent pause in the middle rather than a gap the size of the
                // mute, which its de-jitter would try to conceal frame by frame.
                if ud.muted.load(std::sync::atomic::Ordering::Relaxed) {
                    let whole =
                        (ud.ring.len() / (MIC_FRAME * MIC_CHANNELS)) * (MIC_FRAME * MIC_CHANNELS);
                    ud.ring.drain(..whole);
                    return;
                }
                // Ship every complete 10 ms mono frame.
                while ud.ring.len() >= MIC_FRAME * MIC_CHANNELS {
                    let pcm: Vec<f32> = ud.ring.drain(..MIC_FRAME * MIC_CHANNELS).collect();
                    match ud.encoder.encode_float(&pcm, &mut ud.out) {
                        Ok(len) => {
                            let pts = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos() as u64)
                                .unwrap_or(0);
                            let _ = ud.connector.send_mic(ud.seq, pts, ud.out[..len].to_vec());
                            ud.seq = ud.seq.wrapping_add(1);
                        }
                        Err(e) => tracing::debug!(error = %e, "opus mic encode"),
                    }
                }
            }));
            if outcome.is_err() {
                tracing::error!("panic in pipewire mic callback");
            }
        })
        .register()
        .context("register mic listener")?;

    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    // Mono: the stream's adapter downmixes whatever layout the source really has.
    info.set_channels(MIC_CHANNELS as u32);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize mic format pod")?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&values).context("mic pod from bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("pw mic stream connect")?;

    mainloop.run();
    tracing::debug!("pipewire mic capture loop exited");
    Ok(())
}
