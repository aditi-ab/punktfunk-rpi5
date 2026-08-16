//! Audio: playback (decoded PCM → a PipeWire playback stream) and the microphone uplink
//! (PipeWire capture → Opus → 0xCB datagrams, the inverse of the host's virtual mic).
//!
//! Playback mirrors the host's virtual-mic producer (`punktfunk-host::audio::linux`) with
//! the same adaptive jitter buffer: the session pump pushes one decoded frame per network
//! arrival; PipeWire pulls whole quanta on the device clock. Prime to ~3 quanta before
//! producing, cap the ring so latency stays bounded, re-prime after a real drain.
//!
//! The stream is opened at the format the session NEGOTIATED ([`PlaybackFormat`]), not at a
//! constant: 48 kHz Opus frames of 5 ms on the `0xC9` plane, or 48/96 kHz lossless PCM frames of
//! 1–5 ms on `0xD3` (`design/hi-res-audio.md`). The graph format stays F32LE at every rate and
//! depth — core decodes both planes to f32, and the reason that is deliberate rather than an
//! oversight is argued at the `stride` in the process callback.

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::Arc;

/// The protocol's default rate — and, now that playback takes its rate from the `Welcome`
/// ([`PlaybackFormat`]), the MIC uplink's rate and nothing else. Voice is Opus, and libopus is
/// 48 kHz by construction, so the uplink has no reason to move and no way to.
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

/// The playback format a session RESOLVED, straight off the `Welcome` — never what the client
/// asked for. Passed as one value rather than three positional `u32`s because all three are `u32`
/// and transposing them would open the device at a plausible-looking wrong format.
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
    /// sizes the graph quantum we ask for and the policy's shed/floor arithmetic.
    pub frame_us: u32,
}

impl PlaybackFormat {
    /// Frames (per channel) in one protocol frame — the graph quantum to ask for. Computed in µs
    /// so a sub-millisecond rung does not truncate: 2 500 µs at 48 kHz is 120 frames, not 96.
    fn quantum_frames(&self) -> u32 {
        ((self.rate_hz as u64 * self.frame_us as u64 / 1_000_000) as u32).max(1)
    }

    /// Frames (per channel) per millisecond — 48 at the protocol default, 96 at 96 kHz.
    fn frames_per_ms(&self) -> usize {
        (self.rate_hz / 1000).max(1) as usize
    }
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
    /// Spawn the PipeWire playback thread at the session's RESOLVED format. Failure (no PipeWire
    /// in the session) is survivable — the caller streams video-only.
    pub fn spawn(fmt: PlaybackFormat) -> Result<AudioPlayer> {
        // 64 queued chunks of slack between the pump and the PipeWire loop — 320 ms at the Opus
        // plane's 5 ms frame, proportionally less on a lossless session's shorter one (128 ms at
        // 2 ms), which is still far above anything the de-jitter policy targets. Left as a chunk
        // COUNT rather than scaled to the negotiated frame, matching core's own `AUDIO_QUEUE`,
        // whose comment records the same trade.
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
                if let Err(e) = pw_thread(pcm_rx, recycle_tx, quit_rx, fmt, sync_cb) {
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

/// This backend's de-jitter tuning. Named once so the decode thread can read the same numbers the
/// callback runs on — its drought concealment is bounded by this preset's de-prime fuse, and the
/// two drifting apart is exactly how one platform quietly ends up with a third of another's slack.
pub(crate) const TUNING: punktfunk_core::audio::JitterTuning =
    punktfunk_core::audio::JitterTuning::PIPEWIRE;

/// Can this client render a `rate_hz` stream? — the gate on advertising `CLIENT_CAP_AUDIO_HIRES`,
/// which means *capable **and** the user turned it on* (`design/hi-res-audio.md` §7).
///
/// **Always true here, and that is a statement about PipeWire, not a shortcut.** A playback stream
/// declares its own format and the graph inserts an adapter to reconcile it with the sink, so this
/// client can always OPEN at the resolved rate and always renders every sample the host sends. The
/// WASAPI twin genuinely can fail this test, because shared-mode autoconvert reconciles in the
/// other direction — silently, against an engine format we do not control.
///
/// What PipeWire does NOT promise is that the SINK runs at that rate: a 96 kHz stream into a
/// 48 kHz sink is resampled in the graph, and the detail above 24 kHz is gone. That is the
/// client-side shape of the monitor-mode blind spot in §4.4, and reading the sink's own rate needs
/// a registry lookup this crate does not do. The stream logs what the graph actually granted (its
/// `param_changed` handler) so the waste is at least visible; the remedy is the user's own audio
/// configuration, exactly as the endpoint rate is on Windows.
pub fn can_render_at(_rate_hz: u32) -> bool {
    true
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
    /// The format this stream was opened at, so the callback can report its quantum in
    /// milliseconds and `param_changed` can check what the graph actually granted against it.
    fmt: PlaybackFormat,
    /// What `param_changed` last saw, so a graph RESUME (which re-announces the same format) is
    /// not logged as a format change — the host's virtual sink learned the same lesson.
    negotiated: Option<(u32, u32)>,
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
    fmt: PlaybackFormat,
    sync: Arc<punktfunk_core::audio::AudioSyncCell>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    static PW_INIT: std::sync::Once = std::sync::Once::new();
    PW_INIT.call_once(pw::init);

    let channels = fmt.channels as usize;

    let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw MainLoop")?;
    let context = pw::context::ContextRc::new(&mainloop, None).context("pw Context")?;
    let core = context
        .connect_rc(None)
        .context("pw connect (is PipeWire running in this session?)")?;

    let _quit_guard = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    // The `NODE_LATENCY` ask — one protocol frame, so the graph hands us whole frames and the ring
    // (and so the latency) stays small. `<frames>/<rate>` is how PipeWire spells a latency and BOTH
    // halves move with the session: this was the string literal `"240/48000"`, which on a 96 kHz
    // lossless session would have asked for 240 frames = 2.5 ms — a different quantum than the one
    // the frame duration was negotiated for, at double the callback rate, for no reason anyone
    // intended. Built at run time for exactly that reason; the `properties!` macro takes literals
    // only, so it goes in with `insert` (the same shape as the host's sink-name property).
    let node_latency = format!("{}/{}", fmt.quantum_frames(), fmt.rate_hz);
    let mut props = properties! {
        *pw::keys::MEDIA_TYPE       => "Audio",
        *pw::keys::MEDIA_CATEGORY   => "Playback",
        *pw::keys::MEDIA_ROLE       => "Game",
        *pw::keys::NODE_NAME        => "punktfunk-client",
        *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream",
    };
    props.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
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
        policy: {
            // Both at the RESOLVED format: `new_at_rate` denominates every depth/target/shed
            // figure — and the `buffer_ms`/`target_ms` this client reports — in the right
            // samples-per-millisecond, and `set_frame_us` tells the two frame-denominated
            // decisions (the floor under the effective target, and the one-frame smooth shed) how
            // long a frame is here. Left at the defaults, a 96 kHz session would shed 2.5 frames
            // at a time and crossfade across a whole one.
            let mut p = punktfunk_core::audio::JitterPolicy::new_at_rate(
                TUNING,
                fmt.channels as u8,
                fmt.rate_hz,
            );
            p.set_frame_us(fmt.frame_us);
            p
        },
        channels,
        fmt,
        negotiated: None,
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
        // What the graph GRANTED, not what we asked for (`design/hi-res-audio.md` §9's "read back
        // the actual rate and do not assume"). ⚠ Read it for what it is: this is OUR port's
        // format, and a playback stream's adapter converts it to the sink — so a rate that comes
        // back changed means the graph refused our ask outright, while a rate that comes back
        // UNCHANGED still says nothing about the sink behind it. The sink's own rate is a registry
        // lookup this stream does not do (`can_render_at`).
        .param_changed(|_s, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = AudioInfoRaw::default();
            if info.parse(param).is_err() {
                return;
            }
            let now = (info.rate(), info.channels());
            // A resume re-announces the format we already had; that is the graph waking us, not
            // the stream changing.
            if ud.negotiated == Some(now) {
                return;
            }
            ud.negotiated = Some(now);
            if now.0 != 0 && now.0 != ud.fmt.rate_hz {
                tracing::warn!(
                    granted_hz = now.0,
                    resolved_hz = ud.fmt.rate_hz,
                    "PipeWire granted a different playback rate than the session negotiated — \
                     audio will be resampled and the A/V-sync depth arithmetic is denominated in \
                     the negotiated rate"
                );
            } else {
                tracing::info!(
                    format = ?info.format(),
                    rate = now.0,
                    channels = now.1,
                    "playback format negotiated"
                );
            }
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
                // F32LE interleaved, at EVERY rate and depth this client plays — deliberately,
                // and not an oversight left behind by the lossless plane. Core decodes 16- and
                // 24-bit PCM to f32 (`pcm::to_f32`) precisely so one graph format serves both
                // planes; carrying S24 to PipeWire instead would rewrite this whole callback (the
                // stride, the ring, the crossfade helper, the policy's sample arithmetic) to
                // deliver bits that are already exact in the f32 they arrived in. f32 holds all
                // 24 bits of mantissa with room to spare, so nothing is lost by the choice.
                let stride = 4 * ud.channels;
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
                        // From the session's rate, not from 48: a 96 kHz quantum divided by 48
                        // reads as twice the latency it is, in the one line an on-glass latency
                        // report is triaged from.
                        write_ms = want_frames / ud.fmt.frames_per_ms(),
                        rate_hz = ud.fmt.rate_hz,
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
                        // Concealment must be visible next to the underruns it prevented: a
                        // healthy `underruns` bought with a climbing `plc_ms` is a link in
                        // trouble, not a link that is fine.
                        plc_ms = ud.sync.plc_ms(),
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
    info.set_rate(fmt.rate_hz);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(rate_hz: u32, frame_us: u32) -> PlaybackFormat {
        PlaybackFormat {
            channels: 2,
            rate_hz,
            frame_us,
        }
    }

    /// `NODE_LATENCY` is `<frames>/<rate>` and BOTH halves have to move with the session, or the
    /// graph quantum stops being one protocol frame. The 48 kHz/5 ms row is the literal `240/48000`
    /// this replaced — it must still come out byte-identical, because every Opus session depends on
    /// it and none of them may change.
    #[test]
    fn the_graph_quantum_is_one_protocol_frame_at_every_rung() {
        // (rate, frame_us) → frames per channel.
        for (rate, us, want) in [
            (48_000, 5_000, 240), // the Opus plane, unchanged
            (48_000, 4_000, 192), // 48 kHz / 24-bit lossless at the default MTU
            // The fractional-millisecond rung: 2.5 ms is 120 frames at 48 kHz, and computing it
            // in whole milliseconds would truncate to 2 ms and ask for 96.
            (48_000, 2_500, 120),
            (96_000, 5_000, 480),
            (96_000, 3_000, 288), // 96 kHz / 16-bit
            (96_000, 2_000, 192), // 96 kHz / 24-bit
        ] {
            assert_eq!(fmt(rate, us).quantum_frames(), want, "{rate} Hz / {us} µs");
        }
    }

    /// The one-shot quantum log divides by this, and reading it off a constant is how a 96 kHz
    /// session reports twice the latency it actually has in the line a report is triaged from.
    #[test]
    fn frames_per_ms_follows_the_negotiated_rate() {
        assert_eq!(fmt(48_000, 5_000).frames_per_ms(), 48);
        assert_eq!(fmt(96_000, 2_000).frames_per_ms(), 96);
        // A nonsense rate must not divide by zero in a log line.
        assert_eq!(fmt(0, 5_000).frames_per_ms(), 1);
        assert_eq!(fmt(0, 0).quantum_frames(), 1);
    }
}
