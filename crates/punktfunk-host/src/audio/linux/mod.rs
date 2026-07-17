//! PipeWire desktop-audio capture — via a **host-owned stream sink** (default), or the legacy
//! default-sink-monitor follower (`PUNKTFUNK_STREAM_SINK=0`).
//!
//! **Stream-sink mode.** The capture stream registers itself as an `Audio/Sink` node
//! ("Punktfunk Stream Speaker", unique `node.name` per capturer): host apps play *into* it,
//! PipeWire mixes them, and our `process()` callback receives the mix directly — the same
//! stream-node architecture as [`PwMicSource`] below (inverted), and the documented
//! `pw-loopback --capture-props='media.class=Audio/Sink'` virtual-sink recipe. A session-scoped
//! [`stream_sink`] claim makes it the *default* sink so apps route to it (and back) with the
//! session. Why: capture no longer depends on any hardware sink, whose availability is display
//! hardware state — live-diagnosed 2026-07-14 on a bazzite/TV host, every gamescope modeset
//! dropped the HDMI audio endpoint, WirePlumber ping-ponged the default HDMI↔auto_null ~8×/s,
//! and the old monitor-follower relinked on every flip (Paused→renegotiate→Streaming storms =
//! client crackle). Bonus: the sink advertises the session's true channel count, so games can
//! produce real 5.1/7.1 even when the local hardware is stereo.
//!
//! **Legacy mode** connects an input stream with `stream.capture.sink=true`, which routes the
//! *default* sink's monitor into us — no portal needed (unlike screen capture), but coupled to
//! hardware-default churn as above.
//!
//! In both modes the (`!Send`) MainLoop/Stream live on a dedicated thread; interleaved `f32`
//! chunks leave over a bounded channel (dropped if the encoder falls behind, never blocking
//! the PipeWire loop). The stream is opened at the *session's* channel count (2/6/8); in
//! legacy mode PipeWire's channel-mixer fills missing positions with silence (zero upmix).
//! Dropping the capturer quits the loop thread (via a `pipewire::channel` Terminate message),
//! tearing the stream — and in stream-sink mode the sink node itself — down promptly, so a
//! surround session can replace a stereo capturer without leaking a PipeWire consumer (see
//! CLAUDE.md: a wedged link head-blocks the daemon).

mod stream_sink;

use super::{AudioCapturer, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// Message asking the PipeWire loop thread to quit (sent from `Drop`).
struct Terminate;

/// Whether the host-owned stream sink is active. **Default ON** — decouples capture (and app
/// routing) from hardware-sink availability; see the module docs for the live-diagnosed
/// crackle this fixes. `PUNKTFUNK_STREAM_SINK=0` (also `false`/`no`/`off`) is the escape hatch
/// back to capturing the default sink's monitor.
fn stream_sink_enabled() -> bool {
    std::env::var("PUNKTFUNK_STREAM_SINK")
        .map(|v| !matches!(v.trim(), "0" | "false" | "no" | "off"))
        .unwrap_or(true)
}

pub struct PwAudioCapturer {
    chunks: Receiver<Vec<f32>>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
    /// `Some(node.name)` in stream-sink mode; `None` = legacy monitor follower.
    sink_name: Option<String>,
    /// Whether this capturer currently holds a [`stream_sink`] default-sink claim (session
    /// active). Toggled by open/[`drain`](AudioCapturer::drain) (claim) and
    /// [`idle`](AudioCapturer::idle)/Drop (release).
    claimed: bool,
}

impl PwAudioCapturer {
    pub fn open(channels: u32) -> Result<PwAudioCapturer> {
        anyhow::ensure!(
            matches!(channels, 1 | 2 | 6 | 8),
            "unsupported audio channel count {channels} (want 2, 6 or 8)"
        );
        // Unique per capturer: overlapping instances (mid-session reopen, concurrent sessions)
        // must never alias in metadata claims, and a fresh name gets fresh (unity) WirePlumber
        // volume state instead of whatever a previous run left behind.
        let sink_name = stream_sink_enabled().then(|| {
            use std::sync::atomic::AtomicU64;
            static SEQ: AtomicU64 = AtomicU64::new(0);
            format!(
                "{}-{}-{}",
                stream_sink::SINK_NAME_PREFIX,
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            )
        });
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        // Bring-up handshake (mirrors the virtual mic): a PipeWire that isn't running must
        // surface as an open ERROR — engaging the callers' reopen backoff — and in stream-sink
        // mode the sink node must exist before we claim the default to its name.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let thread_sink_name = sink_name.clone();
        thread::Builder::new()
            .name("punktfunk-pw-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(tx, quit_rx, channels, thread_sink_name, ready_tx) {
                    tracing::error!(error = %format!("{e:#}"), "pipewire audio thread failed");
                }
            })
            .context("spawn pipewire audio thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(anyhow!("pipewire audio init timed out")),
        }
        // The capturer opens at session start, so the routing claim begins here; the paired
        // release is `idle()` (parked between sessions) or Drop.
        let claimed = match &sink_name {
            Some(name) => {
                stream_sink::claim(name);
                true
            }
            None => false,
        };
        Ok(PwAudioCapturer {
            chunks: rx,
            channels,
            quit: quit_tx,
            sink_name,
            claimed,
        })
    }
}

impl Drop for PwAudioCapturer {
    fn drop(&mut self) {
        if self.claimed {
            self.claimed = false;
            stream_sink::release();
        }
        // Ask the loop thread to quit; the stream/core/loop unwind there (RAII). A failed
        // send means the thread already exited — nothing to tear down.
        let _ = self.quit.send(Terminate);
    }
}

impl AudioCapturer for PwAudioCapturer {
    fn next_chunk(&mut self) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(Duration::from_secs(5)) {
            Ok(c) => Ok(c),
            // A quiet sink (paused game, idle desktop) is NOT a failure — return an empty chunk so the
            // caller keeps the capturer alive. Only a dead capture thread is an Err (→ caller reopens).
            Err(RecvTimeoutError::Timeout) => Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => Err(anyhow!("pipewire audio thread ended")),
        }
    }

    fn channels(&self) -> u32 {
        self.channels
    }

    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
        // A parked capturer being reused = a new session starting: re-claim the default sink
        // (released by `idle()` when the previous session parked us).
        if let (Some(name), false) = (&self.sink_name, self.claimed) {
            stream_sink::claim(name);
            self.claimed = true;
        }
    }

    fn idle(&mut self) {
        if self.claimed {
            self.claimed = false;
            stream_sink::release();
        }
    }
}

/// SPA channel position array for the GameStream surround order FL FR FC LFE RL RR [SL SR]
/// (= the PipeWire/PulseAudio default map for 6/8 channels, and the order Moonlight's
/// renderers expect — moonlight-common-c: "we use FL FR C LFE RL RR SL SR"). Values are
/// `enum spa_audio_channel` (spa/param/audio/raw.h): FL=3 FR=4 FC=5 LFE=6 SL=7 SR=8 RL=12
/// RR=13.
fn spa_positions(channels: u32) -> [u32; 64] {
    const FL: u32 = 3;
    const FR: u32 = 4;
    const FC: u32 = 5;
    const LFE: u32 = 6;
    const SL: u32 = 7;
    const SR: u32 = 8;
    const RL: u32 = 12;
    const RR: u32 = 13;
    const MONO: u32 = 2;
    let mut pos = [0u32; 64];
    let order: &[u32] = match channels {
        1 => &[MONO],
        2 => &[FL, FR],
        6 => &[FL, FR, FC, LFE, RL, RR],
        8 => &[FL, FR, FC, LFE, RL, RR, SL, SR],
        _ => unreachable!("validated in open()"),
    };
    pos[..order.len()].copy_from_slice(order);
    pos
}

/// Virtual microphone: a PipeWire `Audio/Source` node host apps can record from. The host pushes
/// decoded client-mic PCM in; the loop thread's producer callback drains it (silence on
/// underrun) into PipeWire buffers. Mirrors [`PwAudioCapturer`] but inverted (Direction::Output).
///
/// **Why a stream node and not a `support.null-audio-sink` adapter** (the canonical
/// virtual-mic recipe): tested live on this project's headless graph (PipeWire 1.6.2,
/// 2026-07-03), an adapter with `media.class=Audio/Source/Virtual` never gets a clock — the
/// {source, recorder} group runs with QUANT/RATE 0 and delivers pure silence — and WirePlumber
/// rerouted a feeder stream targeting it to the *default sink* instead (which would play the
/// client's voice out of the speakers, straight into the desktop-audio capture: echo). The
/// stream node below, with `RT_PROCESS` + `priority.session` (see the property comments), is
/// validated working on PipeWire 1.4 (Bazzite) and 1.6 (this box) in both attach orderings.
/// Do not "modernize" this to the adapter recipe without re-running that validation.
///
/// **Liveness contract** (see [`VirtualMic`]): the loop thread exits on a core error (PipeWire
/// daemon restart — the node is gone) or a stream error, which flips `alive` — `push` then
/// returns `false` and the owning pump reopens against the new daemon, recreating the node.
pub struct PwMicSource {
    pcm: std::sync::mpsc::SyncSender<(std::time::Instant, Vec<f32>)>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
    /// False once the loop thread has exited (daemon/stream death or teardown).
    alive: Arc<AtomicBool>,
    /// One-shot flush request, consumed by the process callback (clears the jitter ring).
    flush: Arc<AtomicBool>,
}

impl PwMicSource {
    pub fn open(channels: u32) -> Result<PwMicSource> {
        anyhow::ensure!(
            matches!(channels, 1 | 2),
            "virtual mic supports 1 or 2 channels, got {channels}"
        );
        let (pcm_tx, pcm_rx) = sync_channel::<(std::time::Instant, Vec<f32>)>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let alive = Arc::new(AtomicBool::new(true));
        let flush = Arc::new(AtomicBool::new(false));
        // Bring-up handshake (mirrors the Windows backend): a PipeWire that isn't running (host
        // service started before the user session) must surface as an open ERROR — engaging the
        // pump's backoff — not as an instantly-dead instance the pump would churn on.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let (alive_t, flush_t) = (alive.clone(), flush.clone());
        thread::Builder::new()
            .name("punktfunk-pw-mic".into())
            .spawn(move || {
                if let Err(e) = mic_pw_thread(pcm_rx, quit_rx, channels, flush_t, ready_tx) {
                    // Reaching here is always a setup/open failure (once the mainloop runs it exits
                    // Ok) — and it was already reported to the pump via the ready handshake, which
                    // owns the throttled operator-facing warn. Keep only a debug breadcrumb.
                    tracing::debug!(error = %format!("{e:#}"), "pipewire virtual-mic setup failed — pump will back off and retry");
                }
                // Whether a clean quit or a daemon death: this instance is done — the pump reopens.
                alive_t.store(false, Ordering::Release);
            })
            .context("spawn pipewire virtual-mic thread")?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(PwMicSource {
                pcm: pcm_tx,
                channels,
                quit: quit_tx,
                alive,
                flush,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(anyhow!("pipewire virtual-mic init timed out")),
        }
    }
}

impl Drop for PwMicSource {
    fn drop(&mut self) {
        let _ = self.quit.send(Terminate);
    }
}

impl VirtualMic for PwMicSource {
    fn push(&self, pcm: &[f32]) -> bool {
        if !self.alive.load(Ordering::Acquire) {
            return false;
        }
        // Timestamped so the process callback can age out chunks that sat in the channel while
        // no recorder was attached (see the staleness logic there).
        match self.pcm.try_send((std::time::Instant::now(), pcm.to_vec())) {
            Ok(()) => true,
            // Behind is fine (drop the chunk); a gone receiver means the loop thread exited.
            Err(std::sync::mpsc::TrySendError::Full(_)) => true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        }
    }
    fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    fn discard(&self) {
        self.flush.store(true, Ordering::Release);
    }
    fn channels(&self) -> u32 {
        self.channels
    }
}

/// Producer-side state for the virtual-mic loop: incoming decoded PCM and a small ring buffer
/// the process callback drains into PipeWire buffers (capped, so latency stays bounded).
/// `primed` is a jitter buffer gate — see the process callback.
struct MicUserData {
    rx: Receiver<(std::time::Instant, Vec<f32>)>,
    ring: VecDeque<f32>,
    channels: usize,
    primed: bool,
    /// One-shot flush request from [`PwMicSource::discard`] (stale-audio drop after a gap).
    flush: Arc<AtomicBool>,
    /// When the process callback last ran — a long gap means the ring content predates the
    /// current consumer (the stream idles with no recorder attached) and must be dropped.
    last_run: Option<std::time::Instant>,
}

/// PCM older than this never reaches a recorder: chunks that aged in the channel while no
/// recorder was attached, and ring content from before a consumer gap, are dropped instead of
/// bursting out as stale audio when recording (re)starts.
const MIC_STALE: Duration = Duration::from_secs(1);

fn mic_pw_thread(
    pcm_rx: Receiver<(std::time::Instant, Vec<f32>)>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
    flush: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    // The PipeWire objects are lifetime-chained (guards borrow the mainloop/core), so setup and
    // the blocking run share one frame; the IIFE lets every setup `?` funnel through the ready
    // handshake below (mirrors the Windows render_thread).
    let result = (|| -> Result<()> {
        pf_capture::pwinit::ensure_init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw mic MainLoop")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw mic Context")?;
        let core = context
            .connect_rc(None)
            .context("pw mic connect (is PipeWire running in this session?)")?;

        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Death detection: a core error (the daemon restarted/went away — our remote node no longer
        // exists) ends this thread, flipping the owner's `alive` flag so the pump reopens against the
        // new daemon. Without this, a PipeWire restart left the loop idling on a dead connection and
        // the mic silently broken for the rest of the host's life.
        let _core_listener = core
            .add_listener_local()
            .error({
                let mainloop = mainloop.clone();
                move |id, _seq, res, message| {
                    tracing::warn!(
                        id,
                        res,
                        message,
                        "pipewire core error — virtual mic reopening"
                    );
                    mainloop.quit();
                }
            })
            .register();

        // media.class=Audio/Source advertises us as a microphone (a recordable source), NOT a
        // playback stream — without it, Direction::Output + Playback would route to the speakers.
        let stream = pw::stream::StreamBox::new(
            &core,
            "punktfunk-mic",
            properties! {
                *pw::keys::MEDIA_TYPE        => "Audio",
                *pw::keys::MEDIA_CLASS       => "Audio/Source",
                *pw::keys::NODE_NAME         => "punktfunk-mic",
                *pw::keys::NODE_DESCRIPTION  => "Punktfunk Remote Microphone",
                // ~5 ms quantum (one Opus frame) so recording apps get smooth low-latency chunks.
                *pw::keys::NODE_LATENCY      => "240/48000",
                // Win WirePlumber's default-source election. This fixes TWO failures (both diagnosed
                // live on a Bazzite host, PipeWire 1.4.10):
                //   1. Apps that record the *default* input (games, Discord, arecord) get the client's
                //      mic — the Linux analogue of the Windows host forcing the default recording
                //      endpoint (audio/windows/audio_control.rs). Without it the source is never the
                //      default, so default-input recorders hear silence.
                //   2. On PipeWire 1.4.x, a *non-default* Audio/Source recorded via `--target` never
                //      gets a driver assigned — the {source, recorder} group stays orphaned (pw-top:
                //      QUANT/RATE 0, `driver-node None`), so the RT `process()` callback never fires and
                //      even an explicitly-selected mic is pure silence. Making it the default source
                //      keeps WirePlumber driving it, so `process()` runs and audio flows. (PipeWire 1.6
                //      drives any recorded source regardless, which is why this only bit the 1.4 host.)
                // Reproduced with a faithful standalone copy of this node: no priority.session → silent,
                // priority.session set → audio, on the same 1.4.10 daemon. Only overrides WirePlumber's
                // *auto* default (a user's explicit default.configured.audio.source still wins); the
                // value clears typical real-hardware source priorities (~1000–1900).
                "priority.session"           => "3000",
            },
        )
        .context("pw mic Stream")?;

        let ud = MicUserData {
            rx: pcm_rx,
            ring: VecDeque::new(),
            channels: channels as usize,
            primed: false,
            flush,
            last_run: None,
        };

        let _listener = stream
            .add_local_listener_with_user_data(ud)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, _ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire virtual-mic stream state");
                    // A stream error is unrecoverable for this instance — exit so the pump reopens.
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(|_s, _ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        "virtual-mic format negotiated"
                    );
                }
            })
            .process(|stream, ud| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    // Stale-audio guard, BEFORE pulling new frames: drop the ring when a flush was
                    // requested (uplink gap — see the pump) or when this callback itself hasn't run
                    // for a while (the stream idled with no recorder attached; whatever the ring
                    // holds predates the consumer). A recorder must never hear a burst of old audio.
                    let now = std::time::Instant::now();
                    let idled = ud
                        .last_run
                        .is_some_and(|t| now.duration_since(t) > MIC_STALE);
                    if ud.flush.swap(false, std::sync::atomic::Ordering::AcqRel) || idled {
                        ud.ring.clear();
                        ud.primed = false;
                    }
                    ud.last_run = Some(now);
                    // Pull all newly-decoded PCM into the ring, aging out chunks that sat in the
                    // channel while nothing consumed them (same staleness rule).
                    while let Ok((t, frame)) = ud.rx.try_recv() {
                        if now.duration_since(t) <= MIC_STALE {
                            ud.ring.extend(frame);
                        }
                    }
                    let stride = 4 * ud.channels; // F32LE interleaved
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let data = &mut datas[0];
                    let want_frames = data.data().map(|s| s.len() / stride).unwrap_or(0);
                    let want = want_frames * ud.channels; // interleaved samples this quantum needs
                    static FIRST: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(true);
                    if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        tracing::info!(
                            quantum_frames = want_frames,
                            quantum_ms = want_frames as f32 / 48.0,
                            "virtual-mic consumer connected"
                        );
                    }

                    // Adaptive jitter buffer. The client pushes 5 ms frames; the recorder pulls a
                    // whole *quantum* (often 20–43 ms) from an independent clock. A drain of one
                    // quantum must not outrun what's buffered, or every call underruns to silence
                    // (the original ~58% gaps). So prime to ~3 quanta before producing, hold there,
                    // and re-prime only after a genuine full drain (the client went quiet). The ring
                    // is capped at a few quanta so latency stays bounded.
                    let target = (3 * want).clamp(720 * ud.channels, 9600 * ud.channels);
                    while ud.ring.len() > target.max(want) + want {
                        ud.ring.pop_front(); // bound latency: drop the oldest beyond ~1 quantum slack
                    }
                    if !ud.primed && ud.ring.len() >= target {
                        ud.primed = true;
                    }

                    let n_frames = if let Some(slice) = data.data() {
                        for k in 0..want {
                            let s = if ud.primed {
                                ud.ring.pop_front().unwrap_or(0.0) // silence on a momentary underrun
                            } else {
                                0.0 // not yet primed — emit silence while the buffer fills
                            };
                            let off = k * 4;
                            slice[off..off + 4].copy_from_slice(&s.to_le_bytes());
                        }
                        want_frames
                    } else {
                        0
                    };
                    if ud.ring.is_empty() {
                        ud.primed = false; // fully drained — re-prime before producing again
                    }
                    let chunk = data.chunk_mut();
                    *chunk.offset_mut() = 0;
                    *chunk.stride_mut() = stride as _;
                    *chunk.size_mut() = (stride * n_frames) as _;
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire virtual-mic callback");
                }
            })
            .register()
            .context("register virtual-mic stream listener")?;

        let mut info = AudioInfoRaw::new();
        info.set_format(AudioFormat::F32LE);
        info.set_rate(SAMPLE_RATE);
        info.set_channels(channels);
        info.set_position(spa_positions(channels));
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

        // RT_PROCESS: run the producer callback on PipeWire's realtime data loop, so the source is a
        // *synchronous* graph node that joins its consumer's driver group and is actually driven. Without
        // it the node is async/main-loop and, in the host's busy multi-stream graph (desktop-audio +
        // video capture + the session), never acquires a driver — it stays suspended and its process()
        // never fires, so every recorder hears pure silence (the long-standing "Linux host mic broken").
        stream
            .connect(
                spa::utils::Direction::Output, // we PRODUCE samples (a source)
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("pw mic stream connect")?;

        // Setup complete: the daemon connection and stream connect succeeded — report ready,
        // then block until quit/death. (A PipeWire that isn't running never reaches this line;
        // its connect error surfaces through the handshake as an OPEN failure, so the pump
        // backs off instead of churning on instantly-dead instances.)
        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire virtual-mic loop exited (source dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}

fn pw_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
    sink_name: Option<String>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    // Setup errors funnel through the ready handshake (mirrors mic_pw_thread's IIFE).
    let result = (|| -> Result<()> {
        pf_capture::pwinit::ensure_init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw audio MainLoop")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw audio Context")?;
        let core = context
            .connect_rc(None)
            .context("pw audio connect (is PipeWire running in this session?)")?;

        // Cross-thread teardown: the capturer's Drop sends Terminate; quit the loop here.
        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Death detection (same contract as the virtual mic below): a core error — the daemon
        // restarted/went away — ends this thread, so the chunk channel disconnects and
        // `next_chunk` returns Err, engaging the sessions' reopen-with-backoff. Without this, a
        // PipeWire restart mid-session left a zombie capture thread whose `next_chunk` returned
        // quiet-sink empty chunks forever — audio silently dead for the rest of the session.
        let _core_listener = core
            .add_listener_local()
            .error({
                let mainloop = mainloop.clone();
                move |id, _seq, res, message| {
                    tracing::warn!(id, res, message, "pipewire core error — audio capture ends");
                    mainloop.quit();
                }
            })
            .register();

        let props = match &sink_name {
            // Stream-sink mode: this stream IS the sink (media.class + Direction::Input). Apps
            // play into it, PipeWire mixes them, process() receives the mix. Mirrors the
            // validated PwMicSource recipe (stream node + RT_PROCESS; see its property
            // comments) — do NOT "modernize" either into a `support.null-audio-sink` adapter
            // without re-running that validation.
            Some(name) => {
                let mut p = properties! {
                    *pw::keys::MEDIA_TYPE       => "Audio",
                    *pw::keys::MEDIA_CLASS      => "Audio/Sink",
                    *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream Speaker",
                    *pw::keys::NODE_VIRTUAL     => "true",
                    // Ask for a ~5ms quantum (= one Opus frame) so buffers arrive smoothly
                    // rather than in bursts the client's jitter buffer would hear as glitching.
                    *pw::keys::NODE_LATENCY     => "240/48000",
                    // LOW priority — the opposite of the mic's 3000: between sessions the sink
                    // node stays alive (parked capturer) but must never win WirePlumber's auto
                    // default election against real hardware; session routing comes from the
                    // stream_sink claim, not from priority.
                    "priority.session"          => "50",
                };
                p.insert(*pw::keys::NODE_NAME, name.as_str());
                p
            }
            // Legacy: capture the default sink's monitor (system output), not a microphone.
            None => properties! {
                *pw::keys::MEDIA_TYPE          => "Audio",
                *pw::keys::MEDIA_CATEGORY      => "Capture",
                *pw::keys::MEDIA_ROLE          => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                *pw::keys::NODE_LATENCY        => "240/48000",
            },
        };
        let stream = pw::stream::StreamBox::new(&core, "punktfunk-audio", props)
            .context("pw audio Stream")?;

        let _listener = stream
            .add_local_listener_with_user_data(tx)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, _ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire audio stream state");
                    // A stream error is unrecoverable for this instance — exit so the sessions'
                    // reopen path builds a fresh one (same contract as the core-error path above).
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(|_stream, _tx, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        "audio format negotiated"
                    );
                }
            })
            .process(|stream, tx| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        return;
                    }
                    let d = &mut datas[0];
                    let (offset, size) = {
                        let c = d.chunk();
                        (c.offset() as usize, c.size() as usize)
                    };
                    let Some(buf) = d.data() else { return };
                    if offset > buf.len() {
                        return;
                    }
                    let region = &buf[offset..(offset + size).min(buf.len())];
                    // Negotiated as F32LE; reinterpret the byte region as interleaved f32.
                    let n = region.len() / 4;
                    static FIRST: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(true);
                    if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        tracing::info!(samples = n, "audio first capture buffer");
                    }
                    let mut samples = Vec::with_capacity(n);
                    for i in 0..n {
                        let b = [
                            region[i * 4],
                            region[i * 4 + 1],
                            region[i * 4 + 2],
                            region[i * 4 + 3],
                        ];
                        samples.push(f32::from_le_bytes(b));
                    }
                    let _ = tx.try_send(samples); // drop if the encoder is behind
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire audio callback — chunk dropped");
                }
            })
            .register()
            .context("register audio stream listener")?;

        // Request F32LE, 48 kHz, at the session's channel count with explicit positions. In
        // legacy mode PipeWire's channel-mixer up/downmixes the sink monitor to this layout;
        // in stream-sink mode this IS the sink's advertised layout (apps mix/route to it).
        let mut info = AudioInfoRaw::new();
        info.set_format(AudioFormat::F32LE);
        info.set_rate(SAMPLE_RATE);
        info.set_channels(channels);
        info.set_position(spa_positions(channels));
        let obj = pw::spa::pod::Object {
            type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: pw::spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        };
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .context("serialize audio format pod")?
        .0
        .into_inner();
        let mut params = [Pod::from_bytes(&values).context("audio pod from bytes")?];

        // RT_PROCESS in stream-sink mode for the same reason as the mic: the sink must be a
        // *synchronous* graph node that joins its producers' driver group and is actually
        // driven (see the mic's connect comment — async device-class stream nodes on a busy
        // graph never acquire a driver and their process() never fires).
        let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
        if sink_name.is_some() {
            flags |= pw::stream::StreamFlags::RT_PROCESS;
        }
        stream
            .connect(
                spa::utils::Direction::Input, // we CONSUME samples (a sink / a monitor tap)
                None,                         // PW_ID_ANY — legacy mode: the default sink monitor
                flags,
                &mut params,
            )
            .context("pw audio stream connect")?;

        // Setup complete: the daemon connection and stream connect succeeded — report ready,
        // then block until quit/death. (The connect is async server-side; if the caller's
        // default-sink claim lands a few ms before the node registers, WirePlumber simply
        // keeps the configured value and elects it the moment the node appears — verified
        // live: configured values persist unelected while their target is absent.)
        let _ = ready.send(Ok(()));
        mainloop.run();
        tracing::debug!("pipewire audio loop exited (capturer dropped)");
        Ok(())
    })();
    if let Err(e) = &result {
        let _ = ready.send(Err(anyhow!("{e:#}")));
    }
    result
}
