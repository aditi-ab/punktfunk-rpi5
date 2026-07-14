//! The embeddable `punktfunk/1` client connector, behind the `quic` feature.
//!
//! [`NativeClient::connect`] runs the full client side of the protocol — QUIC handshake
//! ([`crate::quic`]), UDP data plane ([`crate::session::Session`] on a native thread), input
//! datagrams — and hands the embedder a dead-simple surface: *pull reassembled access units,
//! push input events*. This is what the platform clients (SwiftUI/VideoToolbox, Android, …)
//! link via the C ABI (`punktfunk_connect` & co. in [`crate::abi`]); `punktfunk-probe` is the
//! Rust-native consumer of the same flow.
//!
//! Threading: one worker thread owns a tokio runtime (QUIC control plane only — design
//! invariant) plus a blocking data-plane pump; frames cross to the embedder over a bounded
//! channel. All methods are safe to call from any single embedder thread.

use crate::abr::BitrateController;
use crate::config::{CompositorPref, GamepadPref, Mode, Role};
use crate::error::{PunktfunkError, Result};
use crate::input::InputEvent;
use crate::packet::FLAG_PROBE;
use crate::quic::{
    accept_resync, endpoint, io, wall_clock_ns, window_loss_ppm, BitrateChanged, ClockEcho,
    ClockResync, ColorInfo, HdrMeta, Hello, HidOutput, LossReport, ProbeRequest, ProbeResult,
    Reconfigure, Reconfigured, RequestKeyframe, ResyncStep, RfiRequest, RichInput, SetBitrate,
    Start, Welcome,
};
use crate::session::{Frame, Session};
use crate::transport::UdpTransport;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Join `host` and `port` for `SocketAddr` parsing, bracketing a bare IPv6 literal
/// (`fd00::1` → `[fd00::1]:4770`) — without the brackets the joined string can never parse and
/// the error blames the caller's input. The control/data sockets are still IPv4-bound today, so
/// a v6 dial fails at connect (with an honest IO error); this is the parse-side groundwork for
/// IPv6 support. V4 literals, hostnames, and already-bracketed input pass through unchanged.
fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// A control-stream request the embedder makes on the open handshake stream: a mode switch or a
/// speed test. One outbound channel carries both so the worker's `select!` has a single writer
/// (two `&mut ctrl_send` borrows across select branches don't compile).
enum CtrlRequest {
    Mode(Mode),
    Probe(ProbeRequest),
    Keyframe,
    /// Reference-frame-invalidation recovery: the client saw a `frame_index` gap and reports the
    /// invalidation range so an RFI-capable host re-references a known-good picture instead of
    /// forcing a full IDR. See [`RfiRequest`].
    Rfi(RfiRequest),
    Loss(LossReport),
    /// Adaptive bitrate: ask the host to re-target its encoder (kbps). Sent by the pump's
    /// [`BitrateController`] when the user's bitrate setting is Automatic.
    SetBitrate(u32),
    /// Start a mid-stream clock re-sync batch now (see [`ClockResync`]). Sent by the pump on
    /// its report tick after the first no-op clock flush — the "the clock stepped under me"
    /// signal; the control task also self-triggers one every [`CLOCK_RESYNC_INTERVAL`].
    ClockResync,
}

/// What the worker reports to [`NativeClient::connect`] once the handshake lands: the
/// [`Welcome`]-resolved session parameters (mode, backends, encode/colour/audio geometry) plus the
/// host certificate fingerprint and the connect-time clock offset. Mirrored one-to-one onto the
/// public `NativeClient` fields of the same names.
#[derive(Clone, Copy)]
struct Negotiated {
    mode: Mode,
    compositor: CompositorPref,
    gamepad: GamepadPref,
    /// SHA-256 of the certificate the host actually presented (TOFU callers persist this).
    host_fingerprint: [u8; 32],
    /// The encoder bitrate the host actually configured (kbps); `0` = an older host.
    bitrate_kbps: u32,
    /// Host clock minus client clock (ns); `0` = no skew handshake (old host / synced clocks).
    clock_offset_ns: i64,
    /// Min RTT of the connect-time skew handshake (ns); `None` = the host never answered —
    /// mid-stream re-syncs are pointless then and stay off. The re-sync acceptance guard
    /// compares each batch against this baseline ([`accept_resync`]).
    clock_rtt_ns: Option<u64>,
    /// Resolved encode bit depth: `8`, or `10` for a Main10 / HDR session.
    bit_depth: u8,
    /// Resolved CICP colour signalling.
    color: ColorInfo,
    /// Resolved chroma subsampling as the HEVC `chroma_format_idc` (1 = 4:2:0, 3 = 4:4:4).
    chroma_format: u8,
    /// Resolved audio channel count (2/6/8) — what the Opus decoders must be built from.
    audio_channels: u8,
    /// The single codec the host will emit (`quic::CODEC_*`).
    codec: u8,
}

/// Accumulated state of an in-flight / finished speed test. The data-plane pump mirrors the
/// session's packet-level receive counters here; the control task finalizes the delivered figure
/// and folds in the host's [`ProbeResult`] when it lands. Read by [`NativeClient::probe_result`].
///
/// Counting at the *packet* level (every delivered wire packet) — not whole reassembled probe AUs —
/// is what makes the measurement degrade gracefully: once loss exceeds the FEC budget no AU
/// completes, so the old AU-based count cliffed to zero even though most bytes still arrived.
#[derive(Default)]
struct ProbeState {
    /// A probe is in progress: set by `request_probe`, cleared when the host's [`ProbeResult`]
    /// lands (a re-probe just overwrites the whole state — the latest one wins).
    active: bool,
    /// `session.stats()` receive counters at the burst's start (snapshotted by the pump on its first
    /// tick while active) and latest, mirrored every pump iteration.
    base_packets: Option<u64>,
    base_bytes: Option<u64>,
    rx_packets_now: u64,
    rx_bytes_now: u64,
    /// Delivered wire packets / plaintext bytes (header + shard), frozen when the host's report lands
    /// (so resumed video after the burst can't inflate them).
    delivered_packets: u64,
    delivered_bytes: u64,
    /// The host's end-of-burst report.
    host_goodput_bytes: u64,
    host_au: u32,
    /// Wire packets the host actually put on the link, and the ones its send buffer dropped.
    host_wire_packets: u32,
    host_send_dropped: u32,
    /// The host's measured burst duration (the throughput denominator).
    host_duration_ms: u32,
    /// The host's `ProbeResult` arrived → the measurement is final.
    done: bool,
}

/// A finished/partial speed-test measurement, returned by [`NativeClient::probe_result`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeOutcome {
    /// The host's end-of-burst report has arrived — the numbers below are final.
    pub done: bool,
    /// Delivered wire bytes (header + shard) / packets the client received during the burst.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Application goodput bytes / access units the host offered.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// The burst duration the host measured, in milliseconds (the throughput denominator).
    pub elapsed_ms: u32,
    /// Delivered wire throughput = `recv_bytes * 8 / elapsed_ms` (kilobits/second). The figure to
    /// drive a [`Hello::bitrate_kbps`] choice from (allow headroom for the FEC overhead + loss).
    pub throughput_kbps: u32,
    /// Link loss = `(wire_packets_sent − received) / wire_packets_sent`, percent. Packets the host
    /// put on the wire that didn't arrive.
    pub loss_pct: f32,
    /// Host-side drop = `send_dropped / (wire_packets_sent + send_dropped)`, percent. Packets the
    /// host's send buffer couldn't accept (raise `net.core.wmem_max` / lower the rate). Distinct
    /// from `loss_pct`: this is the host failing to keep up, not the link dropping traffic.
    pub host_drop_pct: f32,
    /// Wire packets the host put on the link and the ones its send buffer dropped (raw counts).
    pub wire_packets_sent: u32,
    pub send_dropped: u32,
}

/// Depth at/above which the pre-decode hand-off queue counts as "not draining" for the clock-free
/// standing-queue detector. A consumer that keeps up (or drains newest-per-vsync, like the Apple
/// client) holds this near 0; a transient Wi-Fi clump or a small jitter buffer spikes it briefly then
/// drains. Sits above a reasonable jitter buffer (~100 ms @ 60 fps) so only a genuine backlog trips it.
const QUEUE_HIGH: usize = 6;

/// Depth at/below which the hand-off queue is considered drained — resets the standing-queue counter.
/// A true standing queue never falls back to this; a clump does within a few frames.
const QUEUE_LOW: usize = 2;

/// Consecutive frames the hand-off queue must sit ≥ [`QUEUE_HIGH`] (never dropping to [`QUEUE_LOW`])
/// before the pump declares a standing backlog and jumps to live. ~0.5 s at 60 fps — long enough that
/// a burst/clump (which drains in a few frames) never reaches it.
const STANDING_FRAMES: u32 = 30;

/// Memory backstop on the pre-decode hand-off queue. The standing-queue detector jumps to live long
/// before this (typically ≤ QUEUE_HIGH + STANDING_FRAMES deep), and a jump already requested a
/// keyframe, so on the rare path that outruns it (a wedged consumer during the flush cooldown) dropping
/// the OLDEST queued AU is safe — the pending IDR re-anchors decode regardless. Purely bounds memory.
const FRAME_QUEUE_HARD_CAP: usize = 90;

/// Backlog latency bound: when completed frames keep arriving further than this behind the host's
/// capture clock (skew-corrected), the pump jumps to live (discards the receive backlog + the queued
/// AUs and requests a keyframe) instead of playing that far behind forever. Deliberately generous — an
/// interactive stream is unusable well before 400 ms, but the bound must sit safely above the skew
/// handshake's own error (≈ RTT/2) plus normal delivery jitter so a healthy stream can never trip it.
/// This is the CLOCK-BASED detector; the clock-free [`QUEUE_HIGH`]/[`STANDING_FRAMES`] detector covers
/// same-clock and no-handshake sessions (where `clock_offset_ns == 0` disarms this one).
const FLUSH_LATENCY: Duration = Duration::from_millis(400);

/// How many CONSECUTIVE over-bound frames arm the clock-based jump (~0.5 s at 60 fps). A genuine
/// standing queue puts EVERY frame over the bound; a one-off burst (an IDR, a Wi-Fi scan blip) clears
/// within a frame or two and never reaches the count.
const FLUSH_AFTER_FRAMES: u32 = 30;

/// Minimum spacing between jump-to-live events, so a bottleneck that instantly rebuilds the queue (a
/// link/consumer that can't sustain the bitrate at all) degrades into a periodic skip + a logged
/// warning instead of a continuous flush/keyframe storm.
const FLUSH_COOLDOWN: Duration = Duration::from_secs(2);

/// A clock-triggered jump-to-live that discarded fewer datagrams than this (and no queued AUs)
/// found NO local backlog: the frames read as late, but nothing here was actually behind. Two
/// causes, and flushing helps neither: a **wall-clock step** (NTP mid-session on either end)
/// shifted the skew-corrected latency by a constant — every future frame reads over-bound and the
/// detector would fire forever, one flush + recovery IDR per cooldown, dragging the bitrate
/// controller to its floor; or the delay is standing in an **upstream queue** (router bufferbloat),
/// which a local flush can't drain — the OWD signal already feeds the bitrate controller, the
/// actual remedy. Even at the 5 Mbps bitrate floor a genuine 400 ms backlog is ~170 datagrams, so
/// 64 cleanly separates "empty" from "real". See `NOOP_CLOCK_FLUSHES_TO_DISARM`.
const NOOP_FLUSH_DATAGRAMS: u64 = 64;

/// Consecutive no-op clock-triggered flushes (see [`NOOP_FLUSH_DATAGRAMS`]) before the clock-based
/// detector is disarmed. The clock-free standing-queue detector stays armed — it measures the
/// local queue directly and can't be fooled by a clock step. No longer for the rest of the
/// session: an applied mid-stream clock re-sync re-arms the detector (the disarm stays as the
/// final backstop between re-syncs).
const NOOP_CLOCK_FLUSHES_TO_DISARM: u32 = 2;

/// Cadence of the control task's periodic mid-stream clock re-sync (see [`ClockResync`]): often
/// enough to bound slow drift and pick up an NTP step within a minute, rare enough to be free
/// (8 tiny control messages per batch). The pump additionally fires one immediately after the
/// FIRST no-op clock flush — the moment a step is actually suspected.
const CLOCK_RESYNC_INTERVAL: Duration = Duration::from_secs(60);

/// Outbound mic uplink queue depth: 5 ms Opus frames, so 64 is ~320 ms of audio — far beyond
/// any worker stall a live mic session survives anyway. On overflow the FRESH frame is dropped
/// (a tokio mpsc can't shed from the head; by the time 320 ms are queued the stream is broken
/// either way, and the bound is about memory, not audio quality) and logged at debug.
const MIC_QUEUE: usize = 64;

/// Outbound control-request queue depth. The requests are sparse (mode switches, keyframe
/// requests, ~1.3 loss reports/s, clock re-syncs) — 32 is hours of headroom; a full queue means
/// the control task is wedged, which callers treat as a closed session.
const CTRL_QUEUE: usize = 32;

/// The pre-decode video hand-off from the data-plane pump to the embedder. Unlike the side planes
/// (self-contained samples that drop the newest on overflow), video AUs are reference-chained under the
/// host's infinite GOP: dropping ANY frame mid-stream corrupts every dependent frame until the next
/// IDR. So this queue is strictly FIFO and never drops a frame from the middle. When the embedder falls
/// PERSISTENTLY behind — the queue stops draining — the pump JUMPS TO LIVE instead ([`clear`] + a
/// keyframe request), so decode resumes cleanly at an IDR rather than ratcheting latency forever (the
/// old bounded channel silently dropped the NEWEST AU on overflow — backwards for a live stream, and a
/// reference-chain break the loss counters never saw). A transient burst fills it briefly and drains on
/// its own, so a clump never costs a keyframe.
///
/// [`clear`]: FrameChannel::clear
struct FrameChannel {
    inner: Mutex<FrameQueue>,
    ready: Condvar,
}

struct FrameQueue {
    q: VecDeque<Frame>,
    /// Set when the pump exits so a blocked [`FrameChannel::pop`] reports the stream ended
    /// ([`PunktfunkError::Closed`]) rather than a spurious timeout (the old mpsc did this on sender drop).
    closed: bool,
}

/// Outcome of [`FrameChannel::pop`] — mirrors the old `recv_timeout` results so `next_frame`'s
/// Timeout/Closed mapping is unchanged.
enum FramePop {
    Frame(Frame),
    Timeout,
    Closed,
}

impl FrameChannel {
    fn new() -> Self {
        Self {
            inner: Mutex::new(FrameQueue {
                q: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    /// Pump side: append a completed AU and wake a blocked consumer. Enforces the memory backstop
    /// ([`FRAME_QUEUE_HARD_CAP`]) by dropping the oldest (see its doc — a jump-to-live keyframe is
    /// already in flight by the time this can bite).
    fn push(&self, frame: Frame) {
        let mut st = self.inner.lock().unwrap();
        st.q.push_back(frame);
        while st.q.len() > FRAME_QUEUE_HARD_CAP {
            st.q.pop_front();
        }
        drop(st);
        self.ready.notify_one();
    }

    /// Pump side: current queued depth — the clock-free standing-queue signal.
    fn depth(&self) -> usize {
        self.inner.lock().unwrap().q.len()
    }

    /// Pump side: discard the whole backlog (the jump-to-live path); returns how many were dropped.
    fn clear(&self) -> usize {
        let mut st = self.inner.lock().unwrap();
        let n = st.q.len();
        st.q.clear();
        n
    }

    /// Pump side: mark the stream ended and wake every blocked consumer.
    fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    /// Consumer side: pop the oldest AU, waiting up to `timeout` for one to arrive.
    fn pop(&self, timeout: Duration) -> FramePop {
        let mut st = self.inner.lock().unwrap();
        if st.q.is_empty() && !st.closed {
            st = self.ready.wait_timeout(st, timeout).unwrap().0;
        }
        if let Some(f) = st.q.pop_front() {
            FramePop::Frame(f)
        } else if st.closed {
            FramePop::Closed
        } else {
            FramePop::Timeout
        }
    }
}

/// Audio packets buffered for the embedder: 64 × 5 ms = 320 ms of slack. A lagging
/// embedder drops the newest packet (the audio renderer conceals the gap).
const AUDIO_QUEUE: usize = 64;

/// Rumble updates buffered for the embedder. Overflow drops the NEWEST update (same
/// `try_send` discipline as the other planes) — the host renews rumble state periodically
/// (v2 envelopes) or re-sends it (legacy v1), so a dropped transition (including a stop) heals
/// within one renewal/refresh period.
const RUMBLE_QUEUE: usize = 16;

/// A rumble update handed to the embedder: `(pad, low, high, ttl_ms)`. `ttl_ms` is `Some(ms)` for
/// a self-terminating v2 envelope (render for at most that long) and `None` for a legacy v1
/// datagram (an old host — the renderer applies its own staleness policy). The seq from a v2
/// envelope is consumed by the reorder gate in the datagram demux and is NOT forwarded.
type RumbleUpdate = (u16, u16, u16, Option<u16>);

/// HID-output (DualSense lightbar / player LEDs / adaptive triggers) buffered for the embedder.
/// Same overflow discipline as rumble; the host re-sends on the next feedback change.
const HIDOUT_QUEUE: usize = 32;

/// Static HDR metadata (ST.2086 mastering + content light level) buffered for the embedder. Tiny
/// and low-rate (one on start, re-sent on mastering changes / keyframes); a small ring is ample.
const HDR_META_QUEUE: usize = 8;

/// Host-timing plane depth (0xCF, one datagram per AU). Sized for a 240 fps stream whose stats
/// consumer drains once per second with headroom; overflow drops the newest sample (try_send) —
/// harmless, it's per-frame observability, not state.
const HOST_TIMING_QUEUE: usize = 512;

/// One Opus packet from the host's audio datagram stream (48 kHz stereo, 5 ms frames).
#[derive(Clone, Debug)]
pub struct AudioPacket {
    pub seq: u32,
    pub pts_ns: u64,
    /// The raw Opus payload — feed it to an Opus decoder as one frame.
    pub data: Vec<u8>,
}

/// At most one client→host RFI request per this window, so a burst of frame-index gaps (a
/// full-screen pan shedding shards) can't storm the control stream. Matches the shared Vulkan pump's
/// recovery-request throttle; the host coalesces further.
const RFI_THROTTLE: Duration = Duration::from_millis(100);

/// State for [`NativeClient::note_frame_index`] — the client-side loss-range detector shared by every
/// embedder (Android, the C-ABI Apple client, the Windows shell pump) so none re-derives the wrapping
/// frame-index arithmetic. `next_expected` is the `frame_index` expected next in receive order;
/// `last_req` throttles the RFI requests a gap fires.
#[derive(Default)]
struct RfiRecovery {
    next_expected: Option<u32>,
    last_req: Option<Instant>,
}

/// What a forward gap should ask the host for: a precise RFI for a recoverable range, a plain
/// keyframe for a range wider than any encoder's reference history
/// ([`crate::packet::RFI_MAX_RANGE`] — a seconds-long outage, or a phantom index jump such as an
/// old host's speed-test burst consuming video indexes), or nothing (contiguous / straggler /
/// throttled).
#[derive(Debug, PartialEq, Eq)]
enum RecoveryAsk {
    None,
    Rfi(u32, u32),
    Keyframe,
}

impl RfiRecovery {
    /// Pure decision behind [`NativeClient::note_frame_index`]: fold one received `frame_index` (in
    /// receive order) observed at `now`, advancing the expectation and returning `(gap, ask)`.
    /// `gap` is whether this frame revealed a forward gap (the embedder arms its post-loss display
    /// freeze on it); `ask` is the (throttled) recovery request to fire — an RFI naming the exact
    /// lost span, or a keyframe when the span exceeds [`crate::packet::RFI_MAX_RANGE`] (RFI is
    /// hopeless there: no encoder holds references that old, and a huge jump is more likely a
    /// resync — e.g. the first real AU after an old host's speed test — than a real loss). Split
    /// out from the connection so the wrapping arithmetic + [`RFI_THROTTLE`] are unit-testable
    /// without a live session (see the tests below).
    fn observe(&mut self, frame_index: u32, now: Instant) -> (bool, RecoveryAsk) {
        match self.next_expected {
            Some(exp) => {
                // Wrapping split at the half-space: a small positive delta is a forward gap
                // (missing frames); a delta in the top half is a straggler behind us.
                let ahead = frame_index.wrapping_sub(exp);
                if ahead == 0 {
                    self.next_expected = Some(frame_index.wrapping_add(1)); // contiguous
                    (false, RecoveryAsk::None)
                } else if ahead < u32::MAX / 2 {
                    // Forward gap: [exp, frame_index-1] lost. Advance past this frame so the same
                    // gap isn't re-detected, then fire a throttled recovery ask for the lost range.
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    let send = self
                        .last_req
                        .is_none_or(|t| now.duration_since(t) >= RFI_THROTTLE);
                    if send {
                        self.last_req = Some(now);
                    }
                    let ask = if !send {
                        RecoveryAsk::None
                    } else if ahead > crate::packet::RFI_MAX_RANGE {
                        RecoveryAsk::Keyframe
                    } else {
                        RecoveryAsk::Rfi(exp, frame_index.wrapping_sub(1))
                    };
                    (true, ask)
                } else {
                    // Straggler behind the delivery point — leave the expectation.
                    (false, RecoveryAsk::None)
                }
            }
            None => {
                self.next_expected = Some(frame_index.wrapping_add(1));
                (false, RecoveryAsk::None)
            }
        }
    }
}

pub struct NativeClient {
    // Each plane's receiver sits behind its own mutex so `NativeClient` is `Sync` and Rust
    // embedders can share one `Arc<NativeClient>` across their plane threads (the same
    // one-thread-per-plane contract the C ABI documents — the lock is uncontended there,
    // and two threads racing one plane now serialize instead of being undefined).
    frames: Arc<FrameChannel>,
    audio: Mutex<Receiver<AudioPacket>>,
    rumble: Mutex<Receiver<RumbleUpdate>>,
    /// Inbound DualSense feedback (lightbar / player LEDs / adaptive triggers) — 0xCD datagrams.
    hidout: Mutex<Receiver<HidOutput>>,
    /// Inbound static HDR metadata (ST.2086 mastering + content light level) — 0xCE datagrams.
    hdr_meta: Mutex<Receiver<HdrMeta>>,
    /// Inbound per-AU host capture→send timings — 0xCF datagrams (the client always advertises
    /// [`quic::VIDEO_CAP_HOST_TIMING`]; an older host simply never sends any).
    host_timing: Mutex<Receiver<crate::quic::HostTiming>>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    /// Outbound mic frames `(seq, pts_ns, opus)` → encoded as 0xCB datagrams by the worker.
    /// Bounded ([`MIC_QUEUE`]): a wedged worker drops fresh frames (logged) instead of queueing
    /// audio-latency (and memory) without limit — mic is best-effort end to end.
    mic_tx: tokio::sync::mpsc::Sender<(u32, u64, Vec<u8>)>,
    /// Outbound rich input (DualSense touchpad / motion) → 0xCC datagrams by the worker.
    rich_input_tx: tokio::sync::mpsc::UnboundedSender<RichInput>,
    /// Outbound control-stream requests (mode switch, speed test) → the worker's control task.
    /// Bounded ([`CTRL_QUEUE`]) — the requests are sparse; a full queue means the control task
    /// is wedged/dead, and callers treat it like a closed session.
    ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Speed-test accumulator, shared with the data-plane pump + control task.
    probe: Arc<Mutex<ProbeState>>,
    shutdown: Arc<AtomicBool>,
    /// Deliberate-quit flag: [`NativeClient::disconnect_quit`] sets it, so the worker closes the QUIC
    /// connection with [`crate::quic::QUIT_CLOSE_CODE`] (a user "stop") instead of code 0 — telling the
    /// host to skip the keep-alive linger. A plain drop leaves it false → an unwanted-disconnect close.
    quit: Arc<AtomicBool>,
    /// Cumulative count of access units the reassembler gave up on (FEC couldn't recover), mirrored
    /// from the data-plane pump's `Session`. A client video loop watches this for increases to request
    /// a recovery keyframe under infinite GOP — the correct loss trigger, since unrecoverable loss
    /// yields reference-missing frames the decoder silently conceals (a decode-error trigger misses them).
    frames_dropped: Arc<AtomicU64>,
    /// Cumulative count of FEC shards the reassembler recovered (parity repaired a lost data
    /// packet), mirrored from the data-plane pump's `Session` like `frames_dropped`. Observability
    /// for the client stats HUDs (the unified spec's per-window `FEC` counter — proof FEC is
    /// earning its keep); readers window it by diffing successive reads.
    fec_recovered: Arc<AtomicU64>,
    /// Client-side RFI-on-loss detector state for [`note_frame_index`](Self::note_frame_index): the
    /// next `frame_index` expected in receive order + the last RFI-request time (throttle). Lets every
    /// embedder share one loss-range detector instead of re-deriving the wrapping frame arithmetic.
    rfi: Mutex<RfiRecovery>,
    /// Kernel ids of the client's latency-critical native threads: the internal data-plane pump
    /// (UDP receive + FEC reassembly) plus any embedder plane threads registered via
    /// [`NativeClient::register_hot_thread`]. The Android client feeds these to an ADPF hint session
    /// so the CPU governor keeps the whole video pipeline on fast cores. Empty on platforms without
    /// `gettid` (see [`current_hot_tid`]).
    hot_tids: Arc<Mutex<Vec<i32>>>,
    /// The LIVE host↔client clock offset (ns): seeded with the connect-time estimate, then kept
    /// fresh by the control task's mid-stream re-syncs (every [`CLOCK_RESYNC_INTERVAL`], plus on
    /// the pump's first no-op clock flush). Shared with the pump and, via
    /// [`clock_offset_shared`](Self::clock_offset_shared), with embedder latency-math threads.
    clock_offset: Arc<AtomicI64>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// The currently active session mode (the Welcome's, then updated by every accepted
    /// [`NativeClient::request_mode`]).
    mode: Arc<std::sync::Mutex<Mode>>,
    /// SHA-256 fingerprint of the certificate the host actually presented. A TOFU caller
    /// (`pin = None`) persists this and passes it as the pin from then on.
    pub host_fingerprint: [u8; 32],
    /// The compositor backend the host actually resolved for this session ([`Welcome::compositor`]).
    /// `Auto` = an older host that didn't say. Clients use it for compositor-specific behavior (e.g.
    /// drawing a client-side cursor by default on gamescope, whose capture carries no cursor).
    pub resolved_compositor: CompositorPref,
    /// The virtual gamepad backend the host actually resolved ([`Welcome::gamepad`]).
    /// `Auto` = an older host that didn't say (assume X-Box 360, no DualSense feedback).
    pub resolved_gamepad: GamepadPref,
    /// The encoder bitrate the host actually configured ([`Welcome::bitrate_kbps`], kbps): our
    /// requested rate clamped to the host's range, or its default if we requested `0`. `0` = an
    /// older host that didn't report it.
    pub resolved_bitrate_kbps: u32,
    /// Host clock minus client clock (ns), from the connect-time skew handshake. Add it to a local
    /// receive/present timestamp to express it in the host's capture clock (the AU `pts_ns`), making
    /// glass-to-glass latency valid across machines. `0` = no correction (an old host that didn't
    /// answer, or genuinely synced clocks). This is the CONNECT-TIME estimate, kept for ABI/compat;
    /// ongoing latency math should read [`clock_offset_now_ns`](Self::clock_offset_now_ns), which
    /// follows mid-stream re-syncs after a wall-clock step or drift.
    pub clock_offset_ns: i64,
    /// The encode bit depth the host resolved for this session ([`Welcome::bit_depth`]): `8`, or
    /// `10` for a Main10 / HDR session. `8` for an older host that didn't report it.
    pub bit_depth: u8,
    /// The colour signalling the host encodes with ([`Welcome::color`]): the client configures its
    /// decoder/presenter from this. [`ColorInfo::SDR_BT709`] for an older host. The static HDR
    /// mastering metadata (when [`ColorInfo::is_hdr`]) arrives via [`NativeClient::next_hdr_meta`].
    pub color: ColorInfo,
    /// The chroma subsampling the host resolved for this session ([`Welcome::chroma_format`]), as the
    /// HEVC `chroma_format_idc`: [`quic::CHROMA_IDC_420`] (4:2:0, the default / older host) or
    /// [`quic::CHROMA_IDC_444`] (full-chroma 4:4:4). The in-band SPS is authoritative; this lets the
    /// client pre-size its decoder. `CHROMA_IDC_420` for an older host that didn't report it.
    pub chroma_format: u8,
    /// The audio channel count the host resolved for this session ([`Welcome::audio_channels`]):
    /// `2` (stereo), `6` (5.1) or `8` (7.1). The client MUST build its Opus (multistream) decoder
    /// from this value (via [`crate::audio::layout_for`]) — never from its own request — so an older
    /// host that omits it (→ `2`) yields working stereo. The `0xC9` audio frames are encoded with the
    /// matching layout.
    pub audio_channels: u8,
    /// The video codec the host resolved and will emit ([`Welcome::codec`]) — [`quic::CODEC_H264`],
    /// [`quic::CODEC_HEVC`] (default / older host), or [`quic::CODEC_AV1`]. The client builds its
    /// decoder from THIS, never assuming HEVC.
    pub codec: u8,
}

/// Pin the calling thread to the user-interactive QoS class on Apple targets.
///
/// The Apple client drains every plane on `.userInteractive` Thread s (video pump, audio,
/// gamepad feedback) and connects on a `.userInitiated` Task. Those consumers block on the
/// std channels these worker threads feed; if the producers run at the default QoS, the
/// kernel sees a high-QoS thread parked waiting on a lower-QoS one and the Thread Performance
/// Checker flags a priority inversion. Matching the producers to the consumers' QoS removes
/// the inversion without slowing the Swift side. Android gets a nice-level analogue (see the
/// android arm below); a no-op elsewhere (the Linux client/host don't run a QoS scheduler, and
/// `punktfunk-probe` doesn't care).
#[cfg(target_vendor = "apple")]
fn pin_thread_user_interactive() {
    // SAFETY: sets only the current thread's QoS class — always valid to call.
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
}
/// Android analogue of the Apple QoS pin: raise the calling thread to nice −8 (the framework's
/// URGENT_DISPLAY band — apps may set negative nice on their own threads). At default nice 0 the
/// EAS scheduler happily parks the data-plane pump (UDP receive + decrypt + FEC — a thread that
/// sleeps between bursts) on a down-clocked little core, and a few ms of scheduling delay during a
/// keyframe burst overflows the socket receive buffer → wire loss the link never saw. −8 keeps the
/// pipeline below the decode thread's −10 (the display path still wins). Best-effort, like Apple's.
#[cfg(target_os = "android")]
fn pin_thread_user_interactive() {
    // SAFETY: `gettid`/`setpriority` on the calling thread are always-safe syscalls; a refusal is
    // reported via the return value (ignored — a missed boost, not an error on the data path).
    unsafe {
        let tid = libc::gettid();
        let _ = libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, -8);
    }
}
#[cfg(not(any(target_vendor = "apple", target_os = "android")))]
fn pin_thread_user_interactive() {}

/// Wall-clock now in nanoseconds (CLOCK_REALTIME basis), to compare against the host-stamped
/// capture `pts_ns` after the skew offset is applied — the same latency math the stats HUDs use.
fn now_realtime_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// The calling thread's kernel id, for hot-thread performance hints (the Android client's ADPF
/// session today; the consumer is platform-specific). Linux/Android expose `gettid`; elsewhere
/// there's nothing to hint with, so registration is a no-op.
#[cfg(any(target_os = "android", target_os = "linux"))]
fn current_hot_tid() -> Option<i32> {
    // SAFETY: `gettid` reads the calling thread's kernel id — an always-safe syscall, no args.
    Some(unsafe { libc::gettid() })
}
#[cfg(not(any(target_os = "android", target_os = "linux")))]
fn current_hot_tid() -> Option<i32> {
    None
}

/// Record the calling thread's id in the shared hot-thread registry (deduped). Best-effort: a
/// platform without `gettid` or a poisoned lock just skips it — a missed performance hint, not an
/// error on the data path.
fn register_hot_tid(reg: &Mutex<Vec<i32>>) {
    if let Some(t) = current_hot_tid() {
        if let Ok(mut v) = reg.lock() {
            if !v.contains(&t) {
                v.push(t);
            }
        }
    }
}

impl NativeClient {
    /// Connect to a `punktfunk/1` host and start the session at (up to) `mode`. Blocks until the
    /// handshake completes or `timeout` elapses.
    ///
    /// `pin`: expected SHA-256 of the host's certificate. `Some` and the host presents
    /// anything else → the handshake is rejected ([`PunktfunkError::Crypto`]). `None` = trust on
    /// first use; check [`NativeClient::host_fingerprint`] after connecting.
    ///
    /// `identity`: this client's persistent self-signed identity (PEM cert + PKCS#8 key,
    /// see [`endpoint::generate_identity`]), presented via TLS client auth so a host can
    /// recognize a paired client. `None` = anonymous (rejected by hosts requiring pairing).
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        host: &str,
        port: u16,
        mode: Mode,
        compositor: CompositorPref,
        gamepad: GamepadPref,
        bitrate_kbps: u32,
        // Client video capabilities advertised to the host (bitfield of quic::VIDEO_CAP_10BIT /
        // VIDEO_CAP_HDR) — the host upgrades to a 10-bit / HDR encode only when the matching bit is
        // set. 0 = the 8-bit BT.709 stream every client understands.
        video_caps: u8,
        // Requested audio channel count (2 = stereo / 6 = 5.1 / 8 = 7.1); the host clamps to what it
        // can capture and echoes the result in [`NativeClient::audio_channels`].
        audio_channels: u8,
        // The codecs this client can decode (bitfield of quic::CODEC_H264 / CODEC_HEVC / CODEC_AV1)
        // and the user's soft preference (a single codec bit, 0 = auto). The host resolves the codec
        // it emits from these and echoes it in [`NativeClient::codec`].
        video_codecs: u8,
        preferred_codec: u8,
        // The client display's HDR colour volume (primaries/white/luminance), read from the OS
        // (e.g. DXGI `GetDesc1`) when presenting HDR. The host forwards it into the virtual
        // display's EDID so host apps tone-map to the client's real panel; `None` = unknown/SDR
        // (the host keeps its built-in EDID defaults). See [`crate::quic::Hello::display_hdr`].
        display_hdr: Option<HdrMeta>,
        launch: Option<String>,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        let frame_chan = Arc::new(FrameChannel::new());
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<RumbleUpdate>(RUMBLE_QUEUE);
        let (hidout_tx, hidout_rx) = std::sync::mpsc::sync_channel::<HidOutput>(HIDOUT_QUEUE);
        let (hdr_meta_tx, hdr_meta_rx) = std::sync::mpsc::sync_channel::<HdrMeta>(HDR_META_QUEUE);
        let (host_timing_tx, host_timing_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::HostTiming>(HOST_TIMING_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (mic_tx, mic_rx) = tokio::sync::mpsc::channel::<(u32, u64, Vec<u8>)>(MIC_QUEUE);
        let (rich_input_tx, rich_input_rx) = tokio::sync::mpsc::unbounded_channel::<RichInput>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(CTRL_QUEUE);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Negotiated>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let mode_slot = Arc::new(std::sync::Mutex::new(mode));
        let probe = Arc::new(Mutex::new(ProbeState::default()));
        let frames_dropped = Arc::new(AtomicU64::new(0));
        let fec_recovered = Arc::new(AtomicU64::new(0));
        let hot_tids = Arc::new(Mutex::new(Vec::new()));
        let clock_offset = Arc::new(AtomicI64::new(0));

        let host = host.to_string();
        let frame_chan_w = frame_chan.clone();
        let shutdown_w = shutdown.clone();
        let quit_w = quit.clone();
        let mode_slot_w = mode_slot.clone();
        let probe_w = probe.clone();
        let frames_dropped_w = frames_dropped.clone();
        let fec_recovered_w = fec_recovered.clone();
        let hot_tids_w = hot_tids.clone();
        let clock_offset_w = clock_offset.clone();
        let ctrl_tx_pump = ctrl_tx.clone(); // the data-plane pump sends adaptive-FEC LossReports
        let worker = std::thread::Builder::new()
            .name("punktfunk-client".into())
            .spawn(move || {
                pin_thread_user_interactive(); // this thread drives the runtime + handshake
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    // Every runtime thread (async workers + the spawn_blocking pool that runs
                    // the data-plane pump) matches the Apple client's QoS — no priority inversion.
                    .on_thread_start(pin_thread_user_interactive)
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(PunktfunkError::Io(e)));
                        return;
                    }
                };
                rt.block_on(worker_main(WorkerArgs {
                    host,
                    port,
                    mode,
                    compositor,
                    gamepad,
                    bitrate_kbps,
                    video_caps,
                    audio_channels,
                    video_codecs,
                    preferred_codec,
                    display_hdr,
                    launch,
                    pin,
                    identity,
                    frames: frame_chan_w,
                    audio_tx,
                    rumble_tx,
                    hidout_tx,
                    hdr_meta_tx,
                    host_timing_tx,
                    input_rx,
                    mic_rx,
                    rich_input_rx,
                    ctrl_rx,
                    ctrl_tx: ctrl_tx_pump,
                    ready_tx,
                    shutdown: shutdown_w,
                    quit: quit_w,
                    mode_slot: mode_slot_w,
                    probe: probe_w,
                    frames_dropped: frames_dropped_w,
                    fec_recovered: fec_recovered_w,
                    hot_tids: hot_tids_w,
                    clock_offset: clock_offset_w,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        let negotiated = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                shutdown.store(true, Ordering::SeqCst);
                return Err(PunktfunkError::Timeout);
            }
        };
        *mode_slot.lock().unwrap() = negotiated.mode;
        Ok(NativeClient {
            frames: frame_chan,
            audio: Mutex::new(audio_rx),
            rumble: Mutex::new(rumble_rx),
            hidout: Mutex::new(hidout_rx),
            hdr_meta: Mutex::new(hdr_meta_rx),
            host_timing: Mutex::new(host_timing_rx),
            input_tx,
            mic_tx,
            rich_input_tx,
            ctrl_tx,
            probe,
            shutdown,
            quit,
            worker: Some(worker),
            frames_dropped,
            fec_recovered,
            rfi: Mutex::new(RfiRecovery::default()),
            hot_tids,
            clock_offset,
            mode: mode_slot,
            host_fingerprint: negotiated.host_fingerprint,
            resolved_compositor: negotiated.compositor,
            resolved_gamepad: negotiated.gamepad,
            resolved_bitrate_kbps: negotiated.bitrate_kbps,
            clock_offset_ns: negotiated.clock_offset_ns,
            bit_depth: negotiated.bit_depth,
            color: negotiated.color,
            chroma_format: negotiated.chroma_format,
            audio_channels: negotiated.audio_channels,
            codec: negotiated.codec,
        })
    }

    /// Run the PIN pairing ceremony against a host: connect (trust-on-first-use — the PIN
    /// proof is what authenticates the certificates), prove knowledge of the PIN the host
    /// is displaying, and return the host's now-verified fingerprint for pinning. The host
    /// persists this client's fingerprint in its paired set.
    ///
    /// `identity` is this client's persistent PEM identity (cert, key) — the same one
    /// later passed to [`NativeClient::connect`]; `pin` is what the user read off the host
    /// (its log / UI); `name` is the label the host stores.
    pub fn pair(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<[u8; 32]> {
        use crate::quic::{pake, PairChallenge, PairProof, PairRequest, PairResult};

        let client_fp = endpoint::fingerprint_of_pem(identity.0)
            .map_err(|_| PunktfunkError::InvalidArg("client cert pem"))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(PunktfunkError::Io)?;
        let pin = pin.to_string();
        let name = name.to_string();
        let remote: std::net::SocketAddr = join_host_port(host, port)
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;

        rt.block_on(async move {
            // The quinn endpoint must be created inside the runtime (it spawns its driver).
            let (ep, observed) = endpoint::client_pinned_with_identity(None, Some(identity));
            let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

            // The SPAKE2 exchange over an already-open bi-stream; never closes the conn (the
            // caller does, then flushes), so any early exit still lets the host see the close.
            let exchange = |conn: quinn::Connection, host_fp: [u8; 32]| async move {
                let (mut send, mut recv) = conn
                    .open_bi()
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                // SPAKE2 as A, binding our fingerprint + the host cert we observed (TOFU).
                let (pake, spake_a) = pake::start(true, &pin, &client_fp, &host_fp);
                io::write_msg(&mut send, &PairRequest { name, spake_a }.encode()).await?;
                let challenge = PairChallenge::decode(&io::read_msg(&mut recv).await?)?;
                let confirms = pake.finish(&challenge.spake_b)?;
                // The host's confirmation proves it reached the same key (right PIN, same
                // certs) — only then do we pin it and send our own confirmation.
                if !pake::verify(&confirms.host, &challenge.confirm) {
                    return Err(PunktfunkError::Crypto); // wrong PIN or MITM
                }
                io::write_msg(
                    &mut send,
                    &PairProof {
                        confirm: confirms.client,
                    }
                    .encode(),
                )
                .await?;
                let result = PairResult::decode(&io::read_msg(&mut recv).await?)?;
                if result.ok {
                    Ok(host_fp)
                } else {
                    Err(PunktfunkError::Crypto) // host rejected post-confirm
                }
            };

            let ceremony = async {
                let conn = ep
                    .connect(remote, "punktfunk")
                    .map_err(|_| PunktfunkError::InvalidArg("connect"))?
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                let host_fp = observed.lock().unwrap().ok_or(PunktfunkError::Crypto)?;
                let outcome = exchange(conn.clone(), host_fp).await;
                // Always tell the host we're done so it never blocks at its read — code 0 on
                // success, 1 on a refused/aborted ceremony.
                let code: u32 = if outcome.is_ok() { 0 } else { 1 };
                conn.close(code.into(), b"pair done");
                outcome
            };
            let outcome = tokio::time::timeout(timeout, ceremony)
                .await
                .map_err(|_| PunktfunkError::Timeout)?;
            // Flush the CONNECTION_CLOSE before the runtime is dropped — otherwise the host
            // may never see it and would block at its read for the full pairing timeout.
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
            outcome
        })
    }

    /// A lightweight, trust-agnostic reachability check: attempt the QUIC/TLS handshake to
    /// `host:port` and report whether the host answered — WITHOUT relying on mDNS presence.
    ///
    /// The saved-hosts "online" pip historically read a host as offline whenever it wasn't
    /// currently advertising on mDNS, so a host reached over a routed network (Tailscale / VPN /
    /// another subnet) — which is mDNS-blind forever — always looked offline even though it was
    /// perfectly reachable (the same failure the dial-first reconnect fix addressed for the
    /// connect action). This probe answers the real question ("does the box respond on the
    /// stream port?") by completing just the handshake and tearing it straight down.
    ///
    /// No pin and no identity are presented: hosts accept the transport-level connection
    /// regardless of pairing (client-cert auth is not mandatory at the QUIC layer —
    /// authorization is enforced per-feature), so a completed handshake means "reachable". A
    /// wrong address, closed port, or unroutable host fails the connect/`timeout` and yields
    /// `false`. Blocks up to `timeout`.
    pub fn probe(host: &str, port: u16, timeout: Duration) -> bool {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return false;
        };
        let host = host.to_string();
        rt.block_on(async move {
            // The stored address may be a hostname (Tailscale MagicDNS, an mDNS `.local` name),
            // not a bare IP literal, so resolve it rather than `SocketAddr::parse`.
            let Ok(mut addrs) = tokio::net::lookup_host((host.as_str(), port)).await else {
                return false;
            };
            let Some(remote) = addrs.next() else {
                return false;
            };
            // TOFU verifier (pin = None) accepts any cert, so a real host always completes the
            // handshake; the only failures are DNS / no route / connect timeout.
            let (ep, _observed) = endpoint::client_pinned_with_identity(None, None);
            let Ok(ep) = ep else {
                return false;
            };
            let reachable = match ep.connect(remote, "punktfunk") {
                Ok(connecting) => {
                    matches!(tokio::time::timeout(timeout, connecting).await, Ok(Ok(_)))
                }
                Err(_) => false,
            };
            ep.close(0u32.into(), b"probe");
            let _ = tokio::time::timeout(Duration::from_millis(200), ep.wait_idle()).await;
            reachable
        })
    }

    /// The currently active session mode — the Welcome's, until an accepted
    /// [`NativeClient::request_mode`] switches it.
    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    /// Ask the host to switch the live session to `mode` (no reconnect). Non-blocking:
    /// the request is queued; on acceptance the stream continues at the new mode (next
    /// frames open with an IDR carrying new parameter sets) and [`NativeClient::mode`]
    /// reflects it. A rejected request leaves the session unchanged.
    pub fn request_mode(&self, mode: Mode) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Mode(mode))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Ask the host's encoder to emit a fresh IDR keyframe now (client recovery on a stalled
    /// decode). Non-blocking, fire-and-forget — the recovered keyframe is the only ack. The
    /// caller should throttle (the decode stays wedged across several frames until the IDR
    /// lands, so requesting on every frame would flood the control stream).
    pub fn request_keyframe(&self) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Keyframe)
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Ask the host to recover from loss by **reference-frame invalidation** rather than a full IDR:
    /// the client reports the range `[first_frame, last_frame]` of access units it can no longer trust
    /// (from the first missing `frame_index` through the newest received). An RFI-capable host
    /// re-references a known-good picture before `first_frame` (AMD LTR / NVENC RFI) and emits a clean
    /// P-frame tagged [`crate::packet::USER_FLAG_RECOVERY_ANCHOR`]; a host that can't RFI forces an IDR
    /// instead (same as [`request_keyframe`](Self::request_keyframe)). Non-blocking, fire-and-forget —
    /// the recovered frame is the only ack; throttle it like the keyframe request. Prefer this over
    /// `request_keyframe` on loss so AMD/RFI hosts avoid the IDR spike; the keyframe request remains
    /// the backstop when the recovery frame itself is lost.
    pub fn request_rfi(&self, first_frame: u32, last_frame: u32) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::Rfi(RfiRequest {
                first_frame,
                last_frame,
            }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Feed each received AU's `frame_index` (in receive order) so the client recovers from loss with
    /// a cheap reference-frame invalidation instead of always paying for a full IDR. On a **forward
    /// gap** — a `frame_index` jump means the intervening frames were lost and the following AUs
    /// reference a picture the decoder never got — this fires a **throttled**
    /// [`request_rfi`](Self::request_rfi) for the lost range `[first_missing, frame_index-1]`. An
    /// RFI-capable host (AMD LTR / NVENC) then re-references a known-good frame (a clean P-frame, no
    /// 20-40x IDR spike); a host that can't RFI forces an IDR, same as the keyframe path.
    ///
    /// Call it for EVERY received frame; it is cheap and idempotent, and the
    /// [`frames_dropped`](Self::frames_dropped)-driven [`request_keyframe`](Self::request_keyframe)
    /// loop stays the backstop for when the recovery frame itself is lost. Returns `true` when a
    /// forward gap was detected on this call (whether or not the RFI was throttled), so a client with
    /// a post-loss display freeze can (re-)arm it on the same signal.
    ///
    /// This centralizes the loss-range detection so every embedder gets identical behavior. (The
    /// in-process Vulkan session pump keeps its own copy because it gates a display freeze on the same
    /// signal and shares one throttle across RFI + keyframe requests.)
    pub fn note_frame_index(&self, frame_index: u32) -> bool {
        // Decide (and update state) under the lock; fire the request after releasing it.
        let (gap, ask) = self
            .rfi
            .lock()
            .unwrap()
            .observe(frame_index, Instant::now());
        match ask {
            RecoveryAsk::Rfi(first, last) => {
                let _ = self.request_rfi(first, last);
            }
            // A gap wider than any encoder's reference history (RFI_MAX_RANGE) — a seconds-long
            // outage or a phantom index jump: RFI can't repair it, resync on a keyframe instead.
            RecoveryAsk::Keyframe => {
                let _ = self.request_keyframe();
            }
            RecoveryAsk::None => {}
        }
        gap
    }

    /// Cumulative access units the host→client reassembler dropped as unrecoverable (FEC couldn't
    /// rebuild them). A video loop polls this and calls [`request_keyframe`](Self::request_keyframe)
    /// when it increases — the correct loss trigger under infinite GOP, where unrecoverable loss
    /// produces reference-missing delta frames the decoder silently conceals (so a decode-error
    /// trigger would rarely fire). Monotonic for the session; compare against the last observed value.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }

    /// Cumulative FEC shards the host→client reassembler recovered (a parity shard repaired a lost
    /// data packet — loss that never became a dropped frame). Monotonic for the session; a stats
    /// HUD windows it by diffing successive reads, pairing it with
    /// [`frames_dropped`](Self::frames_dropped) (the losses FEC could NOT absorb).
    pub fn fec_recovered_shards(&self) -> u64 {
        self.fec_recovered.load(Ordering::Relaxed)
    }

    /// Whether the underlying QUIC session has ended — the worker's connection-close watcher set the
    /// shutdown flag (`conn.closed()` fired: a host suspend / crash / network drop idle-timed the
    /// connection out, or the host closed it), or a deliberate [`disconnect_quit`](Self::disconnect_quit)
    /// / drop did. Once `true`, every `next_*` plane returns [`PunktfunkError::Closed`] and no more
    /// frames will ever arrive. A client watchdog polls this so it can leave a frozen stream and
    /// return to the menu (where the user can wake the host) instead of sitting on the last decoded
    /// frame forever — the poll-friendly counterpart to reacting to a `Closed` in a plane loop.
    pub fn is_session_ended(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }

    /// Register the calling thread as latency-critical so a later
    /// [`hot_thread_ids`](Self::hot_thread_ids) includes it. An embedder calls this from its own
    /// plane threads (e.g. the Android client's decode + audio threads) to fold them into the same
    /// performance-hint session as the internal data-plane pump. Idempotent per thread; a no-op on
    /// platforms without `gettid`.
    pub fn register_hot_thread(&self) {
        register_hot_tid(&self.hot_tids);
    }

    /// Kernel ids of the client's latency-critical threads: the internal data-plane pump (UDP
    /// receive + FEC reassembly) plus any registered via
    /// [`register_hot_thread`](Self::register_hot_thread). The Android client feeds these to an ADPF
    /// hint session so the CPU governor keeps the whole video pipeline on fast cores. Empty where
    /// thread ids aren't available (platforms without `gettid`); call after the first frame so the
    /// pump has registered.
    pub fn hot_thread_ids(&self) -> Vec<i32> {
        self.hot_tids.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// The LIVE host↔client clock offset (ns): the connect-time skew estimate, kept fresh by
    /// mid-stream re-syncs (every 60 s, plus immediately when a wall-clock step is suspected).
    /// Prefer this over the connect-time [`clock_offset_ns`](Self::clock_offset_ns) field for any
    /// ongoing latency math — after an NTP step or slow drift the connect-time value silently
    /// corrupts every capture-clock comparison. `0` = no skew handshake (old host / synced clocks).
    pub fn clock_offset_now_ns(&self) -> i64 {
        self.clock_offset.load(Ordering::Relaxed)
    }

    /// Shared handle to the live clock offset for plane threads that outlive a `&self` borrow
    /// (render/display trackers). Read with [`AtomicI64::load`]`(Ordering::Relaxed)` at each use —
    /// never cache the value across frames. Holding this does NOT keep the session alive (unlike
    /// an `Arc<NativeClient>`, whose drop disconnects).
    pub fn clock_offset_shared(&self) -> Arc<AtomicI64> {
        self.clock_offset.clone()
    }

    /// Start a bandwidth speed test: ask the host to burst filler over the data plane at
    /// `target_kbps` of goodput for `duration_ms`, *briefly pausing video*. Non-blocking — the
    /// measurement accumulates in the background; poll [`NativeClient::probe_result`] until its
    /// `done` flag is set. Starting a probe resets any prior measurement. The host clamps both
    /// fields (≤ 3 Gbps, ≤ 5 s).
    pub fn request_probe(&self, target_kbps: u32, duration_ms: u32) -> Result<()> {
        // Reset the accumulator so a fresh run doesn't blend into the previous one.
        *self.probe.lock().unwrap() = ProbeState {
            active: true,
            ..Default::default()
        };
        self.ctrl_tx
            .try_send(CtrlRequest::Probe(ProbeRequest {
                target_kbps,
                duration_ms,
            }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Read the current speed-test measurement (partial until `done`, final once the host's
    /// end-of-burst report lands). Derives goodput + loss from the accumulated probe bytes.
    pub fn probe_result(&self) -> ProbeOutcome {
        let p = self.probe.lock().unwrap();
        // Delivered figures: live (rx_now − base) while the burst runs, frozen at the host's report.
        let (delivered_packets, delivered_bytes) = if p.done {
            (p.delivered_packets, p.delivered_bytes)
        } else {
            let base_p = p.base_packets.unwrap_or(p.rx_packets_now);
            let base_b = p.base_bytes.unwrap_or(p.rx_bytes_now);
            (
                p.rx_packets_now.saturating_sub(base_p),
                p.rx_bytes_now.saturating_sub(base_b),
            )
        };
        // The host's burst duration is the throughput denominator. bytes × 8 / ms = kilobits/second.
        let window_ms = p.host_duration_ms;
        let throughput_kbps = if window_ms > 0 {
            (delivered_bytes.saturating_mul(8) / window_ms as u64) as u32
        } else {
            0
        };
        // Link loss: wire packets the host put out that didn't arrive. Packet-level, so it degrades
        // smoothly past the FEC budget instead of cliffing to 100% the moment AUs stop completing.
        let loss_pct = if p.host_wire_packets > 0 {
            (p.host_wire_packets as i64 - delivered_packets as i64).max(0) as f64
                / p.host_wire_packets as f64
                * 100.0
        } else {
            0.0
        } as f32;
        // Host-side drop: what the send buffer couldn't even accept (the host-side ceiling).
        let offered_wire = p.host_wire_packets + p.host_send_dropped;
        let host_drop_pct = if offered_wire > 0 {
            p.host_send_dropped as f64 / offered_wire as f64 * 100.0
        } else {
            0.0
        } as f32;
        ProbeOutcome {
            done: p.done,
            recv_bytes: delivered_bytes,
            recv_packets: delivered_packets as u32,
            host_bytes: p.host_goodput_bytes,
            host_packets: p.host_au,
            elapsed_ms: window_ms,
            throughput_kbps,
            loss_pct,
            host_drop_pct,
            wire_packets_sent: p.host_wire_packets,
            send_dropped: p.host_send_dropped,
        }
    }

    /// Pull the next reassembled, FEC-recovered access unit; [`PunktfunkError::NoFrame`] on
    /// timeout, [`PunktfunkError::Closed`]-class errors once the session ended.
    ///
    /// Plane concurrency: each pull method drains its own queue, so video, audio and
    /// rumble may each be pulled from their own thread — but at most one thread per plane
    /// (`&self` here supports the cross-plane sharing; a plane's queue is still
    /// single-consumer by contract).
    pub fn next_frame(&self, timeout: Duration) -> Result<Frame> {
        match self.frames.pop(timeout) {
            FramePop::Frame(f) => Ok(f),
            FramePop::Timeout => Err(PunktfunkError::NoFrame),
            FramePop::Closed => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next Opus audio packet; [`PunktfunkError::NoFrame`] on timeout,
    /// [`PunktfunkError::Closed`] once the session ended. Drain on a dedicated audio thread —
    /// packets arrive every 5 ms.
    pub fn next_audio(&self, timeout: Duration) -> Result<AudioPacket> {
        match self.audio.lock().unwrap().recv_timeout(timeout) {
            Ok(p) => Ok(p),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next rumble update `(pad, low, high)`; same semantics as
    /// [`NativeClient::next_audio`]. Amplitudes are 0..0xFFFF, `(0, 0)` = stop. The self-terminating
    /// TTL of a v2 envelope is dropped here — use [`NativeClient::next_rumble_ttl`] to honor it (a
    /// renderer that only sees `(pad, low, high)` keeps its own staleness policy exactly as before,
    /// which is what makes this back-compatible for un-updated embedders).
    pub fn next_rumble(&self, timeout: Duration) -> Result<(u16, u16, u16)> {
        self.next_rumble_ttl(timeout).map(|(p, l, h, _)| (p, l, h))
    }

    /// Pull the next rumble update including its self-termination TTL: `(pad, low, high, ttl_ms)`.
    /// `ttl_ms` is `Some(ms)` for a v2 envelope — render the level for at most that long, then
    /// silence — and `None` for a legacy v1 datagram (an old host with no lease; fall back to the
    /// renderer's own staleness heuristic). The reorder gate (seq) is applied in the datagram demux
    /// before the update reaches this queue, so a stale/reordered envelope never surfaces here.
    pub fn next_rumble_ttl(&self, timeout: Duration) -> Result<RumbleUpdate> {
        match self.rumble.lock().unwrap().recv_timeout(timeout) {
            Ok(r) => Ok(r),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next DualSense HID-output feedback event (lightbar / player LEDs / adaptive
    /// trigger) the host's virtual pad received from a game; same timeout/closed semantics as
    /// [`NativeClient::next_rumble`]. Replay it on a real DualSense (e.g. via the platform's
    /// `GCDualSenseAdaptiveTrigger` API). Only the DualSense host backend emits these.
    pub fn next_hidout(&self, timeout: Duration) -> Result<HidOutput> {
        match self.hidout.lock().unwrap().recv_timeout(timeout) {
            Ok(h) => Ok(h),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next static HDR metadata update (ST.2086 mastering display + content light level)
    /// the host sent for an HDR session; same timeout/closed semantics as
    /// [`NativeClient::next_hidout`]. The host sends one near session start and re-sends it on
    /// mastering changes / keyframes, so an HDR presenter should drain this on its own thread and
    /// apply the latest value to the display (DXGI `SetHDRMetaData` / `CAEDRMetadata` /
    /// `KEY_HDR_STATIC_INFO`). Only an HDR session (`color.is_hdr()`, PQ) ever emits these.
    pub fn next_hdr_meta(&self, timeout: Duration) -> Result<HdrMeta> {
        match self.hdr_meta.lock().unwrap().recv_timeout(timeout) {
            Ok(m) => Ok(m),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next per-AU host timing (0xCF): the host's capture→sent duration for one access
    /// unit, correlated to the AU by `pts_ns`. Feeds the unified stats HUD's `host` / `network`
    /// split (`network = (received + clock_offset − pts) − host_us`); a stats consumer should
    /// drain this non-blockingly alongside its frame samples. An older host never sends any —
    /// the HUD then keeps the combined `host+network` stage. Same timeout/closed semantics as
    /// [`NativeClient::next_hidout`].
    pub fn next_host_timing(&self, timeout: Duration) -> Result<crate::quic::HostTiming> {
        match self.host_timing.lock().unwrap().recv_timeout(timeout) {
            Ok(t) => Ok(t),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Queue one input event for delivery as a QUIC datagram.
    pub fn send_input(&self, ev: &InputEvent) -> Result<()> {
        self.input_tx.send(*ev).map_err(|_| PunktfunkError::Closed)
    }

    /// Queue one Opus mic frame for delivery as a 0xCB uplink datagram (the inverse of
    /// [`next_audio`](Self::next_audio)). `seq`/`pts_ns` are the caller's own counters (the host
    /// uses them only for diagnostics). The host decodes it into a virtual microphone source.
    /// Best-effort — like every datagram, it's dropped under loss; no retransmit.
    pub fn send_mic(&self, seq: u32, pts_ns: u64, opus: Vec<u8>) -> Result<()> {
        use tokio::sync::mpsc::error::TrySendError;
        match self.mic_tx.try_send((seq, pts_ns, opus)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                // Bounded queue full = the worker stalled for ~MIC_QUEUE x 5 ms. Shed this
                // frame (mic is best-effort end to end) instead of queueing latency/memory.
                tracing::debug!("mic uplink queue full — dropping frame");
                Ok(())
            }
            Err(TrySendError::Closed(_)) => Err(PunktfunkError::Closed),
        }
    }

    /// Queue one rich input event (DualSense touchpad contact or motion sample) for delivery as a
    /// 0xCC datagram. The host applies it to its virtual DualSense pad. Best-effort, dropped under
    /// loss like every datagram. No-op unless the host runs the DualSense gamepad backend.
    pub fn send_rich_input(&self, rich: RichInput) -> Result<()> {
        self.rich_input_tx
            .send(rich)
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Signal a **deliberate quit** (a user "stop", not a network drop): the worker closes the QUIC
    /// connection with [`crate::quic::QUIT_CLOSE_CODE`] instead of code 0, so the host tears the
    /// session's virtual display down immediately and skips the keep-alive linger. Then requests
    /// shutdown. A plain `drop` (without this) closes with code 0 → the host lingers for a reconnect.
    pub fn disconnect_quit(&self) {
        self.quit.store(true, Ordering::SeqCst);
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl Drop for NativeClient {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// Test/A-B hatch shared by the client shells: `PUNKTFUNK_CLIENT_PEAK_NITS=<nits>` synthesizes a
/// display colour volume at that peak (BT.2020 primaries, D65, a 0.005-nit floor, frame-average
/// unknown) for [`Hello::display_hdr`](crate::quic::Hello::display_hdr), overriding whatever the
/// shell read from the OS — so the host-side tone-map target (the virtual display's EDID volume)
/// can be pinned exactly for validation, and shells with no OS display-volume query get a manual
/// knob. `None` when unset/unparsable/zero.
pub fn display_hdr_env_override() -> Option<HdrMeta> {
    let nits: u32 = std::env::var("PUNKTFUNK_CLIENT_PEAK_NITS")
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&n| n > 0)?;
    tracing::info!(
        nits,
        "PUNKTFUNK_CLIENT_PEAK_NITS: overriding the advertised display volume"
    );
    Some(HdrMeta {
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]], // BT.2020 G, B, R
        white_point: [15635, 16450],                                      // D65
        max_display_mastering_luminance: nits.saturating_mul(10_000),
        min_display_mastering_luminance: 50, // 0.005 nits
        max_cll: 0,
        max_fall: 0,
    })
}

struct WorkerArgs {
    host: String,
    port: u16,
    mode: Mode,
    compositor: CompositorPref,
    gamepad: GamepadPref,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    display_hdr: Option<HdrMeta>,
    launch: Option<String>,
    pin: Option<[u8; 32]>,
    identity: Option<(String, String)>,
    frames: Arc<FrameChannel>,
    audio_tx: SyncSender<AudioPacket>,
    rumble_tx: SyncSender<RumbleUpdate>,
    hidout_tx: SyncSender<HidOutput>,
    hdr_meta_tx: SyncSender<HdrMeta>,
    host_timing_tx: SyncSender<crate::quic::HostTiming>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    mic_rx: tokio::sync::mpsc::Receiver<(u32, u64, Vec<u8>)>,
    rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<RichInput>,
    ctrl_rx: tokio::sync::mpsc::Receiver<CtrlRequest>,
    ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    shutdown: Arc<AtomicBool>,
    /// Deliberate-quit flag (see [`NativeClient::quit`]): the worker closes with the quit code if set.
    quit: Arc<AtomicBool>,
    mode_slot: Arc<std::sync::Mutex<Mode>>,
    probe: Arc<Mutex<ProbeState>>,
    frames_dropped: Arc<AtomicU64>,
    fec_recovered: Arc<AtomicU64>,
    hot_tids: Arc<Mutex<Vec<i32>>>,
    /// The live clock offset (see [`NativeClient::clock_offset`]): the worker seeds it with the
    /// connect-time estimate; the control task's mid-stream re-syncs update it.
    clock_offset: Arc<AtomicI64>,
}

/// The worker: QUIC handshake, then the input/datagram/control tasks + the blocking
/// data-plane pump.
async fn worker_main(args: WorkerArgs) {
    let WorkerArgs {
        host,
        port,
        mode,
        compositor,
        gamepad,
        bitrate_kbps,
        video_caps,
        audio_channels,
        video_codecs,
        preferred_codec,
        display_hdr,
        launch,
        pin,
        identity,
        frames,
        audio_tx,
        rumble_tx,
        hidout_tx,
        hdr_meta_tx,
        host_timing_tx,
        mut input_rx,
        mut mic_rx,
        mut rich_input_rx,
        mut ctrl_rx,
        ctrl_tx,
        ready_tx,
        shutdown,
        quit,
        mode_slot,
        probe,
        frames_dropped,
        fec_recovered,
        hot_tids,
        clock_offset,
    } = args;
    let setup = async {
        let remote: std::net::SocketAddr = join_host_port(&host, port)
            .parse()
            .map_err(|_| PunktfunkError::InvalidArg("host:port"))?;
        let (ep, observed) = endpoint::client_pinned_with_identity(
            pin,
            identity.as_ref().map(|(c, k)| (c.as_str(), k.as_str())),
        );
        let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
        let conn = ep
            .connect(remote, "punktfunk")
            .map_err(|_| PunktfunkError::InvalidArg("connect"))?
            .await
            .map_err(|e| {
                // A pin mismatch surfaces as a TLS failure; report it as a crypto error so
                // the embedder can distinguish "wrong host identity" from plain IO trouble.
                let fp_mismatch = pin.is_some()
                    && observed.lock().unwrap().map(|fp| Some(fp) != pin) == Some(true);
                if fp_mismatch {
                    PunktfunkError::Crypto
                } else {
                    PunktfunkError::Io(std::io::Error::other(e.to_string()))
                }
            })?;
        let fingerprint = observed.lock().unwrap().unwrap_or([0u8; 32]);
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

        io::write_msg(
            &mut send,
            &Hello {
                abi_version: crate::WIRE_VERSION,
                mode,
                compositor,
                gamepad,
                bitrate_kbps,
                // No device name yet: the connect ABI has no name parameter (pairing does). The
                // host falls back to a fingerprint-derived label in its pending-approval list.
                name: None,
                // Library id to launch this session, if the embedder asked for one.
                launch: launch.clone(),
                // The embedder's decode/present caps (e.g. the Windows client advertises
                // VIDEO_CAP_10BIT | VIDEO_CAP_HDR). The host only upgrades to a 10-bit / HDR encode
                // when the matching bit is set, so `0` stays an 8-bit BT.709 stream. HOST_TIMING is
                // OR'd in unconditionally: every NativeClient build demuxes the 0xCF plane, and the
                // bit only asks the host for observability datagrams (never changes the encode).
                // PROBE_SEQ likewise: the shared reassembler keeps probe filler in its own window
                // (every embedder inherits it), so the host may burst speed tests without consuming
                // video frame indexes.
                video_caps: video_caps
                    | crate::quic::VIDEO_CAP_HOST_TIMING
                    | crate::quic::VIDEO_CAP_PROBE_SEQ,
                // Requested surround channel count; the host echoes the resolved value in Welcome.
                audio_channels,
                // The codecs this client can decode + its soft preference (0 = auto). The host
                // resolves the emitted codec from these and reports it in `Welcome::codec`.
                video_codecs,
                preferred_codec,
                // The client display's HDR volume → the host's virtual-display EDID (host apps
                // tone-map to the client's real panel). `None` = unknown/SDR.
                display_hdr,
            }
            .encode(),
        )
        .await?;
        let welcome = Welcome::decode(&io::read_msg(&mut recv).await?)?;
        if welcome.compositor != CompositorPref::Auto {
            tracing::info!(
                compositor = welcome.compositor.as_str(),
                "host resolved compositor"
            );
        }
        if welcome.gamepad != GamepadPref::Auto {
            tracing::info!(
                gamepad = welcome.gamepad.as_str(),
                "host resolved gamepad backend"
            );
        }

        // Reserve our data-plane port, then start the host.
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let udp_port = probe.local_addr()?.port();
        drop(probe);
        io::write_msg(
            &mut send,
            &Start {
                client_udp_port: udp_port,
            }
            .encode(),
        )
        .await?;

        // Wall-clock skew handshake on the control stream (before the session's control task takes
        // it): align our clock to the host's so the embedder can express receive/present instants in
        // the host's capture clock (the AU `pts_ns`). 0 ⇒ an old host that didn't answer (shared-clock
        // assumption, as before). This is the substrate for glass-to-glass present-time measurement.
        let (clock_offset_ns, clock_rtt_ns) =
            match crate::quic::clock_sync(&mut send, &mut recv).await {
                Some(skew) => {
                    tracing::info!(
                        offset_ns = skew.offset_ns,
                        rtt_us = skew.rtt_ns / 1000,
                        rounds = skew.rounds,
                        "clock skew estimated (host-client)"
                    );
                    (skew.offset_ns, Some(skew.rtt_ns))
                }
                None => (0, None),
            };

        let host_udp = std::net::SocketAddr::new(remote.ip(), welcome.udp_port);
        let transport =
            UdpTransport::connect(&format!("0.0.0.0:{udp_port}"), &host_udp.to_string())?;
        // Hole-punch the host's data port so video traverses a NAT / stateful inter-VLAN firewall
        // (control + side planes ride the client-initiated QUIC; the raw video UDP needs the client
        // to open the path first). Stops with the session via the shared shutdown flag.
        if let Ok(sock) = transport.try_clone_socket() {
            crate::transport::spawn_data_punch(sock, shutdown.clone());
        }
        let session = Session::new(welcome.session_config(Role::Client), Box::new(transport))?;
        Ok::<_, PunktfunkError>((
            conn,
            session,
            send,
            recv,
            Negotiated {
                mode: welcome.mode,
                compositor: welcome.compositor,
                gamepad: welcome.gamepad,
                host_fingerprint: fingerprint,
                bitrate_kbps: welcome.bitrate_kbps,
                clock_offset_ns,
                clock_rtt_ns,
                bit_depth: welcome.bit_depth,
                color: welcome.color,
                chroma_format: welcome.chroma_format,
                audio_channels: welcome.audio_channels,
                codec: welcome.codec,
            },
            welcome.host_caps,
        ))
    };

    let (conn, mut session, mut ctrl_send, mut ctrl_recv, negotiated, host_caps) = match setup.await
    {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    // Copies the pump needs after `negotiated` is handed over to `connect`.
    let clock_rtt_ns = negotiated.clock_rtt_ns;
    let resolved_bitrate_kbps = negotiated.bitrate_kbps;
    // Seed the live offset with the connect-time estimate BEFORE the embedder can observe the
    // client (ready_tx): clock_offset_now_ns() never reads a pre-handshake 0 on a skewed pair.
    clock_offset.store(negotiated.clock_offset_ns, Ordering::Relaxed);
    // Bumped by the control task each time a re-sync batch is APPLIED; the pump watches it to
    // reset its staleness counters and re-arm the clock-based jump-to-live detector.
    let clock_gen = Arc::new(AtomicU32::new(0));
    let _ = ready_tx.send(Ok(negotiated));

    // Input task: embedder events → QUIC datagrams. Toward a host that advertised
    // HOST_CAP_GAMEPAD_STATE, the per-transition gamepad events every embedder still emits are
    // folded into idempotent, sequence-numbered full-state snapshots (`GamepadSnapshot`): the
    // datagram plane drops and reorders (and sheds oldest-first at the 4 KiB send cap), so a lost
    // per-transition event would corrupt held pad state until the *next* change — a held trigger
    // stuck wrong indefinitely. Snapshots heal on the next send, the seq lets the host drop stale
    // reorders, and a periodic refresh of every touched pad bounds any loss to one refresh
    // interval — the same idempotent-state discipline as the host's 500 ms rumble refresh.
    // Keyboard/mouse/touch events pass through unchanged; an older host (no caps bit) keeps
    // getting the legacy per-transition gamepad events.
    let input_conn = conn.clone();
    let gamepad_snapshots = host_caps & crate::quic::HOST_CAP_GAMEPAD_STATE != 0;
    tokio::spawn(async move {
        use crate::input::{GamepadSnapshot, InputKind, MAX_PADS};
        // Touched pads only: an entry appears on the first gamepad event for that index, so the
        // refresh never conjures a virtual pad the embedder didn't drive.
        let mut pads: [Option<GamepadSnapshot>; MAX_PADS] = [None; MAX_PADS];
        // Per-pad wrapping seq that PERSISTS across a pad's remove/re-add on the same index (the
        // snapshot itself is cleared to `None` on removal). A removal takes `seq[idx] + 1` so it
        // supersedes every prior snapshot; the re-added pad's first snapshot takes the next value
        // after that, so the host's seq gate accepts it instead of rejecting a restarted-at-0 seq.
        let mut seq: [u8; MAX_PADS] = [0; MAX_PADS];
        // Re-sends of a removal still owed on refresh ticks (the removal rides the lossy datagram
        // plane; a single lost one would silently strand a ghost pad on the host — the exact bug
        // the removal fixes). Mirrors the host's rumble stop burst: a few time-spread re-sends,
        // each with a fresh (higher) seq, and canceled the moment the pad is driven again.
        const REMOVE_RESENDS: u8 = 2;
        let mut remove_owed: [u8; MAX_PADS] = [0; MAX_PADS];
        // Per-pad declared controller kind ([`GamepadArrival`]) + its owed re-sends: the host needs
        // the kind before the pad's first frame to build a matching virtual device (mixed types), so
        // like the removal it rides the lossy plane with a small time-spread re-send burst.
        const ARRIVAL_RESENDS: u8 = 2;
        let mut arrival: [Option<u8>; MAX_PADS] = [None; MAX_PADS];
        let mut arrival_owed: [u8; MAX_PADS] = [0; MAX_PADS];
        let mut refresh = tokio::time::interval(Duration::from_millis(100));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                ev = input_rx.recv() => {
                    let Some(ev) = ev else { break };
                    let idx = ev.flags as usize;
                    if gamepad_snapshots
                        && matches!(ev.kind, InputKind::GamepadButton | InputKind::GamepadAxis)
                        && idx < MAX_PADS
                    {
                        // The pad is being driven — cancel any owed removal (a re-plug on this
                        // index; its fresh snapshot seq already supersedes the removal's).
                        remove_owed[idx] = 0;
                        let snap = pads[idx].get_or_insert(GamepadSnapshot {
                            pad: idx as u8,
                            ..Default::default()
                        });
                        // Unknown axis ids don't send (the host's legacy fold drops them too).
                        if snap.fold(&ev) {
                            seq[idx] = seq[idx].wrapping_add(1);
                            snap.seq = seq[idx];
                            let _ = input_conn
                                .send_datagram(snap.to_event().encode().to_vec().into());
                        }
                        continue;
                    }
                    if gamepad_snapshots && ev.kind == InputKind::GamepadRemove && idx < MAX_PADS {
                        // Stop refreshing the pad and forward a seq-stamped removal (in the shared
                        // seq space) so the host tears its virtual device down and no reordered
                        // snapshot can resurrect it; arm the re-send burst against datagram loss.
                        // Drop any owed kind declaration too — a re-plug on this index sends its own.
                        pads[idx] = None;
                        arrival[idx] = None;
                        arrival_owed[idx] = 0;
                        seq[idx] = seq[idx].wrapping_add(1);
                        remove_owed[idx] = REMOVE_RESENDS;
                        let rem = crate::input::InputEvent {
                            flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                            ..ev
                        };
                        let _ = input_conn.send_datagram(rem.encode().to_vec().into());
                        continue;
                    }
                    if gamepad_snapshots && ev.kind == InputKind::GamepadArrival && idx < MAX_PADS {
                        // Remember the declared kind (`code`) and forward it, arming a re-send burst
                        // so the host learns it before the pad's first frame even under loss.
                        arrival[idx] = Some(ev.code as u8);
                        arrival_owed[idx] = ARRIVAL_RESENDS;
                        let _ = input_conn.send_datagram(ev.encode().to_vec().into());
                        continue;
                    }
                    let _ = input_conn.send_datagram(ev.encode().to_vec().into());
                }
                _ = refresh.tick() => {
                    for idx in 0..MAX_PADS {
                        // Re-send an owed kind declaration (independent of whether the pad has state
                        // yet — it may be idle-but-connected). Idempotent on the host.
                        if arrival_owed[idx] > 0 {
                            if let Some(kind) = arrival[idx] {
                                arrival_owed[idx] -= 1;
                                let arr = crate::input::InputEvent {
                                    kind: InputKind::GamepadArrival,
                                    _pad: [0; 3],
                                    code: kind as u32,
                                    x: 0,
                                    y: 0,
                                    flags: idx as u32,
                                };
                                let _ = input_conn.send_datagram(arr.encode().to_vec().into());
                            } else {
                                arrival_owed[idx] = 0;
                            }
                        }
                        if let Some(snap) = pads[idx].as_mut() {
                            seq[idx] = seq[idx].wrapping_add(1);
                            snap.seq = seq[idx];
                            let _ = input_conn.send_datagram(snap.to_event().encode().to_vec().into());
                        } else if remove_owed[idx] > 0 {
                            // Idempotent removal re-send with a fresh seq (the host drops it as a
                            // no-op once the pad is already gone, but a re-plug's later snapshot
                            // still wins by seq).
                            remove_owed[idx] -= 1;
                            seq[idx] = seq[idx].wrapping_add(1);
                            let rem = crate::input::InputEvent {
                                kind: InputKind::GamepadRemove,
                                _pad: [0; 3],
                                code: 0,
                                x: 0,
                                y: 0,
                                flags: crate::input::encode_gamepad_remove(idx as u8, seq[idx]),
                            };
                            let _ = input_conn.send_datagram(rem.encode().to_vec().into());
                        }
                    }
                }
            }
        }
    });

    // Mic task: embedder Opus mic frames → 0xCB uplink datagrams (best-effort, dropped on loss).
    let mic_conn = conn.clone();
    tokio::spawn(async move {
        while let Some((seq, pts_ns, opus)) = mic_rx.recv().await {
            let d = crate::quic::encode_mic_datagram(seq, pts_ns, &opus);
            let _ = mic_conn.send_datagram(d.into());
        }
    });

    // Rich-input task: embedder DualSense touchpad / motion → 0xCC uplink datagrams.
    let rich_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(rich) = rich_input_rx.recv().await {
            let _ = rich_conn.send_datagram(rich.encode().into());
        }
    });

    // Adaptive bitrate ack slot: the control task parks the latest BitrateChanged here; the
    // pump's controller drains it on its report tick (`take()` — an ack is consumed once).
    let bitrate_ack: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    // Control task: the handshake stream stays open for mid-stream renegotiation + speed tests.
    // Outbound requests (mode switch, probe) and inbound replies (Reconfigured, ProbeResult) are
    // multiplexed with `select!`; a single outbound channel (`ctrl_rx`) keeps one writer so the
    // two `&mut ctrl_send` borrows don't collide across branches.
    {
        let mode_slot = mode_slot.clone();
        let probe = probe.clone();
        let bitrate_ack = bitrate_ack.clone();
        let clock_offset = clock_offset.clone();
        let clock_gen = clock_gen.clone();
        tokio::spawn(async move {
            // Mid-stream clock re-sync (see [`ClockResync`]): a batch runs every
            // CLOCK_RESYNC_INTERVAL and whenever the pump asks (CtrlRequest::ClockResync after
            // its first no-op clock flush). Echoes interleave with the other control replies in
            // the read arm below; only when the host answered the connect-time handshake — an
            // old host would just eat the probes.
            let mut resync = ClockResync::new();
            let mut resync_tick = tokio::time::interval_at(
                tokio::time::Instant::now() + CLOCK_RESYNC_INTERVAL,
                CLOCK_RESYNC_INTERVAL,
            );
            resync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    req = ctrl_rx.recv() => {
                        let Some(req) = req else { break }; // client dropped
                        let bytes = match req {
                            CtrlRequest::Mode(m) => Reconfigure { mode: m }.encode(),
                            CtrlRequest::Probe(p) => p.encode(),
                            CtrlRequest::Keyframe => RequestKeyframe.encode(),
                            CtrlRequest::Rfi(r) => r.encode(),
                            CtrlRequest::Loss(r) => r.encode(),
                            CtrlRequest::SetBitrate(k) => SetBitrate { bitrate_kbps: k }.encode(),
                            CtrlRequest::ClockResync => {
                                if clock_rtt_ns.is_none() {
                                    continue; // no connect-time handshake — host can't answer
                                }
                                resync.begin(wall_clock_ns()).encode()
                            }
                        };
                        if io::write_msg(&mut ctrl_send, &bytes).await.is_err() {
                            break;
                        }
                    }
                    _ = resync_tick.tick(), if clock_rtt_ns.is_some() => {
                        let probe = resync.begin(wall_clock_ns());
                        if io::write_msg(&mut ctrl_send, &probe.encode()).await.is_err() {
                            break;
                        }
                    }
                    msg = io::read_msg(&mut ctrl_recv) => {
                        let Ok(msg) = msg else { break }; // stream closed
                        if let Ok(ack) = Reconfigured::decode(&msg) {
                            if ack.accepted {
                                *mode_slot.lock().unwrap() = ack.mode;
                                tracing::info!(mode = ?ack.mode, "host accepted mode switch");
                            } else {
                                tracing::warn!(active = ?ack.mode, "host rejected mode switch");
                            }
                        } else if let Ok(result) = ProbeResult::decode(&msg) {
                            let mut p = probe.lock().unwrap();
                            // Freeze the delivered figures now (the burst is done), before resumed
                            // video can inflate the packet counters.
                            let base_p = p.base_packets.unwrap_or(p.rx_packets_now);
                            let base_b = p.base_bytes.unwrap_or(p.rx_bytes_now);
                            p.delivered_packets = p.rx_packets_now.saturating_sub(base_p);
                            p.delivered_bytes = p.rx_bytes_now.saturating_sub(base_b);
                            p.host_goodput_bytes = result.bytes_sent;
                            p.host_au = result.packets_sent;
                            p.host_wire_packets = result.wire_packets_sent;
                            p.host_send_dropped = result.send_dropped;
                            p.host_duration_ms = result.duration_ms;
                            p.done = true;
                            p.active = false; // burst over — the pump stops mirroring counters
                            tracing::info!(
                                host_goodput_bytes = result.bytes_sent,
                                wire_packets_sent = result.wire_packets_sent,
                                send_dropped = result.send_dropped,
                                duration_ms = result.duration_ms,
                                delivered_packets = p.delivered_packets,
                                "speed-test probe result"
                            );
                        } else if let Ok(ack) = BitrateChanged::decode(&msg) {
                            // Adaptive bitrate: the host's clamp is authoritative — park it for
                            // the pump's controller (which also reads any ack as "this host
                            // renegotiates", arming further steps).
                            tracing::info!(
                                kbps = ack.bitrate_kbps,
                                "host re-targeted encoder bitrate"
                            );
                            *bitrate_ack.lock().unwrap() = Some(ack.bitrate_kbps);
                        } else if let Ok(echo) = ClockEcho::decode(&msg) {
                            match resync.on_echo(&echo, wall_clock_ns()) {
                                ResyncStep::Probe(p) => {
                                    if io::write_msg(&mut ctrl_send, &p.encode()).await.is_err() {
                                        break;
                                    }
                                }
                                ResyncStep::Done { offset_ns, rtt_ns } => {
                                    // Never let a congested window bias the offset (frames read
                                    // late exactly then) — keep the old estimate and let the next
                                    // periodic batch try again.
                                    if accept_resync(rtt_ns, clock_rtt_ns.unwrap_or(0)) {
                                        clock_offset.store(offset_ns, Ordering::Relaxed);
                                        clock_gen.fetch_add(1, Ordering::Relaxed);
                                        tracing::debug!(
                                            offset_ns,
                                            rtt_us = rtt_ns / 1000,
                                            "mid-stream clock re-sync applied"
                                        );
                                    } else {
                                        tracing::debug!(
                                            rtt_us = rtt_ns / 1000,
                                            "clock re-sync batch discarded — RTT above the \
                                             connect-time baseline (congested window)"
                                        );
                                    }
                                }
                                ResyncStep::Idle => {}
                            }
                        } else {
                            tracing::warn!("unknown control message — ignoring");
                        }
                    }
                }
            }
        });
    }

    // Datagram demux: host → client audio/rumble (try_send: a lagging embedder drops the
    // newest packet rather than backing up the QUIC receive path).
    let dgram_conn = conn.clone();
    // Per-pad reorder gate for v2 rumble envelopes (the seq analog of the host's gamepad-state
    // gate): a datagram the network reordered must not roll a stopped motor back on. Legacy v1
    // datagrams carry no seq and bypass it (an old host's own periodic re-send is the only heal).
    let mut rumble_last_seq: [Option<u8>; crate::input::MAX_PADS] = [None; crate::input::MAX_PADS];
    tokio::spawn(async move {
        while let Ok(d) = dgram_conn.read_datagram().await {
            match d.first() {
                Some(&crate::quic::AUDIO_MAGIC) => {
                    if let Some((seq, pts_ns, opus)) = crate::quic::decode_audio_datagram(&d) {
                        let _ = audio_tx.try_send(AudioPacket {
                            seq,
                            pts_ns,
                            data: opus.to_vec(),
                        });
                    }
                }
                Some(&crate::quic::RUMBLE_MAGIC) => {
                    if let Some(u) = crate::quic::decode_rumble_envelope(&d) {
                        // Gate v2 envelopes on their per-pad seq; forward v1 (envelope: None) as-is.
                        let fresh = match u.envelope {
                            Some(env) => {
                                let idx = u.pad as usize;
                                if idx < crate::input::MAX_PADS {
                                    if crate::input::GamepadSnapshot::seq_newer(
                                        env.seq,
                                        rumble_last_seq[idx],
                                    ) {
                                        rumble_last_seq[idx] = Some(env.seq);
                                        true
                                    } else {
                                        false // reordered/duplicate — drop, keep the newer state
                                    }
                                } else {
                                    true // out-of-range pad (host never sends these): no gate
                                }
                            }
                            None => true,
                        };
                        if fresh {
                            let ttl = u.envelope.map(|e| e.ttl_ms);
                            let _ = rumble_tx.try_send((u.pad, u.low, u.high, ttl));
                        }
                    }
                }
                Some(&crate::quic::HIDOUT_MAGIC) => {
                    if let Some(h) = HidOutput::decode(&d) {
                        let _ = hidout_tx.try_send(h);
                    }
                }
                Some(&crate::quic::HDR_META_MAGIC) => {
                    if let Some(m) = crate::quic::decode_hdr_meta_datagram(&d) {
                        let _ = hdr_meta_tx.try_send(m);
                    }
                }
                Some(&crate::quic::HOST_TIMING_MAGIC) => {
                    if let Some(t) = crate::quic::decode_host_timing_datagram(&d) {
                        let _ = host_timing_tx.try_send(t);
                    }
                }
                _ => {} // unknown tag — a newer host; ignore
            }
        }
    });

    // Watch for connection close → stop the pump.
    {
        let shutdown = shutdown.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.closed().await;
            shutdown.store(true, Ordering::SeqCst);
        });
    }

    // Data-plane pump on a blocking thread: poll the session, hand frames to the embedder.
    // try_send drops the newest frame when the embedder lags (freshness over completeness).
    // Speed-test filler ([`FLAG_PROBE`]) is folded into the probe accumulator instead of the
    // decoder queue — it isn't video.
    let pump_shutdown = shutdown.clone();
    let pump_probe = probe.clone();
    let pump_hot_tids = hot_tids.clone();
    let pump_clock_offset = clock_offset.clone();
    let pump_clock_gen = clock_gen.clone();
    let _ = tokio::task::spawn_blocking(move || {
        pin_thread_user_interactive(); // feeds the frame channel → the user-interactive video pump
        register_hot_tid(&pump_hot_tids); // this thread does UDP receive + FEC reassembly — hint it
                                          // Adaptive-FEC loss reporting: every ADAPT_REPORT_INTERVAL, report the loss observed over the
                                          // window (shards FEC recovered, plus a bump if any frame went unrecoverable) so the host can
                                          // size FEC to the link. Suppressed during a speed test (its FLAG_PROBE filler would skew it).
        const ADAPT_REPORT_INTERVAL: Duration = Duration::from_millis(750);
        let mut last_report = Instant::now();
        let (mut last_recovered, mut last_received, mut last_dropped) = (0u64, 0u64, 0u64);
        // PUNKTFUNK_PERF: per-window pump observability — the Session's receive stage split
        // (recv / decrypt / reassemble+FEC, see `Session::take_pump_perf`) and completed-AU
        // inter-arrival jitter. Smoothness has no metric otherwise: jump-to-live counters only
        // fire after the stream is already seconds behind.
        let pump_perf_on = std::env::var("PUNKTFUNK_PERF").is_ok_and(|v| v != "0");
        let mut arrivals_us: Vec<u32> = Vec::new();
        let mut last_arrival: Option<Instant> = None;
        // Adaptive bitrate (see `crate::abr`): armed only when the embedder asked for Automatic
        // (`bitrate_kbps == 0`) and the host echoed the rate it actually configured (an old host
        // echoes 0 → controller stays permanently off). Fed once per report window with the same
        // deltas the LossReport uses, plus the window's mean skew-corrected one-way delay and
        // whether a jump-to-live flush fired.
        let mut abr = BitrateController::new(if bitrate_kbps == 0 {
            resolved_bitrate_kbps
        } else {
            0
        });
        let (mut owd_sum_ns, mut owd_frames) = (0i128, 0u32);
        let mut flush_in_window = false;
        // Jump-to-live state (see the guard in the loop below): the clock-based over-bound run
        // (`stale_frames`, armed only when the skew handshake succeeded so the clocks are comparable),
        // the clock-free non-draining-queue run (`standing_frames`), and the last-jump instant for the
        // shared cooldown.
        let mut stale_frames: u32 = 0;
        let mut standing_frames: u32 = 0;
        let mut last_flush: Option<Instant> = None;
        // Clock-detector health: consecutive clock-triggered flushes that found no local backlog
        // (see NOOP_FLUSH_DATAGRAMS). Reaching NOOP_CLOCK_FLUSHES_TO_DISARM turns the clock-based
        // detector off (a clock step / upstream queue it can't fix) — until a mid-stream clock
        // re-sync lands and re-arms it (`pump_clock_gen` below). The FIRST no-op flush also asks
        // the control task for an immediate re-sync (via the report tick): the flush finding no
        // local backlog IS the "the wall clock stepped under me" signal.
        let mut noop_clock_flushes: u32 = 0;
        let mut clock_detector_armed = true;
        let mut resync_wanted = false;
        let mut seen_clock_gen = pump_clock_gen.load(Ordering::Relaxed);
        while !pump_shutdown.load(Ordering::SeqCst) {
            // The live host↔client offset: re-loaded every iteration so an applied mid-stream
            // re-sync takes effect on the very next frame's latency math.
            let clock_offset_ns = pump_clock_offset.load(Ordering::Relaxed);
            // An applied re-sync invalidates the staleness run measured under the OLD offset:
            // reset the counters and re-arm the clock-based detector if a step had disarmed it.
            let gen = pump_clock_gen.load(Ordering::Relaxed);
            if gen != seen_clock_gen {
                seen_clock_gen = gen;
                stale_frames = 0;
                noop_clock_flushes = 0;
                if !clock_detector_armed {
                    clock_detector_armed = true;
                    tracing::info!(
                        "clock re-sync applied — clock-based jump-to-live re-armed"
                    );
                }
            }
            // Mirror the reassembler's unrecoverable-drop count for the client's keyframe-recovery
            // loop, and (during a speed test) the packet-level receive counters for the throughput
            // measurement. Updated every iteration (not just on a produced frame) so they stay current
            // through a total-loss drought where no AU completes. Cheap: a few relaxed atomic loads.
            let st = session.stats();
            frames_dropped.store(st.frames_dropped, Ordering::Relaxed);
            fec_recovered.store(st.fec_recovered_shards, Ordering::Relaxed);
            let probe_active = {
                let mut p = pump_probe.lock().unwrap();
                if p.active && !p.done {
                    p.rx_packets_now = st.packets_received;
                    p.rx_bytes_now = st.bytes_received;
                    p.base_packets.get_or_insert(st.packets_received);
                    p.base_bytes.get_or_insert(st.bytes_received);
                }
                p.active && !p.done
            };
            if !probe_active && last_report.elapsed() >= ADAPT_REPORT_INTERVAL {
                // A no-op clock flush earlier in this window suspected a wall-clock step: fire
                // the mid-stream re-sync now (once — the 60 s periodic covers everything else).
                if resync_wanted {
                    resync_wanted = false;
                    let _ = ctrl_tx.try_send(CtrlRequest::ClockResync);
                }
                let window_dropped = st.frames_dropped.wrapping_sub(last_dropped);
                let loss_ppm = window_loss_ppm(
                    st.fec_recovered_shards.wrapping_sub(last_recovered),
                    st.packets_received.wrapping_sub(last_received),
                    window_dropped,
                );
                let _ = ctrl_tx.try_send(CtrlRequest::Loss(LossReport { loss_ppm }));
                // Adaptive bitrate: drain any host ack first (its clamp is authoritative), then
                // feed the controller this window's congestion signals; a decision becomes a
                // SetBitrate on the control stream.
                if let Some(acked) = bitrate_ack.lock().unwrap().take() {
                    abr.on_ack(acked);
                }
                let owd_mean_us =
                    (owd_frames > 0).then(|| (owd_sum_ns / owd_frames as i128 / 1000) as i64);
                (owd_sum_ns, owd_frames) = (0, 0);
                if let Some(kbps) = abr.on_window(
                    Instant::now(),
                    window_dropped,
                    loss_ppm,
                    owd_mean_us,
                    flush_in_window,
                ) {
                    tracing::info!(kbps, "adaptive bitrate: requesting encoder re-target");
                    let _ = ctrl_tx.try_send(CtrlRequest::SetBitrate(kbps));
                }
                flush_in_window = false;
                last_report = Instant::now();
                last_recovered = st.fec_recovered_shards;
                last_received = st.packets_received;
                last_dropped = st.frames_dropped;
                if pump_perf_on {
                    if let Some(p) = session.take_pump_perf() {
                        let per_pkt_ns = |ns: u64| ns.checked_div(p.packets).unwrap_or(0);
                        tracing::info!(
                            recv_ms = p.recv_ns / 1_000_000,
                            decrypt_ms = p.decrypt_ns / 1_000_000,
                            reasm_ms = p.reasm_ns / 1_000_000,
                            packets = p.packets,
                            batches = p.batches,
                            pkts_per_batch = p.packets.checked_div(p.batches).unwrap_or(0),
                            decrypt_ns_pkt = per_pkt_ns(p.decrypt_ns),
                            reasm_ns_pkt = per_pkt_ns(p.reasm_ns),
                            "pump stage split (window)"
                        );
                    }
                    // Inter-arrival jitter over the window's completed AUs. `late` counts gaps
                    // over 2× the window median — the "a frame arrived visibly off-beat" tally.
                    if arrivals_us.len() >= 8 {
                        arrivals_us.sort_unstable();
                        let pct = |q: usize| arrivals_us[(arrivals_us.len() - 1) * q / 100];
                        let (p50, p95) = (pct(50), pct(95));
                        let late = arrivals_us.iter().filter(|&&d| d > p50 * 2).count();
                        tracing::info!(
                            frames = arrivals_us.len() + 1,
                            arrival_p50_us = p50,
                            arrival_p95_us = p95,
                            arrival_max_us = arrivals_us.last().copied().unwrap_or(0),
                            late,
                            "frame inter-arrival jitter (window)"
                        );
                    }
                    arrivals_us.clear();
                }
            }
            match session.poll_frame() {
                Ok(frame) => {
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        continue; // speed-test filler, not video — measured via the counters above
                    }
                    if pump_perf_on {
                        let now = Instant::now();
                        if let Some(prev) = last_arrival.replace(now) {
                            // 4096 ≈ 17 s at 240 fps — a stuck window can't grow it unbounded.
                            if arrivals_us.len() < 4096 {
                                arrivals_us.push((now - prev).as_micros().min(u32::MAX as u128)
                                    as u32);
                            }
                        }
                    }
                    // Jump-to-live guard. A standing receive/hand-off queue never drains by itself —
                    // the pump consumes strictly in order at the arrival rate, so once behind, the
                    // stream stays behind for good (observed live: stuck 6–7 s). Pre-decode AUs are
                    // reference-chained (infinite GOP), so we can NOT drop a frame mid-stream to catch
                    // up; the only safe recovery is to discard the whole backlog and re-anchor decode
                    // on a fresh keyframe. Two independent "we're behind" signals arm it, both gated by
                    // FLUSH_COOLDOWN, both suspended during a speed test (the probe MEASURES a saturated
                    // queue; flushing would corrupt its counters):
                    //  * clock-based — completed frames sit > FLUSH_LATENCY behind the skew-corrected
                    //    capture clock for FLUSH_AFTER_FRAMES straight. Needs the skew handshake, and
                    //    also catches kernel/reassembler backlog the hand-off queue hasn't reached yet.
                    //  * clock-free — the pre-decode hand-off queue stopped draining: its depth stayed
                    //    ≥ QUEUE_HIGH (never falling to QUEUE_LOW) for STANDING_FRAMES straight. Works
                    //    with no handshake / a same-clock session (where the clock path is disarmed),
                    //    and is the direct signal that the embedder can't keep up. A transient Wi-Fi
                    //    clump drains in a few frames and never reaches the count.
                    if probe_active {
                        // Keep both detectors disarmed across a speed test so its (deliberately)
                        // saturated queue doesn't leave a primed count that fires the moment it ends.
                        stale_frames = 0;
                        standing_frames = 0;
                    } else {
                        let lat_ns = if clock_offset_ns != 0 {
                            now_realtime_ns() + clock_offset_ns as i128 - frame.pts_ns as i128
                        } else {
                            0
                        };
                        // Feed the adaptive-bitrate controller's OWD window (mean capture→received
                        // delay): rising delay under zero loss is queue growth — the pre-loss
                        // congestion signal. Only meaningful with a clock handshake.
                        if clock_offset_ns != 0 && lat_ns > 0 {
                            owd_sum_ns += lat_ns;
                            owd_frames += 1;
                        }
                        if clock_detector_armed
                            && clock_offset_ns != 0
                            && lat_ns > FLUSH_LATENCY.as_nanos() as i128
                        {
                            stale_frames += 1;
                        } else {
                            stale_frames = 0;
                        }
                        let depth = frames.depth();
                        if depth >= QUEUE_HIGH {
                            standing_frames += 1;
                        } else if depth <= QUEUE_LOW {
                            standing_frames = 0;
                        }
                        let clock_behind = stale_frames >= FLUSH_AFTER_FRAMES;
                        let queue_behind = standing_frames >= STANDING_FRAMES;
                        if (clock_behind || queue_behind)
                            && last_flush.is_none_or(|t| t.elapsed() >= FLUSH_COOLDOWN)
                        {
                            stale_frames = 0;
                            standing_frames = 0;
                            last_flush = Some(Instant::now());
                            flush_in_window = true; // strongest "link can't hold the rate" signal
                            let flushed = session.flush_backlog().unwrap_or(0);
                            let dropped = frames.clear();
                            let _ = ctrl_tx.try_send(CtrlRequest::Keyframe);
                            tracing::warn!(
                                behind_ms = if clock_behind { lat_ns / 1_000_000 } else { -1 },
                                queue_depth = depth,
                                flushed_datagrams = flushed,
                                dropped_frames = dropped,
                                "receive backlog stopped draining — jumped to live (flush + keyframe)"
                            );
                            // Clock-detector health check: a clock-only trigger whose flush found
                            // no local backlog is a false "behind" reading (a wall-clock step, or
                            // an upstream queue a local flush can't drain) — repeated, it would
                            // cost a recovery IDR every cooldown forever. Disarm after two in a
                            // row; the clock-free queue detector keeps covering real backlogs.
                            if clock_behind && !queue_behind
                                && flushed < NOOP_FLUSH_DATAGRAMS
                                && dropped == 0
                            {
                                noop_clock_flushes += 1;
                                if noop_clock_flushes == 1 {
                                    // First no-op flush = a wall-clock step is the prime
                                    // suspect: ask for an immediate re-sync (sent on the next
                                    // report tick). Applied, it resets these counters and
                                    // re-arms the detector before the disarm below triggers.
                                    resync_wanted = true;
                                }
                                if noop_clock_flushes >= NOOP_CLOCK_FLUSHES_TO_DISARM {
                                    clock_detector_armed = false;
                                    tracing::warn!(
                                        "clock-based jump-to-live disarmed — its flushes found no \
                                         local backlog (clock step or upstream queueing suspected); \
                                         the queue-depth detector stays armed"
                                    );
                                }
                            } else {
                                noop_clock_flushes = 0;
                            }
                            continue; // this frame is part of the stale past — don't render it
                        }
                    }
                    frames.push(frame);
                }
                Err(PunktfunkError::NoFrame) => {
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(_) => break,
            }
        }
        // The pump exited (shutdown / fatal session error) — wake any consumer blocked in
        // `next_frame` with a Closed signal instead of a spurious timeout (the old mpsc did this
        // implicitly when the sender dropped).
        frames.close();
    })
    .await;

    // Deliberate quit (a user "stop") closes with the quit code → the host skips the keep-alive
    // linger; a plain drop / disconnect closes with 0 → the host lingers so a reconnect can resume.
    let close_code = if quit.load(Ordering::SeqCst) {
        crate::quic::QUIT_CLOSE_CODE
    } else {
        0
    };
    conn.close(close_code.into(), b"client closed");
}

#[cfg(test)]
mod host_port_tests {
    use super::join_host_port;

    #[test]
    fn brackets_bare_ipv6_only() {
        assert_eq!(join_host_port("192.168.1.9", 4770), "192.168.1.9:4770");
        assert_eq!(join_host_port("myhost", 4770), "myhost:4770");
        assert_eq!(join_host_port("fd00::1", 4770), "[fd00::1]:4770");
        assert_eq!(join_host_port("[fd00::1]", 4770), "[fd00::1]:4770");
        // The bracketed form is what SocketAddr's parser actually accepts.
        assert!(join_host_port("fd00::1", 4770)
            .parse::<std::net::SocketAddr>()
            .is_ok());
    }
}

#[cfg(test)]
mod rfi_recovery_tests {
    //! The client-side loss-range detector shared by every embedder (Android, the C-ABI Apple
    //! client, the Windows shell pump). `observe` is pure over `(frame_index, now)`, so the wrapping
    //! frame arithmetic and the RFI throttle are exercised here without a live session.
    use super::{RecoveryAsk, RfiRecovery, RFI_THROTTLE};
    use std::time::{Duration, Instant};

    // A fixed base instant; offsets model the throttle window deterministically (no sleeping).
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_arms_without_a_gap() {
        let mut r = RfiRecovery::default();
        // The opening frame only seeds the expectation — there is no prior frame to be missing.
        assert_eq!(r.observe(100, base()), (false, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(101));
    }

    #[test]
    fn contiguous_frames_never_gap() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(101, t), (false, RecoveryAsk::None));
        assert_eq!(r.observe(102, t), (false, RecoveryAsk::None));
        assert_eq!(r.observe(103, t), (false, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(104));
    }

    #[test]
    fn forward_gap_reports_the_exact_lost_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t); // expecting 101 next
                           // 101..=104 were lost; 105 arrived. The RFI must name exactly the missing span.
        assert_eq!(r.observe(105, t), (true, RecoveryAsk::Rfi(101, 104)));
        // The expectation advances past the delivered frame so the same gap can't re-fire.
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn single_frame_drop_names_a_unit_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        // Exactly one frame (101) lost → range is the single index [101, 101].
        assert_eq!(r.observe(102, t), (true, RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn throttle_suppresses_bursts_then_re_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        // First gap fires the request and stamps the throttle.
        assert_eq!(r.observe(105, t0), (true, RecoveryAsk::Rfi(101, 104)));
        // A second gap 50 ms later is still a gap, but the request is throttled away.
        assert_eq!(
            r.observe(110, t0 + Duration::from_millis(50)),
            (true, RecoveryAsk::None)
        );
        // Past the window, the request re-opens for the still-accurate lost span.
        assert_eq!(
            r.observe(120, t0 + RFI_THROTTLE + Duration::from_millis(1)),
            (true, RecoveryAsk::Rfi(111, 119))
        );
    }

    #[test]
    fn stragglers_behind_the_delivery_point_are_ignored() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        r.observe(105, t); // expecting 106 next
                           // A reordered late arrival (103, well behind 106) is neither a gap nor a request, and it
                           // must not rewind the expectation — otherwise the next in-order frame would false-gap.
        assert_eq!(r.observe(103, t), (false, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn wraparound_is_contiguous_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t); // expecting u32::MAX next
        assert_eq!(r.observe(u32::MAX, t), (false, RecoveryAsk::None)); // contiguous, wraps to 0
        assert_eq!(r.next_expected, Some(0));
        assert_eq!(r.observe(0, t), (false, RecoveryAsk::None)); // still contiguous across the wrap
        assert_eq!(r.next_expected, Some(1));
    }

    #[test]
    fn gap_range_wraps_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t); // expecting u32::MAX next
                                    // u32::MAX was lost and 1 arrived → the lost span wraps: [u32::MAX, 0].
        assert_eq!(r.observe(1, t), (true, RecoveryAsk::Rfi(u32::MAX, 0)));
        assert_eq!(r.next_expected, Some(2));
    }

    #[test]
    fn huge_gap_resyncs_via_keyframe_not_rfi() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t); // expecting 101 next
                           // A jump wider than any encoder's reference history (RFI_MAX_RANGE): no valid
                           // reference exists for an RFI, and the jump may be a phantom (an old host's
                           // speed-test burst consuming video indexes) — ask for the IDR resync instead.
        let jump = 100 + crate::packet::RFI_MAX_RANGE + 2;
        assert_eq!(r.observe(jump, t), (true, RecoveryAsk::Keyframe));
        // The expectation still advances past the delivered frame (no re-fire on the next one).
        assert_eq!(r.next_expected, Some(jump + 1));
        assert_eq!(r.observe(jump + 1, t), (false, RecoveryAsk::None));
        // A huge gap consumes the shared throttle too — an immediate follow-up gap stays quiet.
        assert_eq!(
            r.observe(jump + 10, t + Duration::from_millis(1)),
            (true, RecoveryAsk::None)
        );
    }
}

#[cfg(test)]
mod frame_channel_tests {
    use super::{FrameChannel, FramePop, FRAME_QUEUE_HARD_CAP};
    use crate::session::Frame;
    use std::time::Duration;

    fn frame(i: u32) -> Frame {
        Frame {
            data: vec![i as u8],
            frame_index: i,
            pts_ns: i as u64,
            flags: 0,
        }
    }

    fn popped(ch: &FrameChannel) -> Option<u32> {
        match ch.pop(Duration::from_millis(0)) {
            FramePop::Frame(f) => Some(f.frame_index),
            _ => None,
        }
    }

    #[test]
    fn fifo_order_and_depth() {
        let ch = FrameChannel::new();
        assert_eq!(ch.depth(), 0);
        ch.push(frame(1));
        ch.push(frame(2));
        assert_eq!(ch.depth(), 2);
        assert_eq!(popped(&ch), Some(1)); // oldest first (never newest-wins pre-decode)
        assert_eq!(popped(&ch), Some(2));
        assert_eq!(ch.depth(), 0);
    }

    #[test]
    fn empty_pop_times_out_not_closed() {
        let ch = FrameChannel::new();
        assert!(matches!(
            ch.pop(Duration::from_millis(1)),
            FramePop::Timeout
        ));
    }

    #[test]
    fn clear_drops_backlog_and_reports_count() {
        let ch = FrameChannel::new();
        for i in 0..5 {
            ch.push(frame(i));
        }
        assert_eq!(ch.clear(), 5); // the jump-to-live discard returns what it dropped
        assert_eq!(ch.depth(), 0);
        assert!(matches!(
            ch.pop(Duration::from_millis(1)),
            FramePop::Timeout
        ));
    }

    #[test]
    fn close_after_drain_reports_closed() {
        let ch = FrameChannel::new();
        ch.push(frame(7));
        ch.close();
        // Queued frames still drain BEFORE the Closed signal.
        assert_eq!(popped(&ch), Some(7));
        assert!(matches!(ch.pop(Duration::from_millis(1)), FramePop::Closed));
    }

    #[test]
    fn hard_cap_drops_oldest() {
        let ch = FrameChannel::new();
        let total = FRAME_QUEUE_HARD_CAP as u32 + 10;
        for i in 0..total {
            ch.push(frame(i));
        }
        // Capped at the backstop; the OLDEST were dropped, so the newest survive in order.
        assert_eq!(ch.depth(), FRAME_QUEUE_HARD_CAP);
        assert_eq!(popped(&ch), Some(total - FRAME_QUEUE_HARD_CAP as u32));
    }
}
