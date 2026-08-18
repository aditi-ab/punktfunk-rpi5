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

mod monitor_rate;
mod pad_card_volume;
pub(crate) mod pad_sink;
pub(crate) mod pad_usb;
mod stream_sink;

use super::{AudioCapturer, MicBackendStats, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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

/// §8.4 condition 4 on Linux (`design/hi-res-audio.md` §4.4 / §8.3). The two capture modes give
/// structurally different answers, and that difference is the whole content of §4.4:
///
/// * **Stream-sink mode (the default).** We register the `Audio/Sink` node ourselves and
///   [`pw_thread`] declares its format, so applications render into it at that rate natively.
///   The rate we claim is the rate we get, by construction — there is no upstream resampler in
///   the path to lie about it, so the answer is yes for every rate the plane supports, and no
///   probe of any kind is needed to say so.
/// * **`PUNKTFUNK_STREAM_SINK=0` (monitor mode).** We capture somebody else's sink through
///   PipeWire's resampler, which reports a clean rate whatever the node upstream of it really
///   runs at — the same blindness WASAPI's autoconvert has. So the answer cannot come from our
///   own stream; it comes from the MONITORED NODE, read out of the registry by
///   [`monitor_rate::monitored_sink_rate`]. A rate that can be read is an
///   [`Engine`](super::CaptureRate::Engine) answer, exactly as a Windows endpoint's mix format
///   is, and the gate compares the request against it.
///
/// ⚠ **The two failure directions are not symmetric, and the code leans on that.** An unreadable
/// rate — a suspended sink, an unset metadata key, a node that vanished, a graph that did not
/// answer inside the probe's timeout — is [`Unknown`](super::CaptureRate::Unknown), which
/// declines and costs the session nothing but today's excellent Opus 48 kHz. A *guessed* rate
/// that turns out wrong costs a session that advertises 96 kHz, spends 4.6 Mbps on it, and
/// carries interpolated 48 kHz with both ends auditing clean. So this never guesses: there is no
/// "assume the graph default", no reading `EnumFormat` (a capability, not a fact), and no
/// falling back to the rate we asked for.
///
/// Note the asymmetry with Windows on purpose: there *every* answer needs a device query, here
/// only the monitor mode does, because in the default mode the host is the one declaring the
/// format.
pub(super) fn probe_capture_rate() -> super::CaptureRate {
    if stream_sink_enabled() {
        return super::CaptureRate::Declared;
    }
    match monitor_rate::monitored_sink_rate() {
        Ok(rate_hz) => {
            tracing::debug!(
                rate_hz,
                "hi-res capture-rate probe: the sink this host would monitor runs at this rate"
            );
            super::CaptureRate::Engine(rate_hz)
        }
        Err(e) => {
            tracing::debug!(
                reason = %format!("{e:#}"),
                "hi-res capture-rate probe: the monitored sink's own rate is not readable — \
                 declining hi-res (PUNKTFUNK_STREAM_SINK=0 captures through PipeWire's resampler, \
                 so the rate our own stream reports proves nothing)"
            );
            super::CaptureRate::Unknown
        }
    }
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
    /// Whether a session is currently CONSUMING this capturer, shared with the PipeWire
    /// thread so the drop counter can tell "the encode thread fell behind" from "nobody is
    /// reading". The capturer is host-lifetime and merely PARKED between sessions
    /// ([`idle`](AudioCapturer::idle)), so without this the producer keeps filling the bounded
    /// hand-off channel, every `try_send` fails once it is full, and the plane reports a 100 %
    /// drop rate — warning that "the stream will click" when there is no stream. A 2026-08-13
    /// field host log carried ten such warnings, up to `dropped_chunks=11251` (= 30 s × 375
    /// chunks/s, i.e. every single chunk), each one straddling a session boundary and each one
    /// meaningless. Distinct from `claimed`, which tracks the sink-routing claim and only
    /// exists when the stream sink is enabled at all.
    active: Arc<AtomicBool>,
    /// The rate the graph actually NEGOTIATED, written by the format callback on the PipeWire
    /// thread and read back by [`AudioCapturer::sample_rate`].
    ///
    /// Seeded with the rate we asked for, because that is the honest answer until the graph has
    /// said otherwise — and in stream-sink mode it is nearly always the final one, since the
    /// host owns the sink and declares its format (`design/hi-res-audio.md` §4.4). In legacy
    /// monitor mode the value is a weaker claim: it is the rate of the resampled stream we are
    /// handed, not of the node upstream of it, which is why the §8.3 gate reads the monitored
    /// node's own rate out of the registry ([`monitor_rate`]) rather than trusting this number.
    negotiated_rate: Arc<AtomicU32>,
}

impl PwAudioCapturer {
    pub fn open(channels: u32, rate_hz: u32) -> Result<PwAudioCapturer> {
        anyhow::ensure!(
            matches!(channels, 1 | 2 | 6 | 8),
            "unsupported audio channel count {channels} (want 2, 6 or 8)"
        );
        anyhow::ensure!(rate_hz > 0, "audio capture rate must be positive");
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
        // Opens at session start (see the routing claim below), so the consumer is live from
        // the first chunk.
        let active = Arc::new(AtomicBool::new(true));
        let thread_active = Arc::clone(&active);
        let negotiated_rate = Arc::new(AtomicU32::new(rate_hz));
        let thread_rate = Arc::clone(&negotiated_rate);
        thread::Builder::new()
            .name("punktfunk-pw-audio".into())
            .spawn(move || {
                if let Err(e) = pw_thread(
                    tx,
                    quit_rx,
                    channels,
                    rate_hz,
                    thread_sink_name,
                    ready_tx,
                    thread_active,
                    thread_rate,
                ) {
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
            active,
            negotiated_rate,
        })
    }
}

impl Drop for PwAudioCapturer {
    fn drop(&mut self) {
        // The receiver dies with us; anything the producer still pushes is unwanted by
        // definition, and it must not be reported as the encode thread falling behind.
        self.active.store(false, Ordering::Relaxed);
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
        self.next_chunk_within(Duration::from_secs(5))
    }

    fn next_chunk_within(&mut self, budget: Duration) -> Result<Vec<f32>> {
        match self.chunks.recv_timeout(budget) {
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

    fn sample_rate(&self) -> u32 {
        self.negotiated_rate.load(Ordering::Relaxed)
    }

    fn drain(&mut self) {
        while self.chunks.try_recv().is_ok() {}
        // A parked capturer being reused = a new session starting: re-claim the default sink
        // (released by `idle()` when the previous session parked us).
        if let (Some(name), false) = (&self.sink_name, self.claimed) {
            stream_sink::claim(name);
            self.claimed = true;
        }
        // Ordered AFTER the backlog drain, so the producer never counts a drop against a
        // channel this call is still emptying.
        self.active.store(true, Ordering::Relaxed);
    }

    fn idle(&mut self) {
        // Parked: from here the channel fills and stays full, and those drops are nobody's
        // fault. See `PwAudioCapturer::active`.
        self.active.store(false, Ordering::Relaxed);
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
    /// Ring policy/telemetry shared with the RT process callback (see [`MicRingShared`]).
    ring: Arc<MicRingShared>,
}

/// Atomics shared between [`PwMicSource`] (the pump's side) and the RT process callback: the
/// pump's adaptive de-jitter target in, ring depth/prime gauges + reset-on-read counters out.
/// All `Relaxed` — a slowly-moving target and telemetry, not synchronization.
#[derive(Default)]
struct MicRingShared {
    /// Pump-set jitter target (per-channel samples). `0` = the pump never spoke (legacy mode,
    /// or its first estimate hasn't landed) → the callback keeps the historical 3-quanta clamp.
    target: AtomicUsize,
    /// Ring depth after the last callback (per-channel samples).
    depth: AtomicUsize,
    /// Effective prime target of the last callback (per-channel samples).
    prime: AtomicUsize,
    /// Full-drain re-prime arms (see [`MicBackendStats`]).
    reprimes: AtomicU64,
    /// Per-channel samples dropped by the overflow cap.
    overflow: AtomicU64,
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
        let ring = Arc::new(MicRingShared::default());
        let (alive_t, flush_t, ring_t) = (alive.clone(), flush.clone(), ring.clone());
        thread::Builder::new()
            .name("punktfunk-pw-mic".into())
            .spawn(move || {
                if let Err(e) = mic_pw_thread(pcm_rx, quit_rx, channels, flush_t, ring_t, ready_tx)
                {
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
                ring,
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
    fn set_target_depth(&self, samples_per_ch: usize) {
        self.ring.target.store(samples_per_ch, Ordering::Relaxed);
    }
    fn depth(&self) -> Option<(usize, usize)> {
        let prime = self.ring.prime.load(Ordering::Relaxed);
        // 0 = the process callback hasn't run yet (no consumer) — nothing meaningful to report.
        (prime > 0).then(|| (self.ring.depth.load(Ordering::Relaxed), prime))
    }
    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats {
            reprimes: self.ring.reprimes.swap(0, Ordering::Relaxed),
            overflow_dropped: self.ring.overflow.swap(0, Ordering::Relaxed),
        }
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
    /// Pump-driven ring policy + telemetry (see [`MicRingShared`]).
    shared: Arc<MicRingShared>,
    /// When the process callback last ran — a long gap means the ring content predates the
    /// current consumer (the stream idles with no recorder attached) and must be dropped.
    last_run: Option<std::time::Instant>,
}

/// PCM older than this never reaches a recorder: chunks that aged in the channel while no
/// recorder was attached, and ring content from before a consumer gap, are dropped instead of
/// bursting out as stale audio when recording (re)starts.
const MIC_STALE: Duration = Duration::from_secs(1);

/// The graph quantum every punktfunk PipeWire stream asks for, in frames: 240 @ 48 kHz = 5 ms,
/// one protocol audio frame. Named so the `NODE_LATENCY` request and the code that CHECKS whether
/// the request was honoured cannot drift apart — the check is only meaningful while it compares
/// against the same number the ask used.
const CAPTURE_QUANTUM_FRAMES: u32 = 240;

/// [`CAPTURE_QUANTUM_FRAMES`] restated at `rate_hz` — the same 5 ms of wall time, whatever the
/// rate. A hi-res session captures at 96 kHz (`design/hi-res-audio.md` §3), where asking for a
/// flat 240 frames would silently halve the quantum to 2.5 ms and double the callback rate for
/// no reason anyone intended; the ask is a LATENCY, and latency is what has to stay constant.
///
/// The desktop-capture site is the only one that takes a negotiated rate. The virtual mic
/// (voice, always 48 kHz) and the pad sinks (DualSense hardware is 48 kHz) keep the constant.
fn capture_quantum_frames(rate_hz: u32) -> u32 {
    // Integer maths on both shipping rates: 48 000/48 000 × 240 = 240, 96 000/48 000 × 240 = 480.
    // `max(1)` only guards a nonsense rate from producing a zero-frame ask.
    ((CAPTURE_QUANTUM_FRAMES as u64 * rate_hz as u64 / SAMPLE_RATE as u64) as u32).max(1)
}

/// Callbacks that must agree on a new buffer size before it replaces the one gaps are scored
/// against. Three is enough to reject a boundary artefact and still adopt a genuine re-plan
/// within ~15 ms.
const QUANTUM_CONFIRM: u8 = 3;

fn mic_pw_thread(
    pcm_rx: Receiver<(std::time::Instant, Vec<f32>)>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
    flush: Arc<AtomicBool>,
    shared: Arc<MicRingShared>,
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
            shared,
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

                    // Jitter buffer. The client pushes frames on its own clock; the recorder
                    // pulls a whole *quantum* (often 20–43 ms) from an independent one. A drain
                    // of one quantum must not outrun what's buffered, or every call underruns
                    // to silence (the original ~58% gaps). Prime target = one quantum (the pull
                    // granularity) + the pump's measured-jitter target (arrival burstiness —
                    // see `mic_jitter`), re-priming only after a genuine full drain (the client
                    // went quiet). Until the pump's first estimate lands — and forever under
                    // PUNKTFUNK_MIC_LEGACY_BUFFER — the historical 3-quanta clamp applies; that
                    // formula scaled with the RECORDING APP's quantum, so a 2048-frame recorder
                    // bought 128+ ms of standing mic latency for jitter it never had.
                    let pump_target = ud.shared.target.load(Ordering::Relaxed) * ud.channels;
                    let target = if pump_target == 0 {
                        (3 * want).clamp(720 * ud.channels, 9600 * ud.channels)
                    } else {
                        want + pump_target
                    };
                    let mut dropped = 0usize;
                    while ud.ring.len() > target.max(want) + want {
                        ud.ring.pop_front(); // bound latency: drop the oldest beyond ~1 quantum slack
                        dropped += 1;
                    }
                    if dropped > 0 {
                        ud.shared
                            .overflow
                            .fetch_add((dropped / ud.channels) as u64, Ordering::Relaxed);
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
                    if ud.primed && ud.ring.is_empty() {
                        ud.primed = false; // fully drained — re-prime before producing again
                        ud.shared.reprimes.fetch_add(1, Ordering::Relaxed);
                    }
                    // Publish depth/prime for the pump's creep trim + telemetry.
                    ud.shared
                        .depth
                        .store(ud.ring.len() / ud.channels, Ordering::Relaxed);
                    ud.shared
                        .prime
                        .store(target / ud.channels, Ordering::Relaxed);
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

#[allow(clippy::too_many_arguments)]
fn pw_thread(
    tx: std::sync::mpsc::SyncSender<Vec<f32>>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
    rate_hz: u32,
    sink_name: Option<String>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
    active: Arc<AtomicBool>,
    negotiated_rate: Arc<AtomicU32>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;
    // ⚠ This boosts the MAINLOOP thread, which is NOT where the capture callback runs.
    //
    // The previous comment here asserted the opposite ("we never hand PipeWire a separate data
    // loop"), and it was wrong: we pass `RT_PROCESS` below, so libpipewire runs `process()` on a
    // data loop it creates and schedules itself. Measured in one live host process on 2026-08-15
    // — this thread at SCHED_OTHER/nice 0, `data-loop.0` at SCHED_RR/20. That mattered more than
    // a stale comment usually does: a field investigation read the boost's success line as
    // evidence that the audio callback was prioritised, and spent a round concluding priorities
    // were "engaged but insufficient" when they had never been applied to the thread in question.
    //
    // The boost is kept — this thread still dispatches state and format events, and it IS the
    // capture thread when `PUNKTFUNK_STREAM_SINK=0` selects the legacy monitor path. What replaces
    // the assumption is a measurement: the callback reports its own scheduling on first entry.
    pf_frame::thread_qos::boost_thread_priority(true);

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

        // Which source the negotiated format below actually describes — see the note there.
        let sink_mode = sink_name.is_some();
        // The `NODE_LATENCY` ask, built at run time because the rate is now a session value:
        // `<quantum frames>/<rate>` is how PipeWire spells a latency, and both halves move
        // together so the ask stays 5 ms at 48 kHz and at 96 kHz alike. Formatted once here
        // rather than at each use so the two property arms cannot drift apart.
        let node_latency = format!("{}/{}", capture_quantum_frames(rate_hz), rate_hz);
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
                    // LOW priority — the opposite of the mic's 3000: between sessions the sink
                    // node stays alive (parked capturer) but must never win WirePlumber's auto
                    // default election against real hardware; session routing comes from the
                    // stream_sink claim, not from priority.
                    "priority.session"          => "50",
                    // Never let the session manager suspend this node (WP-B2). The 2026-08-15
                    // field log shows three `audio format negotiated` lines in the first minute,
                    // each wrapped in a Paused↔Streaming flap: Wine churns its audio device at
                    // launch, the sink goes briefly unused, WirePlumber suspends it on its idle
                    // timeout, and the next app resumes it — and every one of those round trips
                    // is a real hole in a stream someone is listening to.
                    //
                    // Deliberately NOT `node.always-process`: that keeps the node SCHEDULED with
                    // nothing connected, so a host sitting between sessions would run this
                    // callback two hundred times a second forever (risk R5). Disabling the
                    // suspend keeps the node available without asking anyone to drive it.
                    "session.suspend-timeout-seconds" => "0",
                };
                p.insert(*pw::keys::NODE_NAME, name.as_str());
                // Ask for a ~5 ms quantum (= one protocol audio frame) so buffers arrive
                // smoothly rather than in bursts the client's jitter buffer would hear as
                // glitching. Inserted rather than written in the `properties!` literal because
                // the rate is negotiated — same reason as `NODE_NAME` above.
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
            // Legacy: capture the default sink's monitor (system output), not a microphone.
            None => {
                let mut p = properties! {
                    *pw::keys::MEDIA_TYPE          => "Audio",
                    *pw::keys::MEDIA_CATEGORY      => "Capture",
                    *pw::keys::MEDIA_ROLE          => "Music",
                    *pw::keys::STREAM_CAPTURE_SINK => "true",
                };
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
        };
        let stream = pw::stream::StreamBox::new(&core, "punktfunk-audio", props)
            .context("pw audio Stream")?;

        // The capture callback's state: the hand-off channel plus this plane's vitals. Before
        // this it was the bare `tx`, and the desktop-audio plane logged NOTHING between "capture
        // started" and the session ending — no level, no cadence, and in particular no sign of
        // the silent drop below. That is exactly what made the 2026-08-03 Windows field report
        // un-triageable, and the Linux half kept it after the Windows half was fixed.
        struct CapUd {
            tx: std::sync::mpsc::SyncSender<Vec<f32>>,
            channels: u32,
            stats: crate::audio::capture_policy::CaptureStats,
            last_stats: std::time::Instant,
            /// Frames per callback the graph is currently handing us, `0` until the first is
            /// confirmed. Per-open, not a process-wide latch: a host runs for days across many
            /// sessions, so a process-wide form reported the very first capture and then never
            /// again — the one number that identifies a clamped quantum, invisible on every
            /// subsequent open (including every reopen after a device change).
            quantum_frames: usize,
            /// A buffer size seen but not yet believed, with how many callbacks in a row have
            /// agreed on it. Stops one short buffer from moving the gap threshold.
            quantum_candidate: Option<(usize, u8)>,
            /// Whether this open has reported the scheduling of the thread running `process()`.
            reported_sched: bool,
            /// When the callback last ran (WP-A2), so its CADENCE can be scored and not just its
            /// content. Cleared across a state transition — a deliberate Paused span must not
            /// read as one enormous hole. Lives here rather than in `stats` because the stats
            /// reset every reporting window and the cadence does not.
            last_cb: Option<std::time::Instant>,
            /// The quantum actually negotiated, which is what a gap is measured against. Seeded
            /// with the one we ASK for, and corrected on the first callback that carries data —
            /// on a graph that clamped us to 1024 frames, 21.3 ms between callbacks is the deal
            /// we got, not a fault.
            quantum: Duration,
            /// The format currently negotiated, so a renegotiation that resolves to the SAME one
            /// can be told from a real change (WP-B2).
            negotiated: Option<(spa::param::audio::AudioFormat, u32, u32)>,
            /// Shared with the capturer — see [`PwAudioCapturer::active`]. Read on every
            /// failed hand-off to keep parked-capturer backpressure out of the drop count.
            active: Arc<AtomicBool>,
            /// When the stream last left `Streaming`, so the span can be charged to the window
            /// that the span itself stretched. `None` while streaming.
            paused_since: Option<std::time::Instant>,
            /// The rate every frames↔time conversion below is denominated in. A session value
            /// now, not the module constant: at 96 kHz a hardcoded 48 000 would report every
            /// quantum as twice its real duration and `delivered_pct` as half of what arrived.
            rate_hz: u32,
            /// Shared with the capturer — see [`PwAudioCapturer::negotiated_rate`].
            negotiated_rate: Arc<AtomicU32>,
        }
        let ud = CapUd {
            tx,
            channels,
            stats: Default::default(),
            last_stats: std::time::Instant::now(),
            quantum_frames: 0,
            quantum_candidate: None,
            reported_sched: false,
            last_cb: None,
            quantum: Duration::from_micros(
                capture_quantum_frames(rate_hz) as u64 * 1_000_000 / rate_hz as u64,
            ),
            negotiated: None,
            active,
            paused_since: None,
            rate_hz,
            negotiated_rate,
        };
        let _listener = stream
            .add_local_listener_with_user_data(ud)
            .state_changed({
                let mainloop = mainloop.clone();
                move |_s, ud, old, new| {
                    tracing::debug!(?old, ?new, "pipewire audio stream state");
                    // Any transition ends the cadence we were measuring: the span across a
                    // Paused↔Streaming flap is not a gap in delivery, it is a gap in the stream
                    // existing. Scoring it would report one huge hole per renegotiation and bury
                    // the sub-10 ms ones the field log is actually about (WP-A2).
                    ud.last_cb = None;
                    // …but it still has to be reported, because the reporting window is flushed
                    // from the process callback and therefore stretches by the whole span. Charge
                    // it to the window flushed after the resume — the same window it diluted.
                    // Without this the line says `delivered_pct=4 gaps=0` and cannot say whether
                    // that is a dead capture path or a sink nobody was rendering into; the
                    // 2026-08-15 field logs are 40 s of exactly that ambiguity per session start.
                    match new {
                        pw::stream::StreamState::Streaming => {
                            if let Some(since) = ud.paused_since.take() {
                                ud.stats.observe_pause(since.elapsed());
                            }
                        }
                        _ => {
                            ud.paused_since.get_or_insert_with(std::time::Instant::now);
                        }
                    }
                    // A stream error is unrecoverable for this instance — exit so the sessions'
                    // reopen path builds a fresh one (same contract as the core-error path above).
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        mainloop.quit();
                    }
                }
            })
            .param_changed(move |_stream, ud, id, param| {
                let Some(param) = param else { return };
                if id != pw::spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut info = AudioInfoRaw::default();
                if info.parse(param).is_ok() {
                    // Renegotiating to the format we already had is the graph resuming us, not
                    // the stream changing (WP-B2): the field log's minute-1 burst was three of
                    // these, which read as three format changes and were none. Say which it was
                    // — the flap itself stays visible in the state DEBUG lines and in A2's gap
                    // counters, where it belongs.
                    let now = (info.format(), info.rate(), info.channels());
                    if ud.negotiated == Some(now) {
                        tracing::debug!(
                            format = ?now.0,
                            rate = now.1,
                            channels = now.2,
                            "audio format renegotiated, unchanged (the graph resumed our sink)"
                        );
                        return;
                    }
                    ud.negotiated = Some(now);
                    // Report what was GRANTED, not what was asked for
                    // (`design/hi-res-audio.md` §8.1). Everything downstream — the `Welcome`'s
                    // resolved rate, the encode loop's samples-per-frame, the client's device
                    // open — has to follow the same number, and this callback is the only place
                    // the graph ever states it. A rate of `0` means the pod carried none;
                    // keeping the previous value is right there, because "unstated" is not a
                    // claim that the rate changed.
                    if now.1 != 0 {
                        ud.rate_hz = now.1;
                        ud.negotiated_rate.store(now.1, Ordering::Relaxed);
                    }
                    // `stream_sink` says WHICH source this format describes, and that changes how
                    // much it is worth. In stream-sink mode the host owns the sink, so this IS the
                    // format apps render into and the desktop mix cannot have been narrowed before
                    // we saw it. In LEGACY monitor mode we are capturing someone else's sink
                    // through PipeWire's resampler: a 16 kHz Bluetooth headset upstream would
                    // still be reported here as a clean 48 kHz, exactly the way WASAPI's
                    // autoconvert hid the same thing on Windows (the 2026-08-03 report). So this
                    // line is a fact about OUR stream and never about the content in legacy mode
                    // — the monitored node's own rate is a registry lookup, and it lives in
                    // `monitor_rate`, where the hi-res gate reads it before the `Welcome`.
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        stream_sink = sink_mode,
                        "audio format negotiated"
                    );
                }
            })
            .process(|stream, ud| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Score the ARRIVAL before anything can make us return: a callback that ran
                    // and carried nothing still proves the callback ran, and that is a different
                    // fault from one that never ran at all. Stamped first, so the two are
                    // counted independently (WP-A2).
                    let now = std::time::Instant::now();
                    let since_last = ud.last_cb.map(|t| now.duration_since(t));
                    ud.last_cb = Some(now);
                    ud.stats.observe_callback(since_last, ud.quantum);

                    if !ud.reported_sched {
                        ud.reported_sched = true;
                        // Say what the thread that ACTUALLY runs this callback is scheduled as.
                        // Whether the capture callback is realtime decides whether a Wine shader
                        // storm can deschedule it for tens of ms at a 2.7 ms quantum, and until
                        // now no log anywhere carried the answer — only that we had asked for a
                        // boost, on a different thread. Once per open, off the hot path after
                        // that.
                        let (policy, rt_priority, nice) =
                            pf_frame::thread_qos::current_thread_sched();
                        tracing::info!(
                            policy,
                            rt_priority,
                            nice,
                            "audio capture callback scheduling"
                        );
                    }

                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        ud.stats.missed_dequeues += 1;
                        return;
                    };
                    let datas = buffer.datas_mut();
                    if datas.is_empty() {
                        ud.stats.missed_dequeues += 1;
                        return;
                    }
                    let d = &mut datas[0];
                    let (offset, size) = {
                        let c = d.chunk();
                        (c.offset() as usize, c.size() as usize)
                    };
                    let Some(buf) = d.data() else {
                        ud.stats.missed_dequeues += 1;
                        return;
                    };
                    if offset > buf.len() {
                        ud.stats.missed_dequeues += 1;
                        return;
                    }
                    let region = &buf[offset..(offset + size).min(buf.len())];
                    // Negotiated as F32LE; reinterpret the byte region as interleaved f32.
                    let n = region.len() / 4;
                    // Track the quantum the graph is ACTUALLY handing us, not merely the first one
                    // it ever did. The graph re-plans whenever anything else on the box asks for a
                    // different latency, and latching the first callback of the open left every
                    // subsequent gap scored against a buffer size that no longer existed — a
                    // silent corruption of the one metric this whole diagnosis rests on. A new
                    // size has to survive `QUANTUM_CONFIRM` callbacks before it is believed,
                    // because one short buffer at a boundary is not a new deal.
                    let frames = n / (ud.channels.max(1) as usize);
                    if frames > 0 && frames != ud.quantum_frames {
                        let streak = match ud.quantum_candidate {
                            Some((f, c)) if f == frames => c.saturating_add(1),
                            _ => 1,
                        };
                        if streak < QUANTUM_CONFIRM {
                            ud.quantum_candidate = Some((frames, streak));
                        } else {
                            let was = ud.quantum_frames;
                            ud.quantum_frames = frames;
                            ud.quantum_candidate = None;
                            // What a gap is measured against from here on — see `CapUd::quantum`.
                            ud.quantum = Duration::from_micros(
                                frames as u64 * 1_000_000 / ud.rate_hz.max(1) as u64,
                            );
                            let want = capture_quantum_frames(ud.rate_hz) as usize;
                            let negotiated_ms =
                                format!("{:.1}", frames as f32 * 1000.0 / ud.rate_hz.max(1) as f32);
                            if was != 0 {
                                // A mid-open change. Rare, and worth a line of its own: it moves
                                // the gap threshold under a reader who is comparing windows.
                                tracing::info!(
                                    previous_frames = was,
                                    negotiated_frames = frames,
                                    negotiated_ms,
                                    "the audio graph re-planned our quantum mid-stream"
                                );
                            } else if frames > want {
                                // What we ASKED for vs what PipeWire actually handed us. Stating
                                // only the result ("samples=2048") reads as a fact about the
                                // device; stating it next to the request is what makes a clamp
                                // legible. A VM is the common cause — stock `pipewire.conf` raises
                                // `default.clock.min-quantum` to 1024 whenever `cpu.vm.name` is
                                // set, so a 5 ms ask silently becomes 21.3 ms and the audio plane
                                // starts arriving in bursts. That cost a whole field
                                // investigation to find; it should cost one log line.
                                tracing::warn!(
                                    requested_frames = want,
                                    negotiated_frames = frames,
                                    negotiated_ms,
                                    "the audio graph refused our low-latency quantum — capture \
                                     arrives in bursts this size, and the client must buffer at \
                                     least that much to play them smoothly. On a VM this is \
                                     PipeWire's `default.clock.min-quantum = 1024` rule; check \
                                     `pw-metadata -n settings`"
                                );
                            } else {
                                tracing::info!(
                                    requested_frames = want,
                                    negotiated_frames = frames,
                                    "audio capture quantum negotiated"
                                );
                            }
                        }
                    } else if frames == ud.quantum_frames {
                        ud.quantum_candidate = None;
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
                    ud.stats.observe(&samples, ud.channels);
                    // Non-blocking and lossy, as before — but COUNTED, and only while a session
                    // is actually reading. A full channel under a LIVE consumer means the encode
                    // thread is not keeping up, and because the encoder simply concatenates
                    // across the hole every dropped chunk is a click AND a permanent shift of
                    // everything after it. A full channel under a PARKED capturer means nothing
                    // at all: the capturer is host-lifetime, so between sessions the channel
                    // fills once and then refuses everything, which counted as a 100 % drop rate
                    // and warned about a stream that did not exist (`PwAudioCapturer::active`).
                    if ud.tx.try_send(samples).is_err() && ud.active.load(Ordering::Relaxed) {
                        ud.stats.dropped_chunks += 1;
                    }
                    if ud.last_stats.elapsed() >= crate::audio::capture_policy::STATS_EVERY {
                        let (peak_db, rms_db, delivered_pct) =
                            ud.stats.summary(ud.last_stats.elapsed(), ud.rate_hz);
                        if ud.stats.dropped_chunks > 0 {
                            tracing::warn!(
                                dropped_chunks = ud.stats.dropped_chunks,
                                "the audio encode thread could not keep up — captured audio was \
                                 DROPPED; the stream will click and everything after it shifts"
                            );
                        }
                        tracing::info!(
                            peak_db = format!("{peak_db:.1}"),
                            rms_db = format!("{rms_db:.1}"),
                            delivered_pct = format!("{delivered_pct:.0}"),
                            // The shape of whatever `delivered_pct` is short by (WP-A2): one
                            // long hole and three hundred short ones read the same in the
                            // percentage and mean entirely different things.
                            gaps = ud.stats.gaps,
                            max_gap_ms = ud.stats.max_gap_ms(),
                            // The OTHER thing a shortfall can be (see `CaptureStats::pauses`):
                            // time our node was not in the graph at all. `gaps` deliberately
                            // cannot see it, so without these two a paused span and a starved
                            // stream are the same number.
                            pauses = ud.stats.pauses,
                            paused_ms = ud.stats.paused_ms(),
                            missed_dequeues = ud.stats.missed_dequeues,
                            dropped_chunks = ud.stats.dropped_chunks,
                            "desktop audio capture"
                        );
                        ud.stats = Default::default();
                        ud.last_stats = std::time::Instant::now();
                    }
                }));
                if outcome.is_err() {
                    tracing::error!("panic in pipewire audio callback — chunk dropped");
                }
            })
            .register()
            .context("register audio stream listener")?;

        // Request F32LE at the session's rate + channel count with explicit positions. In
        // legacy mode PipeWire's channel-mixer up/downmixes the sink monitor to this layout;
        // in stream-sink mode this IS the sink's advertised layout (apps mix/route to it) —
        // which is exactly why hi-res is structurally honest there and has to be PROVEN in
        // monitor mode (`design/hi-res-audio.md` §4.4): a sink we OWN renders at the rate we
        // declare, while a monitor tap is handed a resampled copy that reports a clean rate
        // whatever ran upstream, so that configuration's rate comes from the registry
        // (`monitor_rate`) and not from here. What was actually granted comes back through
        // `param_changed` above.
        let mut info = AudioInfoRaw::new();
        info.set_format(AudioFormat::F32LE);
        info.set_rate(rate_hz);
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
