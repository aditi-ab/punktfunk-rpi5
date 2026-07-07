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

use crate::config::{CompositorPref, GamepadPref, Mode, Role};
use crate::error::{PunktfunkError, Result};
use crate::input::InputEvent;
use crate::packet::FLAG_PROBE;
use crate::quic::{
    endpoint, io, window_loss_ppm, ColorInfo, HdrMeta, Hello, HidOutput, LossReport, ProbeRequest,
    ProbeResult, Reconfigure, Reconfigured, RequestKeyframe, RichInput, Start, Welcome,
};
use crate::session::{Frame, Session};
use crate::transport::UdpTransport;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A control-stream request the embedder makes on the open handshake stream: a mode switch or a
/// speed test. One outbound channel carries both so the worker's `select!` has a single writer
/// (two `&mut ctrl_send` borrows across select branches don't compile).
enum CtrlRequest {
    Mode(Mode),
    Probe(ProbeRequest),
    Keyframe,
    Loss(LossReport),
}

/// What the worker reports to [`NativeClient::connect`] once the handshake lands: the negotiated
/// mode, the host-resolved compositor backend, the host-resolved gamepad backend, the host's
/// certificate fingerprint, the resolved encoder bitrate (kbps), and the host↔client clock offset
/// (ns, host minus client; 0 = no skew correction / an old host that didn't answer the handshake).
/// The trailing `u8`s are the resolved encode bit depth (8/10), the chroma `chroma_format_idc`
/// (1 = 4:2:0, 3 = 4:4:4), the resolved audio channel count (2/6/8), and the resolved video codec
/// (`quic::CODEC_*`), with [`ColorInfo`] the resolved colour signalling — all from the [`Welcome`].
type Negotiated = (
    Mode,
    CompositorPref,
    GamepadPref,
    [u8; 32],
    u32,
    i64,
    u8,
    ColorInfo,
    u8,
    u8,
    u8,
);

/// Accumulated state of an in-flight / finished speed test. The data-plane pump mirrors the
/// session's packet-level receive counters here; the control task finalizes the delivered figure
/// and folds in the host's [`ProbeResult`] when it lands. Read by [`NativeClient::probe_result`].
///
/// Counting at the *packet* level (every delivered wire packet) — not whole reassembled probe AUs —
/// is what makes the measurement degrade gracefully: once loss exceeds the FEC budget no AU
/// completes, so the old AU-based count cliffed to zero even though most bytes still arrived.
#[derive(Default)]
struct ProbeState {
    /// A probe is in progress (set by `request_probe`, cleared by nothing — the latest one wins).
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
/// `try_send` discipline as the other planes) — the host re-sends rumble state
/// periodically, so a dropped transition (including a stop) heals within ~500 ms.
const RUMBLE_QUEUE: usize = 16;

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

pub struct NativeClient {
    // Each plane's receiver sits behind its own mutex so `NativeClient` is `Sync` and Rust
    // embedders can share one `Arc<NativeClient>` across their plane threads (the same
    // one-thread-per-plane contract the C ABI documents — the lock is uncontended there,
    // and two threads racing one plane now serialize instead of being undefined).
    frames: Arc<FrameChannel>,
    audio: Mutex<Receiver<AudioPacket>>,
    rumble: Mutex<Receiver<(u16, u16, u16)>>,
    /// Inbound DualSense feedback (lightbar / player LEDs / adaptive triggers) — 0xCD datagrams.
    hidout: Mutex<Receiver<HidOutput>>,
    /// Inbound static HDR metadata (ST.2086 mastering + content light level) — 0xCE datagrams.
    hdr_meta: Mutex<Receiver<HdrMeta>>,
    /// Inbound per-AU host capture→send timings — 0xCF datagrams (the client always advertises
    /// [`quic::VIDEO_CAP_HOST_TIMING`]; an older host simply never sends any).
    host_timing: Mutex<Receiver<crate::quic::HostTiming>>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    /// Outbound mic frames `(seq, pts_ns, opus)` → encoded as 0xCB datagrams by the worker.
    mic_tx: tokio::sync::mpsc::UnboundedSender<(u32, u64, Vec<u8>)>,
    /// Outbound rich input (DualSense touchpad / motion) → 0xCC datagrams by the worker.
    rich_input_tx: tokio::sync::mpsc::UnboundedSender<RichInput>,
    /// Outbound control-stream requests (mode switch, speed test) → the worker's control task.
    ctrl_tx: tokio::sync::mpsc::UnboundedSender<CtrlRequest>,
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
    /// Kernel ids of the client's latency-critical native threads: the internal data-plane pump
    /// (UDP receive + FEC reassembly) plus any embedder plane threads registered via
    /// [`NativeClient::register_hot_thread`]. The Android client feeds these to an ADPF hint session
    /// so the CPU governor keeps the whole video pipeline on fast cores. Empty on platforms without
    /// `gettid` (see [`current_hot_tid`]).
    hot_tids: Arc<Mutex<Vec<i32>>>,
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
    /// answer, or genuinely synced clocks).
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
        launch: Option<String>,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        let frame_chan = Arc::new(FrameChannel::new());
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<(u16, u16, u16)>(RUMBLE_QUEUE);
        let (hidout_tx, hidout_rx) = std::sync::mpsc::sync_channel::<HidOutput>(HIDOUT_QUEUE);
        let (hdr_meta_tx, hdr_meta_rx) = std::sync::mpsc::sync_channel::<HdrMeta>(HDR_META_QUEUE);
        let (host_timing_tx, host_timing_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::HostTiming>(HOST_TIMING_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (mic_tx, mic_rx) = tokio::sync::mpsc::unbounded_channel::<(u32, u64, Vec<u8>)>();
        let (rich_input_tx, rich_input_rx) = tokio::sync::mpsc::unbounded_channel::<RichInput>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::unbounded_channel::<CtrlRequest>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Negotiated>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let mode_slot = Arc::new(std::sync::Mutex::new(mode));
        let probe = Arc::new(Mutex::new(ProbeState::default()));
        let frames_dropped = Arc::new(AtomicU64::new(0));
        let hot_tids = Arc::new(Mutex::new(Vec::new()));

        let host = host.to_string();
        let frame_chan_w = frame_chan.clone();
        let shutdown_w = shutdown.clone();
        let quit_w = quit.clone();
        let mode_slot_w = mode_slot.clone();
        let probe_w = probe.clone();
        let frames_dropped_w = frames_dropped.clone();
        let hot_tids_w = hot_tids.clone();
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
                    hot_tids: hot_tids_w,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        let (
            negotiated,
            resolved_compositor,
            resolved_gamepad,
            fingerprint,
            resolved_bitrate_kbps,
            clock_offset_ns,
            bit_depth,
            color,
            chroma_format,
            audio_channels,
            codec,
        ) = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                shutdown.store(true, Ordering::SeqCst);
                return Err(PunktfunkError::Timeout);
            }
        };
        *mode_slot.lock().unwrap() = negotiated;
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
            hot_tids,
            mode: mode_slot,
            host_fingerprint: fingerprint,
            resolved_compositor,
            resolved_gamepad,
            resolved_bitrate_kbps,
            clock_offset_ns,
            bit_depth,
            color,
            chroma_format,
            audio_channels,
            codec,
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
        let remote: std::net::SocketAddr = format!("{host}:{port}")
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
            .send(CtrlRequest::Mode(mode))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Ask the host's encoder to emit a fresh IDR keyframe now (client recovery on a stalled
    /// decode). Non-blocking, fire-and-forget — the recovered keyframe is the only ack. The
    /// caller should throttle (the decode stays wedged across several frames until the IDR
    /// lands, so requesting on every frame would flood the control stream).
    pub fn request_keyframe(&self) -> Result<()> {
        self.ctrl_tx
            .send(CtrlRequest::Keyframe)
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Cumulative access units the host→client reassembler dropped as unrecoverable (FEC couldn't
    /// rebuild them). A video loop polls this and calls [`request_keyframe`](Self::request_keyframe)
    /// when it increases — the correct loss trigger under infinite GOP, where unrecoverable loss
    /// produces reference-missing delta frames the decoder silently conceals (so a decode-error
    /// trigger would rarely fire). Monotonic for the session; compare against the last observed value.
    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
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
            .send(CtrlRequest::Probe(ProbeRequest {
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
    /// [`NativeClient::next_audio`]. Amplitudes are 0..0xFFFF, `(0, 0)` = stop.
    pub fn next_rumble(&self, timeout: Duration) -> Result<(u16, u16, u16)> {
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
        self.mic_tx
            .send((seq, pts_ns, opus))
            .map_err(|_| PunktfunkError::Closed)
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
    launch: Option<String>,
    pin: Option<[u8; 32]>,
    identity: Option<(String, String)>,
    frames: Arc<FrameChannel>,
    audio_tx: SyncSender<AudioPacket>,
    rumble_tx: SyncSender<(u16, u16, u16)>,
    hidout_tx: SyncSender<HidOutput>,
    hdr_meta_tx: SyncSender<HdrMeta>,
    host_timing_tx: SyncSender<crate::quic::HostTiming>,
    input_rx: tokio::sync::mpsc::UnboundedReceiver<InputEvent>,
    mic_rx: tokio::sync::mpsc::UnboundedReceiver<(u32, u64, Vec<u8>)>,
    rich_input_rx: tokio::sync::mpsc::UnboundedReceiver<RichInput>,
    ctrl_rx: tokio::sync::mpsc::UnboundedReceiver<CtrlRequest>,
    ctrl_tx: tokio::sync::mpsc::UnboundedSender<CtrlRequest>,
    ready_tx: std::sync::mpsc::Sender<Result<Negotiated>>,
    shutdown: Arc<AtomicBool>,
    /// Deliberate-quit flag (see [`NativeClient::quit`]): the worker closes with the quit code if set.
    quit: Arc<AtomicBool>,
    mode_slot: Arc<std::sync::Mutex<Mode>>,
    probe: Arc<Mutex<ProbeState>>,
    frames_dropped: Arc<AtomicU64>,
    hot_tids: Arc<Mutex<Vec<i32>>>,
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
        hot_tids,
    } = args;
    let setup = async {
        let remote: std::net::SocketAddr = format!("{host}:{port}")
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
                video_caps: video_caps | crate::quic::VIDEO_CAP_HOST_TIMING,
                // Requested surround channel count; the host echoes the resolved value in Welcome.
                audio_channels,
                // The codecs this client can decode + its soft preference (0 = auto). The host
                // resolves the emitted codec from these and reports it in `Welcome::codec`.
                video_codecs,
                preferred_codec,
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
        let clock_offset_ns = match crate::quic::clock_sync(&mut send, &mut recv).await {
            Some(skew) => {
                tracing::info!(
                    offset_ns = skew.offset_ns,
                    rtt_us = skew.rtt_ns / 1000,
                    rounds = skew.rounds,
                    "clock skew estimated (host-client)"
                );
                skew.offset_ns
            }
            None => 0,
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
            welcome.mode,
            welcome.compositor,
            welcome.gamepad,
            fingerprint,
            welcome.bitrate_kbps,
            clock_offset_ns,
            welcome.bit_depth,
            welcome.color,
            welcome.chroma_format,
            welcome.audio_channels,
            welcome.codec,
        ))
    };

    let (
        conn,
        mut session,
        mut ctrl_send,
        mut ctrl_recv,
        negotiated,
        resolved_compositor,
        resolved_gamepad,
        fingerprint,
        resolved_bitrate_kbps,
        clock_offset_ns,
        bit_depth,
        color,
        chroma_format,
        audio_channels,
        codec,
    ) = match setup.await {
        Ok(t) => t,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let _ = ready_tx.send(Ok((
        negotiated,
        resolved_compositor,
        resolved_gamepad,
        fingerprint,
        resolved_bitrate_kbps,
        clock_offset_ns,
        bit_depth,
        color,
        chroma_format,
        audio_channels,
        codec,
    )));

    // Input task: embedder events → QUIC datagrams.
    let input_conn = conn.clone();
    tokio::spawn(async move {
        while let Some(ev) = input_rx.recv().await {
            let _ = input_conn.send_datagram(ev.encode().to_vec().into());
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

    // Control task: the handshake stream stays open for mid-stream renegotiation + speed tests.
    // Outbound requests (mode switch, probe) and inbound replies (Reconfigured, ProbeResult) are
    // multiplexed with `select!`; a single outbound channel (`ctrl_rx`) keeps one writer so the
    // two `&mut ctrl_send` borrows don't collide across branches.
    {
        let mode_slot = mode_slot.clone();
        let probe = probe.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    req = ctrl_rx.recv() => {
                        let Some(req) = req else { break }; // client dropped
                        let bytes = match req {
                            CtrlRequest::Mode(m) => Reconfigure { mode: m }.encode(),
                            CtrlRequest::Probe(p) => p.encode(),
                            CtrlRequest::Keyframe => RequestKeyframe.encode(),
                            CtrlRequest::Loss(r) => r.encode(),
                        };
                        if io::write_msg(&mut ctrl_send, &bytes).await.is_err() {
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
                            tracing::info!(
                                host_goodput_bytes = result.bytes_sent,
                                wire_packets_sent = result.wire_packets_sent,
                                send_dropped = result.send_dropped,
                                duration_ms = result.duration_ms,
                                delivered_packets = p.delivered_packets,
                                "speed-test probe result"
                            );
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
                    if let Some(r) = crate::quic::decode_rumble_datagram(&d) {
                        let _ = rumble_tx.try_send(r);
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
    let _ = tokio::task::spawn_blocking(move || {
        pin_thread_user_interactive(); // feeds the frame channel → the user-interactive video pump
        register_hot_tid(&pump_hot_tids); // this thread does UDP receive + FEC reassembly — hint it
                                          // Adaptive-FEC loss reporting: every ADAPT_REPORT_INTERVAL, report the loss observed over the
                                          // window (shards FEC recovered, plus a bump if any frame went unrecoverable) so the host can
                                          // size FEC to the link. Suppressed during a speed test (its FLAG_PROBE filler would skew it).
        const ADAPT_REPORT_INTERVAL: Duration = Duration::from_millis(750);
        let mut last_report = Instant::now();
        let (mut last_recovered, mut last_received, mut last_dropped) = (0u64, 0u64, 0u64);
        // Jump-to-live state (see the guard in the loop below): the clock-based over-bound run
        // (`stale_frames`, armed only when the skew handshake succeeded so the clocks are comparable),
        // the clock-free non-draining-queue run (`standing_frames`), and the last-jump instant for the
        // shared cooldown.
        let mut stale_frames: u32 = 0;
        let mut standing_frames: u32 = 0;
        let mut last_flush: Option<Instant> = None;
        while !pump_shutdown.load(Ordering::SeqCst) {
            // Mirror the reassembler's unrecoverable-drop count for the client's keyframe-recovery
            // loop, and (during a speed test) the packet-level receive counters for the throughput
            // measurement. Updated every iteration (not just on a produced frame) so they stay current
            // through a total-loss drought where no AU completes. Cheap: a few relaxed atomic loads.
            let st = session.stats();
            frames_dropped.store(st.frames_dropped, Ordering::Relaxed);
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
                let loss_ppm = window_loss_ppm(
                    st.fec_recovered_shards.wrapping_sub(last_recovered),
                    st.packets_received.wrapping_sub(last_received),
                    st.frames_dropped.wrapping_sub(last_dropped),
                );
                let _ = ctrl_tx.send(CtrlRequest::Loss(LossReport { loss_ppm }));
                last_report = Instant::now();
                last_recovered = st.fec_recovered_shards;
                last_received = st.packets_received;
                last_dropped = st.frames_dropped;
            }
            match session.poll_frame() {
                Ok(frame) => {
                    if frame.flags & FLAG_PROBE as u32 != 0 {
                        continue; // speed-test filler, not video — measured via the counters above
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
                        if clock_offset_ns != 0 && lat_ns > FLUSH_LATENCY.as_nanos() as i128 {
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
                            let flushed = session.flush_backlog().unwrap_or(0);
                            let dropped = frames.clear();
                            let _ = ctrl_tx.send(CtrlRequest::Keyframe);
                            tracing::warn!(
                                behind_ms = if clock_behind { lat_ns / 1_000_000 } else { -1 },
                                queue_depth = depth,
                                flushed_datagrams = flushed,
                                dropped_frames = dropped,
                                "receive backlog stopped draining — jumped to live (flush + keyframe)"
                            );
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
