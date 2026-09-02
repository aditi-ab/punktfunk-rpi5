//! PipeWire desktop-audio capture through a host-owned virtual sink.
//!
//! Default (`PUNKTFUNK_STREAM_SINK` unset): create a `support.null-audio-sink`
//! adapter and capture its monitor. That node is a **driver** (`timerfd` in the
//! daemon data loop), so the capture group owns its clock. A stream node is a
//! follower; PipeWire would schedule the group on any running driver on the box.
//!
//! `=stream`: the capture stream itself is the `Audio/Sink`. Same routing, but
//! the group borrows a driver. One-release escape hatch.
//!
//! `=0`: follow the default sink's monitor. Coupled to hardware-default churn.
//! Both sink modes advertise the session channel count, so a game can produce
//! 5.1/7.1 when local hardware is stereo.
//!
//! `!Send` MainLoop/Stream live on a dedicated thread; interleaved `f32` leaves
//! over a bounded channel (drop, never block the loop). Drop quits the loop
//! and tears the sink so a surround session can replace a stereo capturer
//! without leaking a consumer (a wedged link head-blocks the daemon).

mod monitor_rate;
mod pad_card_volume;
pub(crate) mod pad_sink;
pub(crate) mod pad_usb;
mod stream_sink;

use super::{AudioCapturer, MicBackendStats, VirtualMic, SAMPLE_RATE};
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

struct Terminate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// Host-created `support.null-audio-sink` — a driver of its own group — plus its monitor. Default.
    NullSink,
    /// Capture stream is the `Audio/Sink`; the group borrows a driver. Escape hatch.
    StreamSink,
    /// Tap the default sink's monitor; no host-owned sink.
    Monitor,
}

impl CaptureMode {
    fn owns_sink(self) -> bool {
        !matches!(self, CaptureMode::Monitor)
    }

    fn as_str(self) -> &'static str {
        match self {
            CaptureMode::NullSink => "null-sink",
            CaptureMode::StreamSink => "stream-sink",
            CaptureMode::Monitor => "monitor",
        }
    }
}

/// Whether capture owns a per-capturer sink that isolation can env-route into.
/// Monitor mode (`PUNKTFUNK_STREAM_SINK=0`) has none, so isolation's audio
/// half degrades to shared.
pub(crate) fn sink_capture_active() -> bool {
    capture_mode().owns_sink()
}

fn capture_mode() -> CaptureMode {
    if crate::audio::capture_policy::session_keeps_default() {
        // `CLIENT_CAP_KEEP_HOST_AUDIO`: follow the operator's default sink, no
        // default-sink claim. Wins over `PUNKTFUNK_STREAM_SINK` — "don't touch
        // my devices" is the more restrictive promise.
        return CaptureMode::Monitor;
    }
    capture_mode_from(std::env::var("PUNKTFUNK_STREAM_SINK").ok().as_deref())
}

/// Env grammar without process-global mutation, so the three modes are testable.
/// Unrecognised values (and unset) are NullSink: a typo must not kill audio.
fn capture_mode_from(value: Option<&str>) -> CaptureMode {
    match value.map(str::trim) {
        Some("0" | "false" | "no" | "off") => CaptureMode::Monitor,
        Some("stream") => CaptureMode::StreamSink,
        _ => CaptureMode::NullSink,
    }
}

#[derive(Debug, Clone)]
struct CaptureNodes {
    mode: CaptureMode,
    /// `Audio/Sink` `node.name` in both sink modes; the [`stream_sink`] claim
    /// target. StreamSink: this IS the capture stream. NullSink: the adapter
    /// whose monitor [`capture`](Self::capture) taps.
    sink: Option<String>,
    /// Capture stream `node.name`. Same as [`sink`](Self::sink) only in StreamSink.
    capture: String,
}

/// Linux capture-rate answer for the hi-res gate (`design/hi-res-audio.md`).
///
/// Both sink modes declare the `Audio/Sink` format (`audio.rate` on the
/// adapter, or the stream's negotiated format), so the rate we claim is the
/// rate we get — [`Declared`](super::CaptureRate::Declared), no probe.
///
/// Monitor mode (`PUNKTFUNK_STREAM_SINK=0`) captures through PipeWire's
/// resampler, which reports a clean rate whatever the node upstream really
/// runs at. The answer comes from the monitored node via
/// [`monitor_rate::monitored_sink_rate`], as an
/// [`Engine`](super::CaptureRate::Engine) rate.
///
/// Unreadable (suspended, missing key, timeout) is
/// [`Unknown`](super::CaptureRate::Unknown) and declines. A wrong guess would
/// advertise 96 kHz while carrying interpolated 48 kHz with both ends
/// auditing clean — so this never guesses: no graph-default, no `EnumFormat`
/// (capability, not fact), no fallback to the rate we asked for.
pub(super) fn probe_capture_rate() -> super::CaptureRate {
    if capture_mode().owns_sink() {
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
    sink_name: Option<String>,
    claimed: bool,
    /// Shared with the PipeWire thread so drop counting can tell "encode fell
    /// behind" from "nobody is reading". The capturer is host-lifetime and
    /// parked by [`idle`](AudioCapturer::idle); without this, a full hand-off
    /// channel reports 100 % drop with no stream. Distinct from `claimed`.
    active: Arc<AtomicBool>,
    /// Graph-negotiated rate, written by the format callback, read by
    /// [`AudioCapturer::sample_rate`]. Seeded with the ask. In monitor mode
    /// this is the resampled stream, not the node upstream — the hi-res gate
    /// reads that from [`monitor_rate`], not here.
    negotiated_rate: Arc<AtomicU32>,
}

impl PwAudioCapturer {
    pub fn open(channels: u32, rate_hz: u32) -> Result<PwAudioCapturer> {
        Self::open_named(channels, rate_hz, None)
    }

    /// [`open`](Self::open) with a caller-chosen sink `node.name` so isolation
    /// can pin nested apps (`PULSE_SINK`) to the same node it captures.
    /// Must keep the `punktfunk-speaker` prefix (claim-staleness and the
    /// graph-driver diagnostic match on it). Ignored in monitor mode.
    pub fn open_named(
        channels: u32,
        rate_hz: u32,
        sink_override: Option<&str>,
    ) -> Result<PwAudioCapturer> {
        anyhow::ensure!(
            matches!(channels, 1 | 2 | 6 | 8),
            "unsupported audio channel count {channels} (want 2, 6 or 8)"
        );
        anyhow::ensure!(rate_hz > 0, "audio capture rate must be positive");
        let mode = capture_mode();
        // Unique per capturer: overlapping instances must not alias, and a
        // fresh name gets unity WirePlumber volume, not the previous run's.
        // One sequence for both names so tap and sink log as one capturer.
        let seq = {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let pid = std::process::id();
        let sink_node = sink_override
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}-{pid}-{seq}", stream_sink::SINK_NAME_PREFIX));
        let nodes = CaptureNodes {
            mode,
            // StreamSink: the capture stream IS the sink, so it wears that name.
            // Otherwise a tap name that cannot match the speaker-prefix
            // crash-staleness rule or the graph-driver diagnostic.
            capture: match mode {
                CaptureMode::StreamSink => sink_node.clone(),
                _ => format!("punktfunk-audio-{pid}-{seq}"),
            },
            sink: mode.owns_sink().then_some(sink_node),
        };
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        // PipeWire not running must be an open error (callers' reopen backoff).
        // Stream-sink: the sink node must exist before we claim the default.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let sink_name = nodes.sink.clone();
        // Opens at session start, so the consumer is live from the first chunk.
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
                    nodes,
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
        // Routing claim starts with the session; release is `idle()` or Drop.
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
        // Receiver dies with us; remaining producer pushes must not count as
        // encode-thread-behind.
        self.active.store(false, Ordering::Relaxed);
        if self.claimed {
            self.claimed = false;
            stream_sink::release();
        }
        // Failed send means the thread already exited — nothing to tear down.
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
            // Quiet sink is not a failure — empty chunk keeps the capturer alive.
            // Only a dead capture thread is Err (caller reopens).
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
        // Reused parked capturer = new session: re-claim the default sink.
        if let (Some(name), false) = (&self.sink_name, self.claimed) {
            stream_sink::claim(name);
            self.claimed = true;
        }
        // After the backlog drain, so the producer never counts a drop against
        // a channel this call is still emptying.
        self.active.store(true, Ordering::Relaxed);
    }

    fn idle(&mut self) {
        // Parked: channel fills and stays full; those drops are nobody's fault.
        self.active.store(false, Ordering::Relaxed);
        if self.claimed {
            self.claimed = false;
            stream_sink::release();
        }
    }
}

/// GameStream surround order FL FR FC LFE RL RR [SL SR] as
/// `(enum spa_audio_channel, name)` pairs. Format pods ([`spa_positions`]) and
/// `audio.position` ([`spa_position_names`]) are two views of this list; a
/// disagreement would accept audio in one layout and hand it on in another.
///
/// `enum spa_audio_channel` (spa/param/audio/raw.h): MONO=2 FL=3 FR=4 FC=5
/// LFE=6 SL=7 SR=8 RL=12 RR=13. Names are what `spa_audio_parse_position` accepts.
fn channel_order(channels: u32) -> &'static [(u32, &'static str)] {
    const MONO: (u32, &str) = (2, "MONO");
    const FL: (u32, &str) = (3, "FL");
    const FR: (u32, &str) = (4, "FR");
    const FC: (u32, &str) = (5, "FC");
    const LFE: (u32, &str) = (6, "LFE");
    const SL: (u32, &str) = (7, "SL");
    const SR: (u32, &str) = (8, "SR");
    const RL: (u32, &str) = (12, "RL");
    const RR: (u32, &str) = (13, "RR");
    match channels {
        1 => &[MONO],
        2 => &[FL, FR],
        6 => &[FL, FR, FC, LFE, RL, RR],
        8 => &[FL, FR, FC, LFE, RL, RR, SL, SR],
        _ => unreachable!("validated in open()"),
    }
}

fn spa_positions(channels: u32) -> [u32; 64] {
    let mut pos = [0u32; 64];
    for (slot, (id, _)) in pos.iter_mut().zip(channel_order(channels)) {
        *slot = *id;
    }
    pos
}

/// [`channel_order`] as `audio.position` (`"[ FL FR ]"`) — the null-sink
/// adapter is configured by properties, not a format pod.
fn spa_position_names(channels: u32) -> String {
    let names: Vec<&str> = channel_order(channels).iter().map(|(_, n)| *n).collect();
    format!("[ {} ]", names.join(" "))
}

/// Property set of the host-owned `support.null-audio-sink`.
///
/// Returns `(key, value)` pairs so the tests below can pin each invariant:
///
/// * `factory.name` + no `object.linger`: pipewire-pulse `module-null-sink`;
///   lifetime is this connection — a crash leaves no ghost sink.
/// * `audio.rate`/`channels`/`position`: session format, making
///   [`probe_capture_rate`]'s `Declared` honest.
/// * **`node.force-quantum`, not `node.latency`**: a driver's quantum is the
///   smallest follower `node.latency`, then rounded down to a power of two.
///   A 240-frame (5 ms) ask is served as 128 on stock Linux. This key skips
///   that rounding and, driving only its own group, forces nothing else.
/// * `priority.session = 50`: parked sink must not win automatic default
///   election; routing is the [`stream_sink`] claim.
/// * **No `priority.driver`**: never elected to clock someone else's group.
/// * `session.suspend-timeout-seconds = 0`: suspend/resume is a hole in a
///   live stream (Wine churns its device at launch).
/// * `monitor.*`: pipewire-pulse defaults, so the volume slider still works.
fn null_sink_props(name: &str, channels: u32, rate_hz: u32) -> Vec<(&'static str, String)> {
    vec![
        ("factory.name", "support.null-audio-sink".to_string()),
        ("node.name", name.to_string()),
        ("node.description", "Punktfunk Stream Speaker".to_string()),
        ("media.class", "Audio/Sink".to_string()),
        ("node.virtual", "true".to_string()),
        ("audio.rate", rate_hz.to_string()),
        ("audio.channels", channels.to_string()),
        ("audio.position", spa_position_names(channels)),
        ("priority.session", "50".to_string()),
        ("session.suspend-timeout-seconds", "0".to_string()),
        (
            "node.force-quantum",
            capture_quantum_frames(rate_hz).to_string(),
        ),
        ("monitor.channel-volumes", "true".to_string()),
        ("monitor.passthrough", "true".to_string()),
    ]
}

/// Virtual microphone: a PipeWire `Audio/Source` the host pushes decoded
/// client-mic PCM into. The loop thread's producer callback drains it
/// (silence on underrun). Mirrors [`PwAudioCapturer`] inverted
/// (`Direction::Output`).
///
/// A stream node, not a `support.null-audio-sink` adapter: an
/// `Audio/Source/Virtual` adapter never gets a clock (`QUANT/RATE` 0, silence)
/// and WirePlumber reroutes a feeder targeting it to the *default sink*
/// (client voice out of the speakers, into desktop capture: echo). The
/// desktop **sink** is an adapter (`null_sink_props`); that result does not
/// transfer — WirePlumber has no monitor path for `Audio/Source/Virtual`.
/// Do not switch this to the adapter recipe without re-validating both.
///
/// Loop thread exit (core/stream error) flips `alive`; `push` returns
/// `false` and the pump reopens against the new daemon ([`VirtualMic`]).
pub struct PwMicSource {
    pcm: std::sync::mpsc::SyncSender<(std::time::Instant, Vec<f32>)>,
    channels: u32,
    quit: pipewire::channel::Sender<Terminate>,
    alive: Arc<AtomicBool>,
    /// One-shot, consumed by the process callback (clears the jitter ring).
    flush: Arc<AtomicBool>,
    ring: Arc<MicRingShared>,
}

/// Atomics between [`PwMicSource`] and the RT process callback. All `Relaxed`
/// — slowly-moving target and telemetry, not synchronization.
#[derive(Default)]
struct MicRingShared {
    /// Pump-set jitter target (per-channel samples). `0` = pump never spoke
    /// → callback keeps the historical 3-quanta clamp.
    target: AtomicUsize,
    depth: AtomicUsize,
    prime: AtomicUsize,
    reprimes: AtomicU64,
    overflow: AtomicU64,
}

impl PwMicSource {
    pub fn open(channels: u32) -> Result<PwMicSource> {
        Self::open_named(channels, None)
    }

    /// [`open`](Self::open) with a caller-chosen source `node.name` so
    /// isolation can pin nested apps (`PULSE_SOURCE`) to this session's uplink.
    /// `None` = shared `punktfunk-mic`. PipeWire 1.4 never assigns a driver to
    /// a non-default `Audio/Source` recorded by target — per-session mic needs
    /// the 1.6 daemon; on 1.4 the election losers read silence.
    pub fn open_named(channels: u32, source_name: Option<&str>) -> Result<PwMicSource> {
        anyhow::ensure!(
            matches!(channels, 1 | 2),
            "virtual mic supports 1 or 2 channels, got {channels}"
        );
        let node_name = source_name.unwrap_or("punktfunk-mic").to_string();
        let (pcm_tx, pcm_rx) = sync_channel::<(std::time::Instant, Vec<f32>)>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        let alive = Arc::new(AtomicBool::new(true));
        let flush = Arc::new(AtomicBool::new(false));
        // PipeWire not running must be an open error (pump backoff), not an
        // instantly-dead instance the pump would churn on.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let ring = Arc::new(MicRingShared::default());
        let (alive_t, flush_t, ring_t) = (alive.clone(), flush.clone(), ring.clone());
        thread::Builder::new()
            .name("punktfunk-pw-mic".into())
            .spawn(move || {
                if let Err(e) =
                    mic_pw_thread(pcm_rx, quit_rx, channels, &node_name, flush_t, ring_t, ready_tx)
                {
                    // Setup/open failure only (the running mainloop exits Ok).
                    // Already reported via the ready handshake.
                    tracing::debug!(error = %format!("{e:#}"), "pipewire virtual-mic setup failed — pump will back off and retry");
                }
                // Clean quit or daemon death: this instance is done; the pump reopens.
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
        // Timestamped so the process callback can age out chunks that sat in
        // the channel while no recorder was attached.
        match self.pcm.try_send((std::time::Instant::now(), pcm.to_vec())) {
            Ok(()) => true,
            // Behind is fine (drop the chunk); a gone receiver means the loop exited.
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
        // 0 = process callback has not run yet (no consumer).
        (prime > 0).then(|| (self.ring.depth.load(Ordering::Relaxed), prime))
    }
    fn take_stats(&self) -> MicBackendStats {
        MicBackendStats {
            reprimes: self.ring.reprimes.swap(0, Ordering::Relaxed),
            overflow_dropped: self.ring.overflow.swap(0, Ordering::Relaxed),
        }
    }
}

/// Incoming decoded PCM and a capped ring the process callback drains into
/// PipeWire buffers. `primed` is the jitter-buffer gate — see the callback.
struct MicUserData {
    rx: Receiver<(std::time::Instant, Vec<f32>)>,
    ring: VecDeque<f32>,
    channels: usize,
    primed: bool,
    flush: Arc<AtomicBool>,
    shared: Arc<MicRingShared>,
    /// Last process-callback run. A long gap means the ring predates the
    /// current consumer (idle with no recorder) and must be dropped.
    last_run: Option<std::time::Instant>,
}

/// PCM older than this never reaches a recorder: channel-aged chunks and
/// ring content from before a consumer gap would otherwise burst as stale
/// audio when recording (re)starts.
const MIC_STALE: Duration = Duration::from_secs(1);

/// Graph quantum every punktfunk PipeWire stream asks for, in frames:
/// 240 @ 48 kHz = 5 ms, one protocol audio frame. Named so the `NODE_LATENCY`
/// ask and the honoured-or-not check cannot drift.
const CAPTURE_QUANTUM_FRAMES: u32 = 240;

/// [`CAPTURE_QUANTUM_FRAMES`] at `rate_hz` — the same 5 ms of wall time.
/// A 96 kHz session (`design/hi-res-audio.md`) asking for a flat 240 frames
/// would halve the quantum to 2.5 ms. Virtual mic and pad sinks stay 48 kHz
/// and keep the constant.
fn capture_quantum_frames(rate_hz: u32) -> u32 {
    // `max(1)` guards a nonsense rate from a zero-frame ask.
    ((CAPTURE_QUANTUM_FRAMES as u64 * rate_hz as u64 / SAMPLE_RATE as u64) as u32).max(1)
}

/// Consecutive callbacks that must agree on a new buffer size before it
/// replaces the one gaps are scored against. Three rejects a boundary
/// artefact and still adopts a genuine re-plan within ~15 ms.
const QUANTUM_CONFIRM: u8 = 3;

fn mic_pw_thread(
    pcm_rx: Receiver<(std::time::Instant, Vec<f32>)>,
    quit_rx: pipewire::channel::Receiver<Terminate>,
    channels: u32,
    node_name: &str,
    flush: Arc<AtomicBool>,
    shared: Arc<MicRingShared>,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use pipewire as pw;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;

    // PipeWire objects are lifetime-chained (guards borrow mainloop/core), so
    // setup and the blocking run share one frame; the IIFE funnels every
    // setup `?` through the ready handshake.
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

        // Core error (daemon gone — our node is gone) ends this thread and
        // flips `alive` so the pump reopens. Without this, a restart leaves
        // the loop idling on a dead connection and the mic silent for life.
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

        // `Audio/Source` advertises a recordable microphone. Without it,
        // Direction::Output + Playback would route to the speakers.
        let stream = pw::stream::StreamBox::new(
            &core,
            node_name,
            properties! {
                *pw::keys::MEDIA_TYPE        => "Audio",
                *pw::keys::MEDIA_CLASS       => "Audio/Source",
                *pw::keys::NODE_NAME         => node_name,
                *pw::keys::NODE_DESCRIPTION  => "Punktfunk Remote Microphone",
                // ~5 ms quantum (one Opus frame) so recorders get low-latency chunks.
                *pw::keys::NODE_LATENCY      => "240/48000",
                // Win default-source election. Default-input apps otherwise hear
                // silence; PipeWire 1.4 never drives a non-default `Audio/Source`
                // recorded by target (`QUANT/RATE` 0). 3000 clears typical
                // hardware (~1000–1900); an explicit configured default still wins.
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
                    // Unrecoverable for this instance — exit so the pump reopens.
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
                    // Before pulling new frames: drop the ring on flush (uplink
                    // gap) or when this callback has not run for `MIC_STALE`
                    // (idle, no recorder). A recorder must not hear old audio.
                    let now = std::time::Instant::now();
                    let idled = ud
                        .last_run
                        .is_some_and(|t| now.duration_since(t) > MIC_STALE);
                    if ud.flush.swap(false, std::sync::atomic::Ordering::AcqRel) || idled {
                        ud.ring.clear();
                        ud.primed = false;
                    }
                    ud.last_run = Some(now);
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
                    let want = want_frames * ud.channels;
                    static FIRST: std::sync::atomic::AtomicBool =
                        std::sync::atomic::AtomicBool::new(true);
                    if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
                        tracing::info!(
                            quantum_frames = want_frames,
                            quantum_ms = want_frames as f32 / 48.0,
                            "virtual-mic consumer connected"
                        );
                    }

                    // Prime = one quantum + pump jitter target; re-prime only
                    // after a full drain. Target `0` keeps the 3-quanta clamp,
                    // which scaled with the recorder's quantum (2048 → 128 ms).
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

        // Run the producer on PipeWire's realtime data loop so the source is a
        // synchronous graph node that joins its consumer's driver group.
        // Without it the node is async and, on a busy multi-stream graph, never
        // acquires a driver — `process()` never fires, recorders hear silence.
        stream
            .connect(
                spa::utils::Direction::Output,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            )
            .context("pw mic stream connect")?;

        // Daemon + stream connect succeeded. A PipeWire that isn't running
        // never reaches here; its connect error is an open failure so the
        // pump backs off instead of churning on dead instances.
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
    nodes: CaptureNodes,
    ready: std::sync::mpsc::SyncSender<Result<()>>,
    active: Arc<AtomicBool>,
    negotiated_rate: Arc<AtomicU32>,
) -> Result<()> {
    use pipewire as pw;
    use pw::proxy::ProxyT;
    use pw::{properties::properties, spa};
    use spa::param::audio::{AudioFormat, AudioInfoRaw};
    use spa::pod::Pod;
    use std::cell::RefCell;
    use std::rc::Rc;
    let CaptureNodes {
        mode,
        sink: sink_name,
        capture: capture_name,
    } = nodes;
    // Boosts THIS mainloop thread, not the capture callback. `RT_PROCESS`
    // runs `process()` on libpipewire's data loop (`SCHED_RR`). Kept: this
    // thread still dispatches state/format, and IS the capture thread in
    // monitor mode. The callback reports its own scheduling on first entry.
    pf_frame::thread_qos::boost_thread_priority(true);

    // Setup errors funnel through the ready handshake.
    let result = (|| -> Result<()> {
        pf_capture::pwinit::ensure_init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).context("pw audio MainLoop")?;
        let context = pw::context::ContextRc::new(&mainloop, None).context("pw audio Context")?;
        let core = context
            .connect_rc(None)
            .context("pw audio connect (is PipeWire running in this session?)")?;

        let _quit_guard = quit_rx.attach(mainloop.loop_(), {
            let mainloop = mainloop.clone();
            move |_| mainloop.quit()
        });

        // Core error ends this thread so the chunk channel disconnects and
        // `next_chunk` returns Err (reopen-with-backoff). Without this, a
        // restart left `next_chunk` returning quiet-sink empties forever.
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

        // Null-sink mode: a real `support.null-audio-sink` adapter (a driver
        // with its own `timerfd`). Created before the capture stream so
        // `target.object` resolves; no `object.linger`, so it dies with this
        // connection — which the loop thread's exit relies on.
        let _sink_node = match mode {
            CaptureMode::NullSink => {
                let name = sink_name
                    .as_deref()
                    .context("null-sink mode without a sink name")?;
                let mut props = pw::properties::PropertiesBox::new();
                for (key, value) in null_sink_props(name, channels, rate_hz) {
                    props.insert(key, value);
                }
                let node = core
                    .create_object::<pw::node::Node>("adapter", &props)
                    .context("create the punktfunk stream sink (support.null-audio-sink)")?;
                // Server answers asynchronously: `bound` is the sink existing
                // (graph id for the driver diagnostic); `error` is no adapter
                // factory. Core-error listener ends the thread either way.
                let listener = node
                    .upcast_ref()
                    .add_listener_local()
                    .bound(|id| {
                        tracing::debug!(node_id = id, "punktfunk stream sink registered");
                    })
                    .error(|_seq, res, message| {
                        tracing::warn!(
                            res,
                            message,
                            "the punktfunk stream sink could not be created — this host cannot \
                             capture desktop audio until it can. Set PUNKTFUNK_STREAM_SINK=stream \
                             for the 0.30 topology (no created sink) and please report it"
                        );
                    })
                    .register();
                Some((node, listener))
            }
            _ => None,
        };

        // `<quantum frames>/<rate>` — both halves move so the ask stays 5 ms
        // at 48 kHz and 96 kHz. Formatted once so the property arms cannot drift.
        let node_latency = format!("{}/{}", capture_quantum_frames(rate_hz), rate_hz);
        // `node.driver-id` names who clocks our group. Not in the registry
        // announce set — needs a bind + `info`. The daemon writes the key but
        // flushes on the next info emission, so this is last-known, not realtime.
        struct GraphDriver {
            /// `node.name` of every Node global, so the driver can be named.
            /// Pruned on removal — a host runs for days and streams come and go.
            names: HashMap<u32, String>,
            /// Our node, bound so its `info` — and `node.driver-id` — arrives.
            ours: Option<(pw::node::Node, pw::node::NodeListener)>,
            /// Last reported; log only on change.
            driver: Option<u32>,
        }
        // Null-sink has exactly one right answer (ours). Legacy topologies
        // borrow a driver by design, so the line names it without judging.
        let expected_driver = match mode {
            CaptureMode::NullSink => sink_name.clone(),
            _ => None,
        };
        let watch = Rc::new(RefCell::new(GraphDriver {
            names: HashMap::new(),
            ours: None,
            driver: None,
        }));
        let registry = core.get_registry_rc().context("pw audio registry")?;
        let _registry_listener = registry
            .add_listener_local()
            .global({
                let watch = watch.clone();
                let registry = registry.clone();
                let capture_name = capture_name.clone();
                move |global| {
                    if global.type_ != pw::types::ObjectType::Node {
                        return;
                    }
                    let Some(props) = global.props else { return };
                    let Some(name) = props.get("node.name") else {
                        return;
                    };
                    watch.borrow_mut().names.insert(global.id, name.to_string());
                    if name != capture_name.as_str() || watch.borrow().ours.is_some() {
                        return;
                    }
                    let Ok(node) = registry.bind::<pw::node::Node, _>(global) else {
                        return;
                    };
                    let listener = node
                        .add_listener_local()
                        .info({
                            let watch = watch.clone();
                            let expected = expected_driver.clone();
                            move |info| {
                                let Some(props) = info.props() else { return };
                                // Absent = between drivers (daemon drops the key).
                                // The next assignment reports itself.
                                let Some(id) = props
                                    .get("node.driver-id")
                                    .and_then(|v| v.parse::<u32>().ok())
                                else {
                                    return;
                                };
                                let mut w = watch.borrow_mut();
                                if w.driver == Some(id) {
                                    return;
                                }
                                w.driver = Some(id);
                                let named = w.names.get(&id).cloned();
                                let driver = named.as_deref().unwrap_or("<unnamed>");
                                match expected.as_deref() {
                                    Some(sink) if driver == sink => tracing::info!(
                                        driver,
                                        driver_id = id,
                                        "audio capture graph driver"
                                    ),
                                    Some(sink) => tracing::warn!(
                                        driver,
                                        driver_id = id,
                                        expected = sink,
                                        "our audio capture group is being clocked by another \
                                         node — every hole in this stream is that node's \
                                         scheduling, not ours. Something has linked our sink to \
                                         it (a loopback from its monitor is the usual cause); a \
                                         USB or USB-over-IP sound card here is the 2026-08-18 \
                                         defect"
                                    ),
                                    // Legacy topologies have no driver of their own;
                                    // borrowing one is the design. Which one still matters.
                                    None => tracing::info!(
                                        driver,
                                        driver_id = id,
                                        "audio capture graph driver (borrowed — this topology \
                                         has none of its own)"
                                    ),
                                }
                            }
                        })
                        .register();
                    watch.borrow_mut().ours = Some((node, listener));
                }
            })
            .global_remove({
                let watch = watch.clone();
                move |id| {
                    watch.borrow_mut().names.remove(&id);
                }
            })
            .register();

        let props = match mode {
            // Monitor tap of the adapter above, aimed by name so it can only be ours.
            CaptureMode::NullSink => {
                let name = sink_name
                    .as_deref()
                    .context("null-sink mode without a sink name")?;
                let mut p = properties! {
                    *pw::keys::MEDIA_TYPE          => "Audio",
                    *pw::keys::MEDIA_CATEGORY      => "Capture",
                    *pw::keys::MEDIA_ROLE          => "Music",
                    *pw::keys::STREAM_CAPTURE_SINK => "true",
                    // A passive link does not make either end runnable. Parked,
                    // nothing playing: the group is idle and the timer parks.
                    // A game's (non-passive) link makes the sink runnable and
                    // the graph walks that through the monitor to us.
                    *pw::keys::NODE_PASSIVE        => "true",
                    // Never fall back to a hardware monitor (wrong audio, and
                    // briefly rejoining a hardware driver is this mode's defect).
                    // These two are a pair: WirePlumber reads `dont-fallback`
                    // alone as licence to destroy the stream; `linger` waits.
                    "node.dont-fallback"           => "true",
                    "node.linger"                  => "true",
                };
                p.insert(*pw::keys::NODE_NAME, capture_name.as_str());
                // Spelled out: pipewire-rs exposes `TARGET_OBJECT` only behind
                // `v0_3_44`. WirePlumber matches this against `node.name`.
                p.insert("target.object", name);
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
            // This stream IS the sink. Apps play into it; process() gets the mix.
            CaptureMode::StreamSink => {
                let name = sink_name
                    .as_deref()
                    .context("stream-sink mode without a sink name")?;
                let mut p = properties! {
                    *pw::keys::MEDIA_TYPE       => "Audio",
                    *pw::keys::MEDIA_CLASS      => "Audio/Sink",
                    *pw::keys::NODE_DESCRIPTION => "Punktfunk Stream Speaker",
                    *pw::keys::NODE_VIRTUAL     => "true",
                    // Low on purpose — opposite of the mic's 3000. Parked sink
                    // must not win auto default election; routing is the claim.
                    "priority.session"          => "50",
                    // Wine churns its device at launch; each suspend/resume is a
                    // hole in a live stream. Not `node.always-process`: that
                    // would keep the callback scheduled ~200/s between sessions.
                    "session.suspend-timeout-seconds" => "0",
                };
                p.insert(*pw::keys::NODE_NAME, name);
                // ~5 ms quantum, one protocol frame. Inserted (rate is a
                // session value) so it cannot drift from the other mode arms.
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
            // Default-sink monitor (system output), not a microphone.
            CaptureMode::Monitor => {
                let mut p = properties! {
                    *pw::keys::MEDIA_TYPE          => "Audio",
                    *pw::keys::MEDIA_CATEGORY      => "Capture",
                    *pw::keys::MEDIA_ROLE          => "Music",
                    *pw::keys::STREAM_CAPTURE_SINK => "true",
                };
                p.insert(*pw::keys::NODE_NAME, capture_name.as_str());
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
        };
        let stream = pw::stream::StreamBox::new(&core, "punktfunk-audio", props)
            .context("pw audio Stream")?;

        struct CapUd {
            tx: std::sync::mpsc::SyncSender<Vec<f32>>,
            channels: u32,
            stats: crate::audio::capture_policy::CaptureStats,
            last_stats: std::time::Instant,
            /// Frames per callback, `0` until confirmed. Per-open, not a
            /// process-wide latch: a host runs for days, and a process-wide
            /// form reported the first capture then never again.
            quantum_frames: usize,
            /// Buffer size seen but not yet believed, plus consecutive agrees.
            /// Stops one short buffer from moving the gap threshold.
            quantum_candidate: Option<(usize, u8)>,
            reported_sched: bool,
            /// Last callback time, so cadence can be scored. Cleared across a
            /// state transition — a deliberate Paused span is not one hole.
            /// Not in `stats`: stats reset every window, cadence does not.
            last_cb: Option<std::time::Instant>,
            /// Negotiated quantum; a gap is measured against this. Seeded with
            /// the ask; corrected on first data. A 1024-frame clamp is the
            /// deal we got, not a fault.
            quantum: Duration,
            /// Current format, so a resume to the same one is not a real change.
            negotiated: Option<(spa::param::audio::AudioFormat, u32, u32)>,
            active: Arc<AtomicBool>,
            /// When the stream last left `Streaming`, so the span is charged
            /// to the window it stretched. `None` while streaming.
            paused_since: Option<std::time::Instant>,
            /// Denominator for every frames↔time conversion. At 96 kHz a
            /// hardcoded 48 000 would report every quantum as twice as long.
            rate_hz: u32,
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
                    // A Paused↔Streaming span is a gap in the stream existing,
                    // not in delivery. Scoring it would bury the sub-10 ms holes.
                    ud.last_cb = None;
                    // Still reported: the process-callback window stretches by
                    // the whole span. Charge it to the window flushed after resume.
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
                    // Unrecoverable — exit so sessions reopen a fresh instance.
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
                    // Same format = the graph resumed us, not a stream change.
                    // The flap stays visible in state DEBUG and gap counters.
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
                    // Report what was granted, not asked (`design/hi-res-audio.md`).
                    // Rate `0` means the pod carried none; keep the previous
                    // value — "unstated" is not a claim that the rate changed.
                    if now.1 != 0 {
                        ud.rate_hz = now.1;
                        ud.negotiated_rate.store(now.1, Ordering::Relaxed);
                    }
                    // Sink modes: we own the sink, so this IS the format apps
                    // render into. Monitor mode: PipeWire's resampler reports a
                    // clean rate whatever ran upstream; the node's own rate is
                    // a registry lookup in `monitor_rate`.
                    tracing::info!(
                        format = ?info.format(),
                        rate = info.rate(),
                        channels = info.channels(),
                        mode = mode.as_str(),
                        "audio format negotiated"
                    );
                }
            })
            .process(|stream, ud| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Score arrival before any early return: a callback that
                    // ran empty still ran — different from one that never ran.
                    let now = std::time::Instant::now();
                    let since_last = ud.last_cb.map(|t| now.duration_since(t));
                    ud.last_cb = Some(now);
                    ud.stats.observe_callback(since_last, ud.quantum);

                    if !ud.reported_sched {
                        ud.reported_sched = true;
                        // The thread that actually runs this callback, once per
                        // open. The mainloop boost above never reaches here.
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
                    // Track the quantum the graph is handing us. It re-plans
                    // when anything else asks for a different latency; latching
                    // the first callback scored later gaps against a dead size.
                    // A new size must survive `QUANTUM_CONFIRM` callbacks.
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
                            ud.quantum = Duration::from_micros(
                                frames as u64 * 1_000_000 / ud.rate_hz.max(1) as u64,
                            );
                            let want = capture_quantum_frames(ud.rate_hz) as usize;
                            let negotiated_ms =
                                format!("{:.1}", frames as f32 * 1000.0 / ud.rate_hz.max(1) as f32);
                            if was != 0 {
                                // Moves the gap threshold under a reader comparing windows.
                                tracing::info!(
                                    previous_frames = was,
                                    negotiated_frames = frames,
                                    negotiated_ms,
                                    "the audio graph re-planned our quantum mid-stream"
                                );
                            } else if frames > want {
                                // Asked vs granted. Stock `pipewire.conf` raises
                                // `default.clock.min-quantum` to 1024 in a VM
                                // (`cpu.vm.name` set), so a 5 ms ask becomes 21.3 ms.
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
                    // Lossy and non-blocking. Count only while a session is
                    // reading: a full channel under a live consumer is a click
                    // plus a permanent shift; under a parked capturer it is nothing.
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
                            // Shape of the `delivered_pct` shortfall: one long
                            // hole and three hundred short ones share a percentage.
                            gaps = ud.stats.gaps,
                            max_gap_ms = ud.stats.max_gap_ms(),
                            // Buckets under 20/50/100 ms and ≥ 100 ms, plus the
                            // audio they cost. `gaps=60` does not distinguish them.
                            gap_hist = %ud.stats.gap_hist(),
                            missing_ms = ud.stats.missing_ms(),
                            // Time our node was not in the graph. `gaps` cannot
                            // see it; without this a pause and a starve match.
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

        // F32LE at the session rate + layout. Sink modes: this is the sink's
        // advertised layout. Monitor mode: PipeWire's mixer up/downmixes the
        // sink monitor; the rate is resampled, so hi-res is proven from the
        // registry (`monitor_rate`), not from here (`design/hi-res-audio.md`).
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

        // Same reason as the mic: a synchronous node that joins its driver
        // group. Also puts the callback on a SCHED_RR data loop the mainloop
        // boost above can never reach.
        let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
        if mode.owns_sink() {
            flags |= pw::stream::StreamFlags::RT_PROCESS;
        }
        stream
            .connect(
                spa::utils::Direction::Input,
                None, // PW_ID_ANY — legacy: the default sink monitor
                flags,
                &mut params,
            )
            .context("pw audio stream connect")?;

        // Connect is async server-side; if the default-sink claim lands before
        // the node registers, WirePlumber keeps the configured value and elects
        // it when the node appears.
        tracing::info!(
            mode = mode.as_str(),
            sink = sink_name.as_deref().unwrap_or("<the default sink>"),
            capture = capture_name.as_str(),
            "desktop audio capture topology"
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Three spellings of `PUNKTFUNK_STREAM_SINK`; anything else is the default
    /// so a typo in a debug lever cannot be why a session has no audio.
    #[test]
    fn capture_mode_grammar() {
        assert_eq!(capture_mode_from(None), CaptureMode::NullSink);
        for off in ["0", "false", "no", "off", " off "] {
            assert_eq!(
                capture_mode_from(Some(off)),
                CaptureMode::Monitor,
                "{off:?} selects the legacy monitor follower"
            );
        }
        assert_eq!(capture_mode_from(Some("stream")), CaptureMode::StreamSink);
        assert_eq!(capture_mode_from(Some(" stream ")), CaptureMode::StreamSink);
        for junk in ["1", "yes", "null", "STREAM", ""] {
            assert_eq!(
                capture_mode_from(Some(junk)),
                CaptureMode::NullSink,
                "{junk:?} is not a mode and must fall to the default"
            );
        }
        assert!(CaptureMode::NullSink.owns_sink() && CaptureMode::StreamSink.owns_sink());
        assert!(!CaptureMode::Monitor.owns_sink());
    }

    /// Pod form and property form of the channel map are the same layout.
    /// A disagreement would swap channels with nothing in the log.
    #[test]
    fn channel_map_views_agree() {
        for ch in [1u32, 2, 6, 8] {
            let ids = spa_positions(ch);
            let order = channel_order(ch);
            assert_eq!(order.len(), ch as usize, "{ch} channels");
            for (i, (id, _)) in order.iter().enumerate() {
                assert_eq!(ids[i], *id, "channel {i} of {ch}");
            }
            assert!(
                ids[ch as usize..].iter().all(|&p| p == 0),
                "positions past the channel count stay unset"
            );
        }
        // These exact strings are what PipeWire parses.
        assert_eq!(spa_position_names(2), "[ FL FR ]");
        assert_eq!(spa_position_names(6), "[ FL FR FC LFE RL RR ]");
        assert_eq!(spa_position_names(8), "[ FL FR FC LFE RL RR SL SR ]");
    }

    /// Created-sink invariants. None fail loudly if they silently change.
    #[test]
    fn null_sink_props_hold_their_invariants() {
        let props = null_sink_props("punktfunk-speaker-42-0", 6, 48_000);
        let get = |k: &str| {
            props
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("factory.name"), Some("support.null-audio-sink"));
        assert_eq!(get("media.class"), Some("Audio/Sink"));
        assert_eq!(get("node.name"), Some("punktfunk-speaker-42-0"));
        assert!(
            get("node.name").is_some_and(|n| n.starts_with(stream_sink::SINK_NAME_PREFIX)),
            "the claim's staleness rule matches this prefix"
        );
        assert_eq!(get("audio.channels"), Some("6"));
        assert_eq!(get("audio.rate"), Some("48000"));
        assert_eq!(get("audio.position"), Some("[ FL FR FC LFE RL RR ]"));
        // The 5 ms ask, in the one form PipeWire will not round down to 128.
        assert_eq!(get("node.force-quantum"), Some("240"));
        assert_eq!(get("session.suspend-timeout-seconds"), Some("0"));
        assert_eq!(get("priority.session"), Some("50"));
        // A linger sink wedges routing on a node nothing owns; a driver
        // priority would clock other people's driver-less groups.
        assert_eq!(get("object.linger"), None);
        assert_eq!(get("priority.driver"), None);
        // Quantum is a latency, so it scales with the rate (5 ms either way).
        let hi = null_sink_props("punktfunk-speaker-42-1", 2, 96_000);
        let hi_get = |k: &str| {
            hi.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(hi_get("audio.rate"), Some("96000"));
        assert_eq!(hi_get("node.force-quantum"), Some("480"));
    }
}
