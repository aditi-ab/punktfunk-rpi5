//! The `punktfunk/1` native host: QUIC control plane + the hardened core data plane over UDP.
//! This is punktfunk's own protocol, past the GameStream compatibility layer:
//!
//! * the Welcome negotiates **GF(2¹⁶) Leopard FEC** (inexpressible in GameStream) + AES-GCM;
//! * the client's Hello requests a display mode and the host creates a **native virtual
//!   output** at exactly that size/refresh (same vdisplay backends as the GameStream path);
//! * **input arrives as QUIC datagrams** — encrypted, congestion-managed, no ENet
//!   retransmission spikes — and feeds the session's input injector;
//! * video frames carry a wall-clock `pts_ns`, so a same-host client measures the full
//!   capture→encode→FEC→UDP→reassemble latency per frame.
//!
//! `punktfunk-host punktfunk1-host [--port 9777] [--source synthetic|virtual] [--seconds 30]
//!  [--frames 300]` serves sessions back to back (one at a time — the virtual output and
//!  encoder are single-tenant); `punktfunk-probe --connect host:9777` is the counterpart.
//!  The data plane runs on native threads (no async on the frame path).
//!
//! Alongside video + input, a session carries **audio** (desktop Opus, 5 ms frames, host →
//! client QUIC datagrams tagged [`punktfunk_core::quic::AUDIO_MAGIC`]) and **gamepads** (client
//! GamepadButton/GamepadAxis datagrams accumulated into per-pad state for the virtual xpad;
//! force feedback flows back as [`punktfunk_core::quic::RUMBLE_MAGIC`] datagrams).
//!
//! Trust: the host serves with its persistent identity (`~/.config/punktfunk/cert.pem`, shared
//! with GameStream pairing) and logs the SHA-256 fingerprint clients pin.

// Every `unsafe` block in this file carries a `// SAFETY:` proof; enforce it (unsafe-proof program).
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::{
    mtu1500_shard_payload_for, CompositorPref, FecConfig, FecScheme, GamepadPref, Role,
};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::{
    endpoint, io, BitrateChanged, ClockEcho, ClockProbe, ColorInfo, Hello, LossReport, PairRequest,
    ProbeRequest, ProbeResult, Reconfigure, Reconfigured, RequestKeyframe, RfiRequest, SetBitrate,
    Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::Session;
use rand::RngCore;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

/// Per-thread OS scheduling QoS lives in the shared `pf-frame` leaf crate (plan §W1/§W6);
/// re-exported so `crate::native::boost_thread_priority` stays stable (the GameStream path and the
/// native data plane reach it there).
pub(crate) use pf_frame::thread_qos::boost_thread_priority;

/// Compositor-preference resolution (plan §W1); `serve_session` reaches `resolve_compositor` here.
mod compositor;
use compositor::resolve_compositor;

/// Virtual-gamepad backend resolution (plan §W1); `serve_session` + the `Pads` state machine reach
/// `resolve_gamepad`/`resolve_pad_kind`/`route_decision` here.
mod gamepad;
use gamepad::{resolve_gamepad, resolve_pad_kind, route_decision};

/// The SPAKE2 pairing ceremony (plan §W1); `serve_session` dispatches a PairRequest connection here.
mod pairing;
use pairing::pair_ceremony;

/// The native audio plane (plan §W1); the session setup spawns `audio_thread` here.
mod audio;
use audio::audio_thread;

/// The native input plane (plan §W1); the session setup spawns `input_thread` and feeds it a
/// channel of `ClientInput`. The `Pads` router + rumble live there too.
mod input;
use input::{input_thread, ClientInput};

/// The Hello→Welcome→Start negotiation (plan §W1); `serve_session` calls `handshake::negotiate`
/// after the pairing gate.
mod handshake;

/// The mid-stream control task (plan §W1); `serve_session` spawns `control::run` after the
/// handshake to multiplex renegotiation / speed-test control messages onto the data-plane channels.
mod control;

/// The capture→encode→send data plane (plan §W1); `serve_session` dispatches the synthetic or
/// virtual source here (`synthetic_stream` / `virtual_stream`) and hands the latter a
/// `SessionContext`. `reconfig_allowed` gates mid-stream live reconfigure.
mod stream;
use stream::{reconfig_allowed, synthetic_stream, virtual_stream, SessionContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Punktfunk1Source {
    /// Deterministic test frames (protocol verification; the client byte-checks them).
    Synthetic,
    /// Real capture: virtual display at the client's requested mode → NVENC.
    Virtual,
}

pub struct Punktfunk1Options {
    pub port: u16,
    pub source: Punktfunk1Source,
    /// Virtual-source stream duration.
    pub seconds: u32,
    /// Synthetic-source frame count.
    pub frames: u32,
    /// Exit after this many sessions (0 = serve forever).
    pub max_sessions: u32,
    /// Maximum sessions streaming **at once** (a NVENC/GPU bound); further clients wait in the
    /// accept queue until a slot frees. Concurrent sessions each get their own virtual output +
    /// encoder but share the host-lifetime input/audio/mic services — i.e. multiple devices viewing
    /// (and controlling) the *same* desktop on the shared-desktop backends (kwin/mutter/wlroots).
    /// `0` = unlimited (bounded only by the GPU). Default a conservative few.
    pub max_concurrent: usize,
    /// Only serve clients whose certificate fingerprint is in the paired set. Implies
    /// `allow_pairing` (a host that requires pairing must accept ceremonies to admit
    /// anyone).
    pub require_pairing: bool,
    /// Accept pairing ceremonies (the operator "arming" pairing mode). Default off: a host
    /// with neither flag set rejects unsolicited PairRequests outright, closing that
    /// attack surface. `require_pairing` forces this on.
    pub allow_pairing: bool,
    /// Fixed pairing PIN (tests); `None` = a fresh random 4-digit PIN per ceremony.
    pub pairing_pin: Option<String>,
    /// Paired-clients store path override (tests); `None` = the default config path.
    pub paired_store: Option<std::path::PathBuf>,
    /// Fixed data-plane UDP port. `None`/`Some(0)` (default): bind a random ephemeral port and
    /// **hole-punch** — wait ~2.5 s for the client's punch, then fall back to its reported address
    /// (traverses NAT / a stateful inter-VLAN firewall with no forwarded port, at the cost of the
    /// punch-timeout on a firewall that drops the punch). `Some(p)`: bind that fixed port and
    /// stream **directly** to the client's reported address with no punch-wait — for a host whose
    /// data port is fixed + firewall-opened/forwarded, this removes the punch-timeout delay. A
    /// fixed port only fits one data plane at a time, so a concurrent session finding it busy
    /// falls back to random + hole-punch (see [`bind_data_socket`]).
    pub data_port: Option<u16>,
    /// Control-connection idle timeout — the **disconnect-detection latency** (how long a vanished
    /// client takes to be declared dead, which bounds how fast a dropped session tears down / lingers
    /// and thus the reconnect-overlap window). `None` = the core default (8s). Set from
    /// `PUNKTFUNK_IDLE_TIMEOUT_MS`; clamped to a ≥1s floor with a keep-alive that scales to it so a
    /// live session never false-closes.
    pub idle_timeout: Option<std::time::Duration>,
    /// Advertise this host over mDNS (`_punktfunk._udp`). Default on; `--no-mdns` /
    /// `PUNKTFUNK_MDNS=0` turns it off for multicast-dead environments (bridged Docker, CI netns)
    /// — clients then connect via `--connect HOST:PORT` / a manually-added host, which always works.
    pub mdns: bool,
}

/// Bind the per-session data-plane UDP socket, honoring [`Punktfunk1Options::data_port`]. Returns
/// `(socket, direct)`: `direct = true` (a successfully-bound fixed port) means "stream straight to
/// the client's reported address, no hole-punch"; `false` (random port, or a busy fixed port) means
/// "hole-punch". The socket is held from the handshake through streaming — no drop-then-rebind
/// window in which a concurrent session could steal a fixed port.
fn bind_data_socket(data_port: Option<u16>) -> std::io::Result<(std::net::UdpSocket, bool)> {
    if let Some(p) = data_port.filter(|p| *p != 0) {
        match std::net::UdpSocket::bind(("0.0.0.0", p)) {
            Ok(sock) => return Ok((sock, true)),
            Err(e) => tracing::warn!(
                data_port = p,
                error = %e,
                "fixed --data-port is busy (a concurrent session already holds it?) — \
                 falling back to a random port + hole-punch for this session"
            ),
        }
    }
    Ok((std::net::UdpSocket::bind("0.0.0.0:0")?, false))
}

/// The native (punktfunk/1) trust store + on-demand arming PIN, shared with the management API.
use crate::native_pairing::{NativePairing, PairingDecision};
use crate::send_pacing::{percentile, PaceStat};
/// The shared streaming-stats recorder (web-console capture/graph), shared with the management API
/// and the GameStream loop; threaded into each session's `SessionContext`.
use crate::stats_recorder::StatsRecorder;

/// Minimum spacing between accepted pairing ceremonies (bounds online PIN guessing — with
/// SPAKE2 an attacker already gets only one guess per ceremony; this caps the rate).
const PAIRING_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);

/// Deterministic test frame: `u32 LE index` then `data[i] = idx + i` (wrapping).
pub fn test_frame(idx: u32, len: usize) -> Vec<u8> {
    let mut d = vec![0u8; len];
    d[0..4].copy_from_slice(&idx.to_le_bytes());
    for (i, b) in d.iter_mut().enumerate().skip(4) {
        *b = (idx as u8).wrapping_add(i as u8);
    }
    d
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn run(opts: Punktfunk1Options) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("tokio runtime")?;
    // Standalone CLI: arm at startup iff --allow-pairing/--require-pairing (back-compat — the PIN
    // is logged). The unified `serve --native` path instead arms on demand via the management API.
    let np = Arc::new(NativePairing::load_with(
        opts.paired_store.clone(),
        opts.pairing_pin.clone(),
        opts.allow_pairing || opts.require_pairing,
    )?);
    // Standalone `punktfunk1-host` has no mgmt API to arm capture, so this recorder stays disarmed
    // (harmless — the loops' `is_armed()` gate is always false). The unified `serve` shares one
    // recorder across mgmt + both streaming paths instead.
    let stats = StatsRecorder::new(crate::stats_recorder::default_dir());
    // Standalone `punktfunk1-host` runs no management API, so advertise no `mgmt` port (0).
    rt.block_on(serve(opts, 0, np, stats))
}

fn fingerprint_hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

/// The persistent listener: accept clients back to back on one endpoint. Sessions are
/// served one at a time (the virtual output + NVENC are single-tenant); a client that
/// connects mid-session waits in the accept queue. A failed session logs and the loop
/// keeps serving — only endpoint-level failures are fatal.
/// Config for the native (punktfunk/1) host when the unified `serve` runs it in-process.
pub(crate) struct NativeServe {
    pub port: u16,
    /// Gate sessions on pairing. **Default on** — an open host any LAN device can stream from is
    /// insecure; `serve --open` turns it off (trusted single-user setups). Pairing is armed on
    /// demand from the web console (arm → PIN); paired devices persist.
    pub require_pairing: bool,
    /// The management API's TCP port, advertised over mDNS so a client browses the game library on
    /// the same host IP (the unified `serve` always runs the mgmt API, so this is its bind port).
    pub mgmt_port: u16,
    /// Fixed data-plane UDP port (`--data-port` / `PUNKTFUNK_DATA_PORT`); see
    /// [`Punktfunk1Options::data_port`]. `None` = random port + hole-punch (the default).
    pub data_port: Option<u16>,
    /// Advertise over mDNS (`--no-mdns` / `PUNKTFUNK_MDNS=0` turns it off). Gates the native
    /// `_punktfunk._udp` advert AND the GameStream `_nvstream` advert — the serve-level knob for
    /// multicast-dead environments; see [`Punktfunk1Options::mdns`].
    pub mdns: bool,
}

/// Options for the native host when the unified `serve --native` runs it: real virtual capture,
/// persistent (no session/duration cut), pairing armed on demand via the management API (the
/// shared [`NativePairing`] starts disarmed).
/// Default cap on simultaneously-streaming sessions (each holds an NVENC session; high-res
/// split-encode holds two). Conservative — consumer NVENC historically capped concurrent sessions;
/// overflow clients wait in the accept queue. Override with `--max-concurrent`.
pub(crate) const DEFAULT_MAX_CONCURRENT: usize = 4;

/// The control-connection idle timeout (disconnect-detection latency) from
/// `PUNKTFUNK_IDLE_TIMEOUT_MS`; `None` (unset/invalid/zero) = the core default (8s). Clamped
/// downstream to a ≥1s floor with a keep-alive that scales to it, so a live session never false-closes.
pub(crate) fn idle_timeout_from_env() -> Option<std::time::Duration> {
    std::env::var("PUNKTFUNK_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(std::time::Duration::from_millis)
}

pub(crate) fn native_serve_opts(cfg: &NativeServe) -> Punktfunk1Options {
    Punktfunk1Options {
        port: cfg.port,
        source: Punktfunk1Source::Virtual,
        seconds: 7 * 24 * 3600, // per-session cap; large enough not to cut a live stream
        frames: 0,
        max_sessions: 0,
        max_concurrent: DEFAULT_MAX_CONCURRENT,
        require_pairing: cfg.require_pairing,
        allow_pairing: false,
        pairing_pin: None,
        paired_store: None,
        data_port: cfg.data_port,
        idle_timeout: idle_timeout_from_env(),
        mdns: cfg.mdns,
    }
}

pub(crate) async fn serve(
    opts: Punktfunk1Options,
    mgmt_port: u16,
    np: Arc<NativePairing>,
    stats: Arc<StatsRecorder>,
) -> Result<()> {
    let identity = crate::gamestream::cert::ServerIdentity::load_or_create()
        .context("load host identity (~/.config/punktfunk)")?;
    let fingerprint = endpoint::fingerprint_of_pem(&identity.cert_pem)
        .map_err(|e| anyhow!("cert fingerprint: {e}"))?;
    let ep = endpoint::server_with_identity_idle(
        ([0, 0, 0, 0], opts.port).into(),
        &identity.cert_pem,
        &identity.key_pem,
        opts.idle_timeout.unwrap_or(endpoint::DEFAULT_IDLE_TIMEOUT),
    )
    .map_err(|e| anyhow!("QUIC server endpoint: {e}"))?;
    tracing::info!(
        port = opts.port,
        source = ?opts.source,
        fingerprint = %fingerprint_hex(&fingerprint),
        "punktfunk/1 host listening (QUIC) — clients pin this fingerprint"
    );

    // mDNS: advertise the native service so clients auto-discover this host (the analogue of the
    // GameStream _nvstream advert; both run in the unified host). Held for the host's lifetime —
    // dropping `_advert` unregisters. Best-effort: a discovery failure must not stop streaming
    // (manual `--connect HOST:PORT` always works), so we log and continue.
    let _advert = if !opts.mdns {
        tracing::info!(
            "mDNS advertisement disabled (--no-mdns / PUNKTFUNK_MDNS) — clients connect by address"
        );
        None
    } else {
        match crate::gamestream::Host::detect() {
        Ok(h) => crate::discovery::advertise_native(
            &h.hostname,
            h.local_ip,
            opts.port,
            &fingerprint_hex(&fingerprint),
            opts.require_pairing,
            &h.uniqueid,
            // 0 = standalone `punktfunk1-host` (no mgmt API) → don't advertise an `mgmt` port.
            (mgmt_port != 0).then_some(mgmt_port),
        )
        .map_err(|e| tracing::warn!(error = %format!("{e:#}"), "native mDNS advertise failed (continuing)"))
        .ok(),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "host detect for mDNS failed (continuing)");
            None
        }
        }
    };

    // One audio capturer for the whole host lifetime, handed from session to session
    // (avoids a PipeWire stream setup per session — see AudioCapSlot).
    let audio_cap: AudioCapSlot = Arc::new(std::sync::Mutex::new(None));
    // One pointer/keyboard injector for the whole host lifetime (see InjectorService): the
    // RemoteDesktop-portal grant is established ONCE and reused, instead of a CreateSession per
    // session — which, under rapid client reconnects, raced a prior session's portal teardown and
    // wedged KWin's EIS setup ("EIS setup timed out"). Gamepads stay per-session (uinput).
    let injector = crate::inject::InjectorService::start();
    // One virtual microphone for the whole host lifetime (see [`crate::audio::MicPump`]): the
    // client's mic uplink (0xCB) is Opus-decoded and fed into a persistent virtual mic host apps
    // record from (Linux PipeWire Audio/Source; Windows a virtual audio device's render endpoint).
    // The pump opens the backend EAGERLY (the mic device exists before any game launches and
    // binds its capture device) and self-heals when the backend dies (PipeWire restart, Windows
    // endpoint churn).
    let mic_service = crate::audio::MicPump::start();
    // Host-lifetime worker that fires debounced TV-session restores (the managed gamescope path
    // restores the box's autologin gaming session on idle, not per-disconnect — see
    // `vdisplay::restore_managed_session`). Held for serve()'s lifetime; dropping it stops it.
    let _restore_worker = crate::vdisplay::start_restore_worker();
    // A3: recover a TV takeover stranded by a crashed previous host instance (persisted to
    // $XDG_RUNTIME_DIR) — schedule a restore after a reconnect grace. No-op on a clean start.
    crate::vdisplay::restore_takeover_on_startup();
    // Host-lifetime cover-art warmer: fetches + caches GOG/Xbox cover art (no-auth api.gog.com /
    // displaycatalog) off the hot path so `all_games()` (the library list + launch resolve) never
    // blocks on the network. A no-op on a host whose stores all carry their own art.
    let _art_warmer = crate::library::start_art_warmer();
    // Pairing state (arming PIN + trust store) is shared with the management API. If it was armed
    // at startup (the CLI flags), surface the PIN the headless operator reads from the log; the
    // web console arms it on demand instead (a fresh, time-limited PIN).
    let st = np.status();
    if let Some(pin) = &st.pin {
        tracing::info!(
            paired = st.paired_clients,
            require = opts.require_pairing,
            "pairing armed — enter the PIN shown on the console to pair a client"
        );
        // The PIN is a shared secret: print it straight to the operator's terminal, NOT through
        // tracing. A tracing event also lands in the DEBUG log ring that field bug reports ship
        // (GET /api/v1/logs), which must never carry the pairing secret.
        eprintln!("[punktfunk] pairing PIN: {pin}  (enter this on the client to pair)");
    }
    let last_pairing = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
    let opts = Arc::new(opts);

    // Concurrency: serve up to `max_concurrent` sessions at once. Each gets its own virtual output +
    // NVENC encoder; they share the host-lifetime input/audio/mic services — i.e. multiple devices
    // viewing (and controlling) the SAME desktop on the shared-desktop backends. A permit is taken
    // before accepting, so overflow clients wait in QUIC's accept backlog until a slot frees;
    // `max_concurrent == 0` means unlimited (GPU-bounded). The heavy handshake + pipeline run inside
    // the spawned task, so a slow client never blocks the accept loop.
    let permits = match opts.max_concurrent {
        0 => tokio::sync::Semaphore::MAX_PERMITS,
        n => n,
    };
    let sem = Arc::new(tokio::sync::Semaphore::new(permits));
    let mut sessions = tokio::task::JoinSet::new();
    let max_sessions = opts.max_sessions;
    let mut accepted = 0u32;
    tracing::info!(
        max_concurrent = opts.max_concurrent,
        "accepting sessions (concurrent)"
    );

    loop {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .expect("session semaphore is never closed");
        let incoming = match ep.accept().await {
            Some(i) => i,
            None => break, // endpoint closed
        };
        // Complete the QUIC handshake in the accept loop (it's ~1 RTT): a failed handshake (e.g. a
        // pin mismatch — the client aborts) must NOT consume a session slot, mirroring the old
        // serial loop. The slow part (control handshake, pairing, the capture/encode pipeline) runs
        // in the spawned task, so a slow client still never blocks accepting the next one.
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "QUIC accept failed");
                continue; // `permit` drops here → slot freed; not counted toward max_sessions
            }
        };
        let peer = conn.remote_address();
        tracing::info!(%peer, "punktfunk/1 client connected");
        let opts = opts.clone();
        let audio_cap = audio_cap.clone();
        let np = np.clone();
        let last_pairing = last_pairing.clone();
        let stats = stats.clone();
        let inj_tx = injector.sender();
        let mic_tx = mic_service.sender();
        // The session permit + the pool it came from are handed to serve_session, which owns the
        // permit's lifetime: it's released while a knock is parked for delegated approval and
        // re-acquired on approval, so the hold is no longer a simple closure-scoped binding.
        let sem_session = sem.clone();
        sessions.spawn(async move {
            match serve_session(
                conn,
                &opts,
                &audio_cap,
                inj_tx,
                mic_tx,
                &fingerprint,
                &np,
                &last_pairing,
                stats,
                permit,
                sem_session,
            )
            .await
            {
                Ok(()) => tracing::info!(%peer, "session complete"),
                Err(e) => {
                    tracing::warn!(%peer, error = %format!("{e:#}"), "session ended with error")
                }
            }
        });
        accepted += 1;
        if max_sessions != 0 && accepted >= max_sessions {
            break;
        }
    }
    // Stop accepting; let the in-flight sessions finish (max_sessions reached or endpoint closed).
    while sessions.join_next().await.is_some() {}
    ep.wait_idle().await;
    Ok(())
}

/// The accept loop is sequential, so the control phase must be bounded — a client that
/// connects and never finishes the handshake would otherwise wedge the host for everyone.
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// QUIC application error code the host closes with on a `mode_conflict = reject` admission refusal,
/// carrying the human-readable busy reason (live mode + client label) the client surfaces. A distinct
/// code lets a client tell "host busy" apart from a transport failure. Shared with the clients via
/// `punktfunk_core::reject` so they can decode it (`RejectReason::Busy`).
const REJECT_BUSY_CODE: u32 = punktfunk_core::reject::REJECT_BUSY_CLOSE_CODE;

/// Make a gate rejection legible to the client BEFORE erroring out of the session task: close
/// with the typed application code (`punktfunk_core::reject`) so the client renders the real
/// reason ("pairing not armed", "denied in the console") — the task's `Err` then only logs.
/// Without this the dropped connection closes with a bare code 0, indistinguishable on the
/// client from transport trouble (the "not accepted" support-thread failure mode).
fn close_rejected(conn: &quinn::Connection, reason: punktfunk_core::reject::RejectReason) {
    conn.close(reason.close_code().into(), reason.to_string().as_bytes());
}

/// QUIC application error code a client closes with on a **deliberate quit** (a user "stop", not a
/// network drop). The host reads it off the connection's `ApplicationClosed` reason and tears the
/// session's virtual display down IMMEDIATELY, skipping the keep-alive linger — an unwanted disconnect
/// (idle timeout / reset / any other code) still lingers so a reconnect can resume. Shared with the
/// clients via `punktfunk_core::quic::QUIT_CLOSE_CODE`.
const QUIT_CODE: u32 = punktfunk_core::quic::QUIT_CLOSE_CODE;

/// Encoder bitrate (kbps) the host falls back to when the client expresses no preference
/// (`Hello::bitrate_kbps == 0`) — the long-standing 20 Mbps default. A client that knows its
/// link (e.g. after a speed test) requests an explicit rate instead.
const DEFAULT_BITRATE_KBPS: u32 = 20_000;
/// Bounds a client's requested bitrate before configuring NVENC: a 500 kbps floor keeps the stream
/// above unusable, and a **2 Gbps** ceiling is generous headroom over the 1 Gbps+ target that
/// GF(2¹⁶) Leopard FEC was built to reach — it lifts the GF(2⁸)/~1 Gbps wall, and at 1 Gbps a frame
/// is only a few-hundred shards in one block (far under the 65535 limit). Enough for 5K@240 with
/// margin. Resolved value is echoed in `Welcome::bitrate_kbps`. The native data plane batches sends
/// (`sendmmsg`) and paces each frame on a dedicated send thread (microburst cap), validated to a
/// clean 1 Gbps with zero send-buffer drops; sustained overruns are still counted as
/// `packets_send_dropped`.
const MIN_BITRATE_KBPS: u32 = 500;
// 8 Gbps ceiling — headroom for a 2.5 Gbps link and the 5 Gbps path (home-worker-3 → Mac Studio,
// Mac is 10G). The encoder is pixel-rate bound, not bitrate bound (NVENC emits multi-Gbps trivially;
// ~1 Gpix/s per engine, ~2 with the auto 2-way split), so the real ceiling is the transport send
// path (UDP GSO + per-packet alloc removal), not this number.
const MAX_BITRATE_KBPS: u32 = 8_000_000;

/// Resolve a client's [`Hello::bitrate_kbps`] request to the rate the host will configure:
/// `0` → host default; anything else clamped into `[MIN, MAX]`.
fn resolve_bitrate_kbps(requested: u32) -> u32 {
    if requested == 0 {
        DEFAULT_BITRATE_KBPS
    } else {
        requested.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
    }
}

/// [`resolve_bitrate_kbps`] with the codec's floor semantics: PyroWave has no useful
/// low-rate regime (wavelet quality collapses far above the H.26x floor — plan §4.6), so
/// an Automatic client (`0`) gets the codec's ~1.6 bpp operating point for the negotiated
/// mode instead of the 20 Mbps H.26x default. The rate is then PINNED for the session:
/// the client's ABR controller stays off for this codec and the host refuses mid-stream
/// retargets. An explicit client rate is honored unchanged (the operator knows the link).
fn resolve_bitrate_kbps_for(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
) -> u32 {
    if requested == 0 && codec == crate::encode::Codec::PyroWave {
        // ~1.6 bpp for 4:2:0. 4:4:4 doubles the samples per pixel (3 vs 1.5) but chroma
        // compresses better than luma → ×1.625 ≈ 2.6 bpp; 16-bit planes add ~15 % (both
        // factors measured against the Phase-0 fixture matrix, design/pyrowave-444-hdr.md).
        let bpp_x10: u64 = if chroma.is_444() { 26 } else { 16 };
        let mut bps =
            mode.width as u64 * mode.height as u64 * u64::from(mode.refresh_hz.max(1)) * bpp_x10
                / 10;
        if bit_depth >= 10 {
            bps = bps * 115 / 100;
        }
        let pin = u32::try_from(bps / 1000)
            .unwrap_or(MAX_BITRATE_KBPS)
            .clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        // Operator link ceiling. PyroWave's Automatic pin is open-loop (all-intra, so ABR and the
        // capacity probe are off) — at a high pixel rate it can outrun the physical link (e.g.
        // 4:4:4 + HDR at 5120x1440@240 pins ~5.3 Gbps, over a 5 GbE link), and the overshoot just
        // becomes packet loss / partial frames. `PUNKTFUNK_PYROWAVE_MAX_MBPS` lets a host on a
        // constrained link cap the pin to what the fabric carries; unset ⇒ no cap (unchanged).
        if let Some(ceiling) = pyrowave_auto_pin_ceiling_kbps() {
            if pin > ceiling {
                tracing::warn!(
                    pin_kbps = pin,
                    ceiling_kbps = ceiling,
                    "PyroWave Automatic bitrate pin exceeds PUNKTFUNK_PYROWAVE_MAX_MBPS — capping \
                     to the link ceiling (set an explicit client bitrate to choose your own)"
                );
                return ceiling.max(MIN_BITRATE_KBPS);
            }
        }
        return pin;
    }
    resolve_bitrate_kbps(requested)
}

/// Operator ceiling for PyroWave's open-loop Automatic bitrate pin: `PUNKTFUNK_PYROWAVE_MAX_MBPS`
/// (megabits/s) → kbps, or `None` when unset/zero/invalid (no cap — the raw bpp pin stands).
/// Only consulted for `requested == 0` PyroWave sessions; an explicit client bitrate bypasses it.
fn pyrowave_auto_pin_ceiling_kbps() -> Option<u32> {
    std::env::var("PUNKTFUNK_PYROWAVE_MAX_MBPS")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&m| m > 0)
        .map(|m| m.saturating_mul(1000))
}

/// Resolve the audio channel count the session will capture + encode from the client's request.
/// Normalizes to one of 2 (stereo) / 6 (5.1) / 8 (7.1); anything else (older client, garbage)
/// becomes stereo. Both backends can produce the requested count (PipeWire pads/upmixes positions,
/// WASAPI loopback up/downmixes via AUTOCONVERTPCM), so no capability clamp is needed here — the
/// surround channels just carry up/downmixed content when the host's sink has fewer real channels.
fn resolve_audio_channels(requested: u8) -> u8 {
    punktfunk_core::audio::normalize_channels(requested)
}

/// Static FEC override: `PUNKTFUNK_FEC_PCT`, when set, PINS the recovery percent and DISABLES
/// adaptive FEC — so a speed test / measurement keeps a fixed, known overhead. `None` ⇒ adaptive
/// FEC (the host sizes recovery to the loss the client reports). `0` disables FEC entirely.
/// Clamped to ≤ 90.
fn fec_static_override() -> Option<u8> {
    std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .map(|p| p.min(90))
}

/// Adaptive-FEC band + starting point. Every recovery shard is extra wire bytes AND an extra
/// packet, so on a clean link FEC decays toward [`FEC_MIN`] (fewer packets — the win for a
/// packet-rate-bound uplink like the Steam Deck's WiFi tx); loss ramps it toward [`FEC_MAX`].
/// Sessions start moderate so the first frames (before any loss report) are protected.
const FEC_MIN: u8 = 1;
const FEC_MAX: u8 = 50;
const FEC_ADAPTIVE_START: u8 = 10;

/// Map the client's reported data-plane loss (ppm of shards, see [`LossReport`]) to a recovery
/// percentage. FEC must EXCEED the loss rate to recover a block, so target ≈ loss × 1.4 + 1 pt of
/// margin, clamped to the band. A clean link (≈0 ppm) lands on [`FEC_MIN`].
fn adapt_fec(loss_ppm: u32) -> u8 {
    let loss_pct = loss_ppm as f64 / 10_000.0; // ppm → percent
    let target = (loss_pct * 1.4).ceil() as u32 + 1;
    target.clamp(FEC_MIN as u32, FEC_MAX as u32) as u8
}

/// Apply the latest adaptive-FEC target to the session if it changed (cheap relaxed load + compare),
/// called once per frame on the data-plane send path.
fn apply_fec_target(session: &mut Session, fec_target: &AtomicU8) {
    let t = fec_target.load(Ordering::Relaxed);
    if session.fec_percent() != t {
        session.set_fec_percent(t);
    }
}

/// Persistent audio-capturer slot, reused across sessions (same pattern as the GameStream
/// path): keeps one warm PipeWire capture stream instead of a connect/negotiate cycle —
/// and a daemon-side node churn — per session. (Drop now tears a capturer down cleanly.)
type AudioCapSlot = Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>;

/// How long the host keeps an unpaired knock PARKED — connection held open — waiting for the
/// operator to click Approve in the console (delegated approval, roadmap §8b-1). The QUIC
/// keep-alive (4 s, under the 8 s idle timeout) holds the path warm meanwhile, so on approval the
/// device pairs and streams with NO reconnect. Bounded well under the pending entry's TTL (10 min);
/// the client uses a comparable connect timeout, and a client that gives up first closes the
/// connection (the host stops waiting at once).
const PENDING_APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_secs(180);

/// One client session: handshake → input/audio planes → data plane until done/disconnect.
/// Everything torn down on return (RAII: virtual output, encoder, threads via channel close).
/// A connection whose first message is a PairRequest runs the pairing ceremony instead.
// Each argument is a distinct host-lifetime handle threaded from `serve` (config, the audio +
// injector services, the trust store, pairing state) — bundling them into a context struct would
// obscure more than it'd save.
#[allow(clippy::too_many_arguments)]
async fn serve_session(
    conn: quinn::Connection,
    opts: &Punktfunk1Options,
    audio_cap: &AudioCapSlot,
    inj_tx: std::sync::mpsc::Sender<InputEvent>,
    mic_tx: std::sync::mpsc::SyncSender<Vec<u8>>,
    host_fp: &[u8; 32],
    np: &NativePairing,
    last_pairing: &std::sync::Mutex<Option<std::time::Instant>>,
    stats: Arc<StatsRecorder>,
    // The session slot. Owned here (not just held by the spawning task) because an unpaired knock
    // RELEASES it while parked for delegated approval, then RE-ACQUIRES one on approval — so a
    // parked knock can't hold a streaming slot. `sem` is the pool it re-acquires from.
    mut permit: tokio::sync::OwnedSemaphorePermit,
    sem: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let peer = conn.remote_address();

    // First message decides what this connection is: a pairing ceremony or a session.
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| anyhow!("control stream timeout"))?
        .context("accept control stream")?;
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("first message timeout"))??;
    if let Ok(req) = PairRequest::decode(&first) {
        // The client fingerprint (cert possession is proven by the QUIC handshake) is needed to honor
        // a fingerprint-bound PIN window (#9): a window the operator armed for a SPECIFIC device must
        // not be consumable — or burnable — by any other fingerprint.
        let Some(client_fp) = endpoint::peer_fingerprint(&conn) else {
            close_rejected(
                &conn,
                punktfunk_core::reject::RejectReason::IdentityRequired,
            );
            anyhow::bail!("pairing requires the client to present a certificate");
        };
        let client_fp_hex = fingerprint_hex(&client_fp);
        // Resolve the live arming PIN per attempt (so a lapsed window no longer pairs), honoring any
        // fingerprint binding.
        let pin = match np.pin_for_attempt(&client_fp_hex) {
            crate::native_pairing::PinAttempt::Pin(pin) => pin,
            crate::native_pairing::PinAttempt::Disarmed => {
                close_rejected(&conn, punktfunk_core::reject::RejectReason::PairingNotArmed);
                anyhow::bail!(
                    "pairing not armed (arm it in the console, or start with --allow-pairing)"
                )
            }
            // Armed for a DIFFERENT device — reject without running the ceremony, so this attempt does
            // NOT consume (burn) the operator's window for the device they actually selected (#9).
            crate::native_pairing::PinAttempt::BoundToOther => {
                close_rejected(
                    &conn,
                    punktfunk_core::reject::RejectReason::PairingBoundToOtherDevice,
                );
                anyhow::bail!(
                    "pairing is armed for a different device — this attempt does not consume the window"
                )
            }
        };
        {
            let mut last = last_pairing.lock().unwrap();
            if let Some(t) = *last {
                if t.elapsed() < PAIRING_COOLDOWN {
                    close_rejected(
                        &conn,
                        punktfunk_core::reject::RejectReason::PairingRateLimited,
                    );
                    anyhow::bail!("pairing rate-limited — retry shortly");
                }
            }
            *last = Some(std::time::Instant::now());
        }
        return pair_ceremony(&conn, send, recv, req, host_fp, np, &pin).await;
    }

    // Pairing gate for a session Hello (a PairRequest was handled above). Lifted OUT of the
    // `handshake` future below for two reasons: (1) the approval wait must not be bound by the
    // short HANDSHAKE_TIMEOUT — a human reads the console and clicks Approve; (2) the NVENC session
    // permit is released while parked, so a knock awaiting approval can't hold a streaming slot.
    // On approval the device is now paired, so the handshake proceeds and the session starts with
    // NO client reconnect (delegated approval, roadmap §8b-1).
    if opts.require_pairing {
        // Decode just enough to gate (the Hello carries the device name for the pending label);
        // the `handshake` future re-decodes for the real session — a few dozen bytes, negligible.
        let gate_hello = Hello::decode(&first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
        if gate_hello.abi_version != punktfunk_core::WIRE_VERSION {
            close_rejected(
                &conn,
                punktfunk_core::reject::RejectReason::WireVersionMismatch,
            );
            anyhow::bail!(
                "wire version mismatch: client {} host {}",
                gate_hello.abi_version,
                punktfunk_core::WIRE_VERSION
            );
        }
        let fp = endpoint::peer_fingerprint(&conn);
        let known = fp
            .as_ref()
            .map(|fp| np.is_paired(&fingerprint_hex(fp)))
            .unwrap_or(false);
        if !known {
            // An anonymous client (no certificate) has no identity to approve — reject outright
            // (the PIN ceremony is its way in). Mirrors the prior behavior for anonymous knocks.
            let Some(fp) = fp else {
                close_rejected(
                    &conn,
                    punktfunk_core::reject::RejectReason::IdentityRequired,
                );
                anyhow::bail!(
                    "unpaired anonymous client rejected (this host requires pairing — present a \
                     client identity and approve it in the console, or run the PIN ceremony)"
                );
            };
            let fp_hex = fingerprint_hex(&fp);
            // Sanitize the wire-supplied name before it reaches the log / console (untrusted: an
            // unpaired device could embed terminal escapes / bidi overrides); note_pending stores
            // the same sanitized form and derives a fingerprint label when empty.
            let label = crate::native_pairing::sanitize_device_name(
                gate_hello.name.as_deref().unwrap_or(""),
                &fp_hex,
            );
            tracing::info!(name = %label, fingerprint = %fp_hex,
                "unpaired device knocked — parking connection for delegated approval in the console");
            // Record the QUIC-validated source IP so the pending queue's per-source cap can stop one
            // host from flooding/evicting genuine knocks (#13). The returned knock generation makes
            // this connection the ONE an approval admits — a retrying client parks a fresh
            // connection per knock, and admitting every parked sibling on a single Approve spun up
            // three concurrent Mutter virtual monitors and segfaulted gnome-shell (2026-07-10).
            let knock_seq = np.note_pending(&label, &fp_hex, Some(peer.ip()));
            // Free the session slot while a human decides — a parked knock must not hold an NVENC
            // permit (a handful of parked knocks would otherwise block every real session).
            drop(permit);
            let decision = tokio::select! {
                d = np.wait_for_decision(&fp_hex, knock_seq, PENDING_APPROVAL_WAIT) => d,
                // The client gave up (closed the connection) before a decision — stop waiting.
                _ = conn.closed() => anyhow::bail!("client disconnected before pairing approval"),
            };
            match decision {
                PairingDecision::Approved => {
                    tracing::info!(name = %label, fingerprint = %fp_hex,
                        "device approved in console — admitting session (no reconnect)");
                }
                PairingDecision::Denied => {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::Denied);
                    anyhow::bail!("pairing request denied in the console")
                }
                PairingDecision::TimedOut => {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::ApprovalTimeout);
                    anyhow::bail!(
                        "pairing request not approved within {PENDING_APPROVAL_WAIT:?} \
                         — the device can knock again"
                    )
                }
                PairingDecision::Superseded => {
                    close_rejected(&conn, punktfunk_core::reject::RejectReason::Superseded);
                    anyhow::bail!(
                        "parked knock superseded by a newer connection from the same device — \
                         only the newest is admitted on approval"
                    )
                }
            }
            // Re-acquire a session slot for the now-approved session (waits if all slots are busy,
            // exactly like any freshly accepted client).
            permit = sem
                .clone()
                .acquire_owned()
                .await
                .expect("session semaphore is never closed");
        }
    }
    // Held for the rest of the session (RAII frees the slot on return). For an already-paired
    // client this is the original permit; for a just-approved knock it's the re-acquired one.
    let _permit = permit;

    let source = opts.source;
    let frames = opts.frames;
    let data_port = opts.data_port;
    // Session-transition trace (latency plan P0.1): zeroed here — the Hello is in hand, pairing
    // gates are behind us — and finished by the send thread when the FIRST video packet leaves.
    // The completed totals surface per session in `session_status` (→ mgmt `/status`).
    let bringup = crate::bringup::Trace::start("bringup", Arc::new(AtomicU32::new(0)));
    // The mid-stream resize counterpart: each accepted Reconfigure runs its own trace into this
    // shared slot (latest wins), registered alongside the bring-up total.
    let resize_ms: Arc<AtomicU32> = Arc::new(AtomicU32::new(0));

    // Stop signal: stream duration elapsed or the client went away. Created (with its watcher)
    // BEFORE the handshake so the Welcome-time display prep can already observe a client that
    // vanished mid-handshake (its build-retry loop aborts on `stop`).
    let stop = Arc::new(AtomicBool::new(false));
    // Deliberate-quit signal: set (before `stop`, so the display lease reads it on teardown) when
    // the client closed the connection with `QUIT_CODE` — a user "stop", which skips the
    // keep-alive linger. A bare disconnect / idle timeout leaves it false → the display lingers
    // for a reconnect.
    let quit = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        let quit = quit.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            let reason = conn.closed().await;
            if matches!(&reason, quinn::ConnectionError::ApplicationClosed(ac)
                if ac.error_code == quinn::VarInt::from_u32(QUIT_CODE))
            {
                quit.store(true, Ordering::SeqCst);
            }
            stop.store(true, Ordering::SeqCst);
        });
    }

    let (hello, welcome, udp_port, data_sock, direct, start, compositor, prep) =
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            handshake::negotiate(
                &conn,
                &mut send,
                &mut recv,
                &first,
                source,
                frames,
                data_port,
                &bringup,
                quit.clone(),
                stop.clone(),
            ),
        )
        .await
        .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (ctrl_send, ctrl_recv) = (send, recv);
    // Can this session's backend live-reconfigure (mid-stream Reconfigure)? Gated OFF for:
    //   * gamescope (all sub-modes): a spawn respawn restarts the game, managed restarts the box's
    //     game-mode session, attach doesn't own the display — a resize must never relaunch the title
    //     (design/midstream-resolution-resize.md H1/D3). The client keeps scaling client-side.
    //   * an `identity: per-client-mode` policy: the mode is part of the display-identity slot key,
    //     so a resize would resolve a DIFFERENT slot — on Windows a fresh monitor ADD instead of the
    //     in-place reconfigure, on KWin a differently-named output — defeating the policy's
    //     per-resolution identity. Honest downgrade: reject, client scales (H5).
    // The SYNTHETIC source stays reconfigurable on purpose (nothing to rebuild — the ack round-trip
    // is the whole effect): it is the compositor-free protocol test source, and the C-ABI roundtrip
    // test + client harnesses exercise the Reconfigure/Reconfigured plumbing through it.
    // Captured once at session setup; the control task answers `accepted: false` when gated.
    let live_reconfig_ok = {
        let per_client_mode_identity = crate::vdisplay::policy::prefs()
            .configured_effective()
            .is_some_and(|e| e.identity == crate::vdisplay::policy::Identity::PerClientMode);
        reconfig_allowed(compositor, per_client_mode_identity)
    };
    // Negotiated codec (HEVC / H.264 / AV1), derived from the Welcome. `Copy`, so the control task's
    // `async move` captures a copy and it stays usable for the data-plane SessionContext below.
    let codec = crate::encode::Codec::from_wire(welcome.codec);
    let client_udp = std::net::SocketAddr::new(peer.ip(), start.client_udp_port);
    tracing::info!(
        %client_udp,
        udp_port,
        mode = ?hello.mode,
        compositor = compositor.map(|c| c.id()).unwrap_or("none"),
        gamepad = welcome.gamepad.as_str(),
        "handshake complete — streaming"
    );

    // Control task: the handshake stream stays open for mid-stream renegotiation and speed
    // tests. A validated Reconfigure is acked, then handed to the data-plane thread, which
    // rebuilds capture/encoder/virtual output at the new mode (the data plane itself is
    // untouched). A ProbeRequest is handed to the data plane, which bursts FLAG_PROBE filler and
    // hands back a ProbeResult that this task writes to the client. The two control directions
    // (inbound requests, outbound probe results) are multiplexed with `select!`.
    let (reconfig_tx, reconfig_rx) = std::sync::mpsc::channel::<punktfunk_core::Mode>();
    let (keyframe_tx, keyframe_rx) = std::sync::mpsc::channel::<()>();
    // Client LTR-RFI recovery: the control task forwards each `RfiRequest`'s lost-frame range here;
    // the encode loop prefers `Encoder::invalidate_ref_frames` (a clean re-anchor P-frame) over a
    // full IDR when the encoder supports it (native-AMF LTR / Windows NVENC).
    let (rfi_tx, rfi_rx) = std::sync::mpsc::channel::<(u32, u32)>();
    let (bitrate_tx, bitrate_rx) = std::sync::mpsc::channel::<u32>();
    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<ProbeRequest>();
    let (probe_result_tx, probe_result_rx) = tokio::sync::mpsc::unbounded_channel::<ProbeResult>();
    // Mode-switch outcome, data plane → control task (same pattern as `probe_result_tx`): the accept
    // ack is written BEFORE the rebuild, so a failed rebuild (host stays at the old mode) or a
    // backend that honored a different refresh must CORRECT the client's mode slot with a second
    // `Reconfigured { accepted: true, mode: <actually live> }` — the client handler treats any
    // accepted ack as "the active mode is now X" and fixes itself; old clients just log it.
    let (reconfig_result_tx, reconfig_result_rx) =
        tokio::sync::mpsc::unbounded_channel::<Reconfigured>();
    // Adaptive FEC: the control task maps each client LossReport to a recovery percent and publishes
    // it here; the data-plane send loop reads + applies it per frame. Disabled (pinned) when
    // PUNKTFUNK_FEC_PCT is set. Seeded with the session's starting FEC so it's a no-op until a report.
    let adaptive_fec = fec_static_override().is_none();
    let fec_target = Arc::new(AtomicU8::new(welcome.fec.fec_percent));
    let fec_target_ctl = fec_target.clone();
    // The session's negotiated rate — the pin PyroWave retarget-refusals ack (§4.6).
    let session_bitrate_kbps = welcome.bitrate_kbps;
    // Shared-clipboard enable state (client `ClipControl` → host). The coordinator reads it to
    // decide whether to forward host copies; the control task flips it on each `ClipControl`.
    let clip_enabled = Arc::new(AtomicBool::new(false));
    // Start the host clipboard coordinator. On success it watches the session clipboard, forwards
    // host copies as `ClipOffer`s (`clip.offer_rx` → control task → client), installs client
    // offers as a lazy source, and owns the fetch-stream accept loop. `available` is false when
    // there's no backend (gamescope / older GNOME / an unsupported platform) — the control task
    // then answers `ClipControl` with `BACKEND_UNAVAILABLE` and the decline loop below handles
    // stray fetch streams.
    let clip = pf_clipboard::start(conn.clone(), clip_enabled.clone(), compositor.is_some()).await;
    let clip_available = clip.available;
    tokio::spawn(control::run(
        ctrl_send,
        ctrl_recv,
        hello.mode,
        codec,
        live_reconfig_ok,
        adaptive_fec,
        session_bitrate_kbps,
        fec_target_ctl,
        reconfig_tx,
        keyframe_tx,
        rfi_tx,
        bitrate_tx,
        probe_tx,
        probe_result_rx,
        reconfig_result_rx,
        clip_enabled,
        clip,
    ));
    // Fetch streams with no backend behind them are answered `CLIP_FETCH_UNAVAILABLE` instead of
    // hanging (the coordinator owns `accept_bi` when a backend is live — exactly one consumer).
    if !clip_available && pf_clipboard::enabled() {
        pf_clipboard::spawn_decline_loop(conn.clone());
    }

    // Input plane: QUIC datagrams → channel → a native per-session thread. Pointer/keyboard
    // events are forwarded to the host-lifetime [`InjectorService`] (`inj_tx`) so the portal
    // grant persists across sessions; this thread owns the session's virtual gamepads (uinput,
    // per-session) and sends force feedback back over `conn`. It exits when the channel closes
    // (datagram task ends on disconnect) — fresh gamepad state per session.
    //
    // ONE channel for both event kinds deliberately: rich input (gyro at the pad's report
    // rate) used to ride a second channel that the thread only drained after the main
    // channel's 4 ms recv timeout — every motion sample of a pure-gyro aim (no button
    // traffic) ate up to 4 ms of added latency/jitter. A single channel wakes the thread on
    // whichever arrives.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<ClientInput>();
    let rich_tx = input_tx.clone();
    let input_handle = {
        let conn = conn.clone();
        let gamepad = welcome.gamepad;
        std::thread::Builder::new()
            .name("punktfunk1-input".into())
            .spawn(move || input_thread(input_rx, conn, inj_tx, gamepad))
            .context("spawn input thread")?
    };
    // One reader for ALL client→host datagrams, demuxed by magic byte (two read_datagram loops
    // would race for datagrams): 0xCB → mic uplink (Opus, forwarded to the host-lifetime mic
    // service), 0xCC → rich input (DualSense touchpad / motion, to the per-session input thread),
    // 0xC8 → input (also the input thread). The magics are disjoint, so decode order doesn't
    // matter. Unknown tags are ignored.
    let input_conn = conn.clone();
    tokio::spawn(async move {
        let (mut input_count, mut mic_count, mut rich_count) = (0u64, 0u64, 0u64);
        while let Ok(d) = input_conn.read_datagram().await {
            if let Some((_seq, _pts, opus)) = punktfunk_core::quic::decode_mic_datagram(&d) {
                mic_count += 1;
                // Host-lifetime mic service (bounded queue): `try_send` drops the frame when the
                // service is full or gone, never blocking this datagram loop (security-review S6).
                let _ = mic_tx.try_send(opus.to_vec());
            } else if let Some(rich) = punktfunk_core::quic::RichInput::decode(&d) {
                rich_count += 1;
                if rich_tx.send(ClientInput::Rich(rich)).is_err() {
                    break;
                }
            } else if let Some(mut ev) = InputEvent::decode(&d) {
                input_count += 1;
                // Wire hygiene: KEY_FLAG_SEMANTIC_VK is an in-process tag (GameStream ingest
                // only) — strip it from network events so a client can't flip the host's
                // key-decoding convention. Other kinds keep flags verbatim (MouseMoveAbs packs
                // its reference extent there).
                if matches!(
                    ev.kind,
                    punktfunk_core::input::InputKind::KeyDown
                        | punktfunk_core::input::InputKind::KeyUp
                ) {
                    ev.flags &= !crate::inject::KEY_FLAG_SEMANTIC_VK;
                }
                if input_tx.send(ClientInput::Event(ev)).is_err() {
                    break;
                }
            }
        }
        tracing::info!(
            input = input_count,
            mic = mic_count,
            rich = rich_count,
            "client datagram stream ended"
        );
    });

    // (The stop/quit flags + their disconnect watcher are created above, before the handshake, so
    // the Welcome-time display prep can observe a mid-handshake disconnect.)
    // Lifecycle events (RFC §4): this point — handshake complete, pairing/admission passed — is
    // where the client counts as CONNECTED; the close watcher below pairs it with the
    // disconnect + its decoded reason. A client rejected earlier never emits either.
    let event_client = crate::events::ClientRef {
        name: hello.name.clone().unwrap_or_default(),
        fingerprint: endpoint::peer_fingerprint(&conn).map(|fp| fingerprint_hex(&fp)),
        plane: crate::events::Plane::Native,
    };
    crate::events::emit(crate::events::EventKind::ClientConnected {
        client: event_client.clone(),
    });
    {
        let conn = conn.clone();
        tokio::spawn(async move {
            let reason = conn.closed().await;
            let why = match &reason {
                quinn::ConnectionError::ApplicationClosed(ac)
                    if ac.error_code == quinn::VarInt::from_u32(QUIT_CODE) =>
                {
                    crate::events::DisconnectReason::Quit
                }
                quinn::ConnectionError::TimedOut => crate::events::DisconnectReason::Timeout,
                _ => crate::events::DisconnectReason::Error,
            };
            crate::events::emit(crate::events::EventKind::ClientDisconnected {
                client: event_client,
                reason: why,
            });
        });
    }

    // Register this now-live session for mode-conflict admission (Stage 4): carry its identity, the
    // negotiated mode, and its stop flag so a LATER connecting client's admission can see it and
    // (under `steal`) signal it. The guard removes the entry when this session ends.
    let _live_guard = {
        let id = endpoint::peer_fingerprint(&conn);
        let label = id
            .map(|fp| {
                fp.iter()
                    .take(4)
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            })
            .unwrap_or_else(|| "client".to_string());
        crate::vdisplay::admission::register(
            id,
            (
                welcome.mode.width,
                welcome.mode.height,
                welcome.mode.refresh_hz,
            ),
            stop.clone(),
            label,
        )
    };

    // Audio plane (virtual source only — synthetic runs are protocol tests): desktop Opus
    // → host→client QUIC datagrams, on its own native thread. Best-effort on every failure
    // (no PipeWire audio, spawn error): the session continues without audio — and a spawn
    // error must NOT early-return here, the threads above are already running.
    let audio_handle = if opts.source == Punktfunk1Source::Virtual {
        let conn = conn.clone();
        let stop = stop.clone();
        let cap = audio_cap.clone();
        let channels = welcome.audio_channels;
        std::thread::Builder::new()
            .name("punktfunk1-audio".into())
            .spawn(move || audio_thread(conn, stop, cap, channels))
            .map_err(|e| tracing::warn!(error = %e, "audio thread spawn failed — session continues without audio"))
            .ok()
    } else {
        None
    };

    // HDR static metadata (ST.2086 mastering + CEA-861.3 content light level), host → client, sent
    // once at session start when an HDR session was negotiated, as a generic HDR10 baseline. The
    // virtual-source stream loop then sends the source display's REAL mastering metadata (Windows
    // GetDesc1) as soon as capture starts and re-sends it on keyframes; the client applies the
    // latest it receives. This baseline covers the synthetic source and the pre-capture gap.
    if welcome.color.is_hdr() {
        // Prefer the CLIENT's own display volume (Hello::display_hdr): the virtual display's EDID
        // now advertises it, so host apps tone-map to exactly that volume — echoing it here keeps
        // the mastering metadata honest end-to-end. Generic HDR10 only for older clients.
        let meta = hello
            .display_hdr
            .unwrap_or_else(pf_frame::hdr::generic_hdr10);
        let _ = conn.send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(&meta).into());
        tracing::info!(
            client_volume = hello.display_hdr.is_some(),
            "sent HDR10 static metadata (0xCE baseline)"
        );
    }

    // Test hook (synthetic source only): a scripted feedback burst on the host→client
    // planes — rumble (0xCA) + DualSense HID-output (0xCD) — so loopback tests can assert
    // the client's feedback path without a real game writing output reports to a real pad.
    if opts.source == Punktfunk1Source::Synthetic
        && std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1")
    {
        use punktfunk_core::quic::HidOutput;
        // v2 envelope (seq 0, 400 ms TTL) so the loopback/probe assertion covers the self-
        // terminating tail, not just the level.
        let d = punktfunk_core::quic::encode_rumble_datagram_v2(0, 0x4000, 0x8000, 0, 400);
        let _ = conn.send_datagram(d.to_vec().into());
        for h in [
            HidOutput::Led {
                pad: 0,
                r: 10,
                g: 20,
                b: 30,
            },
            HidOutput::PlayerLeds {
                pad: 0,
                bits: 0b00100,
            },
            HidOutput::Trigger {
                pad: 0,
                which: 1,
                effect: vec![0x21, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            },
        ] {
            let _ = conn.send_datagram(h.encode().into());
        }
        tracing::info!("PUNKTFUNK_TEST_FEEDBACK: scripted rumble + hidout burst sent");
    }

    // Data plane on a native thread (no async on the hot path — design invariant).
    let cfg = welcome.session_config(Role::Host);
    let source = opts.source;
    let (seconds, frames) = (opts.seconds, opts.frames);
    let mode = hello.mode;
    // Script-facing runtime marker: `$XDG_RUNTIME_DIR/punktfunk/stream` exists (with this session's
    // negotiated mode) for exactly as long as this session streams. Held by RAII to session end, so
    // every exit path — clean disconnect, error, panic-unwind — retracts it. Lets a launch wrapper
    // branch "streaming → run the game as-is; not → my local multi-head gamescope" (see the module).
    let _stream_marker = crate::stream_marker::announce(crate::stream_marker::StreamInfo {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr: welcome.color.is_hdr(),
        client: hello.name.clone().unwrap_or_default(),
        launch: hello.launch.clone(),
    });
    // The session's launch, threaded into the data plane. Windows carries the store-qualified id
    // (spawned into the interactive user session once capture is live); other hosts resolve the id
    // to its shell command HERE against the host's own library — a client can only ever pick an
    // existing title, never send a command — and the data plane runs it per-backend (nested into a
    // bare-spawn gamescope, or spawned into the live session once capture is up).
    #[cfg(target_os = "windows")]
    let launch_for_dp = hello.launch.clone();
    #[cfg(not(target_os = "windows"))]
    let launch_for_dp = hello.launch.as_deref().and_then(|id| {
        match crate::library::launch_command(id) {
            Some(cmd) => {
                tracing::info!(launch_id = id, command = %cmd, "resolved library launch for this session");
                Some(cmd)
            }
            None => {
                tracing::warn!(
                    launch_id = id,
                    "client requested a launch id not in this host's library — ignoring"
                );
                None
            }
        }
    });
    // Per-title prep steps (RFC §6) for a launched CUSTOM library title: run synchronously
    // before the data plane starts (so before the display opens and the title spawns); the
    // guard's drop — any serve_session exit — runs the undos in reverse, best-effort.
    // `block_in_place`: prep is blocking operator code and this is a multi-thread runtime;
    // the closure only runs when the title actually has prep steps.
    let _prep = hello.launch.as_deref().and_then(|id| {
        let cmds = crate::library::prep_for(id);
        let env = [("PF_APP_ID".to_string(), id.to_string())];
        (!cmds.is_empty())
            .then(|| tokio::task::block_in_place(|| crate::hooks::run_prep(&cmds, &env)))
    });
    let bitrate_kbps = welcome.bitrate_kbps; // resolved encoder bitrate (Hello clamped, or default)
                                             // "Automatic" request: the resolved rate is a host default — for PyroWave a per-mode
                                             // bpp pin the data plane re-resolves on a mid-stream mode switch.
    let bitrate_auto = hello.bitrate_kbps == 0;
    let bit_depth = welcome.bit_depth; // resolved encode bit depth (8, or 10 when negotiated)
                                       // Resolved chroma — derive the typed value back from the wire byte the Welcome carried (so the
                                       // session uses exactly what the client was told). `Yuv444` only when the handshake gate passed.
    let chroma = if welcome.chroma_format == punktfunk_core::quic::CHROMA_IDC_444 {
        crate::encode::ChromaFormat::Yuv444
    } else {
        crate::encode::ChromaFormat::Yuv420
    };
    let stop_stream = stop.clone();
    let quit_stream = quit.clone();
    // The client display's HDR volume (Hello): the virtual display's EDID advertises it (host apps
    // tone-map to the client's real panel) and the 0xCE mastering metadata echoes it. `None` =
    // older client / no HDR display → the built-in defaults everywhere.
    let client_hdr = hello.display_hdr;
    let fec_target_dp = fec_target.clone(); // data-plane handle to the adaptive-FEC target
    let conn_stream = conn.clone(); // for sending the source's real HDR metadata (0xCE) mid-stream
                                    // Per-AU host-timing emission (0xCF): only when the client advertised the cap bit. All
                                    // first-party clients do (the core connector ORs it in); an older client leaves it clear
                                    // and gets no extra datagrams.
    let timing_conn =
        (hello.video_caps & punktfunk_core::quic::VIDEO_CAP_HOST_TIMING != 0).then(|| conn.clone());
    // Probe-sequence capability: the client reassembles speed-test filler in its own index window,
    // so mid-session bursts don't consume video frame indexes. An older client (bit clear) gets
    // mid-session probes declined instead — see `run_probe_burst`.
    let probe_seq = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ != 0;
    let stats_dp = stats; // data-plane handle to the shared stats recorder
                          // Short label for web-console stats captures: the client's cert-fingerprint prefix, else its
                          // peer IP (no fingerprint = anonymous TOFU/--open client).
    let client_label = endpoint::peer_fingerprint(&conn)
        .map(|fp| fingerprint_hex(&fp)[..12].to_string())
        .unwrap_or_else(|| conn.remote_address().ip().to_string());
    // Transition-trace handles for the data plane (P0.1): the punch stamp + the virtual-stream
    // stages ride the same per-session trace; resizes write their totals into the shared slot.
    let bringup_dp = bringup.clone();
    let resize_ms_dp = resize_ms.clone();
    let result: Result<()> = async {
        tokio::task::spawn_blocking(move || -> Result<()> {
            // Bring up the (already-bound) data-plane socket. Default: hole-punch — wait briefly
            // for the client's punch, then stream to its OBSERVED source, so video traverses a
            // NAT / stateful inter-VLAN firewall (control + side planes ride the client-initiated
            // QUIC, but the raw video UDP needs the client to open the path first); falls back to
            // the reported address for clients that don't punch (flat-LAN, unchanged). With a fixed
            // `--data-port` (`direct`), skip the punch-wait and stream straight to the reported
            // address — the operator declared a reachable, firewall-opened port, so there's no
            // punch-timeout to pay. (Direct trusts the reported port: it can't cross a client-side
            // NAT that remaps it.)
            let bound = if direct {
                UdpTransport::from_socket(data_sock, &client_udp.to_string()).map(|t| (t, false))
            } else {
                UdpTransport::from_socket_punch(
                    data_sock,
                    &client_udp.to_string(),
                    std::time::Duration::from_millis(2500),
                )
            };
            let (transport, punched) = match bound {
                Ok(v) => v,
                Err(e) => {
                    // Surface the failure here directly: a data-plane bind error would otherwise be
                    // reported only after teardown (and a teardown stall could swallow it entirely).
                    tracing::error!(error = %e, %client_udp, udp_port, "data-plane socket setup failed");
                    return Err(anyhow::Error::new(e)).context("bind data plane");
                }
            };
            bringup_dp.mark("punch_done");
            tracing::info!(
                %client_udp,
                udp_port,
                direct,
                punched,
                "data plane bound (direct=true → fixed --data-port, streaming to the reported \
                 address with no hole-punch; else punched=true → the client's observed source, \
                 false → no punch seen, the reported address)"
            );
            let mut session = Session::new(cfg, Box::new(transport))
                .map_err(|e| anyhow!("host session: {e:?}"))?;
            match source {
                Punktfunk1Source::Synthetic => synthetic_stream(
                    &mut session,
                    frames,
                    &stop_stream,
                    &probe_rx,
                    &probe_result_tx,
                    &fec_target_dp,
                    timing_conn.as_ref(),
                    probe_seq,
                ),
                Punktfunk1Source::Virtual => {
                    let compositor = compositor
                        .expect("the Virtual source resolves a compositor during the handshake");
                    let ctx = SessionContext {
                        session,
                        mode,
                        seconds,
                        stop: stop_stream,
                        quit: quit_stream,
                        reconfig: reconfig_rx,
                        keyframe: keyframe_rx,
                        rfi: rfi_rx,
                        bitrate_rx,
                        compositor,
                        bitrate_kbps,
                        bitrate_auto,
                        bit_depth,
                        chroma,
                        codec,
                        probe_rx,
                        probe_result_tx,
                        reconfig_result_tx,
                        fec_target: fec_target_dp,
                        conn: conn_stream,
                        timing_conn,
                        probe_seq,
                        stats: stats_dp,
                        client_label,
                        launch: launch_for_dp,
                        client_hdr,
                        bringup: bringup_dp,
                        resize_ms: resize_ms_dp,
                    };
                    match prep {
                        // P1.1: the display prep started at Welcome on its own thread — hand it
                        // the post-punch context and adopt its result as the stream result (that
                        // thread runs `virtual_stream` on the pipeline it already built).
                        Some((ctx_tx, prep_thread)) => match ctx_tx.send(ctx) {
                            Ok(()) => match prep_thread.join() {
                                Ok(r) => r,
                                Err(_) => Err(anyhow!("prepared stream thread panicked")),
                            },
                            // The prep thread died before the hand-off (panicked during prep —
                            // its guard/lease unwound): run the stream inline instead.
                            Err(std::sync::mpsc::SendError(ctx)) => {
                                tracing::warn!(
                                    "display-prep thread gone before hand-off — building inline"
                                );
                                virtual_stream(ctx, None)
                            }
                        },
                        None => virtual_stream(ctx, None),
                    }
                }
            }
        })
        .await
        .context("stream thread")??;
        // Give the client a moment to drain before the close.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Ok(())
    }
    .await;

    // Teardown on EVERY path (a failed data plane must not leave the connection open with
    // audio still streaming): stop the audio thread, close, then join both side-plane
    // threads so the next session starts fresh (closing the connection ends the datagram
    // task, which drops the input channel, which exits the input thread + its gamepads).
    stop.store(true, Ordering::SeqCst);
    conn.close(
        if result.is_ok() { 0u32 } else { 1u32 }.into(),
        if result.is_ok() { b"done" } else { b"error" },
    );
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(h) = audio_handle {
            let _ = h.join();
        }
        let _ = input_handle.join();
    })
    .await;
    // The capture (and our gamescope session's VirtualOutput) are gone by here. If this was the
    // host-managed gamescope path on a box that autologs into gaming mode (Bazzite default), put the
    // TV's gaming session back so it's the default when no one is streaming.
    crate::vdisplay::restore_managed_session();
    result
}

/// Backoff between reopen attempts after a host-lifetime service's backend (a capturer) fails
/// to open or its worker dies, so a persistently-unavailable resource isn't hammered. (The
/// virtual mic has its own tuning — see [`crate::audio::MicPump`].)
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Pack a `(width, height, refresh_hz)` mode into one atomic word (w:16|h:16|hz:16) for the live
/// stats-mode slot — one store/load instead of three racy ones. Every dimension fits: the codec
/// max dimension caps w/h well under 2^16 (`validate_dimensions`), refresh likewise.
fn pack_mode(width: u32, height: u32, refresh_hz: u32) -> u64 {
    ((width as u64 & 0xffff) << 32)
        | ((height as u64 & 0xffff) << 16)
        | (refresh_hz as u64 & 0xffff)
}

/// Unpack a [`pack_mode`] word back into `(width, height, refresh_hz)`.
pub(crate) fn unpack_mode(packed: u64) -> (u32, u32, u32) {
    (
        ((packed >> 32) & 0xffff) as u32,
        ((packed >> 16) & 0xffff) as u32,
        (packed & 0xffff) as u32,
    )
}

/// Recover the integer refresh rate a pipeline was actually built at from its frame interval
/// (`interval` is constructed as `1/effective_hz` in `build_pipeline`, so the round-trip is exact).
/// This is the backend-honored rate — it differs from the requested mode when e.g. KWin caps a
/// virtual output at 60 Hz.
fn interval_hz(interval: std::time::Duration) -> u32 {
    (1.0 / interval.as_secs_f64()).round() as u32
}

/// The mode a pipeline is ACTUALLY delivering, for the H2/H3 corrective ack: the captured frame's
/// real dimensions (`build_pipeline` opens the encoder at `frame.{width,height}`, so this is exactly
/// what the client decodes) paced at the rate the pipeline achieved ([`interval_hz`]). It diverges
/// from the requested mode when a backend can't honor it: KWin caps a virtual output's refresh, or —
/// the case this exists for — Windows pf-vdisplay rejects an in-place `SetMode` to a resolution not
/// in the running monitor's advertised EDID list and the host falls back to the actual display mode
/// (`capture::idd_push`: "sizing the ring to the display's actual mode"). Comparing this against the
/// already-acked request decides whether a corrective `Reconfigured` ack is owed so the client
/// doesn't believe it got a resolution it never received.
fn delivered_mode(
    frame_width: u32,
    frame_height: u32,
    interval: std::time::Duration,
) -> punktfunk_core::Mode {
    punktfunk_core::Mode {
        width: frame_width,
        height: frame_height,
        refresh_hz: interval_hz(interval),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_mode_pack_roundtrips_and_interval_recovers_hz() {
        // The live-stats mode slot (H3): pack → unpack is exact for real modes.
        for (w, h, hz) in [(1280u32, 720u32, 60u32), (3840, 2160, 144), (320, 200, 24)] {
            assert_eq!(unpack_mode(pack_mode(w, h, hz)), (w, h, hz));
        }
        // `interval` is built as 1/effective_hz — the round-trip recovers the integer rate.
        for hz in [24u32, 30, 60, 75, 90, 120, 144, 165, 240] {
            let interval = std::time::Duration::from_secs_f64(1.0 / hz as f64);
            assert_eq!(interval_hz(interval), hz);
        }
    }

    #[test]
    fn delivered_mode_reports_captured_dims_and_triggers_corrective_ack() {
        let hz60 = std::time::Duration::from_secs_f64(1.0 / 60.0);
        let requested = punktfunk_core::Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
        };

        // Honored: the captured frame matches the request → no corrective ack owed (`== requested`).
        let honored = delivered_mode(2560, 1440, hz60);
        assert_eq!(honored, requested);

        // Resolution fallback (Windows pf-vdisplay rejected the out-of-list SetMode, host stayed at
        // the actual display mode): the frame's real dims flow through, so the delivered mode differs
        // from the acked request and a corrective ack IS owed — the exact gap this fixes.
        let fell_back = delivered_mode(1920, 1080, hz60);
        assert_ne!(fell_back, requested);
        assert_eq!(
            fell_back,
            punktfunk_core::Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60
            }
        );

        // Refresh cap (KWin) is still caught: same dims, achieved rate recovered from the interval.
        let capped = delivered_mode(2560, 1440, std::time::Duration::from_secs_f64(1.0 / 30.0));
        assert_ne!(capped, requested);
        assert_eq!(capped.refresh_hz, 30);
    }

    #[test]
    fn pyrowave_bitrate_pins_to_bpp_default() {
        use punktfunk_core::config::Mode;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        use crate::encode::ChromaFormat;
        // Automatic (0) on PyroWave → the ~1.6 bpp operating point, not the 20 Mbps H.26x
        // default (which would turn wavelets to mush — plan §4.6).
        let kbps = resolve_bitrate_kbps_for(
            crate::encode::Codec::PyroWave,
            0,
            &mode,
            ChromaFormat::Yuv420,
            8,
        );
        assert_eq!(kbps, 1920 * 1080 * 60 * 16 / 10 / 1000);
        // 4:4:4 scales the pin to ~2.6 bpp, 10-bit adds 15 % (design/pyrowave-444-hdr.md §2.5).
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &mode,
                ChromaFormat::Yuv444,
                8
            ),
            1920 * 1080 * 60 * 26 / 10 / 1000
        );
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &mode,
                ChromaFormat::Yuv444,
                10
            ),
            (1920u64 * 1080 * 60 * 26 / 10 * 115 / 100 / 1000) as u32
        );
        // An explicit client rate is honored (clamped like any other codec)...
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                130_000,
                &mode,
                ChromaFormat::Yuv420,
                8
            ),
            130_000
        );
        // ...and the H.26x codecs keep the legacy default.
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::H265,
                0,
                &mode,
                ChromaFormat::Yuv420,
                8
            ),
            DEFAULT_BITRATE_KBPS
        );
    }

    #[test]
    fn pyrowave_auto_pin_respects_operator_ceiling() {
        use crate::encode::{ChromaFormat, Codec};
        use punktfunk_core::config::Mode;
        // 5120x1440@240 4:4:4 10-bit pins ~5.29 Gbps open-loop — above a 5 GbE link.
        let mode = Mode {
            width: 5120,
            height: 1440,
            refresh_hz: 240,
        };
        let uncapped =
            resolve_bitrate_kbps_for(Codec::PyroWave, 0, &mode, ChromaFormat::Yuv444, 10);
        assert!(
            uncapped > 5_000_000,
            "expected the open-loop pin, got {uncapped}"
        );
        // With the operator ceiling set, the Automatic pin is capped to the link rate...
        std::env::set_var("PUNKTFUNK_PYROWAVE_MAX_MBPS", "4500");
        assert_eq!(
            resolve_bitrate_kbps_for(Codec::PyroWave, 0, &mode, ChromaFormat::Yuv444, 10),
            4_500_000
        );
        // ...but a pin already under the ceiling is untouched (1080p60 4:2:0 ≈ 199 Mbps)...
        let small = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        assert_eq!(
            resolve_bitrate_kbps_for(Codec::PyroWave, 0, &small, ChromaFormat::Yuv420, 8),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // ...and an explicit client rate bypasses the ceiling entirely.
        assert_eq!(
            resolve_bitrate_kbps_for(Codec::PyroWave, 6_000_000, &mode, ChromaFormat::Yuv444, 10),
            6_000_000
        );
        std::env::remove_var("PUNKTFUNK_PYROWAVE_MAX_MBPS");
    }

    #[test]
    fn adapt_fec_maps_loss_to_recovery_band() {
        // A perfectly clean window (0 loss) lands on the floor.
        assert_eq!(adapt_fec(0), FEC_MIN);
        // Any nonzero loss rounds up past the floor (ceil) — tiny but never below the cushion.
        assert_eq!(adapt_fec(1), 2);
        // FEC exceeds the loss it covers (×1.4 + 1pt headroom).
        assert_eq!(adapt_fec(50_000), 8); // 5% loss → ceil(7)+1 = 8
        assert_eq!(adapt_fec(100_000), 15); // 10% → ceil(14)+1 = 15
                                            // Heavy loss saturates at the ceiling, never beyond.
        assert_eq!(adapt_fec(1_000_000), FEC_MAX); // 100% → clamped
        assert!(adapt_fec(u32::MAX) <= FEC_MAX);
    }

    #[test]
    fn data_socket_defaults_to_random_hole_punch() {
        // No fixed port (and the explicit-0 alias) → a random ephemeral port, and NOT direct: the
        // caller hole-punches.
        for req in [None, Some(0)] {
            let (sock, direct) = bind_data_socket(req).expect("bind random data socket");
            assert!(!direct, "req={req:?} must hole-punch, not stream direct");
            assert_ne!(sock.local_addr().unwrap().port(), 0);
        }
    }

    #[test]
    fn data_socket_fixed_binds_direct_then_falls_back_when_busy() {
        // Learn a currently-free port (bind :0, read it, drop — the same reserve-then-rebind the
        // host itself uses; a race here would only make the assert below flaky, not wrong).
        let free = std::net::UdpSocket::bind("0.0.0.0:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();

        // A free fixed port binds exactly it, in DIRECT mode (no hole-punch).
        let (held, direct) = bind_data_socket(Some(free)).expect("bind fixed data socket");
        assert!(direct, "a fixed --data-port must stream direct");
        assert_eq!(held.local_addr().unwrap().port(), free);

        // While it's held, a second session on the same fixed port can't bind it → it must fall
        // back to a random port + hole-punch rather than fail (so concurrency never regresses).
        let (fallback, direct2) = bind_data_socket(Some(free)).expect("busy fixed port falls back");
        assert!(!direct2, "a busy fixed port must fall back to hole-punch");
        assert_ne!(
            fallback.local_addr().unwrap().port(),
            free,
            "the fallback must not reuse the busy fixed port"
        );
    }

    /// Freeze the gamepad wire contract: every button bit + axis id pinned to its exact value in
    /// `punktfunk_core::input::gamepad` — the single source both the punktfunk/1 native wire and the
    /// GameStream/Limelight wire read from (they are one and the same). Renumbering a bit in core
    /// silently breaks every already-shipped client, so it must fail here first. This is the host
    /// counterpart to the client-side C-ABI cross-checks in the Apple/Android gamepad tests.
    #[test]
    fn gamepad_wire_bits_are_pinned() {
        use punktfunk_core::input::gamepad as pf;
        // buttonFlags — low 16 bits. The injectors now name these straight from core::input::gamepad
        // (the GameStream junk-drawer aliases were removed in the pf-inject un-coupling), so this pins
        // core directly.
        assert_eq!(pf::BTN_DPAD_UP, 0x0000_0001);
        assert_eq!(pf::BTN_DPAD_DOWN, 0x0000_0002);
        assert_eq!(pf::BTN_DPAD_LEFT, 0x0000_0004);
        assert_eq!(pf::BTN_DPAD_RIGHT, 0x0000_0008);
        assert_eq!(pf::BTN_START, 0x0000_0010);
        assert_eq!(pf::BTN_BACK, 0x0000_0020);
        assert_eq!(pf::BTN_LS_CLICK, 0x0000_0040);
        assert_eq!(pf::BTN_RS_CLICK, 0x0000_0080);
        assert_eq!(pf::BTN_LB, 0x0000_0100);
        assert_eq!(pf::BTN_RB, 0x0000_0200);
        assert_eq!(pf::BTN_GUIDE, 0x0000_0400);
        assert_eq!(pf::BTN_A, 0x0000_1000);
        assert_eq!(pf::BTN_B, 0x0000_2000);
        assert_eq!(pf::BTN_X, 0x0000_4000);
        assert_eq!(pf::BTN_Y, 0x0000_8000);
        // buttonFlags2 — high 16 bits: back-grip paddles, plus the touchpad-click / Share bits the
        // DualSense/DS4 protos consume.
        assert_eq!(pf::BTN_PADDLE1, 0x0001_0000);
        assert_eq!(pf::BTN_PADDLE2, 0x0002_0000);
        assert_eq!(pf::BTN_PADDLE3, 0x0004_0000);
        assert_eq!(pf::BTN_PADDLE4, 0x0008_0000);
        assert_eq!(pf::BTN_TOUCHPAD, 0x0010_0000);
        assert_eq!(pf::BTN_MISC1, 0x0020_0000);
        // Axis ids — dense, 0-based.
        assert_eq!(
            [
                pf::AXIS_LS_X,
                pf::AXIS_LS_Y,
                pf::AXIS_RS_X,
                pf::AXIS_RS_Y,
                pf::AXIS_LT,
                pf::AXIS_RT,
            ],
            [0, 1, 2, 3, 4, 5]
        );
    }

    /// Pull and byte-verify `count` synthetic frames through the C ABI connection.
    unsafe fn pull_verified(conn: *mut punktfunk_core::abi::PunktfunkConnection, count: u32) {
        use punktfunk_core::error::PunktfunkStatus;
        let mut got = 0u32;
        // SAFETY: the inferred type is the `#[repr(C)]` POD `PunktfunkFrame` (a raw `*const u8`, a
        // `usize`, and integer fields); all-zero is a valid bit pattern for every field (a null
        // `data`, `len == 0`). It is only ever read after `next_au` below fully overwrites it on `Ok`,
        // so the zeroed value is never observed.
        let mut frame = unsafe { std::mem::zeroed() };
        while got < count {
            // SAFETY: `conn` is the live, non-null `*mut PunktfunkConnection` from `punktfunk_connect`
            // (the caller asserts non-null and does not close it until after this returns), meeting the
            // ABI's "valid handle". `&mut frame` is an exclusive, writable borrow of the local
            // `PunktfunkFrame` that outlives this synchronous call. This single test thread is the only
            // video puller, satisfying the one-video-thread rule.
            match unsafe {
                punktfunk_core::abi::punktfunk_connection_next_au(conn, &mut frame, 2000)
            } {
                PunktfunkStatus::Ok => {
                    // SAFETY: on `Ok`, `next_au` set `frame.data`/`frame.len` to the reassembled AU
                    // buffer the connection owns; per the ABI contract that borrow stays valid until
                    // the NEXT `next_au` call on this handle. We read the whole slice here (the assert
                    // + length-checked indexing) before the loop's next `next_au`, and `conn` outlives
                    // it — so the pointer is live, exactly `len` bytes, read-only, single-threaded (no
                    // aliasing/use-after-free).
                    let data = unsafe { std::slice::from_raw_parts(frame.data, frame.len) };
                    let idx = u32::from_le_bytes(data[0..4].try_into().unwrap());
                    assert_eq!(
                        data,
                        &test_frame(idx, data.len())[..],
                        "frame {idx} content"
                    );
                    got += 1;
                }
                PunktfunkStatus::NoFrame => continue,
                other => panic!("next_au: {other:?}"),
            }
        }
    }

    /// End-to-end through the C ABI — the exact contract platform clients (Swift) link:
    /// in-process punktfunk/1 host, `punktfunk_connect` (TOFU → pinned reconnect) →
    /// `punktfunk_connection_next_au` pulls verified frames → `punktfunk_connection_send_input`
    /// In-process-host tests each spin up a host on a fixed loopback port and share the process-global
    /// admission table, so they must NOT run concurrently: a same-identity connection in one test would
    /// fire the reconnect-preempt (`preempt_same_identity`) against another test's live session and
    /// close it. Serialize them on this lock. Poison-tolerant (`into_inner`) so a failing test doesn't
    /// cascade a poison error into the others.
    static SESSION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// enqueues → `punktfunk_connection_close`. Three sequential sessions against ONE host
    /// process prove the persistent listener, and a wrong pin is rejected.
    #[test]
    fn c_abi_connection_roundtrip() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::abi::{
            punktfunk_connect, punktfunk_connection_close, punktfunk_connection_mode,
            punktfunk_connection_send_input,
        };
        use punktfunk_core::error::PunktfunkStatus;

        let host = std::thread::spawn(|| {
            run(Punktfunk1Options {
                port: 19777,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 3,
                max_concurrent: 1,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                data_port: None,
                idle_timeout: None,
                mdns: false, // unit tests must not advertise on the LAN
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Session 1: TOFU (no pin) — observe the host fingerprint.
        let addr = std::ffi::CString::new("127.0.0.1").unwrap();
        let mut observed = [0u8; 32];
        // SAFETY: `addr` is a live `CString` ("127.0.0.1") whose `as_ptr()` is the NUL-terminated
        // UTF-8 host string the contract requires; `pin_sha256`/cert/key are NULL (all permitted), and
        // `observed.as_mut_ptr()` is the local `[u8; 32]` — exactly the 32 writable bytes the contract
        // demands, not aliased during the call. Every pointer references a live local that outlives the
        // blocking connect.
        let conn = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                std::ptr::null(),
                observed.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn.is_null(), "punktfunk_connect failed");
        assert_ne!(observed, [0u8; 32], "fingerprint not reported");

        let (mut w, mut h, mut hz) = (0u32, 0u32, 0u32);
        // SAFETY: `conn` is the live, non-null connection handle just asserted above; `&mut w/h/hz` are
        // exclusive, writable borrows of local `u32`s that outlive this synchronous call — the three
        // writable out-params the contract names.
        let st = unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) };
        assert_eq!(st, PunktfunkStatus::Ok);
        assert_eq!((w, h, hz), (1280, 720, 60));

        // Mid-stream renegotiation: request a new mode, the host acks on the control
        // stream, and punktfunk_connection_mode reflects the switch.
        // SAFETY: `conn` is the live, non-null connection handle (the only pointer arg); the remaining
        // arguments are by-value integers. The handle outlives this non-blocking enqueue.
        let st = unsafe {
            punktfunk_core::abi::punktfunk_connection_request_mode(conn, 1920, 1080, 144)
        };
        assert_eq!(st, PunktfunkStatus::Ok);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // SAFETY: same as the earlier `punktfunk_connection_mode` call — `conn` is the live handle
            // and `&mut w/h/hz` are exclusive writable borrows of locals that outlive this synchronous
            // call.
            let st = unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) };
            assert_eq!(st, PunktfunkStatus::Ok);
            if (w, h, hz) == (1920, 1080, 144) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mode switch not acked (still {w}x{h}@{hz})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        // SAFETY: `pull_verified` requires a live connection handle it alone pulls video from; `conn` is
        // the open, non-null handle from `punktfunk_connect` and this is the only thread touching it.
        unsafe { pull_verified(conn, 25) };

        let ev = punktfunk_core::input::InputEvent {
            kind: punktfunk_core::input::InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        // SAFETY: `conn` is the live handle; `&ev` borrows the local `InputEvent`, valid and immutable
        // for this synchronous enqueue — the contract's "valid InputEvent" pointer.
        let st = unsafe { punktfunk_connection_send_input(conn, &ev) };
        assert_eq!(st, PunktfunkStatus::Ok);
        // SAFETY: `conn` was returned by `punktfunk_connect` and is never used after this call (session
        // 2 below uses a fresh `conn2`); `close` takes ownership and frees the handle exactly once.
        unsafe { punktfunk_connection_close(conn) };

        // Session 2 (same host process — the listener survived): pin the fingerprint.
        // SAFETY: as for session 1 — `addr` is the live NUL-terminated host string; here
        // `observed.as_ptr()` is the 32-byte pin (the fingerprint captured above, a valid `[u8; 32]`),
        // `observed_sha256_out` is NULL and cert/key are NULL. All pointers reference live locals for
        // the duration of the blocking connect.
        let conn2 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                observed.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn2.is_null(), "pinned reconnect failed");
        // SAFETY: `conn2` is the live, non-null pinned handle, pulled only from this thread —
        // `pull_verified`'s requirement.
        unsafe { pull_verified(conn2, 25) };
        // SAFETY: `conn2` came from `punktfunk_connect` and is not used after this; `close` frees it once.
        unsafe { punktfunk_connection_close(conn2) };

        // Session 3: a wrong pin must be rejected by the handshake.
        let bad = [0xAAu8; 32];
        // SAFETY: same shape as the prior connects — `addr` is the live host string, `bad.as_ptr()` is
        // the 32-byte `[0xAA; 32]` pin, and out/cert/key are NULL; all reference live locals across the
        // blocking call. (The handshake is expected to fail and return NULL here, which is sound.)
        let conn3 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                bad.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(conn3.is_null(), "wrong pin must fail the handshake");

        // The host saw the rejected handshake attempt as session 3? No — a TLS-failed
        // handshake never yields a connection, so accept() is still waiting. Connect once
        // more (TOFU) to complete the host's third session and let it exit.
        // SAFETY: same as session 1's connect — `addr` is the live host string, pin/out/cert/key all
        // NULL; the pointers reference live locals for the duration of the blocking connect.
        let conn4 = unsafe {
            punktfunk_connect(
                addr.as_ptr(),
                19777,
                1280,
                720,
                60,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                10_000,
            )
        };
        assert!(!conn4.is_null());
        // SAFETY: `conn4` is the live, non-null handle, pulled only from this thread.
        unsafe { pull_verified(conn4, 25) };
        // SAFETY: `conn4` came from `punktfunk_connect` and is unused after this; `close` frees it once.
        unsafe { punktfunk_connection_close(conn4) };

        host.join().unwrap().unwrap();
    }

    /// Shared clipboard end to end over a real synthetic session
    /// (`design/clipboard-and-file-transfer.md`): with the operator policy enabled, the host
    /// advertises the capability, acknowledges an enable with a `ClipState`, and — a synthetic
    /// session mirrors no compositor, so no clipboard backend binds — declines a fetch with an
    /// `Error` the client surfaces. Exercises the whole 0x40-0x44 control+fetch path across two real
    /// endpoints (client `NativeClient` ↔ host `serve_session`). The live-backend paths (a real
    /// compositor) are covered by the on-glass test against GNOME/Hyprland.
    #[test]
    fn clipboard_control_and_fetch_decline_over_session() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::clipboard::ClipEventCore;
        use punktfunk_core::quic::{
            CLIP_FILE_INDEX_NONE, CLIP_FLAG_FILES, CLIP_POLICY_FILES, HOST_CAP_CLIPBOARD,
        };

        // Restore the env even on a panicking assert (the poisoned lock is recovered above, so a
        // leaked var could otherwise reach the next session test).
        struct EnvGuard(&'static str);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                std::env::remove_var(self.0);
            }
        }
        let _env = EnvGuard("PUNKTFUNK_CLIPBOARD");
        // Operator policy on. Session tests serialize on SESSION_TEST_LOCK, and only the session
        // path (a session test) reads this env, so the mutation is race-free here.
        std::env::set_var("PUNKTFUNK_CLIPBOARD", "1");

        let host = std::thread::spawn(|| {
            run(Punktfunk1Options {
                port: 19781,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 600, // keep the session alive well past the control exchange
                max_sessions: 1,
                max_concurrent: 1,
                require_pairing: false,
                allow_pairing: false,
                pairing_pin: None,
                paired_store: None,
                data_port: None,
                idle_timeout: None,
                mdns: false,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));

        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let client = NativeClient::connect(
            "127.0.0.1",
            19781,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,    // bitrate_kbps
            0,    // video_caps
            2,    // audio_channels
            0,    // video_codecs (HEVC-only)
            0,    // preferred_codec
            None, // display_hdr
            None, // launch
            None, // pin (TOFU)
            None, // identity (host doesn't require pairing)
            std::time::Duration::from_secs(10),
        )
        .expect("client connects to synthetic host");

        assert_ne!(
            client.host_caps() & HOST_CAP_CLIPBOARD,
            0,
            "an enabled host advertises HOST_CAP_CLIPBOARD"
        );

        // A bounded poll over the clipboard event plane.
        let poll = |pred: &dyn Fn(&ClipEventCore) -> bool| -> Option<ClipEventCore> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while std::time::Instant::now() < deadline {
                match client.next_clip(std::time::Duration::from_millis(200)) {
                    Ok(ev) if pred(&ev) => return Some(ev),
                    Ok(_) => {}
                    Err(punktfunk_core::PunktfunkError::NoFrame) => {}
                    Err(_) => break, // session closed
                }
            }
            None
        };

        // Enable sync (requesting files) → the host acks with a ClipState. A synthetic session
        // mirrors no compositor, so no clipboard backend binds: the host refuses the enable with
        // `BACKEND_UNAVAILABLE` while still reporting the operator policy (files permitted).
        client.clip_control(true, CLIP_FLAG_FILES).unwrap();
        let state = poll(&|e| matches!(e, ClipEventCore::State { .. }))
            .expect("host replies with a ClipState ack");
        match state {
            ClipEventCore::State {
                enabled,
                policy,
                reason,
            } => {
                assert!(!enabled, "no backend for a synthetic session → not enabled");
                assert_eq!(
                    reason,
                    punktfunk_core::quic::CLIP_REASON_BACKEND_UNAVAILABLE,
                    "the refusal reason is BACKEND_UNAVAILABLE"
                );
                assert_ne!(
                    policy & CLIP_POLICY_FILES,
                    0,
                    "PUNKTFUNK_CLIPBOARD=1 permits files"
                );
            }
            _ => unreachable!(),
        }

        // Fetch the host clipboard: a synthetic session has no backend, so the host declines and
        // the client surfaces an Error for that transfer id.
        let xfer = client
            .clip_fetch(1, "text/plain;charset=utf-8".into(), CLIP_FILE_INDEX_NONE)
            .unwrap();
        let err = poll(&|e| matches!(e, ClipEventCore::Error { id, .. } if *id == xfer))
            .expect("host declines the fetch (no backend) → Error event");
        assert!(matches!(err, ClipEventCore::Error { .. }));

        drop(client);
        host.join().unwrap().unwrap();
    }

    fn test_paired_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("punktfunk-paired-test-{}.json", std::process::id()))
    }

    /// Delegated approval (§8b-1) end to end in-process, the SEAMLESS flow: an
    /// identified-but-unpaired client's knock on a pairing-required host is PARKED (connection held
    /// open) and shows up as a pending request (fingerprint-derived label — the connector sends no
    /// Hello name); the operator approves it WHILE the client waits, and the SAME connection is
    /// admitted to a session with no PIN and no reconnect.
    #[test]
    fn delegated_approval_admits_after_knock() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let store =
            std::env::temp_dir().join(format!("pf-approval-test-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&store);
        let np = Arc::new(NativePairing::load_with(Some(store.clone()), None, false).unwrap());
        let np_host = np.clone();
        let host = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(serve(
                Punktfunk1Options {
                    port: 19779,
                    source: Punktfunk1Source::Synthetic,
                    seconds: 0,
                    frames: 25,
                    max_sessions: 1, // the single parked-then-approved session (no reconnect)
                    max_concurrent: 1,
                    require_pairing: true,
                    allow_pairing: false,
                    pairing_pin: None,
                    paired_store: None, // unused: the shared `np` IS the store handle
                    data_port: None,
                    idle_timeout: None,
                    mdns: false,
                },
                0, // no mgmt API in this test → advertise no `mgmt` mDNS port
                np_host,
                StatsRecorder::new(
                    std::env::temp_dir().join(format!("pf-approval-stats-{}", std::process::id())),
                ),
            ))
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let (cert, key) = endpoint::generate_identity().unwrap();
        let expected_fp = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // Approver thread: wait for the parked knock to register, assert its label, then APPROVE it
        // WHILE the client is still parked — the console "click accept" flow.
        let np_approve = np.clone();
        let expect_fp = expected_fp.clone();
        let approver = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
            let pend = loop {
                if let Some(p) = np_approve
                    .pending()
                    .into_iter()
                    .find(|p| p.fingerprint == expect_fp)
                {
                    break p;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the knock must register while the client is parked"
                );
                std::thread::sleep(std::time::Duration::from_millis(40));
            };
            assert!(
                pend.name.starts_with("device "),
                "no Hello name → fingerprint-derived label, got {:?}",
                pend.name
            );
            np_approve
                .approve_pending(pend.id, Some("Approved Device"))
                .unwrap()
                .expect("pending id must approve");
        });

        // The knock: a SINGLE connect that parks until approved, then streams — no reconnect. The
        // timeout is generous (it covers the park + the approver's poll latency).
        let client = NativeClient::connect(
            "127.0.0.1",
            19779,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,    // video_caps
            2,    // audio_channels (stereo)
            0,    // video_codecs (0 → HEVC-only)
            0,    // preferred_codec (auto)
            None, // display_hdr
            None, // launch
            None, // pin: TOFU — the operator's approval (not a PIN) authorizes this client
            Some((cert, key)),
            std::time::Duration::from_secs(15),
        )
        .expect("approved mid-park → session admitted with no reconnect");
        approver.join().unwrap();
        assert!(
            np.is_paired(&expected_fp),
            "approval must pin the knocking fingerprint"
        );
        assert_eq!(np.list()[0].name, "Approved Device");
        drop(client);
        let _ = std::fs::remove_file(&store);
        host.join().unwrap().unwrap();
    }

    /// The PIN pairing ceremony + the --require-pairing gate, end to end in-process:
    /// wrong PIN rejected; right PIN pairs and returns the host fingerprint; a paired
    /// identity gets a session on a pairing-required host; an anonymous client does not.
    #[test]
    fn pairing_ceremony_and_gate() {
        let _serial = SESSION_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use punktfunk_core::client::NativeClient;
        use punktfunk_core::quic::endpoint;

        let host = std::thread::spawn(|| {
            run(Punktfunk1Options {
                port: 19778,
                source: Punktfunk1Source::Synthetic,
                seconds: 0,
                frames: 25,
                max_sessions: 4,
                max_concurrent: 1,
                require_pairing: true,
                allow_pairing: false,
                pairing_pin: Some("4321".into()),
                paired_store: Some(test_paired_path()),
                data_port: None,
                idle_timeout: None,
                mdns: false,
            })
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let timeout = std::time::Duration::from_secs(10);
        let (cert, key) = endpoint::generate_identity().unwrap();
        let identity = (cert.as_str(), key.as_str());
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // 1: anonymous session on a pairing-required host → rejected (independent of the PIN window).
        assert!(
            NativeClient::connect(
                "127.0.0.1",
                19778,
                mode,
                CompositorPref::Auto,
                GamepadPref::Auto,
                0,
                0,    // video_caps
                2,    // audio_channels (stereo)
                0,    // video_codecs
                0,    // preferred_codec
                None, // display_hdr
                None, // launch
                None,
                None,
                timeout
            )
            .is_err(),
            "anonymous session must be rejected"
        );

        // 2: correct PIN → paired, host fingerprint returned. The ONE online attempt CONSUMES the
        // arming window (single-use), verified by step 4.
        let host_fp =
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "test-client", timeout)
                .expect("pairing with the right PIN");
        assert!(test_paired_path().exists());

        // 3: the paired identity gets a session — pinned to the ceremony's fingerprint.
        let client = NativeClient::connect(
            "127.0.0.1",
            19778,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            0,    // video_caps
            2,    // audio_channels (stereo)
            0,    // video_codecs
            0,    // preferred_codec
            None, // display_hdr
            None, // launch
            Some(host_fp),
            Some((cert.clone(), key.clone())),
            timeout,
        )
        .expect("paired session");
        assert_eq!(client.host_fingerprint, host_fp);
        // The Welcome always reports a CONCRETE resolved gamepad backend. (Not asserted
        // against a specific one: resolve_gamepad honors an ambient PUNKTFUNK_GAMEPAD —
        // a dev box exporting it must not fail the suite.)
        assert_ne!(client.resolved_gamepad, GamepadPref::Auto);
        drop(client);

        // 4: SINGLE-USE PIN — the completed ceremony in step 2 consumed the arming window, so a
        // second pairing attempt (even with the CORRECT PIN) is now rejected. This is the documented
        // "one online guess" guarantee: an attacker can't brute-force the static 4-digit PIN. (The
        // operator re-arms via the console / restart for the next device.)
        std::thread::sleep(PAIRING_COOLDOWN + std::time::Duration::from_millis(200));
        assert!(
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "too-late", timeout).is_err(),
            "the PIN window must be single-use (one online guess)"
        );
        let _ = std::fs::remove_file(test_paired_path()); // tidy /tmp

        host.join().unwrap().unwrap();
    }
}
