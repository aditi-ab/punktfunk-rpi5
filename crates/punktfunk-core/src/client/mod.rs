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

use crate::clipboard::{ClipCommand, ClipEventCore};
use crate::config::{CompositorPref, GamepadPref, Mode};
use crate::error::{PunktfunkError, Result};
use crate::input::InputEvent;
use crate::quic::{
    endpoint, ClipControl, ClipKind, ClipOffer, ColorInfo, HdrMeta, HidOutput, ProbeRequest,
    RfiRequest, RichInput,
};
use crate::session::Frame;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod control;
mod frame_channel;
mod pairing;
mod planes;
mod probe;
mod pump;
mod recovery;
mod rumble;
mod worker;

pub use self::planes::AudioPacket;
pub use self::probe::ProbeOutcome;
pub use self::rumble::{ActuatorQuirks, RumbleCommand};

use self::control::{CtrlRequest, Negotiated};
use self::frame_channel::{DecodeLatAcc, FrameChannel, FramePop};
use self::planes::{
    RumbleUpdate, AUDIO_QUEUE, CLIP_EVENT_QUEUE, CURSOR_SHAPE_QUEUE, CURSOR_STATE_QUEUE,
    HDR_META_QUEUE, HIDOUT_QUEUE, HOST_TIMING_QUEUE, RUMBLE_QUEUE,
};
use self::probe::ProbeState;
use self::pump::run_pump;
use self::recovery::{RecoveryAsk, RfiRecovery};
use self::worker::WorkerArgs;

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

/// Outbound mic uplink queue depth: 5 ms Opus frames, so 64 is ~320 ms of audio — far beyond
/// any worker stall a live mic session survives anyway. On overflow the FRESH frame is dropped
/// (a tokio mpsc can't shed from the head; by the time 320 ms are queued the stream is broken
/// either way, and the bound is about memory, not audio quality) and logged at debug.
const MIC_QUEUE: usize = 64;

/// Outbound control-request queue depth. The requests are sparse (mode switches, keyframe
/// requests, ~1.3 loss reports/s, clock re-syncs) — 32 is hours of headroom; a full queue means
/// the control task is wedged, which callers treat as a closed session.
const CTRL_QUEUE: usize = 32;

pub struct NativeClient {
    // Each plane's receiver sits behind its own mutex so `NativeClient` is `Sync` and Rust
    // embedders can share one `Arc<NativeClient>` across their plane threads (the same
    // one-thread-per-plane contract the C ABI documents — the lock is uncontended there,
    // and two threads racing one plane now serialize instead of being undefined).
    frames: Arc<FrameChannel>,
    audio: Mutex<Receiver<AudioPacket>>,
    rumble: Mutex<Receiver<RumbleUpdate>>,
    /// The shared rumble policy engine ([`RumbleCommand`] API — the uniform per-platform-policy
    /// replacement). Fed by the datagram demux in parallel with the raw `rumble` queue; an
    /// embedder consumes ONE of the two APIs (documented on [`NativeClient::next_rumble_command`]).
    rumble_sched: Arc<rumble::RumbleShared>,
    /// Inbound DualSense feedback (lightbar / player LEDs / adaptive triggers) — 0xCD datagrams.
    hidout: Mutex<Receiver<HidOutput>>,
    /// Inbound static HDR metadata (ST.2086 mastering + content light level) — 0xCE datagrams.
    hdr_meta: Mutex<Receiver<HdrMeta>>,
    /// Inbound per-AU host capture→send timings — 0xCF datagrams (the client always advertises
    /// [`quic::VIDEO_CAP_HOST_TIMING`]; an older host simply never sends any).
    host_timing: Mutex<Receiver<crate::quic::HostTiming>>,
    /// Inbound cursor shapes (control-stream [`crate::quic::CursorShape`]) — only a session
    /// that advertised [`quic::CLIENT_CAP_CURSOR`] against a [`quic::HOST_CAP_CURSOR`] host
    /// ever receives any.
    cursor_shape: Mutex<Receiver<crate::quic::CursorShape>>,
    /// Inbound per-frame cursor state — `0xD0` datagrams (same negotiation gate as shapes).
    cursor_state: Mutex<Receiver<crate::quic::CursorState>>,
    input_tx: tokio::sync::mpsc::UnboundedSender<InputEvent>,
    /// Outbound mic frames `(seq, pts_ns, opus)` → encoded as 0xCB datagrams by the worker.
    /// Bounded ([`MIC_QUEUE`]): a wedged worker drops fresh frames (logged) instead of queueing
    /// audio-latency (and memory) without limit — mic is best-effort end to end.
    mic_tx: tokio::sync::mpsc::Sender<(u32, u64, Vec<u8>)>,
    /// Outbound 0xCC rich-input plane, PRE-ENCODED datagrams: [`RichInput`] touchpad/motion
    /// (encoded in [`NativeClient::send_rich_input`]) and stylus [`crate::quic::PenBatch`]es
    /// (encoded in [`NativeClient::send_pen`]) share the channel — the worker's task just
    /// forwards bytes, so a new 0xCC kind never touches the pump.
    rich_input_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Outbound control-stream requests (mode switch, speed test) → the worker's control task.
    /// Bounded ([`CTRL_QUEUE`]) — the requests are sparse; a full queue means the control task
    /// is wedged/dead, and callers treat it like a closed session.
    ctrl_tx: tokio::sync::mpsc::Sender<CtrlRequest>,
    /// Inbound shared-clipboard events (remote offers, host acks, fetch-requests, fetched
    /// payloads), drained by [`NativeClient::next_clip`] → the C ABI poll. Fed by the control task
    /// (metadata) and the clipboard task (fetch data).
    clip: Mutex<Receiver<ClipEventCore>>,
    /// Outbound clipboard fetch/serve/cancel commands → the worker's clipboard task
    /// ([`crate::clipboard::run`]). Unbounded like `input_tx`; the commands are sparse and each
    /// carries at most one paste's bytes.
    clip_cmd_tx: tokio::sync::mpsc::UnboundedSender<ClipCommand>,
    /// Monotonic id for outbound fetches ([`NativeClient::clip_fetch`]); stays below
    /// [`crate::clipboard::INBOUND_REQ_FLAG`] so it never collides with an inbound serve `req_id`.
    next_xfer_id: AtomicU32,
    /// Wrapping per-connection [`crate::quic::PenBatch::seq`] counter, stamped by
    /// [`NativeClient::send_pen`] (the host's reorder gate compares it).
    pen_seq: AtomicU16,
    /// The host capability bitfield ([`crate::quic::Welcome::host_caps`]) — see
    /// [`NativeClient::host_caps`].
    pub host_caps: u8,
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
    /// Decode-stage latency samples from the embedder ([`report_decode_us`](Self::report_decode_us)),
    /// drained per window by the data-plane pump to feed the adaptive-bitrate controller's decode
    /// signal. Shared with the pump; see [`DecodeLatAcc`].
    decode_lat: Arc<Mutex<DecodeLatAcc>>,
    /// Whether the adaptive-bitrate controller is armed for this session (Automatic bitrate and not
    /// a rate-pinned PyroWave stream) — exposed via [`wants_decode_latency`](Self::wants_decode_latency)
    /// so an embedder skips the per-frame decode measurement when the controller wouldn't use it.
    wants_decode: bool,
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
    /// The session's wire shard payload (bytes of AU per datagram) — the parse-window size
    /// for chunk-aligned AUs ([`crate::packet::USER_FLAG_CHUNK_ALIGNED`], plan §4.4).
    pub shard_payload: u16,
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
        // Non-video client capabilities ([`crate::quic::Hello::client_caps`]) — set
        // [`crate::quic::CLIENT_CAP_CURSOR`] ONLY if this embedder actually renders the host
        // cursor locally (shape + state planes): the host stops compositing the pointer into
        // the video for a session that advertises it, so a non-rendering embedder that sets it
        // streams with NO visible cursor at all. `0` = today's composited behavior.
        client_caps: u8,
        launch: Option<String>,
        pin: Option<[u8; 32]>,
        identity: Option<(String, String)>,
        timeout: Duration,
    ) -> Result<NativeClient> {
        let frame_chan = Arc::new(FrameChannel::new());
        let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<AudioPacket>(AUDIO_QUEUE);
        let (rumble_tx, rumble_rx) = std::sync::mpsc::sync_channel::<RumbleUpdate>(RUMBLE_QUEUE);
        let rumble_sched = Arc::new(rumble::RumbleShared::new());
        let rumble_feed = rumble::RumbleFeed(rumble_sched.clone());
        let (hidout_tx, hidout_rx) = std::sync::mpsc::sync_channel::<HidOutput>(HIDOUT_QUEUE);
        let (hdr_meta_tx, hdr_meta_rx) = std::sync::mpsc::sync_channel::<HdrMeta>(HDR_META_QUEUE);
        let (host_timing_tx, host_timing_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::HostTiming>(HOST_TIMING_QUEUE);
        let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let (mic_tx, mic_rx) = tokio::sync::mpsc::channel::<(u32, u64, Vec<u8>)>(MIC_QUEUE);
        let (rich_input_tx, rich_input_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (ctrl_tx, ctrl_rx) = tokio::sync::mpsc::channel::<CtrlRequest>(CTRL_QUEUE);
        let (clip_event_tx, clip_event_rx) =
            std::sync::mpsc::sync_channel::<ClipEventCore>(CLIP_EVENT_QUEUE);
        let (clip_cmd_tx, clip_cmd_rx) = tokio::sync::mpsc::unbounded_channel::<ClipCommand>();
        let (cursor_shape_tx, cursor_shape_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::CursorShape>(CURSOR_SHAPE_QUEUE);
        let (cursor_state_tx, cursor_state_rx) =
            std::sync::mpsc::sync_channel::<crate::quic::CursorState>(CURSOR_STATE_QUEUE);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Negotiated>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let mode_slot = Arc::new(std::sync::Mutex::new(mode));
        let probe = Arc::new(Mutex::new(ProbeState::default()));
        let frames_dropped = Arc::new(AtomicU64::new(0));
        let fec_recovered = Arc::new(AtomicU64::new(0));
        let hot_tids = Arc::new(Mutex::new(Vec::new()));
        let clock_offset = Arc::new(AtomicI64::new(0));
        let decode_lat = Arc::new(Mutex::new(DecodeLatAcc::default()));

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
        let decode_lat_w = decode_lat.clone();
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
                rt.block_on(run_pump(WorkerArgs {
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
                    client_caps,
                    launch,
                    pin,
                    identity,
                    connect_timeout: timeout,
                    frames: frame_chan_w,
                    audio_tx,
                    rumble_tx,
                    rumble_feed,
                    hidout_tx,
                    hdr_meta_tx,
                    host_timing_tx,
                    cursor_shape_tx,
                    cursor_state_tx,
                    input_rx,
                    mic_rx,
                    rich_input_rx,
                    ctrl_rx,
                    ctrl_tx: ctrl_tx_pump,
                    clip_event_tx,
                    clip_cmd_rx,
                    ready_tx,
                    shutdown: shutdown_w,
                    quit: quit_w,
                    mode_slot: mode_slot_w,
                    probe: probe_w,
                    frames_dropped: frames_dropped_w,
                    fec_recovered: fec_recovered_w,
                    hot_tids: hot_tids_w,
                    clock_offset: clock_offset_w,
                    decode_lat: decode_lat_w,
                }));
            })
            .map_err(PunktfunkError::Io)?;

        let negotiated = match ready_rx.recv_timeout(timeout) {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                // A connect we already reported as failed must not leave a lingering host
                // session if the handshake lands late: mark it a deliberate QUIT (not a plain
                // drop / close code 0) so the worker's close tells the host to tear down now
                // instead of holding the session (and its virtual display) for a reconnect
                // that will never come.
                quit.store(true, Ordering::SeqCst);
                shutdown.store(true, Ordering::SeqCst);
                return Err(PunktfunkError::Timeout);
            }
        };
        *mode_slot.lock().unwrap() = negotiated.mode;
        Ok(NativeClient {
            frames: frame_chan,
            audio: Mutex::new(audio_rx),
            rumble: Mutex::new(rumble_rx),
            rumble_sched,
            hidout: Mutex::new(hidout_rx),
            hdr_meta: Mutex::new(hdr_meta_rx),
            host_timing: Mutex::new(host_timing_rx),
            cursor_shape: Mutex::new(cursor_shape_rx),
            cursor_state: Mutex::new(cursor_state_rx),
            input_tx,
            mic_tx,
            rich_input_tx,
            ctrl_tx,
            clip: Mutex::new(clip_event_rx),
            clip_cmd_tx,
            next_xfer_id: AtomicU32::new(1),
            pen_seq: AtomicU16::new(0),
            host_caps: negotiated.host_caps,
            probe,
            shutdown,
            quit,
            worker: Some(worker),
            frames_dropped,
            fec_recovered,
            rfi: Mutex::new(RfiRecovery::default()),
            hot_tids,
            clock_offset,
            decode_lat,
            // The controller arms exactly when the pump does — all three terms, not two: Automatic
            // (the user asked for bitrate 0), not a rate-pinned PyroWave stream, AND the host
            // echoed the rate it actually configured. Dropping the last term made this
            // over-advertise against an old host that reports no rate, so an embedder fed decode
            // latency to a controller that never runs.
            wants_decode: bitrate_kbps == 0
                && negotiated.codec != crate::quic::CODEC_PYROWAVE
                && negotiated.bitrate_kbps > 0,
            mode: mode_slot,
            host_fingerprint: negotiated.host_fingerprint,
            resolved_compositor: negotiated.compositor,
            resolved_gamepad: negotiated.gamepad,
            resolved_bitrate_kbps: negotiated.bitrate_kbps,
            shard_payload: negotiated.shard_payload,
            clock_offset_ns: negotiated.clock_offset_ns,
            bit_depth: negotiated.bit_depth,
            color: negotiated.color,
            chroma_format: negotiated.chroma_format,
            audio_channels: negotiated.audio_channels,
            codec: negotiated.codec,
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

    /// Tell the host who renders the pointer (cursor-forward sessions —
    /// design/remote-desktop-sweep.md §8): `true` = this client draws it locally (the desktop
    /// mouse model; the host excludes the pointer from the video and forwards shape/state),
    /// `false` = the host composites it into the video (the capture model — full fidelity,
    /// the pre-channel behavior). Call on every mouse-model flip; idempotent, latest-wins,
    /// no-op on hosts without [`HOST_CAP_CURSOR`](crate::quic::HOST_CAP_CURSOR).
    pub fn set_cursor_render(&self, client_draws: bool) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::CursorRender(crate::quic::CursorRenderMode {
                client_draws,
            }))
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

    /// Report one decoded frame's decode-stage latency, in microseconds: the wall-clock elapsed from
    /// the access unit leaving [`next_frame`](Self::next_frame) to its decoded output becoming
    /// available (dequeued from the decoder). This feeds the "Automatic" bitrate controller's decode
    /// signal — the only one that sees the client's own decoder, so the rate can be capped at the
    /// real decode limit instead of climbing to the network link ceiling and choking a slower HW
    /// decoder (the LAN-vs-mobile-decoder case). Measure from the AU handoff, NOT from the codec-queue
    /// call, so decoder-input backpressure (the backlog) is included; exclude the presenter's vsync
    /// wait so a paced/capped frame rate doesn't read as decode latency. Cheap and lock-brief — the
    /// embedder may call it every frame unconditionally; the controller ignores it when Automatic is
    /// off and the pump drains it every window regardless, so the accumulator stays bounded.
    pub fn report_decode_us(&self, us: u32) {
        let mut acc = self.decode_lat.lock().unwrap();
        acc.sum_us += us as u64;
        acc.count += 1;
    }

    /// Whether [`report_decode_us`](Self::report_decode_us) is worth calling this session: `true`
    /// only when the adaptive-bitrate controller is armed (Automatic bitrate, non-PyroWave), so an
    /// embedder can skip the per-frame decode-latency measurement entirely for explicit-bitrate and
    /// PyroWave sessions (where the signal is ignored). Constant for the session — check once.
    pub fn wants_decode_latency(&self) -> bool {
        self.wants_decode
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
            duration_ms,
            ..Default::default()
        };
        let sent = self
            .ctrl_tx
            .try_send(CtrlRequest::Probe(ProbeRequest {
                target_kbps,
                duration_ms,
            }))
            .map_err(|_| PunktfunkError::Closed);
        if sent.is_err() {
            // Nothing was asked of the host, so nothing will ever answer. Leaving `active` latched
            // would suppress the pump's entire report tick for the rest of the session (the pump
            // mirrors the startup path's rollback at the same point).
            self.probe.lock().unwrap().active = false;
        }
        sent
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
        // Saturating: both counters arrive verbatim off the wire (same discipline as the
        // saturating_sub/mul above — a hostile sum must not overflow-panic a debug build).
        let offered_wire = p.host_wire_packets.saturating_add(p.host_send_dropped);
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

    /// Pull the next EFFECTIVE rumble command from the shared policy engine — the uniform
    /// replacement for per-platform rumble policy (`design/rumble-root-fix.md` §D). Unlike
    /// [`NativeClient::next_rumble_ttl`], the caller never sees a TTL and never owns a deadline:
    /// the engine emits the level on every wire update (renewals re-arm duration-parameterized
    /// APIs), an explicit zero at lease expiry / legacy staleness / connection close, and
    /// quirk-declared keepalives ([`NativeClient::set_rumble_quirks`]). Apply commands verbatim:
    /// `(0, 0)` = stop now; non-zero = run at this level, with `backstop_ms` as the safety-net
    /// duration for APIs that take one. [`PunktfunkError::NoFrame`] on timeout;
    /// [`PunktfunkError::Closed`] once the session ended AND every close-drain stop was delivered.
    ///
    /// One puller thread, and one API: an embedder uses EITHER this or
    /// `next_rumble`/`next_rumble_ttl` for a connection's lifetime, never both (both consume the
    /// same wire plane; the raw queue keeps filling harmlessly while this API is used).
    pub fn next_rumble_command(&self, timeout: Duration) -> Result<RumbleCommand> {
        match self.rumble_sched.next_command(timeout) {
            Ok(Some(c)) => Ok(c),
            Ok(None) => Err(PunktfunkError::NoFrame),
            Err(rumble::Closed) => Err(PunktfunkError::Closed),
        }
    }

    /// Declare a physical actuator's quirks for wire pad `pad` (see [`ActuatorQuirks`]) —
    /// typically at controller attach. All-default quirks (the initial state) describe a
    /// well-behaved actuator; only decaying actuators (Steam Deck, DualSense-over-BT raw HID)
    /// need a keepalive.
    pub fn set_rumble_quirks(&self, pad: u16, quirks: ActuatorQuirks) {
        self.rumble_sched.set_quirks(pad, quirks);
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

    /// Pull the next host cursor shape (design/remote-desktop-sweep.md M2): RGBA bitmap +
    /// hotspot, sent on pointer-bitmap change over the reliable control stream. The embedder
    /// caches by `serial` and builds an OS cursor from it; [`NativeClient::next_cursor_state`]
    /// references shapes by serial. Only a session that advertised
    /// [`crate::quic::CLIENT_CAP_CURSOR`] against a capable host receives any. Same
    /// timeout/closed semantics as [`NativeClient::next_hidout`].
    pub fn next_cursor_shape(&self, timeout: Duration) -> Result<crate::quic::CursorShape> {
        match self.cursor_shape.lock().unwrap().recv_timeout(timeout) {
            Ok(s) => Ok(s),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
    }

    /// Pull the next per-frame cursor state (`0xD0`): position, visibility and the M3
    /// relative-mode hint, referencing a shape by serial. Latest-wins — an embedder should
    /// drain the queue and apply only the newest. Same negotiation gate and timeout/closed
    /// semantics as [`NativeClient::next_cursor_shape`].
    pub fn next_cursor_state(&self, timeout: Duration) -> Result<crate::quic::CursorState> {
        match self.cursor_state.lock().unwrap().recv_timeout(timeout) {
            Ok(s) => Ok(s),
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

    /// The host capability bitfield the [`crate::quic::Welcome`] carried
    /// ([`crate::quic::HOST_CAP_GAMEPAD_STATE`], [`crate::quic::HOST_CAP_CLIPBOARD`]). A native
    /// client tests `host_caps() & HOST_CAP_CLIPBOARD` to decide whether to offer the
    /// shared-clipboard toggle.
    pub fn host_caps(&self) -> u8 {
        self.host_caps
    }

    /// Enable or disable the shared clipboard for this session (`design/clipboard-and-file-transfer.md`
    /// §3.1). Opt-in: nothing is announced or served until this crosses with `enabled = true`.
    /// `flags` carries [`crate::quic::CLIP_FLAG_FILES`]. Non-blocking; the host replies with a
    /// `State` event ([`NativeClient::next_clip`]).
    pub fn clip_control(&self, enabled: bool, flags: u8) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::ClipControl(ClipControl { enabled, flags }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Announce that the local clipboard changed — the lazy format-list offer. `seq` is a
    /// monotonic per-sender counter (newest wins); `kinds` is the advertised formats (≤
    /// [`crate::quic::CLIP_MAX_KINDS`]). The bytes cross only if the host later fetches.
    pub fn clip_offer(&self, seq: u32, kinds: Vec<ClipKind>) -> Result<()> {
        self.ctrl_tx
            .try_send(CtrlRequest::ClipOffer(ClipOffer { seq, kinds }))
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Start pulling one format (`mime`) of the host's current offer `seq` — lazily, when a local
    /// app pastes. `file_index` selects a file for a file transfer, or
    /// [`crate::quic::CLIP_FILE_INDEX_NONE`] for a non-file format. Returns the `xfer_id` echoed on
    /// the resulting `Data` / `Error` / `Cancelled` event.
    pub fn clip_fetch(&self, seq: u32, mime: String, file_index: u32) -> Result<u32> {
        let xfer_id = self.next_xfer_id.fetch_add(1, Ordering::Relaxed);
        // Stay in the low id space (inbound serve ids carry the high bit); wrap defensively.
        let xfer_id = xfer_id & !crate::clipboard::INBOUND_REQ_FLAG;
        self.clip_cmd_tx
            .send(ClipCommand::Fetch {
                xfer_id,
                seq,
                file_index,
                mime,
            })
            .map_err(|_| PunktfunkError::Closed)?;
        Ok(xfer_id)
    }

    /// Provide bytes answering a `FetchRequest` event (the host is pasting our offered data). Call
    /// repeatedly to stream a large payload; `last = true` completes it. `clip_cancel(req_id)`
    /// aborts instead.
    pub fn clip_serve(&self, req_id: u32, bytes: Vec<u8>, last: bool) -> Result<()> {
        self.clip_cmd_tx
            .send(ClipCommand::Serve {
                req_id,
                bytes,
                last,
            })
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Cancel a clipboard transfer by id — either an outbound fetch (`xfer_id` from
    /// [`NativeClient::clip_fetch`]) or an inbound serve (`req_id` from a `FetchRequest` event).
    pub fn clip_cancel(&self, id: u32) -> Result<()> {
        self.clip_cmd_tx
            .send(ClipCommand::Cancel { id })
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Pull the next shared-clipboard event (remote offer, host ack/state, fetch-request, fetched
    /// data, cancel, error); same timeout/closed semantics as [`NativeClient::next_hidout`]. A
    /// native client drains this on its own thread and drives the OS pasteboard from it.
    pub fn next_clip(&self, timeout: Duration) -> Result<ClipEventCore> {
        match self.clip.lock().unwrap().recv_timeout(timeout) {
            Ok(e) => Ok(e),
            Err(RecvTimeoutError::Timeout) => Err(PunktfunkError::NoFrame),
            Err(RecvTimeoutError::Disconnected) => Err(PunktfunkError::Closed),
        }
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
            .send(rich.encode())
            .map_err(|_| PunktfunkError::Closed)
    }

    /// Queue one stylus sample batch for delivery as a `0xCC/0x05` pen datagram
    /// (design/pen-tablet-input.md). `samples` are state-full and oldest-first (a capture
    /// callback's coalesced samples), at most [`crate::quic::PEN_BATCH_MAX`] per call — split
    /// longer runs into consecutive calls so the stamped wrapping `seq` keeps them ordered.
    /// Best-effort like every datagram: a lost batch self-heals on the next one (the samples
    /// carry full state, the host diffs — see [`crate::quic::PenTracker`]).
    ///
    /// Requires the host to have advertised [`crate::quic::HOST_CAP_PEN`]; toward an older
    /// host this returns `Unsupported` (embedders keep their pen-as-touch fallback instead of
    /// spraying 240 Hz datagrams the host drops unread).
    pub fn send_pen(&self, samples: &[crate::quic::PenSample]) -> Result<()> {
        if self.host_caps & crate::quic::HOST_CAP_PEN == 0 {
            return Err(PunktfunkError::Unsupported(
                "host did not advertise HOST_CAP_PEN",
            ));
        }
        if samples.is_empty() || samples.len() > crate::quic::PEN_BATCH_MAX {
            return Err(PunktfunkError::InvalidArg(
                "pen batch must hold 1..=PEN_BATCH_MAX samples",
            ));
        }
        let seq = self.pen_seq.fetch_add(1, Ordering::Relaxed);
        self.rich_input_tx
            .send(crate::quic::PenBatch::new(seq, samples).encode())
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
