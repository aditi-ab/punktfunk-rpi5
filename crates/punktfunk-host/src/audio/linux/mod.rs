//! PipeWire desktop-audio capture — through a **host-owned virtual sink** (default), the 0.30
//! stream-sink node (`PUNKTFUNK_STREAM_SINK=stream`), or the legacy default-sink-monitor
//! follower (`PUNKTFUNK_STREAM_SINK=0`).
//!
//! **Null-sink mode (the default).** The host creates a real `support.null-audio-sink` adapter
//! ("Punktfunk Stream Speaker", unique `node.name` per capturer) and captures its **monitor**;
//! host apps play into it, PipeWire mixes them, and our `process()` callback receives the mix. A
//! session-scoped [`stream_sink`] claim makes it the *default* sink so apps route to it (and
//! back) with the session.
//!
//! Why a node we create rather than our own capture stream wearing `media.class=Audio/Sink`:
//! **a stream is structurally a follower and never drives**, so PipeWire schedules the resulting
//! driver-less group — {game streams -> our sink} — on the highest-priority *running* driver
//! anywhere on the box. Field-diagnosed 2026-08-18: on a host with a DualSense forwarded over
//! VirtualHere, our capture group was clocked for a whole 15-minute session by that pad's
//! USB-over-IP sound card — a device nothing was linked to, whose frame counter is a kernel stub
//! (`vhci_get_frame_number()` logs and returns 0) — giving 3.9 delivery holes a second, and
//! **15.4 % of the audio the user heard was silence this host synthesized** over them. A
//! `support.null-audio-sink` is a **driver**: it carries its own `timerfd` inside the daemon's
//! realtime data loop, so our group owns its clock and no hardware (or network-attached) device
//! can be elected to schedule it. It is also exactly the node `pactl load-module
//! module-null-sink` creates — the most exercised virtual-sink recipe on Linux.
//!
//! **`=stream` (one-release escape hatch).** The 0.30 topology: the capture stream itself is the
//! `Audio/Sink` node — same routing, same claim, but the group borrows a driver as above.
//!
//! **`=0` (legacy).** An input stream with `stream.capture.sink=true` and no target, which
//! PipeWire routes to whatever the *default* sink is — so it is coupled to hardware-default
//! churn: live-diagnosed 2026-07-14 on a bazzite/TV host, every gamescope modeset dropped the
//! HDMI audio endpoint, WirePlumber ping-ponged the default HDMI<->auto_null ~8x/s, and the
//! monitor follower relinked on every flip (Paused->renegotiate->Streaming storms = client
//! crackle). Both sink modes are immune — nothing about a host-owned sink depends on display
//! hardware — and both advertise the session's true channel count, so games can produce real
//! 5.1/7.1 even when the local hardware is stereo.
//!
//! In every mode the (`!Send`) MainLoop/Stream live on a dedicated thread; interleaved `f32`
//! chunks leave over a bounded channel (dropped if the encoder falls behind, never blocking the
//! PipeWire loop). The stream is opened at the *session's* channel count (2/6/8); in legacy mode
//! PipeWire's channel-mixer fills missing positions with silence (zero upmix). Dropping the
//! capturer quits the loop thread (via a `pipewire::channel` Terminate message), tearing the
//! stream — and in the sink modes the sink node itself — down promptly, so a surround session
//! can replace a stereo capturer without leaking a PipeWire consumer (see CLAUDE.md: a wedged
//! link head-blocks the daemon).

mod monitor_rate;
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

/// Message asking the PipeWire loop thread to quit (sent from `Drop`).
struct Terminate;

/// Which topology this host captures desktop audio through — see the module docs for what each
/// one costs and why the default moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    /// Create a `support.null-audio-sink` — a node that DRIVES its own graph group — and capture
    /// its monitor. **Default.**
    NullSink,
    /// The capture stream itself is the `Audio/Sink` node (the 0.30 topology), so the group it
    /// forms with its producers borrows a driver from elsewhere on the box. One-release escape
    /// hatch, kept so a field A/B needs no build.
    StreamSink,
    /// No host-owned sink at all: follow whatever the default sink is and tap its monitor.
    Monitor,
}

impl CaptureMode {
    /// Both sink modes mint a sink node and [`claim`](stream_sink::claim) it as the default
    /// output; only [`Monitor`](Self::Monitor) does not.
    fn owns_sink(self) -> bool {
        !matches!(self, CaptureMode::Monitor)
    }

    /// For the line that says which topology is live — the first thing to read in a log where
    /// audio arrives in a shape nobody expected.
    fn as_str(self) -> &'static str {
        match self {
            CaptureMode::NullSink => "null-sink",
            CaptureMode::StreamSink => "stream-sink",
            CaptureMode::Monitor => "monitor",
        }
    }
}

/// `PUNKTFUNK_STREAM_SINK`: `stream` = [`StreamSink`](CaptureMode::StreamSink),
/// `0`/`false`/`no`/`off` = [`Monitor`](CaptureMode::Monitor), anything else (including unset) =
/// [`NullSink`](CaptureMode::NullSink).
///
/// An unrecognised value resolves to the default rather than failing: this is a field-debugging
/// lever, and a typo in it must not cost a session its audio.
fn capture_mode() -> CaptureMode {
    capture_mode_from(std::env::var("PUNKTFUNK_STREAM_SINK").ok().as_deref())
}

/// [`capture_mode`] without the environment, so the grammar is testable without a process-global
/// mutation (and so the three modes each have a test at all).
fn capture_mode_from(value: Option<&str>) -> CaptureMode {
    match value.map(str::trim) {
        Some("0" | "false" | "no" | "off") => CaptureMode::Monitor,
        Some("stream") => CaptureMode::StreamSink,
        _ => CaptureMode::NullSink,
    }
}

/// The graph identity of one capturer: which topology, and the `node.name`s it owns.
#[derive(Debug, Clone)]
struct CaptureNodes {
    mode: CaptureMode,
    /// The `Audio/Sink` node's name — `Some` in both sink modes, and what the [`stream_sink`]
    /// default-sink claim points at. In [`StreamSink`](CaptureMode::StreamSink) mode this IS the
    /// capture stream; in [`NullSink`](CaptureMode::NullSink) mode it is the adapter the host
    /// creates, whose monitor [`capture`](Self::capture) taps.
    sink: Option<String>,
    /// The capture stream's own `node.name`. Aliases [`sink`](Self::sink) only in
    /// [`StreamSink`](CaptureMode::StreamSink) mode, where they are one node.
    capture: String,
}

/// §8.4 condition 4 on Linux (`design/hi-res-audio.md` §4.4 / §8.3). The two capture modes give
/// structurally different answers, and that difference is the whole content of §4.4:
///
/// * **Both sink modes (the default `null-sink`, and `stream`).** The `Audio/Sink` node is ours
///   and we declare its format — as the created adapter's `audio.rate` in null-sink mode, as the
///   stream's own negotiated format in stream-sink mode — so applications render into it at that
///   rate natively. The rate we claim is the rate we get, by construction: there is no upstream
///   resampler in the path to lie about it, so the answer is yes for every rate the plane
///   supports, and no probe of any kind is needed to say so.
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
        let mode = capture_mode();
        // Unique per capturer: overlapping instances (mid-session reopen, concurrent sessions)
        // must never alias in metadata claims or in a `target.object` lookup, and a fresh name
        // gets fresh (unity) WirePlumber volume state instead of whatever a previous run left
        // behind. ONE sequence number for both names, so a line about the tap and a line about
        // its sink are visibly the same capturer.
        let seq = {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            SEQ.fetch_add(1, Ordering::Relaxed)
        };
        let pid = std::process::id();
        let sink_node = format!("{}-{pid}-{seq}", stream_sink::SINK_NAME_PREFIX);
        let nodes = CaptureNodes {
            mode,
            // In stream-sink mode the capture stream IS the sink, so it wears the sink's name.
            // Otherwise it is a tap of its own and gets a name that can never be mistaken for a
            // sink: `stream_sink`'s crash-staleness rule matches on the speaker prefix, and the
            // graph-driver diagnostic finds our node by exactly this string.
            capture: match mode {
                CaptureMode::StreamSink => sink_node.clone(),
                _ => format!("punktfunk-audio-{pid}-{seq}"),
            },
            sink: mode.owns_sink().then_some(sink_node),
        };
        let (tx, rx) = sync_channel::<Vec<f32>>(64);
        let (quit_tx, quit_rx) = pipewire::channel::channel::<Terminate>();
        // Bring-up handshake (mirrors the virtual mic): a PipeWire that isn't running must
        // surface as an open ERROR — engaging the callers' reopen backoff — and in stream-sink
        // mode the sink node must exist before we claim the default to its name.
        let (ready_tx, ready_rx) = sync_channel::<Result<()>>(1);
        let sink_name = nodes.sink.clone();
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

/// The GameStream surround order FL FR FC LFE RL RR [SL SR] (= the PipeWire/PulseAudio default
/// map for 6/8 channels, and the order Moonlight's renderers expect — moonlight-common-c: "we
/// use FL FR C LFE RL RR SL SR"), as `(enum spa_audio_channel, name)` pairs.
///
/// Two things need this order in two spellings — a format pod ([`spa_positions`]) for a stream,
/// and an `audio.position` string ([`spa_position_names`]) for a node we create by properties —
/// and a channel map that disagrees with itself between them would mean the sink accepts audio
/// in one layout and hands it on in another. They are two views of THIS one list, so they
/// cannot drift.
///
/// Values are `enum spa_audio_channel` (spa/param/audio/raw.h): MONO=2 FL=3 FR=4 FC=5 LFE=6
/// SL=7 SR=8 RL=12 RR=13; the names are the spellings `spa_audio_parse_position` accepts.
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

/// [`channel_order`] as the SPA position array a format pod carries.
fn spa_positions(channels: u32) -> [u32; 64] {
    let mut pos = [0u32; 64];
    for (slot, (id, _)) in pos.iter_mut().zip(channel_order(channels)) {
        *slot = *id;
    }
    pos
}

/// [`channel_order`] as `audio.position` spells it (`"[ FL FR ]"`) — the
/// `support.null-audio-sink` adapter is configured by properties, not by a format pod.
fn spa_position_names(channels: u32) -> String {
    let names: Vec<&str> = channel_order(channels).iter().map(|(_, n)| *n).collect();
    format!("[ {} ]", names.join(" "))
}

/// The property set of the host-owned `support.null-audio-sink` — the sink apps play into in
/// [`NullSink`](CaptureMode::NullSink) mode.
///
/// A pure function returning `(key, value)` pairs rather than a built `Properties`, because the
/// invariants below are the whole design and none of them is checkable at run time on the
/// developer's machine — the tests at the bottom of this file are:
///
/// * `factory.name` + no `object.linger`: the adapter recipe pipewire-pulse's own
///   `module-null-sink` uses (`pactl load-module module-null-sink`), and a node whose lifetime is
///   this connection's — a host that crashes leaves no ghost sink behind, and WirePlumber falls
///   back to automatic election for local audio.
/// * `audio.rate`/`audio.channels`/`audio.position`: the sink is created at the *session's*
///   format, which is what makes [`probe_capture_rate`]'s `Declared` answer honest and lets a
///   game render real 5.1/7.1 into a host whose own hardware is stereo.
/// * **`node.force-quantum`, not `node.latency`**: a driver's quantum is the smallest
///   `node.latency` among its followers, clamped — and then, because PipeWire's
///   `default.clock.power-of-two-quantum` defaults to *true*, rounded DOWN to a power of two.
///   That is why our 240-frame (5 ms) ask has been silently served as 128 on every stock Linux
///   host. `node.force-quantum` skips the rounding, and because this sink drives only its own
///   group it forces nothing on anybody else's device — which is exactly why the same key would
///   have been the wrong answer while we were borrowing somebody's hardware clock.
/// * `priority.session = 50`: LOW on purpose. Between sessions the sink stays alive (the
///   capturer is parked, not torn down) and must never win WirePlumber's *automatic* default
///   election against real hardware; routing comes from the [`stream_sink`] claim.
/// * **No `priority.driver`**: 0 means the graph never elects this node to clock somebody else's
///   driver-less group. It drives ours because our stream is linked to it, and nothing else.
/// * `session.suspend-timeout-seconds = 0`: Wine churns its audio device through a game's first
///   minute; each suspend/resume round trip is a real hole in a stream someone is listening to.
/// * `monitor.*`: pipewire-pulse's own defaults for a null sink, so the volume slider on
///   "Punktfunk Stream Speaker" keeps behaving the way it does on the 0.30 stream sink.
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
/// ⚠ The desktop **sink** now IS an adapter (`null_sink_props`), and that is not a contradiction:
/// this result is about an `Audio/Source/Virtual` adapter — the direction WirePlumber has no
/// monitor path for and reroutes feeders away from — while a null-sink adapter captured through
/// its monitor is the direction every virtual-sink recipe uses. The two were validated
/// separately, and neither result transfers to the other.
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

        // In null-sink mode the sink apps play into is a REAL node this host creates: a
        // `support.null-audio-sink` adapter, the same object `pactl load-module module-null-sink`
        // makes. Unlike a stream node it is a **driver** — the null sink publishes
        // `node.driver=true` and the adapter forwards it — carrying its own `timerfd` inside the
        // daemon's realtime data loop, so the group {game streams → this sink → our monitor tap}
        // owns its clock and PipeWire never borrows one from an unrelated device (module docs).
        //
        // Created BEFORE the capture stream connects, on the same connection, so the sink is
        // registered first and the tap's `target.object` resolves without waiting. It is
        // destroyed with this connection (no `object.linger`), which is what the loop thread's
        // exit relies on.
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
                // The server answers asynchronously: `bound` is the sink existing (and its graph
                // id, which the driver diagnostic compares against), `error` is a daemon without
                // the adapter factory or the null-sink plugin — rare, and today's only new way to
                // have no audio at all, so it says what to set instead of dying quietly. The
                // core-error listener above ends the thread either way, which puts the session's
                // reopen-with-backoff in charge, exactly as a stream error does.
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

        // The `NODE_LATENCY` ask, built at run time because the rate is now a session value:
        // `<quantum frames>/<rate>` is how PipeWire spells a latency, and both halves move
        // together so the ask stays 5 ms at 48 kHz and at 96 kHz alike. Formatted once here
        // rather than at each use so the two property arms cannot drift apart.
        let node_latency = format!("{}/{}", capture_quantum_frames(rate_hz), rate_hz);
        // ── Which node is clocking us ────────────────────────────────────────────────────
        // `node.driver-id` on our own node names the driver of the group we are scheduled in.
        // It is deliberately NOT in the registry's announce set (`pw_impl_node_register`'s key
        // list), so it takes a bind and the node's `info` event; the daemon republishes the
        // props whenever the graph is recalculated.
        //
        // This line exists because on 2026-08-14 the question "what is clocking desktop audio?"
        // cost four field logs, a bespoke probe script and a `pw-top` DRIVER column to answer —
        // and the answer was a sound card attached over the network that nothing was linked to.
        // Whatever the next such box is, it now says so itself, in the log the reporter already
        // sends. Reported on CHANGE, not per window: it moves a handful of times a session, and
        // the capture summary is written from the RT callback while this arrives on the main
        // loop.
        struct GraphDriver {
            /// `node.name` of every Node global, so the driver can be NAMED and not just
            /// numbered. Pruned on removal — a host runs for days and streams come and go.
            names: HashMap<u32, String>,
            /// Our own node, bound so that its `info` — and with it `node.driver-id` — arrives.
            ours: Option<(pw::node::Node, pw::node::NodeListener)>,
            /// The last driver reported, so only changes are logged.
            driver: Option<u32>,
        }
        // In null-sink mode there is exactly one right answer and it is ours; the legacy
        // topologies borrow a driver by design, so there the line names it without judging it.
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
                                // Absent = we are between drivers (the daemon drops the key from
                                // a node that has none), which is not worth a line: the next
                                // assignment reports itself.
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
                                    // Both legacy topologies have no driver of their own, so
                                    // borrowing one is the design and not a fault — but WHICH one
                                    // is still the first thing anybody investigating wants.
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
            // Null-sink mode: the sink is the adapter created above and this stream is a MONITOR
            // TAP of it — the same `stream.capture.sink=true` recipe as the legacy arm below,
            // except aimed by name so it can only ever be ours.
            CaptureMode::NullSink => {
                let name = sink_name
                    .as_deref()
                    .context("null-sink mode without a sink name")?;
                let mut p = properties! {
                    *pw::keys::MEDIA_TYPE          => "Audio",
                    *pw::keys::MEDIA_CATEGORY      => "Capture",
                    *pw::keys::MEDIA_ROLE          => "Music",
                    *pw::keys::STREAM_CAPTURE_SINK => "true",
                    // A passive link does not, on its own, make either end runnable. Between
                    // sessions — parked capturer, nothing playing into the sink — the group is
                    // therefore idle and the null sink's timer parks with it, so this topology
                    // costs nothing while nobody is streaming. That is the objection (R5) which
                    // kept `node.always-process` off the 0.30 stream sink, answered by
                    // construction rather than by a knob. While a game plays, its own
                    // (non-passive) link makes the sink runnable and the graph walks that through
                    // the monitor to us, so nothing about capture changes.
                    *pw::keys::NODE_PASSIVE        => "true",
                    // Wait for OUR sink; never fall back to a hardware sink's monitor, not even
                    // for the moment before ours registers — recording the box's real output
                    // would be the wrong audio, and briefly rejoining a hardware driver's group
                    // is the defect this whole mode exists to remove.
                    //
                    // ⚠ These two are a PAIR. WirePlumber (0.5 `find-defined-target.lua`) reads
                    // `node.dont-fallback` alone as licence to DESTROY this stream the moment the
                    // target is not visible ("defined target not found"); `node.linger` is what
                    // turns that into "wait for it". Never ship one without the other.
                    "node.dont-fallback"           => "true",
                    "node.linger"                  => "true",
                };
                p.insert(*pw::keys::NODE_NAME, capture_name.as_str());
                // Spelled out because pipewire-rs only exposes `TARGET_OBJECT` behind its
                // `v0_3_44` feature, and a key constant is not worth widening the API surface
                // this crate compiles against. WirePlumber matches this value against
                // `node.name` (or `object.serial`) — 0.5 `find-defined-target.lua`.
                p.insert("target.object", name);
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
            // Stream-sink mode: this stream IS the sink (media.class + Direction::Input). Apps
            // play into it, PipeWire mixes them, process() receives the mix. Mirrors the
            // validated PwMicSource recipe (stream node + RT_PROCESS; see its property
            // comments). Kept as the escape hatch from the mode above for one release.
            CaptureMode::StreamSink => {
                let name = sink_name
                    .as_deref()
                    .context("stream-sink mode without a sink name")?;
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
                p.insert(*pw::keys::NODE_NAME, name);
                // Ask for a ~5 ms quantum (= one protocol audio frame) so buffers arrive
                // smoothly rather than in bursts the client's jitter buffer would hear as
                // glitching. Inserted rather than written in the `properties!` literal because
                // the rate is negotiated — same reason as `NODE_NAME` above.
                p.insert(*pw::keys::NODE_LATENCY, node_latency.as_str());
                p
            }
            // Legacy: capture the default sink's monitor (system output), not a microphone.
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
                    // `mode` says WHICH source this format describes, and that changes how much
                    // it is worth. In both sink modes the host owns the sink, so this IS the
                    // format apps render into and the desktop mix cannot have been narrowed
                    // before we saw it. In LEGACY monitor mode we are capturing someone else's
                    // sink through PipeWire's resampler: a 16 kHz Bluetooth headset upstream
                    // would still be reported here as a clean 48 kHz, exactly the way WASAPI's
                    // autoconvert hid the same thing on Windows (the 2026-08-03 report). So this
                    // line is a fact about OUR stream and never about the content in legacy mode
                    // — the monitored node's own rate is a registry lookup, and it lives in
                    // `monitor_rate`, where the hi-res gate reads it before the `Welcome`.
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
                            // …and their SHAPE: bucket counts under 20/50/100 ms and ≥ 100 ms,
                            // plus the audio they cost. Sixty 30 ms stalls and fifty-nine
                            // hiccups plus one outage share `gaps=60`; they do not share this.
                            gap_hist = %ud.stats.gap_hist(),
                            missing_ms = ud.stats.missing_ms(),
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

        // RT_PROCESS in both sink modes for the same reason as the mic: the node must be a
        // *synchronous* graph node that joins the driver group it belongs to and is actually
        // driven (see the mic's connect comment — async device-class stream nodes on a busy
        // graph never acquire a driver and their process() never fires). It also puts the
        // callback on a data loop libpipewire schedules at SCHED_RR, which is where a capture
        // callback belongs and where the mainloop boost above can never reach.
        let mut flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
        if mode.owns_sink() {
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

    /// The three spellings of `PUNKTFUNK_STREAM_SINK`, and the rule that anything else is the
    /// default: a typo in a field-debugging variable must never be the reason a session has no
    /// audio.
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

    /// The pod form and the property form of the channel map describe the SAME layout. They are
    /// consumed by different things (a stream's format vs a created node's `audio.position`) and
    /// a disagreement would mean the sink takes audio in one order and hands it on in another —
    /// silently, as a channel swap nobody can see in a log.
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
        // Spelled out, because these exact strings are what PipeWire parses.
        assert_eq!(spa_position_names(2), "[ FL FR ]");
        assert_eq!(spa_position_names(6), "[ FL FR FC LFE RL RR ]");
        assert_eq!(spa_position_names(8), "[ FL FR FC LFE RL RR SL SR ]");
    }

    /// The invariants of the created sink, each of which is a decision that cost a field
    /// investigation to reach (see [`null_sink_props`]) and none of which fails loudly if it
    /// silently changes.
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
        // The 5 ms ask, stated in the one form PipeWire will not round down to 128.
        assert_eq!(get("node.force-quantum"), Some("240"));
        assert_eq!(get("session.suspend-timeout-seconds"), Some("0"));
        assert_eq!(get("priority.session"), Some("50"));
        // A sink that outlives its creator would wedge routing on a node nothing owns; a sink
        // with a driver priority would be elected to clock OTHER people's driver-less groups,
        // which is the very defect this mode exists to end.
        assert_eq!(get("object.linger"), None);
        assert_eq!(get("priority.driver"), None);
        // Hi-res: the quantum is a LATENCY, so it scales with the rate (5 ms either way).
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
