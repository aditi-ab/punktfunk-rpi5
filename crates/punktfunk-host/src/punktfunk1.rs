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
    endpoint, io, BitrateChanged, ClockEcho, ClockProbe, ColorInfo, Hello, LossReport,
    PairChallenge, PairProof, PairRequest, PairResult, ProbeRequest, ProbeResult, Reconfigure,
    Reconfigured, RequestKeyframe, RfiRequest, SetBitrate, Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::Session;
use rand::RngCore;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

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
            "PAIRING ARMED — enter this PIN on the client to pair: {pin}"
        );
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
/// code lets a client tell "host busy" apart from a transport failure.
const REJECT_BUSY_CODE: u32 = 0x42;

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

/// Pairing needs a human in the loop (reading the PIN off the host, typing it into the
/// client), so its budget is far larger than the machine-speed session handshake.
const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// How long the host keeps an unpaired knock PARKED — connection held open — waiting for the
/// operator to click Approve in the console (delegated approval, roadmap §8b-1). The QUIC
/// keep-alive (4 s, under the 8 s idle timeout) holds the path warm meanwhile, so on approval the
/// device pairs and streams with NO reconnect. Bounded well under the pending entry's TTL (10 min);
/// the client uses a comparable connect timeout, and a client that gives up first closes the
/// connection (the host stops waiting at once).
const PENDING_APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_secs(180);

/// The host side of the SPAKE2 pairing ceremony (see `punktfunk_core::quic::pake`):
/// generate + display a PIN, run SPAKE2 as B binding both cert fingerprints, verify the
/// client's key-confirmation MAC (its single online guess), and persist the client's
/// fingerprint on success.
async fn pair_ceremony(
    conn: &quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    req: PairRequest,
    host_fp: &[u8; 32],
    np: &NativePairing,
    pin: &str,
) -> Result<()> {
    use punktfunk_core::quic::pake;
    let client_fp = endpoint::peer_fingerprint(conn)
        .ok_or_else(|| anyhow!("pairing requires the client to present a certificate"))?;

    tracing::info!(
        name = %req.name,
        client = %fingerprint_hex(&client_fp),
        "PAIRING REQUEST — verifying against the armed PIN"
    );

    // SPAKE2 as B; bind our own host_fp + the client cert we actually received.
    let (pake, spake_b) = pake::start(false, pin, &client_fp, host_fp);
    let confirms = pake.finish(&req.spake_a)?; // Err only on a malformed peer message

    io::write_msg(
        &mut send,
        &PairChallenge {
            spake_b,
            confirm: confirms.host,
        }
        .encode(),
    )
    .await?;

    // SINGLE-USE PIN: we've now sent the host key-confirmation, which lets the client TEST this one
    // guess (a right PIN → its proof will match; a wrong PIN → the client detects the mismatch and
    // aborts *without* sending its proof). So consume the PIN HERE — before reading the proof —
    // regardless of the outcome: an attacker gets EXACTLY ONE online guess (the documented guarantee),
    // not an unbounded brute-force of the 4-digit space against a static, never-rotating PIN. A
    // malformed request that errored at `pake.finish` above never reached here, so it doesn't burn the
    // window (no DoS from garbage). The operator re-arms (web console / restart) for the next device —
    // including after a successful pair; the protocol gives no reliable host-observable "wrong PIN"
    // signal to scope this to failures only (the client just disconnects).
    np.disarm();

    let proof = tokio::time::timeout(PAIRING_TIMEOUT, io::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("pairing timed out waiting for the client's confirmation"))??;
    let proof = PairProof::decode(&proof).map_err(|e| anyhow!("PairProof decode: {e:?}"))?;

    // A wrong PIN (or a MITM with mismatched cert views) yields a different SPAKE2 key, so
    // the client's confirmation MAC won't match ours — one online attempt, no offline search.
    let ok = pake::verify(&confirms.client, &proof.confirm);

    if ok {
        if let Err(e) = np.add(&req.name, &fingerprint_hex(&client_fp)) {
            tracing::error!(error = %format!("{e:#}"), "could not persist paired clients");
        }
        tracing::info!(name = %req.name, "pairing complete — client trusted");
    } else {
        tracing::warn!(name = %req.name, "pairing FAILED (wrong PIN) — fingerprint not stored");
    }
    io::write_msg(&mut send, &PairResult { ok }.encode()).await?;
    let _ = send.finish();
    // Wait for the client to acknowledge by closing, so the PairResult isn't dropped by our
    // close on a slow link (bounded so a vanished client can't wedge the sequential host).
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;
    conn.close(0u32.into(), b"pairing done");
    anyhow::ensure!(ok, "pairing rejected (wrong PIN)");
    Ok(())
}

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
        let client_fp = endpoint::peer_fingerprint(&conn)
            .ok_or_else(|| anyhow!("pairing requires the client to present a certificate"))?;
        let client_fp_hex = fingerprint_hex(&client_fp);
        // Resolve the live arming PIN per attempt (so a lapsed window no longer pairs), honoring any
        // fingerprint binding.
        let pin = match np.pin_for_attempt(&client_fp_hex) {
            crate::native_pairing::PinAttempt::Pin(pin) => pin,
            crate::native_pairing::PinAttempt::Disarmed => anyhow::bail!(
                "pairing not armed (arm it in the console, or start with --allow-pairing)"
            ),
            // Armed for a DIFFERENT device — reject without running the ceremony, so this attempt does
            // NOT consume (burn) the operator's window for the device they actually selected (#9).
            crate::native_pairing::PinAttempt::BoundToOther => anyhow::bail!(
                "pairing is armed for a different device — this attempt does not consume the window"
            ),
        };
        {
            let mut last = last_pairing.lock().unwrap();
            if let Some(t) = *last {
                anyhow::ensure!(
                    t.elapsed() >= PAIRING_COOLDOWN,
                    "pairing rate-limited — retry shortly"
                );
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
        anyhow::ensure!(
            gate_hello.abi_version == punktfunk_core::WIRE_VERSION,
            "wire version mismatch: client {} host {}",
            gate_hello.abi_version,
            punktfunk_core::WIRE_VERSION
        );
        let fp = endpoint::peer_fingerprint(&conn);
        let known = fp
            .as_ref()
            .map(|fp| np.is_paired(&fingerprint_hex(fp)))
            .unwrap_or(false);
        if !known {
            // An anonymous client (no certificate) has no identity to approve — reject outright
            // (the PIN ceremony is its way in). Mirrors the prior behavior for anonymous knocks.
            let Some(fp) = fp else {
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
                PairingDecision::Denied => anyhow::bail!("pairing request denied in the console"),
                PairingDecision::TimedOut => anyhow::bail!(
                    "pairing request not approved within {PENDING_APPROVAL_WAIT:?} \
                     — the device can knock again"
                ),
                PairingDecision::Superseded => anyhow::bail!(
                    "parked knock superseded by a newer connection from the same device — \
                     only the newest is admitted on approval"
                ),
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
    let handshake = async {
        let mut hello = Hello::decode(&first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
        anyhow::ensure!(
            hello.abi_version == punktfunk_core::WIRE_VERSION,
            "wire version mismatch: client {} host {}",
            hello.abi_version,
            punktfunk_core::WIRE_VERSION
        );
        // The pairing gate (require_pairing → paired? else park for delegated approval) ran above,
        // before this future, so a client reaching here is paired (or the host is `--open`).

        // Codec negotiation: pick the one codec this host will emit (its GPU-probed backend
        // capability ∩ the client's advertised codecs, honoring the client's soft preference).
        // A GPU-less software host emits H.264 only, so an HEVC-only client shares nothing with
        // it → refuse honestly rather than send a stream it can't decode.
        let host_codecs = crate::encode::Codec::host_wire_caps();
        let codec_bit =
            punktfunk_core::quic::resolve_codec(hello.video_codecs, host_codecs, hello.preferred_codec)
                .ok_or_else(|| {
                anyhow!(
                    "no shared video codec: client advertised 0x{:02x}, host can emit 0x{:02x} \
                     (a software-encode host produces H.264 — the client must advertise CODEC_H264)",
                    hello.video_codecs,
                    host_codecs
                )
            })?;
        let codec = crate::encode::Codec::from_wire(codec_bit);
        tracing::info!(
            ?codec,
            client_codecs = format_args!("0x{:02x}", hello.video_codecs),
            host_codecs = format_args!("0x{host_codecs:02x}"),
            "video codec negotiated"
        );

        // Mode-conflict ADMISSION (Stage 4): a DIFFERENT client connecting while another client's
        // session is live is resolved by the `mode_conflict` policy BEFORE the Welcome — `separate`
        // (default, no change), `join` (serve at the live mode — an honest downgrade the client
        // renders from the Welcome), `steal` (preempt the victim), or `reject` (refuse the handshake).
        // A same-client reconnect never conflicts. THIS session registers in the live set once its
        // data plane is up (below the handshake), so a later client can see + steal it.
        {
            use crate::vdisplay::admission::{admit, preempt_same_identity, Admission};
            let peer_fp = endpoint::peer_fingerprint(&conn);

            // Same-client RECONNECT preempt (design §5.3 "preempts downstream"): if THIS client
            // already has a live session, it's the zombie of an unwanted disconnect whose QUIC idle
            // timer hasn't fired yet (detection lags a drop by up to `max_idle_timeout`). Signal it to
            // stop and give it the release grace so it tears its display down — which, keep-alive on,
            // lingers — and THIS reconnect REUSES that kept display below instead of landing on a
            // fresh SECOND one. Independent of the mode_conflict arm (it's our OWN prior session, not
            // a conflict with a different client), and it runs before we register ourselves so we
            // never signal our own stop flag.
            let own_zombies = preempt_same_identity(peer_fp);
            if !own_zombies.is_empty() {
                tracing::info!(
                    count = own_zombies.len(),
                    "reconnect: preempting this client's own zombie session(s) so the kept display is reused"
                );
                for z in &own_zombies {
                    z.store(true, Ordering::SeqCst);
                }
                // Same blind release grace the steal path uses — lets the zombie's loops notice the
                // stop flag and drop its display (→ Lingering) before we acquire below.
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            }

            match admit(peer_fp) {
                Admission::Separate => {}
                Admission::Join(m) => {
                    tracing::info!(
                        requested =
                            %format_args!("{}x{}@{}", hello.mode.width, hello.mode.height, hello.mode.refresh_hz),
                        live = %format_args!("{}x{}@{}", m.0, m.1, m.2),
                        "mode-conflict: JOIN — admitting at the live display's mode"
                    );
                    hello.mode.width = m.0;
                    hello.mode.height = m.1;
                    hello.mode.refresh_hz = m.2;
                }
                Admission::Steal(victims) => {
                    tracing::info!(
                        victims = victims.len(),
                        "mode-conflict: STEAL — preempting the live session(s)"
                    );
                    for v in &victims {
                        v.store(true, Ordering::SeqCst);
                    }
                    // Give the victims the release grace to tear their display down before we acquire.
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                }
                Admission::Reject(reason) => {
                    tracing::warn!("mode-conflict: REJECT — {reason}");
                    // Deliver the reason to the client as a TYPED refusal: close the QUIC connection
                    // with the BUSY application code + the reason bytes, which the client reads from
                    // the `ApplicationClosed` error (so its UI can say "host is streaming X to <name>")
                    // instead of seeing a bare connection drop. Then end the handshake.
                    conn.close(REJECT_BUSY_CODE.into(), reason.as_bytes());
                    anyhow::bail!("{reason}");
                }
            }
        }

        crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
            .context("client-requested mode")?;

        // Resolve the client's compositor preference to a concrete backend *now*, so the Welcome
        // can report what we'll actually drive. Only the Virtual source has a compositor; the
        // synthetic source has no virtual output. Blocking probes → spawn_blocking.
        let compositor = match source {
            Punktfunk1Source::Virtual => {
                let pref = hello.compositor;
                // Dedicated game session (B0): a launching client under `game_session=dedicated`
                // (gamescope available) gets its own headless gamescope spawn at the client mode. Gate on
                // whether the launch id actually RESOLVES to a command in the host's library — an unknown
                // id must fall back to normal auto routing, not a blank "sleep infinity" gamescope
                // (review #9). (dedicated is Linux-only; the resolver is the non-Windows launch_command.)
                #[cfg(not(target_os = "windows"))]
                let has_resolvable_launch = hello
                    .launch
                    .as_deref()
                    .and_then(crate::library::launch_command)
                    .is_some();
                #[cfg(target_os = "windows")]
                let has_resolvable_launch = false;
                let dedicated =
                    crate::vdisplay::wants_dedicated_game_session(has_resolvable_launch);
                Some(
                    tokio::task::spawn_blocking(move || resolve_compositor(pref, dedicated))
                        .await
                        .context("resolve compositor task")??,
                )
            }
            Punktfunk1Source::Synthetic => None,
        };

        // A requested library launch (the client sends only the store-qualified id; we look it up
        // in OUR library so a client can't inject a command) is resolved below — after the Welcome,
        // where it's threaded per-session into the data plane as `SessionContext.launch` (no
        // process-global env: the old `PUNKTFUNK_GAMESCOPE_APP` write leaked across sessions, and
        // only gamescope's bare-spawn path ever read it, so launches on every other backend were
        // silently dropped).

        // Resolve the client's gamepad-backend preference (pure env/cfg check — no probing
        // needed; the actual pads are created lazily by the input thread).
        let gamepad = resolve_gamepad(hello.gamepad);

        // Resolve the encoder bitrate (client request clamped to a sane range, or host default).
        let bitrate_kbps = resolve_bitrate_kbps(hello.bitrate_kbps);
        tracing::info!(
            requested_kbps = hello.bitrate_kbps,
            resolved_kbps = bitrate_kbps,
            "encoder bitrate"
        );

        // Resolve the audio channel count (client request → stereo / 5.1 / 7.1). The capturer opens
        // at this count: PipeWire synthesizes the requested positions (padding with silence when the
        // sink has fewer), WASAPI loopback up/downmixes via AUTOCONVERTPCM — so a client always gets
        // the channels it asked for, and the Welcome echoes the value the audio thread will encode.
        let audio_channels = resolve_audio_channels(hello.audio_channels);
        tracing::info!(
            requested = hello.audio_channels,
            resolved = audio_channels,
            "audio channels"
        );

        // Resolve the encode bit depth: HEVC Main10 only when the client advertised it AND the host
        // opted in (PUNKTFUNK_10BIT). A client that can't decode 10-bit (caps bit clear, or an older
        // client) always gets the 8-bit stream. PUNKTFUNK_10BIT is the host policy gate until a
        // mgmt/console toggle replaces it. 10-bit is HEVC-only (like the 4:4:4 gate below): now that
        // the client can steer the codec to H.264/AV1, a non-HEVC session must stay 8-bit — the
        // encoders' 10-bit path is Main10, and AV1 10-bit stays off until live-confirmed (the same
        // stance as the GameStream Main10 advertisement).
        let host_wants_10bit = crate::config::config().ten_bit;
        let client_supports_10bit = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_10BIT != 0;
        let bit_depth: u8 =
            if host_wants_10bit && client_supports_10bit && codec == crate::encode::Codec::H265 {
                10
            } else {
                8
            };
        tracing::info!(
            bit_depth,
            host_wants_10bit,
            client_supports_10bit,
            client_video_caps = hello.video_caps,
            "encode bit depth"
        );

        // Resolve the chroma subsampling: full-chroma HEVC 4:4:4 only when ALL of — the host
        // allows it (PUNKTFUNK_444, default ON; the CLIENT's 4:4:4 setting — default OFF — is the
        // per-session policy switch behind VIDEO_CAP_444), the client advertised VIDEO_CAP_444,
        // the session is single-process (the two-process WGC relay encodes 4:2:0 in v1), and the
        // active GPU/driver actually supports a 4:4:4 encode (probed, cached). The native path
        // always encodes HEVC. We resolve this BEFORE the Welcome so `chroma_format` reflects
        // what we'll really emit — the honest-downgrade channel: if any gate fails the client is
        // told 4:2:0 before it builds its decoder. The probe opens a tiny encoder; it runs only
        // when the earlier gates pass and is cached after the first.
        let host_wants_444 = crate::config::config().four_four_four;
        let client_supports_444 = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_444 != 0;
        // The active capturer must be able to deliver a full-chroma (RGB) source — the honest-downgrade
        // gate. Linux's portal capturer can; the Windows IDD-push path delivers subsampled NV12/P010
        // today (full-chroma IDD-push capture is a follow-up), so it returns false there and the host
        // negotiates 4:2:0. (Replaces the old `single_process` gate — single-process is now the only
        // topology, and 4:4:4 routed to DDA, which was removed.)
        let capture_supports_444 = crate::capture::capturer_supports_444();
        // The GPU probe opens a real (tiny) encoder on first use, so run it off the reactor like the
        // compositor probe above (blocking probes → spawn_blocking). Short-circuit so it only runs when
        // the cheap gates already pass. The result is cached process-wide (a negative latches until
        // restart — acceptable: a GPU either supports HEVC 4:4:4 or it doesn't, and a transient open
        // failure here is rare since the session's own encoder isn't open yet).
        let gpu_supports_444 = if codec == crate::encode::Codec::H265
            && host_wants_444
            && client_supports_444
            && capture_supports_444
        {
            tokio::task::spawn_blocking(|| {
                crate::encode::can_encode_444(crate::encode::Codec::H265)
            })
            .await
            .context("4:4:4 capability probe task")?
        } else {
            false
        };
        let chroma = if gpu_supports_444 {
            crate::encode::ChromaFormat::Yuv444
        } else {
            crate::encode::ChromaFormat::Yuv420
        };
        tracing::info!(
            chroma = ?chroma,
            host_wants_444,
            client_supports_444,
            capture_supports_444,
            "encode chroma"
        );

        // Linux 4:4:4 rides the CPU swscale → 8-bit `YUV444P` path (see `encode/linux`) — there
        // is no 10-bit 4:4:4 input there, so a 10-bit-negotiated session would silently encode
        // 8-bit. Resolve the depth DOWN before the Welcome so the wire never overstates what the
        // stream carries. (Windows NVENC composes Main 4:4:4 10 from an RGB input, so it keeps
        // the resolved depth — this clamp is Linux-only.)
        #[cfg(target_os = "linux")]
        let bit_depth: u8 = if chroma.is_444() && bit_depth == 10 {
            tracing::info!("4:4:4 on the Linux path encodes 8-bit YUV444P — resolving bit depth 8");
            8
        } else {
            bit_depth
        };

        // Reserve the data-plane UDP socket up front and HOLD it through streaming (no
        // bind→read→drop→rebind window a concurrent session could race for a fixed port). A fixed
        // `--data-port` yields `direct = true` (stream straight to the client's reported address,
        // no punch-wait); otherwise a random ephemeral port + hole-punch.
        let (data_sock, direct) = bind_data_socket(data_port)?;
        let udp_port = data_sock.local_addr()?.port();

        let mut key = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut key);
        // Fresh per-session salt alongside the fresh key. GCM nonce uniqueness only *requires* one
        // of the two to be unique per session (the nonce is salt || sequence under the session
        // key), but a constant salt would make a key-reuse bug catastrophic instead of merely
        // wrong — this keeps the second line of defense real. Negotiated via Welcome, so clients
        // just follow.
        let mut salt = [0u8; 4];
        rand::thread_rng().fill_bytes(&mut salt);
        let welcome = Welcome {
            abi_version: punktfunk_core::WIRE_VERSION,
            udp_port,
            mode: hello.mode,
            // The post-GameStream point of punktfunk/1: Leopard GF(2¹⁶) FEC + real encryption.
            fec: FecConfig {
                scheme: FecScheme::Gf16,
                // Static override pins it; otherwise sessions start at the adaptive midpoint and the
                // host re-sizes FEC live from the client's LossReports (adaptive FEC).
                fec_percent: fec_static_override().unwrap_or(FEC_ADAPTIVE_START),
                max_data_per_block: 4096,
            },
            // The largest even payload whose sealed datagram (header + shard + crypto) fits an
            // unfragmented UDP packet on a 1500 MTU for THIS client's address family — 1408 over
            // IPv4 (1472 = the exact ceiling), 1388 over IPv6 (40-byte header, and v6 routers
            // don't fragment: overshooting there blackholes instead of degrading). The data plane
            // dials the same family as this QUIC connection, so the remote decides. The previous
            // hardcoded 1452 overshot the v4 ceiling (its math forgot the header/crypto ride
            // inside the UDP payload) and silently IP-fragmented EVERY video datagram, doubling
            // per-datagram loss on Wi-Fi — the "100 Mbps badly fails on the phone" root cause.
            // Negotiated, so the client follows. Jumbo (≈8900) is a future negotiated bump (needs
            // MAX_DATAGRAM_BYTES raised + end-to-end 9000 MTU).
            shard_payload: mtu1500_shard_payload_for(peer.ip()) as u16,
            encrypt: true,
            key,
            salt,
            frames: match source {
                Punktfunk1Source::Synthetic => frames,
                Punktfunk1Source::Virtual => 0, // unbounded — client streams until we close
            },
            // Report the resolved backends back to the client (compositor: Auto for the
            // synthetic source).
            compositor: compositor
                .map(|c| c.as_pref())
                .unwrap_or(CompositorPref::Auto),
            gamepad,
            bitrate_kbps,
            bit_depth,
            // Colour signalling the client configures its decoder/presenter from. A negotiated
            // 10-bit session is our HDR path (BT.2020 PQ — what the NVENC HEVC VUI emits from a
            // 10-bit capture format); 8-bit stays BT.709 SDR. The mastering metadata (ST.2086 +
            // CLL) rides the 0xCE datagram below. (A future step can refine this to the capturer's
            // actual monitor HDR state and announce a mid-stream flip.)
            color: if bit_depth >= 10 {
                ColorInfo::HDR10_BT2020_PQ
            } else {
                ColorInfo::SDR_BT709
            },
            // The chroma the encoder will actually emit (resolved + GPU-probed above) — 4:4:4 only
            // when every gate passed, else 4:2:0. The client sizes its decoder from this.
            chroma_format: chroma.idc(),
            // The resolved audio channel count the audio thread will capture + Opus-(multi)stream
            // encode (2/6/8). The client builds its decoder from this echoed value.
            audio_channels,
            // The negotiated codec the encoder will emit (client preference ∩ GPU capability;
            // HEVC-precedence tie-break). The client builds its decoder from this instead of
            // assuming HEVC.
            codec: codec_bit,
            // This host applies sequence-gated gamepad-state snapshots (InputKind::GamepadState),
            // so capable clients send those instead of the loss-fragile per-transition events.
            host_caps: punktfunk_core::quic::HOST_CAP_GAMEPAD_STATE,
        };
        io::write_msg(&mut send, &welcome.encode()).await?;

        let start = Start::decode(&io::read_msg(&mut recv).await?)
            .map_err(|e| anyhow!("Start decode: {e:?}"))?;
        Ok::<_, anyhow::Error>((
            hello, welcome, udp_port, data_sock, direct, start, compositor,
        ))
    };
    let (hello, welcome, udp_port, data_sock, direct, start, compositor) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (mut ctrl_send, mut ctrl_recv) = (send, recv);
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
    let (probe_result_tx, mut probe_result_rx) =
        tokio::sync::mpsc::unbounded_channel::<ProbeResult>();
    // Mode-switch outcome, data plane → control task (same pattern as `probe_result_tx`): the accept
    // ack is written BEFORE the rebuild, so a failed rebuild (host stays at the old mode) or a
    // backend that honored a different refresh must CORRECT the client's mode slot with a second
    // `Reconfigured { accepted: true, mode: <actually live> }` — the client handler treats any
    // accepted ack as "the active mode is now X" and fixes itself; old clients just log it.
    let (reconfig_result_tx, mut reconfig_result_rx) =
        tokio::sync::mpsc::unbounded_channel::<Reconfigured>();
    // Adaptive FEC: the control task maps each client LossReport to a recovery percent and publishes
    // it here; the data-plane send loop reads + applies it per frame. Disabled (pinned) when
    // PUNKTFUNK_FEC_PCT is set. Seeded with the session's starting FEC so it's a no-op until a report.
    let adaptive_fec = fec_static_override().is_none();
    let fec_target = Arc::new(AtomicU8::new(welcome.fec.fec_percent));
    let fec_target_ctl = fec_target.clone();
    tokio::spawn(async move {
        let mut active = hello.mode;
        // Host-side switch rate limit (a backstop against a hostile/broken client spamming
        // Reconfigure into pipeline-rebuild churn — the drain-to-newest in the data plane already
        // coalesces a well-behaved resize drag; compliant clients self-limit to ≥ 1 s).
        const MIN_SWITCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
        let mut last_accepted_switch: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                msg = io::read_msg(&mut ctrl_recv) => {
                    let Ok(msg) = msg else { break }; // stream closed
                    if let Ok(req) = Reconfigure::decode(&msg) {
                        let now = std::time::Instant::now();
                        let valid = req.mode.refresh_hz > 0
                            && crate::encode::validate_dimensions(
                                codec,
                                req.mode.width,
                                req.mode.height,
                            )
                            .is_ok();
                        let too_soon = last_accepted_switch
                            .is_some_and(|t| now.duration_since(t) < MIN_SWITCH_INTERVAL);
                        let ok = if !live_reconfig_ok {
                            // Backend can't live-reconfigure (gamescope / synthetic /
                            // per-client-mode identity — see the gate above): honest downgrade,
                            // the client keeps scaling client-side.
                            tracing::info!(mode = ?req.mode,
                                "mode switch rejected (backend cannot live-reconfigure)");
                            false
                        } else if !valid {
                            tracing::warn!(mode = ?req.mode, "mode switch rejected (invalid dimensions)");
                            false
                        } else if too_soon {
                            tracing::warn!(mode = ?req.mode, "mode switch rejected (rate-limited)");
                            false
                        } else {
                            true
                        };
                        if ok {
                            active = req.mode;
                            last_accepted_switch = Some(now);
                            tracing::info!(mode = ?req.mode, "mode switch accepted");
                        }
                        let ack = Reconfigured { accepted: ok, mode: active };
                        if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                            break;
                        }
                        if ok && reconfig_tx.send(req.mode).is_err() {
                            break; // data plane gone
                        }
                    } else if RequestKeyframe::decode(&msg).is_ok() {
                        // Client recovery: its decoder wedged — force the next encoded frame to
                        // be an IDR. Coalesced in the encode loop (a wedge fires several before
                        // the IDR lands); a send error just means the data plane is gone.
                        tracing::debug!("client requested keyframe (decode recovery)");
                        if keyframe_tx.send(()).is_err() {
                            break; // data plane gone
                        }
                    } else if let Ok(req) = RfiRequest::decode(&msg) {
                        // Client LTR-RFI recovery: it lost the frame range `[first, last]` and asks
                        // the encoder to re-reference a known-good older frame instead of paying for
                        // a full IDR. The encode loop attempts `invalidate_ref_frames`, falling back
                        // to a coalesced keyframe when the encoder can't (range too old / no RFI).
                        tracing::debug!(
                            first = req.first_frame,
                            last = req.last_frame,
                            "client requested reference-frame invalidation (loss recovery)"
                        );
                        if rfi_tx.send((req.first_frame, req.last_frame)).is_err() {
                            break; // data plane gone
                        }
                    } else if let Ok(rep) = LossReport::decode(&msg) {
                        // Adaptive FEC: size recovery to the loss the client is seeing. The data-plane
                        // send loop reads `fec_target_ctl` and applies it per frame. Ignored when FEC
                        // is pinned via PUNKTFUNK_FEC_PCT.
                        if adaptive_fec {
                            // Fast attack, slow decay: jump straight to what the reported loss
                            // needs, but come DOWN only one point per clean report (~750 ms). The
                            // memoryless controller ping-ponged on periodic burst loss (Wi-Fi
                            // scans / BT coexistence, a burst every few seconds): a single clean
                            // window dropped FEC back to the floor, so every next burst hit an
                            // unprotected stream — an unrecoverable frame, a freeze, and a
                            // recovery-IDR burst, once per cycle. Decaying over ~10 windows keeps
                            // the stream covered across the gap while still converging to FEC_MIN
                            // on a genuinely clean link.
                            let prev = fec_target_ctl.load(Ordering::Relaxed);
                            let target = adapt_fec(rep.loss_ppm).max(prev.saturating_sub(1));
                            fec_target_ctl.store(target, Ordering::Relaxed);
                            if prev != target {
                                tracing::info!(
                                    loss_ppm = rep.loss_ppm,
                                    fec_pct = target,
                                    prev_fec_pct = prev,
                                    "adaptive FEC adjusted"
                                );
                            }
                        }
                    } else if let Ok(req) = SetBitrate::decode(&msg) {
                        // Mid-stream bitrate renegotiation (adaptive bitrate): clamp exactly like
                        // the Hello request, ack the resolved value, then hand it to the data-plane
                        // thread, which rebuilds the encoder in place at the same mode — the fresh
                        // encoder's first frame is an IDR with in-band parameter sets, so the
                        // client's decoder follows without a reconnect.
                        let resolved = resolve_bitrate_kbps(req.bitrate_kbps);
                        tracing::info!(
                            requested_kbps = req.bitrate_kbps,
                            resolved_kbps = resolved,
                            "mid-stream bitrate change requested"
                        );
                        let ack = BitrateChanged {
                            bitrate_kbps: resolved,
                        };
                        if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                            break;
                        }
                        if bitrate_tx.send(resolved).is_err() {
                            break; // data plane gone
                        }
                    } else if let Ok(req) = ProbeRequest::decode(&msg) {
                        tracing::info!(
                            target_kbps = req.target_kbps,
                            duration_ms = req.duration_ms,
                            "speed-test probe requested"
                        );
                        if probe_tx.send(req).is_err() {
                            break; // data plane gone
                        }
                    } else if let Ok(probe) = ClockProbe::decode(&msg) {
                        // Wall-clock skew handshake: echo the client's t1 with our receive (t2) and
                        // send (t3) stamps, both in the host clock the AU pts_ns uses. Answered
                        // inline on the control stream — cheap, no data-plane involvement.
                        let t2_ns = now_ns();
                        let echo = ClockEcho {
                            t1_ns: probe.t1_ns,
                            t2_ns,
                            t3_ns: now_ns(),
                        };
                        if io::write_msg(&mut ctrl_send, &echo.encode()).await.is_err() {
                            break;
                        }
                    } else {
                        tracing::warn!("unknown control message — ignoring");
                    }
                }
                result = probe_result_rx.recv() => {
                    let Some(result) = result else { break }; // data plane gone
                    if io::write_msg(&mut ctrl_send, &result.encode()).await.is_err() {
                        break;
                    }
                }
                correction = reconfig_result_rx.recv() => {
                    // H2 rollback/correction ack: the data plane reports the mode ACTUALLY live
                    // after a rebuild that failed (stayed at the old mode) or that the backend
                    // honored at a different refresh. Track it so a later rejection's
                    // `mode: active` echo is truthful too.
                    let Some(ack) = correction else { break }; // data plane gone
                    active = ack.mode;
                    if io::write_msg(&mut ctrl_send, &ack.encode()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

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

    // Stop signal: stream duration elapsed or the client went away.
    let stop = Arc::new(AtomicBool::new(false));
    // Deliberate-quit signal: set (before `stop`, so the display lease reads it on teardown) when the
    // client closed the connection with `QUIT_CODE` — a user "stop", which skips the keep-alive linger.
    // A bare disconnect / idle timeout leaves it false → the display lingers for a reconnect.
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
            .map_err(|e| tracing::error!(error = %e, "audio thread spawn failed — session continues without audio"))
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
        let meta = hello.display_hdr.unwrap_or_else(crate::hdr::generic_hdr10);
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
    let bitrate_kbps = welcome.bitrate_kbps; // resolved encoder bitrate (Hello clamped, or default)
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
                    virtual_stream(SessionContext {
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
                    })
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

/// Per-pad accumulated state: punktfunk/1 gamepad events are incremental (one button or axis
/// per datagram, see `punktfunk_core::input::gamepad`), the virtual xpad applies full frames.
/// A snapshot-capable client replaces the whole state at once ([`PadState::set_snapshot`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PadState {
    buttons: u32,
    left_trigger: u8,
    right_trigger: u8,
    ls_x: i16,
    ls_y: i16,
    rs_x: i16,
    rs_y: i16,
}

impl PadState {
    /// Fold one wire event into the state. `false` = unknown axis id (event dropped).
    fn apply(&mut self, ev: &InputEvent) -> bool {
        if ev.kind == InputKind::GamepadButton {
            if ev.x != 0 {
                self.buttons |= ev.code;
            } else {
                self.buttons &= !ev.code;
            }
            return true;
        }
        use punktfunk_core::input::gamepad::*;
        let stick = ev.x.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
        let trigger = ev.x.clamp(0, 255) as u8;
        match ev.code {
            AXIS_LS_X => self.ls_x = stick,
            AXIS_LS_Y => self.ls_y = stick,
            AXIS_RS_X => self.rs_x = stick,
            AXIS_RS_Y => self.rs_y = stick,
            AXIS_LT => self.left_trigger = trigger,
            AXIS_RT => self.right_trigger = trigger,
            _ => return false,
        }
        true
    }

    /// Replace the whole state from one client snapshot (the [`InputKind::GamepadState`] form).
    fn set_snapshot(&mut self, s: &punktfunk_core::input::GamepadSnapshot) {
        self.buttons = s.buttons;
        self.left_trigger = s.left_trigger;
        self.right_trigger = s.right_trigger;
        self.ls_x = s.ls_x;
        self.ls_y = s.ls_y;
        self.rs_x = s.rs_x;
        self.rs_y = s.rs_y;
    }

    fn frame(&self, index: usize, active_mask: u16) -> crate::gamestream::gamepad::GamepadFrame {
        crate::gamestream::gamepad::GamepadFrame {
            index: index as i16,
            active_mask,
            buttons: self.buttons,
            left_trigger: self.left_trigger,
            right_trigger: self.right_trigger,
            ls_x: self.ls_x,
            ls_y: self.ls_y,
            rs_x: self.rs_x,
            rs_y: self.rs_y,
        }
    }
}

/// Highest pad index addressable on the wire (`flags` field / snapshot `pad`); the uinput
/// manager caps actual pad creation at its own MAX_PADS.
const MAX_WIRE_PADS: usize = punktfunk_core::input::MAX_PADS;

/// Backoff between reopen attempts after a host-lifetime service's backend (a capturer) fails
/// to open or its worker dies, so a persistently-unavailable resource isn't hammered. (The
/// virtual mic has its own tuning — see [`crate::audio::MicPump`].)
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Per-pad virtual-gamepad router: each pad index is served by a backend of that pad's declared
/// kind ([`InputKind::GamepadArrival`](punktfunk_core::input::InputKind::GamepadArrival)), so ONE
/// session can MIX controller types — pad 0 a DualSense, pad 1 an Xbox pad. A pad the client never
/// declares uses `default` (the session kind resolved from the Hello — the pre-existing single-kind
/// behaviour).
///
/// Backends are created lazily per kind (an empty manager holds no device), and each owns only the
/// indices routed to it. A manager's `active_mask` unplug sweep stays correct across managers
/// because an index another manager owns is `None` in this one, so the sweep never touches it.
///
/// - Xbox 360 / One — uinput on Linux ([`GamepadManager`](crate::inject::gamepad::GamepadManager),
///   two identities), the XUSB companion driver (classic XInput) on Windows.
/// - DualSense / DualSense Edge / DualShock 4 — Linux UHID `hid-playstation`, or the Windows UMDF
///   minidriver (device-type 0/2/1).
/// - Steam Deck — Linux UHID `hid-steam` (or usbip/gadget), or the Windows UMDF minidriver
///   (device-type 3, Steam-Input-promoted).
///
/// [`resolve_pad_kind`] folds any kind a platform can't build into one it can, so this never
/// constructs a manager the build lacks.
struct Pads {
    /// Declared (and host-resolved) kind per pad index; `default` until a `GamepadArrival` lands.
    kinds: [GamepadPref; MAX_WIRE_PADS],
    /// The kind of the manager that currently OWNS a built device at each index (`None` = no
    /// device). A live device stays in its manager even if `kinds[idx]` later changes (the rare
    /// arrival-after-first-frame reorder), so a pad is never duplicated across managers and its
    /// removal always reaches the manager that actually holds it.
    owner: [Option<GamepadPref>; MAX_WIRE_PADS],
    xbox360: Option<crate::inject::gamepad::GamepadManager>,
    #[cfg(target_os = "linux")]
    xboxone: Option<crate::inject::gamepad::GamepadManager>,
    #[cfg(target_os = "linux")]
    dualsense: Option<crate::inject::dualsense::DualSenseManager>,
    #[cfg(target_os = "linux")]
    dualsense_edge: Option<crate::inject::dualsense::DualSenseEdgeManager>,
    #[cfg(target_os = "linux")]
    dualshock4: Option<crate::inject::dualshock4::DualShock4Manager>,
    #[cfg(target_os = "linux")]
    steamdeck: Option<crate::inject::steam_controller::SteamControllerManager>,
    #[cfg(target_os = "linux")]
    switchpro: Option<crate::inject::switch_pro::SwitchProManager>,
    #[cfg(target_os = "linux")]
    steamctrl: Option<crate::inject::steam_controller::SteamCtrlManager>,
    #[cfg(target_os = "windows")]
    dualsense_win: Option<crate::inject::dualsense_windows::DualSenseWindowsManager>,
    #[cfg(target_os = "windows")]
    dualsense_edge_win: Option<crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager>,
    #[cfg(target_os = "windows")]
    dualshock4_win: Option<crate::inject::dualshock4_windows::DualShock4WindowsManager>,
    #[cfg(target_os = "windows")]
    steamdeck_win: Option<crate::inject::steam_deck_windows::SteamDeckWindowsManager>,
}

impl Pads {
    /// `default` is the session kind (see [`resolve_gamepad`]); every pad starts on it until the
    /// client declares its own kind.
    fn new(default: GamepadPref) -> Pads {
        let default = resolve_pad_kind(default);
        tracing::info!(
            default = default.as_str(),
            "gamepad backends: per-pad router (session default)"
        );
        Pads {
            kinds: [default; MAX_WIRE_PADS],
            owner: [None; MAX_WIRE_PADS],
            xbox360: None,
            #[cfg(target_os = "linux")]
            xboxone: None,
            #[cfg(target_os = "linux")]
            dualsense: None,
            #[cfg(target_os = "linux")]
            dualsense_edge: None,
            #[cfg(target_os = "linux")]
            dualshock4: None,
            #[cfg(target_os = "linux")]
            steamdeck: None,
            #[cfg(target_os = "linux")]
            switchpro: None,
            #[cfg(target_os = "linux")]
            steamctrl: None,
            #[cfg(target_os = "windows")]
            dualsense_win: None,
            #[cfg(target_os = "windows")]
            dualsense_edge_win: None,
            #[cfg(target_os = "windows")]
            dualshock4_win: None,
            #[cfg(target_os = "windows")]
            steamdeck_win: None,
        }
    }

    /// Record a pad's client-declared kind (resolved to a buildable backend). Takes effect on the
    /// pad's next frame; the arrival is sent before the pad's first input, so a device already
    /// built under the wrong kind is only the rare arrival-after-first-frame reorder — it then
    /// keeps the earlier kind until re-plug (no live device swap).
    fn set_kind(&mut self, idx: usize, kind: GamepadPref) {
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let resolved = resolve_pad_kind(kind);
        if self.kinds[idx] != resolved {
            tracing::info!(
                pad = idx,
                kind = resolved.as_str(),
                "gamepad kind declared (per-pad)"
            );
        }
        self.kinds[idx] = resolved;
    }

    fn handle(&mut self, ev: &crate::gamestream::gamepad::GamepadEvent) {
        use crate::gamestream::gamepad::GamepadEvent;
        // Present = a create/update frame (the pad's mask bit is set); a cleared bit is the
        // removal frame emitted by the native detach path (`GamepadRemove`).
        let (idx, present) = match ev {
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                (idx, f.active_mask & (1 << idx) != 0)
            }
            GamepadEvent::Arrival { index, .. } => (*index as usize, true),
        };
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let (kind, new_owner) = route_decision(self.owner[idx], self.kinds[idx], present);
        self.owner[idx] = new_owner;
        self.route_handle(kind, ev);
    }

    /// Dispatch a decoded event to the manager for `kind`, creating it lazily.
    fn route_handle(&mut self, kind: GamepadPref, ev: &crate::gamestream::gamepad::GamepadEvent) {
        match kind {
            #[cfg(target_os = "linux")]
            GamepadPref::DualSense => self
                .dualsense
                .get_or_insert_with(crate::inject::dualsense::DualSenseManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::DualSenseEdge => self
                .dualsense_edge
                .get_or_insert_with(crate::inject::dualsense::DualSenseEdgeManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::DualShock4 => self
                .dualshock4
                .get_or_insert_with(crate::inject::dualshock4::DualShock4Manager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SteamDeck => self
                .steamdeck
                .get_or_insert_with(crate::inject::steam_controller::SteamControllerManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SwitchPro => self
                .switchpro
                .get_or_insert_with(crate::inject::switch_pro::SwitchProManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController => self
                .steamctrl
                .get_or_insert_with(crate::inject::steam_controller::SteamCtrlManager::new)
                .handle(ev),
            #[cfg(target_os = "linux")]
            GamepadPref::XboxOne => self
                .xboxone
                .get_or_insert_with(|| {
                    crate::inject::gamepad::GamepadManager::with_identity(
                        crate::inject::gamepad::PadIdentity::xbox_one(),
                    )
                })
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::DualSense => self
                .dualsense_win
                .get_or_insert_with(crate::inject::dualsense_windows::DualSenseWindowsManager::new)
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::DualSenseEdge => self
                .dualsense_edge_win
                .get_or_insert_with(
                    crate::inject::dualsense_edge_windows::DualSenseEdgeWindowsManager::new,
                )
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::DualShock4 => self
                .dualshock4_win
                .get_or_insert_with(
                    crate::inject::dualshock4_windows::DualShock4WindowsManager::new,
                )
                .handle(ev),
            #[cfg(target_os = "windows")]
            GamepadPref::SteamDeck => self
                .steamdeck_win
                .get_or_insert_with(crate::inject::steam_deck_windows::SteamDeckWindowsManager::new)
                .handle(ev),
            _ => self
                .xbox360
                .get_or_insert_with(crate::inject::gamepad::GamepadManager::new)
                .handle(ev),
        }
    }

    /// Apply a rich client→host event (touchpad / motion) to the pad's kind manager, if it exists
    /// (rich before the first frame = no device yet = a no-op anyway). The X-Box pads have no rich
    /// plane, so those indices ignore it.
    fn apply_rich(&mut self, rich: punktfunk_core::quic::RichInput) {
        use punktfunk_core::quic::RichInput;
        let idx = match rich {
            RichInput::Touchpad { pad, .. }
            | RichInput::Motion { pad, .. }
            | RichInput::TouchpadEx { pad, .. } => pad as usize,
        };
        // Route to the manager that actually owns the device (falling back to the declared kind
        // before the first frame builds it), so a pad's touchpad/motion never lands on the wrong
        // backend after a kind change.
        let kind = self
            .owner
            .get(idx)
            .copied()
            .flatten()
            .or_else(|| self.kinds.get(idx).copied())
            .unwrap_or(GamepadPref::Xbox360);
        match kind {
            #[cfg(target_os = "linux")]
            GamepadPref::DualSense => {
                if let Some(m) = &mut self.dualsense {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::DualSenseEdge => {
                if let Some(m) = &mut self.dualsense_edge {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::DualShock4 => {
                if let Some(m) = &mut self.dualshock4 {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SteamDeck => {
                if let Some(m) = &mut self.steamdeck {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SwitchPro => {
                if let Some(m) = &mut self.switchpro {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "linux")]
            GamepadPref::SteamController => {
                if let Some(m) = &mut self.steamctrl {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::DualSense => {
                if let Some(m) = &mut self.dualsense_win {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::DualSenseEdge => {
                if let Some(m) = &mut self.dualsense_edge_win {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::DualShock4 => {
                if let Some(m) = &mut self.dualshock4_win {
                    m.apply_rich(rich)
                }
            }
            #[cfg(target_os = "windows")]
            GamepadPref::SteamDeck => {
                if let Some(m) = &mut self.steamdeck_win {
                    m.apply_rich(rich)
                }
            }
            _ => {}
        }
    }

    /// Service feedback for every instantiated backend each cycle. `rumble` carries motor
    /// force-feedback on the universal plane (every backend, tagged with its own pad index);
    /// `hidout` carries rich feedback (lightbar / player LEDs / adaptive triggers) for the UHID/UMDF
    /// pads. The `&mut` closure re-borrows satisfy `FnMut` for each backend.
    fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16),
        mut hidout: impl FnMut(punktfunk_core::quic::HidOutput),
    ) {
        if let Some(m) = &mut self.xbox360 {
            m.pump_rumble(&mut rumble); // the X-Box pad has no rich-feedback plane
        }
        #[cfg(target_os = "linux")]
        {
            if let Some(m) = &mut self.xboxone {
                m.pump_rumble(&mut rumble);
            }
            if let Some(m) = &mut self.dualsense {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualsense_edge {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualshock4 {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamdeck {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.switchpro {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamctrl {
                m.pump(&mut rumble, &mut hidout);
            }
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(m) = &mut self.dualsense_win {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualsense_edge_win {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.dualshock4_win {
                m.pump(&mut rumble, &mut hidout);
            }
            if let Some(m) = &mut self.steamdeck_win {
                m.pump(&mut rumble, &mut hidout);
            }
        }
    }

    /// Keep every instantiated virtual UHID/UMDF pad alive during input silence (re-emit its HID
    /// report so the kernel driver / SDL don't drop a held-steady pad). The X-Box pads need no
    /// heartbeat (evdev holds last-known state). Per-pad gap timers inside each manager govern the
    /// actual emit cadence, not this per-tick call.
    fn heartbeat(&mut self) {
        #[cfg(target_os = "linux")]
        {
            let gap = std::time::Duration::from_millis(8);
            if let Some(m) = &mut self.dualsense {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualsense_edge {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualshock4 {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamdeck {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.switchpro {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamctrl {
                m.heartbeat(gap);
            }
        }
        #[cfg(target_os = "windows")]
        {
            let gap = std::time::Duration::from_millis(8);
            if let Some(m) = &mut self.dualsense_win {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualsense_edge_win {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.dualshock4_win {
                m.heartbeat(gap);
            }
            if let Some(m) = &mut self.steamdeck_win {
                m.heartbeat(gap);
            }
        }
    }
}

/// The per-pad routing decision for one frame ([`Pads::handle`]): given `owner` (the manager
/// holding a live device at this index, if any), the client-`declared` kind, and whether this is a
/// create/update frame (`present`) vs a removal, return `(kind to route to, new owner)`.
///
/// A live device stays in its owning manager even if the declared kind later changes (so a pad is
/// never duplicated across managers); the declared kind takes effect only when no device exists
/// yet; a removal routes to the owner's manager (so it tears the right device down) and clears the
/// owner.
fn route_decision(
    owner: Option<GamepadPref>,
    declared: GamepadPref,
    present: bool,
) -> (GamepadPref, Option<GamepadPref>) {
    match (owner, present) {
        (Some(k), true) => (k, Some(k)), // keep the existing device in its manager
        (Some(k), false) => (k, None),   // removal → owner's manager, then clear
        (None, true) => (declared, Some(declared)), // create in the declared kind's manager
        (None, false) => (declared, None), // removal with no device — a harmless no-op
    }
}

/// Resolve one client-declared per-pad kind to a backend this host can actually build (mixed
/// types): the platform map + the runtime UHID / Steam-conflict degrades that [`resolve_gamepad`]
/// applies to the session default, minus the Auto/env session logic (a per-pad declaration is
/// always a concrete kind).
fn resolve_pad_kind(kind: GamepadPref) -> GamepadPref {
    let chosen = pick_gamepad(
        kind,
        None,
        cfg!(target_os = "linux"),
        cfg!(target_os = "windows"),
    );
    degrade_steam_on_conflict(degrade_if_no_uhid(chosen))
}

/// One client→host input item, both planes on ONE channel so the input thread wakes the
/// moment either arrives (a second rich channel drained after the 4 ms recv timeout cost
/// every pure-gyro motion sample up to 4 ms of quantization).
enum ClientInput {
    /// The 0xC8 plane: pointer / keyboard / gamepad button+axis.
    Event(InputEvent),
    /// The 0xCC plane: touchpad contacts + motion samples.
    Rich(punktfunk_core::quic::RichInput),
}

/// Default TTL stamped on a non-zero rumble envelope (0xCA v2): how long the client renders the
/// level before silencing unless the host renews it. Tolerates 2–3 lost renewals (same loss
/// margin the old flat 500 ms refresh gave) while capping a host-abandoned rumble at this on every
/// client — versus the per-platform client heuristics it replaces (SDL 1.5 s, Apple 1.6 s, Android
/// up to the QUIC idle-timeout). Overridable via `PUNKTFUNK_RUMBLE_TTL_MS` (floored at
/// [`RUMBLE_TTL_FLOOR_MS`] so expiry jitter stays below the clients' tick granularity).
const RUMBLE_TTL_MS: u16 = 400;
/// Floor for the `PUNKTFUNK_RUMBLE_TTL_MS` hatch — below this the ~50 ms client ticks make expiry
/// audible (see `rumble-envelope-plan.md` §5).
const RUMBLE_TTL_FLOOR_MS: u16 = 150;
/// Ceiling for the `PUNKTFUNK_RUMBLE_TTL_MS` hatch. A lease longer than a few seconds defeats the
/// design's "an abandoned rumble stops promptly" goal, and keeping it well under `u16::MAX` means
/// the wire never emits a TTL a narrower client-side slot could mistake for a sentinel.
const RUMBLE_TTL_CEIL_MS: u16 = 5_000;
/// Floor for the derived renewal interval (renew = ttl × 3/10) so an aggressive TTL hatch can't
/// spin the renewal loop faster than this.
const RUMBLE_RENEW_FLOOR_MS: u64 = 60;
/// How many times a transition-to-zero (a stop) is re-sent on the renewal ticks after the
/// immediate stop datagram, before the pad goes quiet. Covers stop-datagram loss for legacy
/// clients (a v2 client also self-silences at TTL); even a fully lost burst heals via the client's
/// own expiry. `3` total zero sends = the immediate one + this many renewal re-sends.
const RUMBLE_STOP_BURST: u8 = 2;

/// Send one rumble datagram on the universal 0xCA plane. `envelope_on` picks the self-terminating
/// v2 form (`[level][seq][ttl_ms]`, the default) or the legacy v1 level datagram (the
/// `PUNKTFUNK_RUMBLE_ENVELOPE=0` bisect hatch). Best-effort like every side-plane datagram.
fn send_rumble(
    conn: &quinn::Connection,
    envelope_on: bool,
    pad: u16,
    low: u16,
    high: u16,
    seq: u8,
    ttl_ms: u16,
) {
    let d: Vec<u8> = if envelope_on {
        punktfunk_core::quic::encode_rumble_datagram_v2(pad, low, high, seq, ttl_ms).to_vec()
    } else {
        punktfunk_core::quic::encode_rumble_datagram(pad, low, high).to_vec()
    };
    let _ = conn.send_datagram(d.into());
}

/// The per-session input thread: route pointer/keyboard events to the host-lifetime injector
/// service (`inj_tx`) and gamepad events to this session's [`Pads`] router (`gamepad` — the
/// resolved Hello preference is the per-pad default; clients declare each pad's kind so a session
/// can mix uinput X-Box pads and virtual DualSense pads), with rich
/// client→host input (touchpad / motion, [`ClientInput::Rich`]) applied on arrival and
/// feedback pumped between events — rumble on the universal datagram plane, DualSense
/// LED/trigger feedback on the HID-output plane. The gamepads are created and torn down with
/// the session; the pointer/keyboard injector (and its portal grant) lives in the service,
/// across sessions.
///
/// Rumble is emitted as self-terminating 0xCA v2 envelopes (`[level][seq][ttl_ms]`): the host owns
/// the timeline, renewing an active level every ~`RUMBLE_TTL_MS × 3/10` ms and letting an
/// abandoned one expire client-side, so "stuck rumble" is inexpressible on the wire (see
/// `punktfunk-planning/design/rumble-envelope-plan.md`). `PUNKTFUNK_RUMBLE_ENVELOPE=0` reverts to
/// legacy v1 level datagrams + the flat 500 ms refresh (bisect hatch).
fn input_thread(
    rx: std::sync::mpsc::Receiver<ClientInput>,
    conn: quinn::Connection,
    inj_tx: std::sync::mpsc::Sender<InputEvent>,
    gamepad: GamepadPref,
) {
    let mut pads = Pads::new(gamepad);
    // Motion-cadence observability (debug level): inter-arrival percentiles per 5 s window,
    // the measurement a "gyro feels floaty" report needs. Bounded: 5 s at even a 1 kHz pad
    // is 5000 u32s.
    let mut motion_gaps_us: Vec<u32> = Vec::new();
    let mut last_motion: Option<std::time::Instant> = None;
    let mut motion_window = std::time::Instant::now();
    let mut pad_state = [PadState::default(); MAX_WIRE_PADS];
    let mut pad_mask = 0u16;
    // Last applied snapshot seq per pad (`None` until the first one): the reorder gate for
    // `InputKind::GamepadState` — a late datagram with an older seq must not roll held state back.
    let mut pad_seq: [Option<u8>; MAX_WIRE_PADS] = [None; MAX_WIRE_PADS];
    // Rumble self-terminating envelopes (0xCA v2). Each non-zero level is authorized for
    // `rumble_ttl_ms`; the host renews an active pad every `rumble_renew` and lets an abandoned
    // one expire on the client, so a dropped transition heals on the next renewal and a stop that
    // is lost heals via the stop burst (or the client's own TTL expiry). `rumble_seq` is the
    // per-pad wrapping reorder counter (bumped on changes AND renewals) the client gates on;
    // `rumble_stop_burst` counts the post-stop zero re-sends still owed. `PUNKTFUNK_RUMBLE_ENVELOPE=0`
    // reverts to legacy v1 datagrams re-sent flat every 500 ms.
    let mut rumble_state = [(0u16, 0u16); MAX_WIRE_PADS];
    let mut rumble_seen = [false; MAX_WIRE_PADS];
    let mut rumble_seq = [0u8; MAX_WIRE_PADS];
    let mut rumble_stop_burst = [0u8; MAX_WIRE_PADS];
    let mut last_refresh = std::time::Instant::now();
    let rumble_envelope_on = std::env::var("PUNKTFUNK_RUMBLE_ENVELOPE").as_deref() != Ok("0");
    let rumble_ttl_ms: u16 = std::env::var("PUNKTFUNK_RUMBLE_TTL_MS")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .map(|v| v.clamp(RUMBLE_TTL_FLOOR_MS, RUMBLE_TTL_CEIL_MS))
        .unwrap_or(RUMBLE_TTL_MS);
    // Renew at 30 % of the TTL (≈120 ms for the 400 ms default) so 2–3 renewals cover the lease;
    // in legacy mode the periodic block instead runs the old flat 500 ms full-state refresh.
    let rumble_refresh_interval = if rumble_envelope_on {
        std::time::Duration::from_millis((rumble_ttl_ms as u64 * 3 / 10).max(RUMBLE_RENEW_FLOOR_MS))
    } else {
        std::time::Duration::from_millis(500)
    };
    // Pointer buttons / keys the client currently holds down. The injector is host-lifetime, so a
    // press left dangling by an abrupt client disconnect stays latched in the compositor across the
    // reconnect (Mutter keeps the implicit pointer grab of the still-pressed button — a stuck
    // left-button-down then turns every later click into a drag: windows move, but clicking buttons
    // and text inputs does nothing). We synthesize the matching up-events when this session ends —
    // see the release loop after the `break`.
    // Sets (not Vecs) so the presence test is O(1), not O(n) per event, and bounded by `MAX_HELD`
    // so a client flooding distinct never-released codes can't grow the tracking state or spike the
    // input thread (security-review 2026-06-28 S3). A real keyboard+mouse holds far fewer at once;
    // codes past the cap simply aren't tracked for end-of-session release (worst case: one unreleased
    // key on a pathological disconnect, which the injector's own state still bounds).
    const MAX_HELD: usize = 256;
    let mut held_buttons: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut held_keys: std::collections::HashSet<u32> = std::collections::HashSet::new();
    loop {
        match rx.recv_timeout(std::time::Duration::from_millis(4)) {
            // Rich input (touchpad / motion) — applied the moment it arrives; the single
            // channel means a gyro sample never waits out the 4 ms timeout behind an idle
            // button plane.
            Ok(ClientInput::Rich(rich)) => {
                if matches!(rich, punktfunk_core::quic::RichInput::Motion { .. }) {
                    let now = std::time::Instant::now();
                    if let Some(prev) = last_motion.replace(now) {
                        let gap = now.duration_since(prev);
                        if gap < std::time::Duration::from_secs(1) {
                            motion_gaps_us.push(gap.as_micros() as u32);
                        }
                    }
                    if motion_window.elapsed() >= std::time::Duration::from_secs(5)
                        && !motion_gaps_us.is_empty()
                    {
                        motion_gaps_us.sort_unstable();
                        let p = |q: f64| {
                            motion_gaps_us[(q * (motion_gaps_us.len() - 1) as f64) as usize]
                        };
                        tracing::debug!(
                            samples = motion_gaps_us.len() + 1,
                            gap_p50_us = p(0.5),
                            gap_p95_us = p(0.95),
                            gap_max_us = motion_gaps_us.last().copied().unwrap_or(0),
                            "motion cadence (client gyro inter-arrival, 5 s window)"
                        );
                        motion_gaps_us.clear();
                        motion_window = std::time::Instant::now();
                    }
                }
                pads.apply_rich(rich);
            }
            Ok(ClientInput::Event(ev)) => match ev.kind {
                InputKind::GamepadButton | InputKind::GamepadAxis => {
                    // A bad index / unknown axis just doesn't update a pad — fall through (no
                    // `continue`) so the rich-input drain + feedback pump below still run every
                    // iteration (the DualSense GET_REPORT handshake must be serviced promptly).
                    let idx = ev.flags as usize;
                    if idx < MAX_WIRE_PADS && pad_state[idx].apply(&ev) {
                        pad_mask |= 1 << idx;
                        let frame = pad_state[idx].frame(idx, pad_mask);
                        pads.handle(&crate::gamestream::gamepad::GamepadEvent::State(frame));
                    }
                }
                InputKind::GamepadState => {
                    // Idempotent full-state snapshot from a capable client (see
                    // `GamepadSnapshot`): applied only when its seq supersedes the last one, so
                    // a datagram the network reordered can't roll held state backwards. The
                    // client refreshes touched pads every ~100 ms, so an unchanged refresh is
                    // the common case — skip the frame emit then (an XInput packet-number bump
                    // for identical state is pure churn), but always advance the gate.
                    use punktfunk_core::input::GamepadSnapshot;
                    if let Some(snap) = GamepadSnapshot::from_event(&ev) {
                        let idx = snap.pad as usize;
                        if idx < MAX_WIRE_PADS && GamepadSnapshot::seq_newer(snap.seq, pad_seq[idx])
                        {
                            pad_seq[idx] = Some(snap.seq);
                            let before = pad_state[idx];
                            pad_state[idx].set_snapshot(&snap);
                            let first = pad_mask & (1 << idx) == 0;
                            if first || pad_state[idx] != before {
                                pad_mask |= 1 << idx;
                                let frame = pad_state[idx].frame(idx, pad_mask);
                                pads.handle(&crate::gamestream::gamepad::GamepadEvent::State(
                                    frame,
                                ));
                            }
                        }
                    }
                }
                InputKind::GamepadRemove => {
                    // Mid-session hot-unplug from a snapshot-capable client (the native plane's
                    // `activeGamepadMask` equivalent). Seq-gated in the SAME per-pad sequence
                    // space as snapshots, so a snapshot the network reordered past this removal
                    // is dropped (older seq) and can't resurrect the pad — while a later re-plug
                    // on the same index arrives with a still-newer seq and is accepted. Clearing
                    // the `active_mask` bit and re-emitting the frame fires every backend's
                    // unplug sweep (`inject/*/gamepad.rs`), tearing down just this pad's device.
                    let (pad, seq) = punktfunk_core::input::decode_gamepad_remove(ev.flags);
                    let idx = pad as usize;
                    if idx < MAX_WIRE_PADS
                        && punktfunk_core::input::GamepadSnapshot::seq_newer(seq, pad_seq[idx])
                    {
                        pad_seq[idx] = Some(seq);
                        if pad_mask & (1 << idx) != 0 {
                            pad_mask &= !(1 << idx);
                            pad_state[idx] = PadState::default();
                            let frame = pad_state[idx].frame(idx, pad_mask);
                            pads.handle(&crate::gamestream::gamepad::GamepadEvent::State(frame));
                            tracing::info!(pad = idx, "gamepad unplugged (native detach)");
                        }
                        // Fresh feedback bookkeeping so a later re-plug on this index inherits no
                        // stale rumble lease/seq (a lease still ticking would buzz the new pad).
                        rumble_state[idx] = (0, 0);
                        rumble_seen[idx] = false;
                        rumble_seq[idx] = 0;
                        rumble_stop_burst[idx] = 0;
                    }
                }
                InputKind::GamepadArrival => {
                    // Per-pad controller kind declaration (mixed types): route this pad's future
                    // frames to a backend of the declared kind. `code` = the GamepadPref wire byte,
                    // `flags` = pad index. Applied before the pad's first frame (the client sends it
                    // on slot open), so the device is built as the right type from the start.
                    let idx = ev.flags as usize;
                    let kind = GamepadPref::from_u8(ev.code as u8);
                    pads.set_kind(idx, kind);
                }
                _ => {
                    // Track press/release so a mid-press disconnect can be undone below.
                    match ev.kind {
                        InputKind::MouseButtonDown if held_buttons.len() < MAX_HELD => {
                            held_buttons.insert(ev.code);
                        }
                        InputKind::MouseButtonUp => {
                            held_buttons.remove(&ev.code);
                        }
                        InputKind::KeyDown if held_keys.len() < MAX_HELD => {
                            held_keys.insert(ev.code);
                        }
                        InputKind::KeyUp => {
                            held_keys.remove(&ev.code);
                        }
                        _ => {}
                    }
                    // Pointer/keyboard → the host-lifetime injector service (one persistent
                    // portal session for every punktfunk/1 session). A send error only means the
                    // service thread is gone (host shutting down) — dropping the event is fine,
                    // input is lossy by design.
                    let _ = inj_tx.send(ev);
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        // Service feedback every iteration (≤4 ms latency; games block on EVIOCSFF, and the
        // DualSense kernel handshake must be answered promptly). Rumble → the universal 0xCA
        // plane; DualSense rich feedback (lightbar / player LEDs / adaptive triggers) → 0xCD.
        pads.pump(
            |pad, low, high| {
                let idx = pad as usize;
                if idx < MAX_WIRE_PADS {
                    let prev = rumble_state[idx];
                    // Log the silent→active transition (once per buzz) so a live test can tell
                    // "host never gets rumble from the game" apart from "client doesn't render it".
                    if prev == (0, 0) && (low != 0 || high != 0) {
                        tracing::info!(pad, low, high, "rumble: forwarding to client (0xCA)");
                    }
                    rumble_state[idx] = (low, high);
                    rumble_seen[idx] = true;
                    // Bump the reorder counter on every change, then arm the stop burst on a
                    // transition to zero (so a lost stop still reaches a legacy client) and clear
                    // it when the game re-asserts a non-zero level.
                    rumble_seq[idx] = rumble_seq[idx].wrapping_add(1);
                    if (low, high) == (0, 0) {
                        rumble_stop_burst[idx] = if prev != (0, 0) { RUMBLE_STOP_BURST } else { 0 };
                    } else {
                        rumble_stop_burst[idx] = 0;
                    }
                    let ttl = if (low, high) == (0, 0) {
                        0
                    } else {
                        rumble_ttl_ms
                    };
                    send_rumble(
                        &conn,
                        rumble_envelope_on,
                        pad,
                        low,
                        high,
                        rumble_seq[idx],
                        ttl,
                    );
                } else {
                    // Out-of-range pad (a backend never produces these) — forward without gating.
                    send_rumble(&conn, rumble_envelope_on, pad, low, high, 0, rumble_ttl_ms);
                }
            },
            |h| {
                let _ = conn.send_datagram(h.encode().into());
            },
        );
        // Keep the virtual DualSense from going silent during steady input (no-op for X-Box): a
        // held-steady pad sends no wire events, so without a periodic re-emit the kernel/SDL drop
        // it as unplugged. The 8 ms gap inside heartbeat() governs the rate, not this ≤4 ms tick.
        pads.heartbeat();
        if last_refresh.elapsed() >= rumble_refresh_interval {
            last_refresh = std::time::Instant::now();
            if rumble_envelope_on {
                // Renewal: refresh an active pad's lease (bump seq, fresh TTL), and drain each
                // pad's post-stop zero burst, then let it go quiet — no perpetual zero refreshes.
                for i in 0..MAX_WIRE_PADS {
                    if !rumble_seen[i] {
                        continue;
                    }
                    let (low, high) = rumble_state[i];
                    if (low, high) != (0, 0) {
                        rumble_seq[i] = rumble_seq[i].wrapping_add(1);
                        send_rumble(
                            &conn,
                            true,
                            i as u16,
                            low,
                            high,
                            rumble_seq[i],
                            rumble_ttl_ms,
                        );
                    } else if rumble_stop_burst[i] > 0 {
                        rumble_stop_burst[i] -= 1;
                        rumble_seq[i] = rumble_seq[i].wrapping_add(1);
                        send_rumble(&conn, true, i as u16, 0, 0, rumble_seq[i], 0);
                    }
                }
            } else {
                // Legacy: re-send the current level of every seen pad every 500 ms (v1).
                for (i, &(low, high)) in rumble_state.iter().enumerate() {
                    if rumble_seen[i] {
                        let d = punktfunk_core::quic::encode_rumble_datagram(i as u16, low, high);
                        let _ = conn.send_datagram(d.to_vec().into());
                    }
                }
            }
        }
    }
    // Session ended (client gone). Release anything still held through the host-lifetime injector —
    // its EIS connection (and any implicit grab Mutter holds for our pressed button) outlives this
    // session, so without this a button pressed at disconnect stays latched and breaks clicks for
    // the next session. Mirror of the injector's own release_all, but keyed off the session, which
    // is where a client actually vanishes mid-press.
    if !held_buttons.is_empty() || !held_keys.is_empty() {
        tracing::debug!(
            buttons = held_buttons.len(),
            keys = held_keys.len(),
            "input: releasing held buttons/keys at session end"
        );
    }
    for code in held_buttons {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::MouseButtonUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
    for code in held_keys {
        let _ = inj_tx.send(InputEvent {
            kind: InputKind::KeyUp,
            _pad: [0; 3],
            code,
            x: 0,
            y: 0,
            flags: 0,
        });
    }
}

/// Opus encoder for the native audio plane: a plain stereo encoder (the live-validated,
/// byte-identical path) or a libopus *multistream* encoder for 5.1/7.1, both behind one
/// `encode_float`. Surround uses the safe `opus::MSEncoder` (no `audiopus_sys`).
#[cfg(any(target_os = "linux", target_os = "windows"))]
enum NativeAudioEnc {
    Stereo(opus::Encoder),
    Surround(opus::MSEncoder),
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
impl NativeAudioEnc {
    /// Build the encoder for `channels` (2/6/8), hard-CBR + RESTRICTED_LOWDELAY like the
    /// GameStream path; bitrate from the shared layout table (stereo keeps the validated 128 kbps).
    fn new(channels: u8) -> Result<NativeAudioEnc, opus::Error> {
        if channels == 2 {
            let mut e = opus::Encoder::new(
                crate::audio::SAMPLE_RATE,
                opus::Channels::Stereo,
                opus::Application::LowDelay,
            )?;
            e.set_bitrate(opus::Bitrate::Bits(128_000)).ok();
            e.set_vbr(false).ok();
            Ok(NativeAudioEnc::Stereo(e))
        } else {
            let l = punktfunk_core::audio::layout_for(channels, false);
            let mut e = opus::MSEncoder::new(
                crate::audio::SAMPLE_RATE,
                l.streams,
                l.coupled,
                l.mapping,
                opus::Application::LowDelay,
            )?;
            e.set_bitrate(opus::Bitrate::Bits(l.bitrate)).ok();
            e.set_vbr(false).ok();
            Ok(NativeAudioEnc::Surround(e))
        }
    }

    fn encode_float(&mut self, frame: &[f32], out: &mut [u8]) -> Result<usize, opus::Error> {
        match self {
            NativeAudioEnc::Stereo(e) => e.encode_float(frame, out),
            NativeAudioEnc::Surround(e) => e.encode_float(frame, out),
        }
    }
}

/// The audio thread: desktop capture → Opus (48 kHz, 5 ms, CBR — same tuning as the GameStream
/// path) → `AUDIO_MAGIC` datagrams, at the negotiated `channels` (2 stereo / 6 = 5.1 / 8 = 7.1,
/// canonical wire order FL FR FC LFE RL RR SL SR). QUIC already encrypts; no extra layer. The
/// capturer comes from (and returns to) the persistent slot — see [`AudioCapSlot`].
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn audio_thread(
    conn: quinn::Connection,
    stop: Arc<AtomicBool>,
    audio_cap: AudioCapSlot,
    channels: u8,
) {
    use crate::audio::SAMPLE_RATE;
    const FRAME_MS: usize = 5;
    const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000; // 240
    let want = punktfunk_core::audio::normalize_channels(channels);

    // Reuse the cached capturer ONLY when its channel count matches this session's; a stereo
    // capturer left by a prior session must not feed a 5.1/7.1 session (the encoder + the client's
    // decoder are sized for `want`, so a mismatched capturer would garble/desync the audio).
    let capturer = match audio_cap.lock().unwrap().take() {
        Some(mut c) if c.channels() == want as u32 => {
            c.drain(); // discard audio captured between sessions
            c
        }
        prev => {
            drop(prev); // wrong channel count (or none): clean teardown, open fresh at `want`
            match crate::audio::open_audio_capture(want as u32) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "punktfunk/1 audio unavailable — session continues without it");
                    return;
                }
            }
        }
    };
    let mut enc = match NativeAudioEnc::new(want) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "opus encoder");
            *audio_cap.lock().unwrap() = Some(capturer);
            return;
        }
    };

    let frame_len = SAMPLES_PER_FRAME * want as usize;
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    // Sized for the largest surround frame (7.1 HQ ≈ 1.3 KB at 5 ms); ample for normal quality.
    let mut opus_buf = vec![0u8; 4096];
    let mut seq: u32 = 0;
    // Reopen-with-backoff: hold the capturer in an Option so a mid-session capture-thread death
    // (device unplug, daemon restart) reopens instead of muting the rest of a multi-hour session.
    // A quiet sink is NOT a death — `next_chunk` returns an empty chunk on its idle timeout — so only
    // a genuine thread-ended Err drops the capturer. Reopens are throttled by INJECTOR_REOPEN_BACKOFF.
    // The Opus encoder and the monotonic `seq` are kept across reopens (the client sees a gap, not a
    // restart). The first open already happened above; failing THAT still ends the session quietly.
    let mut capturer = Some(capturer);
    let mut last_failed: Option<std::time::Instant> = None;
    tracing::info!(
        channels = want,
        "punktfunk/1 audio streaming (Opus 48 kHz, 5 ms datagrams)"
    );
    'session: while !stop.load(Ordering::SeqCst) {
        if capturer.is_none() {
            if last_failed.is_some_and(|t| t.elapsed() < INJECTOR_REOPEN_BACKOFF) {
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            match crate::audio::open_audio_capture(want as u32) {
                Ok(c) => {
                    tracing::info!("punktfunk/1 audio capture reopened");
                    capturer = Some(c);
                    last_failed = None;
                    acc.clear(); // drop the partial frame straddling the gap
                }
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "audio reopen failed — will retry");
                    last_failed = Some(std::time::Instant::now());
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }
        }
        let chunk = match capturer.as_mut().unwrap().next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture lost — reopening");
                capturer = None;
                last_failed = Some(std::time::Instant::now());
                continue;
            }
        };
        acc.extend_from_slice(&chunk);
        while acc.len() >= frame_len {
            let frame: Vec<f32> = acc.drain(..frame_len).collect();
            let pts_ns = now_ns();
            match enc.encode_float(&frame, &mut opus_buf) {
                Ok(n) => {
                    let d =
                        punktfunk_core::quic::encode_audio_datagram(seq, pts_ns, &opus_buf[..n]);
                    if conn.send_datagram(d.into()).is_err() {
                        break 'session; // connection gone
                    }
                    seq = seq.wrapping_add(1);
                }
                Err(e) => tracing::warn!(error = %e, "opus encode"),
            }
        }
    }
    // Return the live capturer for the next session (None if it died and never reopened).
    if let Some(c) = capturer {
        *audio_cap.lock().unwrap() = Some(c);
    }
}

/// Stub — punktfunk/1 audio needs Linux (PipeWire capture + libopus); non-Linux dev builds
/// run sessions without it, same as when the capturer fails to open.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn audio_thread(
    _conn: quinn::Connection,
    _stop: Arc<AtomicBool>,
    _audio_cap: AudioCapSlot,
    _channels: u8,
) {
    tracing::warn!("punktfunk/1 audio requires Linux or Windows — session continues without it");
}

/// Advance the intra-refresh wave position and decide whether this emitted AU is a wave boundary
/// that should carry [`USER_FLAG_RECOVERY_POINT`](punktfunk_core::packet::USER_FLAG_RECOVERY_POINT).
///
/// `ir_wave_pos` counts frames since the last IDR/wave start; a real IDR re-phases it to 0 (an IDR
/// restarts the encoder's wave AND is itself a clean anchor, so it is never additionally marked).
/// Every `period`-th non-IDR AU is a boundary — the client lifts its post-loss freeze on the SECOND
/// such mark. Pure so the marking cadence is unit-tested without a GPU (see the pump's use in the
/// encode-poll loop).
fn mark_recovery_boundary(ir_wave_pos: &mut u32, is_keyframe: bool, period: u32) -> bool {
    if is_keyframe {
        *ir_wave_pos = 0;
        false
    } else {
        *ir_wave_pos += 1;
        if *ir_wave_pos >= period {
            *ir_wave_pos = 0;
            true
        } else {
            false
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn synthetic_stream(
    session: &mut Session,
    frames: u32,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    fec_target: &AtomicU8,
    timing_conn: Option<&quinn::Connection>,
    probe_seq: bool,
) -> Result<()> {
    let interval = std::time::Duration::from_millis(1000 / 60);
    for idx in 0..frames {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        apply_fec_target(session, fec_target);
        // Service speed-test probes between synthetic frames (loopback bandwidth tests).
        service_probes(session, stop, probe_rx, probe_result_tx, probe_seq);
        let data = test_frame(idx, 64 * 1024);
        let pts_ns = now_ns();
        session
            .submit_frame(&data, pts_ns, (FLAG_PIC | FLAG_SOF) as u32)
            .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
        // Host timing (0xCF) for protocol tests: near-zero here (no capture/encode), but it
        // proves the plane end-to-end on a pure loopback run.
        if let Some(tc) = timing_conn {
            let t = punktfunk_core::quic::HostTiming {
                pts_ns,
                host_us: (now_ns().saturating_sub(pts_ns) / 1000).min(u32::MAX as u64) as u32,
            };
            let _ = tc.send_datagram(punktfunk_core::quic::encode_host_timing_datagram(&t).into());
        }
        std::thread::sleep(interval);
    }
    tracing::info!(frames, "synthetic stream complete");
    Ok(())
}

/// Pure selection of the session's virtual-gamepad backend: the client's explicit `pref` wins,
/// then the host's `PUNKTFUNK_GAMEPAD` env var (under a client `Auto`), then X-Box 360.
///
/// `linux`/`windows` flag the host platform. DualSense and DualShock 4 each have both a Linux (UHID
/// hid-playstation) and a Windows (UMDF minidriver) backend; on any other platform such a wish degrades
/// to X-Box 360 (never an error: a session without rich pads still streams). X-Box One/Series is a
/// distinct uinput *identity* on Linux, but XInput-identical to the 360 pad on Windows (the XUSB
/// companion presents a 360 identity), so it degrades to `Xbox360` there.
fn pick_gamepad(pref: GamepadPref, env: Option<&str>, linux: bool, windows: bool) -> GamepadPref {
    let want = match pref {
        GamepadPref::Auto => env
            .and_then(GamepadPref::from_name)
            .unwrap_or(GamepadPref::Auto),
        explicit => explicit,
    };
    match want {
        // DualSense / DualShock 4: Linux UHID hid-playstation, or the Windows UMDF minidriver backend.
        GamepadPref::DualSense if linux || windows => GamepadPref::DualSense,
        GamepadPref::DualShock4 if linux || windows => GamepadPref::DualShock4,
        // One/Series: a real, distinct uinput identity on Linux; folded into the 360 backend on
        // Windows (XInput can't tell them apart anyway).
        GamepadPref::XboxOne if linux => GamepadPref::XboxOne,
        // Steam Deck / classic Steam Controller: Linux UHID hid-steam (Windows Steam devices
        // are the N4 spike).
        GamepadPref::SteamDeck if linux => GamepadPref::SteamDeck,
        GamepadPref::SteamController if linux => GamepadPref::SteamController,
        // Windows virtual Deck: the UMDF device-type-3 identity, Steam-Input-promoted via the
        // MI_02 hardware-id synthesis (gamepad-new-types N4) — native Deck glyphs + trackpads +
        // gyro + back grips, replacing the old fold to DualSense.
        GamepadPref::SteamDeck if windows => GamepadPref::SteamDeck,
        // DualSense Edge: Linux UHID hid-playstation / Windows UMDF (device-type 2) — the plain
        // DualSense plus native back/Fn buttons, so the wire paddles stop hitting the fold/drop
        // policy. Degrades to Xbox360 elsewhere like its siblings.
        GamepadPref::DualSenseEdge if linux || windows => GamepadPref::DualSenseEdge,
        // Switch Pro: Linux UHID hid-nintendo (≥ 5.16) — correct Nintendo glyphs + positional
        // layout + gyro + HD rumble. No Windows backend; folds to Xbox360 there.
        GamepadPref::SwitchPro if linux => GamepadPref::SwitchPro,
        _ => GamepadPref::Xbox360,
    }
}

/// Runtime degrade for the Linux UHID backends (DualSense / DualShock 4 / Steam Deck): if
/// `/dev/uhid` can't be opened for write *now*, fall back to the uinput X-Box 360 pad rather than a
/// dead controller (the UHID device-create would just fail). Cheap — opens + drops the char device,
/// no `UHID_CREATE2`, so no device is created. A no-op on non-Linux (those backends are UMDF/uinput).
#[cfg(target_os = "linux")]
fn degrade_if_no_uhid(chosen: GamepadPref) -> GamepadPref {
    let needs_uhid = matches!(
        chosen,
        GamepadPref::DualSense
            | GamepadPref::DualSenseEdge
            | GamepadPref::DualShock4
            | GamepadPref::SteamDeck
            | GamepadPref::SteamController
            | GamepadPref::SwitchPro
    );
    if needs_uhid
        && std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uhid")
            .is_err()
    {
        tracing::warn!(
            wanted = chosen.as_str(),
            "/dev/uhid not writable — falling back to the X-Box 360 pad"
        );
        return GamepadPref::Xbox360;
    }
    chosen
}

#[cfg(not(target_os = "linux"))]
fn degrade_if_no_uhid(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// True if a **physical** Valve Steam controller (`28DE`) is already attached. The host's own Steam
/// Input is then managing a `28DE` device, and presenting a second (virtual) one makes Steam juggle
/// two Decks — confirmed conflict-prone on a Deck-as-host (the physical `28DE:1205` + Steam's
/// `28DE:11FF` XInput output pad are both live). HID device dirs are named `BUS:VID:PID.INST`
/// (uppercase); a UHID virtual device resolves through `/devices/virtual/…`, a real one does not.
///
/// Punktfunk's OWN virtual Decks must never count: the usbip/gadget transports present a real USB
/// device (vhci resolves through `vhci_hcd`, NOT `/devices/virtual/`), so a just-ended session's
/// pad still detaching — or a concurrent session's live one — read as "physical" and degraded
/// every back-to-back Deck session to DualSense (observed live on Bazzite 2026-07-04). Ours are
/// recognizable by the `PFDK…` serial ([`steam_proto::deck_serial`]) in `HID_UNIQ`, with the
/// vhci path as belt and braces.
#[cfg(target_os = "linux")]
fn physical_steam_controller_present() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/bus/hid/devices") else {
        return false;
    };
    entries.flatten().any(|e| {
        if !e.file_name().to_string_lossy().contains(":28DE:") {
            return false;
        }
        if std::fs::read_to_string(e.path().join("uevent"))
            .is_ok_and(|u| u.lines().any(|l| l.starts_with("HID_UNIQ=PFDK")))
        {
            return false; // one of our own virtual Decks
        }
        match std::fs::read_link(e.path()) {
            Ok(target) => {
                let t = target.to_string_lossy();
                !t.contains("/virtual/") && !t.contains("vhci_hcd")
            }
            Err(_) => true,
        }
    })
}

/// Gate a virtual Steam pad off when a physical Steam controller is attached (§ conflict). Degrade to
/// DualSense (then the uhid ladder), which Steam treats as an ordinary, distinct pad. Override with
/// `PUNKTFUNK_STEAM_FORCE=1` when the host has no competing Steam Input (e.g. a remote-only box).
#[cfg(target_os = "linux")]
fn degrade_steam_on_conflict(chosen: GamepadPref) -> GamepadPref {
    if !matches!(
        chosen,
        GamepadPref::SteamDeck | GamepadPref::SteamController
    ) {
        return chosen;
    }
    let forced = std::env::var("PUNKTFUNK_STEAM_FORCE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !forced && physical_steam_controller_present() {
        tracing::warn!(
            wanted = chosen.as_str(),
            "a physical Steam controller is attached — the host's Steam Input would manage two 28DE \
             devices; falling back to DualSense (set PUNKTFUNK_STEAM_FORCE=1 to override)"
        );
        return degrade_if_no_uhid(GamepadPref::DualSense);
    }
    chosen
}

#[cfg(not(target_os = "linux"))]
fn degrade_steam_on_conflict(chosen: GamepadPref) -> GamepadPref {
    chosen
}

/// Resolve the client's gamepad-backend preference (the env/logging shell around
/// [`pick_gamepad`]). Always concrete — the `Welcome` reports what the session will drive.
fn resolve_gamepad(pref: GamepadPref) -> GamepadPref {
    let env = crate::config::config().gamepad.clone();
    let chosen = pick_gamepad(
        pref,
        env.as_deref(),
        cfg!(target_os = "linux"),
        cfg!(target_os = "windows"),
    );
    // Runtime degrade (separate from the compile-time platform check above): the Linux UHID
    // backends need `/dev/uhid` usable *now*, else creating the device just fails and the controller
    // goes dead — fall back to the always-available uinput X-Box 360 pad instead.
    let chosen = degrade_if_no_uhid(chosen);
    // Conflict gate: don't present a virtual Steam (28DE) pad when the host already has a physical
    // Steam controller — its own Steam Input would then manage two Decks (confirmed conflict-prone on
    // a Deck-as-host). `PUNKTFUNK_STEAM_FORCE=1` overrides.
    let chosen = degrade_steam_on_conflict(chosen);
    match pref {
        GamepadPref::Auto => {
            // The operator's env knob deserves a diagnostic when it didn't drive the
            // choice — a typo, or a DualSense wish on a non-UHID host, would otherwise
            // degrade silently.
            if let Some(env) = env.as_deref() {
                if GamepadPref::from_name(env) != Some(chosen) {
                    tracing::warn!(
                        env,
                        chosen = chosen.as_str(),
                        "PUNKTFUNK_GAMEPAD unrecognized or unavailable — falling back"
                    );
                }
            }
            tracing::info!(gamepad = chosen.as_str(), "gamepad backend (client: auto)")
        }
        want if want == chosen => {
            tracing::info!(gamepad = chosen.as_str(), "honoring client gamepad request")
        }
        want => tracing::warn!(
            requested = want.as_str(),
            chosen = chosen.as_str(),
            "client-requested gamepad backend unavailable — falling back"
        ),
    }
    chosen
}

/// Pure selection: choose the backend to drive from the client's `pref`, the set `available`
/// right now, and the auto-`detected` default. A concrete preference wins only if it's available;
/// otherwise (and for `Auto`) fall back to the detected default. `None` only when nothing is
/// available *and* nothing was detected — the caller turns that into a handshake error.
fn pick_compositor(
    pref: CompositorPref,
    available: &[crate::vdisplay::Compositor],
    detected: Option<crate::vdisplay::Compositor>,
) -> Option<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    match Compositor::from_pref(pref) {
        Some(want) if available.contains(&want) => Some(want),
        // `CompositorPref::Wlroots` names the wlroots *family* (D2): sway/river ([`Wlroots`]) and
        // Hyprland are distinct backends but mutually-exclusive live sessions, so honor the request
        // with whichever family member is actually available — the detected one if it's a family
        // member, else the first available of the two.
        Some(Compositor::Wlroots) => match detected {
            Some(d @ (Compositor::Wlroots | Compositor::Hyprland)) => Some(d),
            _ => [Compositor::Wlroots, Compositor::Hyprland]
                .into_iter()
                .find(|c| available.contains(c))
                .or(detected),
        },
        _ => detected,
    }
}

/// Resolve the client's compositor preference to a concrete backend (the I/O shell around
/// [`pick_compositor`]): enumerate what's available, auto-detect the default, pick, and log
/// whether the explicit request was honored or fell back. Runs blocking probes — call off the
/// async reactor (`spawn_blocking`).
fn resolve_compositor(
    pref: CompositorPref,
    dedicated_launch: bool,
) -> Result<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    // Windows has a single virtual-display backend (SudoVDA); vdisplay::open ignores the compositor
    // arg there, so short-circuit the Linux session-detection state machine with a placeholder.
    #[cfg(target_os = "windows")]
    {
        let _ = (pref, dedicated_launch);
        Ok(Compositor::Kwin)
    }
    #[cfg(not(target_os = "windows"))]
    {
        // A client is (re)connecting → cancel any pending TV-session restore so the box stays in the
        // streamed session (covers the keep-alive REUSE reconnect, which skips create_managed_session's
        // own cancel — review #3). No-op when nothing is pending.
        crate::vdisplay::cancel_pending_tv_restore();
        // Explicit operator override (legacy / CI / forcing a backend for a test) wins and is assumed
        // to come with a hand-set env — don't retarget the process env in that case.
        let overridden = crate::config::config().compositor.is_some();
        let detected = if overridden {
            crate::vdisplay::detect().ok()
        } else {
            // Auto: detect the LIVE session (Gaming vs Desktop) and retarget the process env at it so
            // every backend (video capture + input) this connect opens against the active session —
            // this is the state machine that lets one host follow a Bazzite box across Gaming↔Desktop.
            let active = crate::vdisplay::detect_active_session();
            // A4: if the compositor instance changed since the last connect (an idle-time Game↔Desktop
            // switch), bump the epoch + invalidate the old backend's kept displays so this connect never
            // reuses a node id from the dead instance.
            crate::vdisplay::observe_session_instance(&active);
            crate::vdisplay::apply_session_env(&active);
            tracing::info!(
                active = ?active.kind,
                wayland = active.env.wayland_display.as_deref().unwrap_or("-"),
                "detected active graphical session"
            );
            crate::vdisplay::compositor_for_kind(active.kind)
        };
        // Dedicated game session (design/gamemode-and-dedicated-sessions.md B0): a launching session
        // under `game_session=dedicated` (gamescope confirmed available) forces its OWN headless
        // gamescope spawn at the client's mode, overriding the detected desktop/game-mode backend. The
        // env was already retargeted above (for XDG_RUNTIME_DIR / the PipeWire daemon); we just pin the
        // backend + input to the spawn sub-mode. Skipped under an explicit operator compositor pin.
        if dedicated_launch && !overridden {
            crate::vdisplay::apply_input_env(Compositor::Gamescope, true);
            tracing::info!(
                "dedicated game session — routing to a headless gamescope spawn at the client mode"
            );
            return Ok(Compositor::Gamescope);
        }
        let available = crate::vdisplay::available();
        let chosen = match pick_compositor(pref, &available, detected) {
            Some(c) => c,
            None => {
                // No live session — the state a compositor crash leaves behind (gnome-shell
                // SIGSEGV → GDM greeter, whose auto-login is once-per-boot). If the operator
                // configured a recovery hook, fire it (debounced) and tell the client to retry:
                // its next knock lands in the recovered desktop.
                if crate::vdisplay::try_recover_session() {
                    anyhow::bail!(
                        "no live graphical session for this uid — host session recovery launched \
                         (PUNKTFUNK_RECOVER_SESSION_CMD); retry in a few seconds"
                    );
                }
                anyhow::bail!(
                    "no usable compositor (no live graphical session for this uid; set \
                     PUNKTFUNK_COMPOSITOR or start a desktop/gaming session)"
                );
            }
        };
        if !overridden {
            // Point input at the same backend and resolve the gamescope sub-mode (managed where the
            // session infra exists, attach to a foreign gamescope, else per-session bare spawn).
            crate::vdisplay::apply_input_env(chosen, false);
        }
        let avail_ids: Vec<&str> = available.iter().map(|c| c.id()).collect();
        match Compositor::from_pref(pref) {
            Some(want) if want == chosen => {
                tracing::info!(
                    compositor = chosen.id(),
                    "honoring client compositor request"
                )
            }
            Some(want) => tracing::warn!(
                requested = want.id(),
                chosen = chosen.id(),
                available = ?avail_ids,
                "client-requested compositor unavailable — falling back to auto-detect"
            ),
            None => tracing::info!(
                compositor = chosen.id(),
                "auto-detected compositor (client: auto)"
            ),
        }
        Ok(chosen)
    }
}

/// Bounds a speed-test [`ProbeRequest`] before bursting: a 3 Gbps / 5 s ceiling keeps a probe from
/// monopolizing the link or stalling the stream for too long. The ceiling is set ABOVE the session
/// bitrate cap ([`MAX_BITRATE_KBPS`], 2 Gbps) on purpose — a probe should be able to demonstrate
/// headroom past the rate a session will actually be configured to use, so the client can pick a
/// confident 1 Gbps+ bitrate. GF(2¹⁶) FEC makes multi-Gbps reachable on a LAN.
const MAX_PROBE_KBPS: u32 = 10_000_000;
const MAX_PROBE_MS: u32 = 5_000;

/// Run a bandwidth probe over `session`: burst zero-filled access units flagged [`FLAG_PROBE`] at
/// `req.target_kbps` of goodput for `req.duration_ms` (both clamped to `MAX_PROBE_*`), pacing by a
/// "bytes allowed so far" budget so scheduling jitter doesn't overshoot the target. Returns what
/// was actually offered so the client can compute delivery ratio (`received / bytes_sent`) and
/// throughput. Video is paused for the duration (the caller's loop is blocked here) — a speed test
/// is a deliberate, short interruption the client initiates.
fn run_probe_burst(
    session: &mut Session,
    req: ProbeRequest,
    stop: &AtomicBool,
    probe_seq: bool,
) -> ProbeResult {
    let target_kbps = req.target_kbps.min(MAX_PROBE_KBPS);
    let duration_ms = req.duration_ms.min(MAX_PROBE_MS);
    // Probe filler is sealed in the PROBE index space (its own frame counter — video indexes are
    // owned by the encode loop and must stay 1:1 with the encoder's RFI bookkeeping). A client
    // that didn't advertise VIDEO_CAP_PROBE_SEQ reassembles everything in one window and would
    // drop probe-space frames as stale against the video stream — measuring garbage — so its
    // mid-session probe is DECLINED (zeroed result) instead. Old sealing (probe filler consuming
    // video indexes) is not an option anymore: those indexes are invisible to every client gap
    // detector and read as a phantom multi-thousand-frame loss after the burst.
    if !probe_seq {
        tracing::info!(
            "declining speed-test probe: client predates VIDEO_CAP_PROBE_SEQ (its reassembler \
             cannot window probe-space frames)"
        );
        return ProbeResult {
            bytes_sent: 0,
            packets_sent: 0,
            duration_ms: 0,
            wire_packets_sent: 0,
            send_dropped: 0,
        };
    }
    if target_kbps == 0 || duration_ms == 0 {
        return ProbeResult {
            bytes_sent: 0,
            packets_sent: 0,
            duration_ms: 0,
            wire_packets_sent: 0,
            send_dropped: 0,
        };
    }
    // kbps -> bytes/s (x1000/8).
    let bytes_per_sec = target_kbps as u64 * 125;
    // Keep each AU a SMALL burst (~16 KB ≈ a dozen MTU shards) and let the byte budget below pace
    // the rate finely. The old 256 KB cap blasted ~200 packets into the send buffer per submit, so
    // a small buffer (e.g. the Deck's 416 KB) overflowed on a single AU and the test measured
    // self-inflicted buffer overflow instead of the link — mirror how `paced_submit` spreads the
    // real video path's frames so the probe stresses the same way a real stream does.
    let chunk = (bytes_per_sec / 240).clamp(1200, 16 * 1024) as usize;
    let filler = vec![0u8; chunk];
    // Wire-packet accounting via session-stat deltas: `packets_sent` counts every sealed wire packet
    // (seal_frame), `packets_send_dropped` every one the send buffer rejected (WouldBlock/ENOBUFS).
    // Their delta over the burst is exact — and isolates host-side drops from link loss for the
    // client. Video is paused for the burst (the data-plane loop is blocked here), so these deltas
    // are pure probe traffic.
    let wire0 = session.stats().packets_sent;
    let drop0 = session.stats().packets_send_dropped;
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(duration_ms as u64);
    let mut bytes_sent = 0u64;
    let mut packets_sent = 0u32; // probe access-unit count (goodput chunks)
    while std::time::Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        let allowed = (start.elapsed().as_secs_f64() * bytes_per_sec as f64) as u64;
        if bytes_sent < allowed {
            // A full send buffer drops on WouldBlock/ENOBUFS (UdpTransport returns Ok) — that loss is
            // part of what the probe measures (it surfaces as send_dropped), so keep going. Sealed
            // in the probe index space (FLAG_PROBE + its own counter) — never a video frame_index.
            let _ = session.submit_probe_frame(&filler, now_ns());
            bytes_sent += chunk as u64;
            packets_sent += 1;
        } else {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }
    let actual_ms = start.elapsed().as_millis() as u32;
    let wire_offered = (session.stats().packets_sent - wire0) as u32;
    let send_dropped = (session.stats().packets_send_dropped - drop0) as u32;
    let wire_packets_sent = wire_offered.saturating_sub(send_dropped);
    tracing::info!(
        target_kbps,
        duration_ms = actual_ms,
        bytes_sent,
        au_count = packets_sent,
        wire_offered,
        wire_packets_sent,
        send_dropped,
        "speed-test probe burst complete"
    );
    ProbeResult {
        bytes_sent,
        packets_sent,
        duration_ms: actual_ms,
        wire_packets_sent,
        send_dropped,
    }
}

/// Drain any pending speed-test requests and run each burst, replying with its [`ProbeResult`].
/// Called once per data-plane loop iteration so a probe runs between frames. `probe_seq` = the
/// client advertised [`punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ`] (see [`run_probe_burst`]).
fn service_probes(
    session: &mut Session,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    probe_seq: bool,
) {
    while let Ok(req) = probe_rx.try_recv() {
        let result = run_probe_burst(session, req, stop, probe_seq);
        let _ = probe_result_tx.send(result);
    }
}

/// Seal one access unit and send it with MICROBURST pacing (the shared
/// [`send_pacing`](crate::send_pacing) policy, native parameterization): the first `burst_cap`
/// bytes go out immediately (one absorbed burst the NIC / socket tx-buffer can swallow), and
/// only the OVERFLOW beyond that is spread in 16-packet chunks across ~90% of the time to
/// `deadline`. So a normal-bitrate frame (≤ cap) leaves in one immediate burst at ~0 added
/// latency, while a genuine IDR / sustained-high-bitrate frame (≫ cap) still spreads — keeping
/// the freeze fix exactly where it's needed (an unpaced line-rate burst overruns the kernel tx
/// buffer → EAGAIN drop → under infinite GOP, a freeze until the next keyframe). With no slack
/// (encode ≈ interval) the budget collapses to 0 and even the overflow goes out immediately, so
/// this is never slower than unpaced.
#[allow(clippy::too_many_arguments)]
fn paced_submit(
    session: &mut Session,
    data: &[u8],
    pts_ns: u64,
    flags: u32,
    frame_index: u32,
    deadline: std::time::Instant,
    burst_cap: usize,
) -> Result<PaceStat> {
    let wires = session
        .seal_frame_at(data, pts_ns, flags, frame_index)
        .map_err(|e| anyhow!("seal_frame: {e:?}"))?;
    let mut refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
    // FEC/recovery test knob (PUNKTFUNK_VIDEO_DROP) — same knob the GameStream plane honors.
    crate::send_pacing::inject_video_drop(&mut refs);
    let cfg = crate::send_pacing::PaceCfg {
        burst_bytes: Some(burst_cap),
        chunk: crate::send_pacing::ChunkPolicy::Fixed(16),
        sleep_floor: std::time::Duration::from_micros(500),
    };
    let result = crate::send_pacing::pace_frame(
        &refs,
        crate::send_pacing::PaceBudget::UntilDeadline {
            deadline,
            fraction: 0.9,
        },
        &cfg,
        |chunk| session.send_sealed(chunk).map(|_| ()),
    );
    drop(refs); // release the borrow of `wires` so it can return to the seal pool
    session.reclaim_wires(wires);
    result.map_err(|e| anyhow!("send_sealed: {e:?}"))
}

/// One encoded frame handed from the capture/encode thread to the send thread (the encode|send
/// split). The send thread does FEC+seal+paced-send while this thread captures+encodes the next.
struct FrameMsg {
    data: Vec<u8>,
    capture_ns: u64,
    flags: u32,
    /// The wire `frame_index` this AU is sealed with. Assigned by the encode loop's
    /// session-lifetime counter (`au_seq`) — the loop owns the video numbering so the index it
    /// PREDICTED at submit time (`au_seq + inflight`, handed to `Encoder::submit_indexed`) is
    /// exactly what the packetizer stamps, keeping the encoder's RFI bookkeeping 1:1 with the
    /// wire across encoder rebuilds/resets. Sealed via `Session::seal_frame_at`.
    frame_index: u32,
    /// When this frame's packets should have fully left (the next frame's due time) = the pacing
    /// budget. In the past when the send thread is behind → immediate send (catch up).
    deadline: std::time::Instant,
    /// submit→encoded latency (µs), measured on the encode thread, carried for the perf histogram.
    encode_us: u32,
    /// Capture-delivery → encoder-submit age (µs) of a fresh frame — the PipeWire delivery +
    /// channel-queue time the old pre-submit stamp made invisible. Always measured (two integer
    /// ops); 0 for repeats/tail frames. The wire pts (`capture_ns`) anchors at the same delivery
    /// stamp, so client-side latency figures include this window too.
    queue_us: u32,
    /// Per-stage µs splits, measured on the capture/encode thread (0 when neither `PUNKTFUNK_PERF`
    /// nor a stats capture is armed). The send thread accumulates them for the web-console sample:
    /// `cap_us` = `try_latest` (ring read + colour convert), `submit_us` = NVENC `encode_picture`
    /// launch, `wait_us` = `lock_bitstream` (the scheduling wait + ASIC encode = the "encode" stage).
    cap_us: u32,
    submit_us: u32,
    wait_us: u32,
    /// This frame is a re-encoded hold (the source had no fresh frame): a source-starvation signal
    /// the send thread folds into `repeat_fps`.
    repeat: bool,
    /// Whether the per-stage splits (`cap_us`/`submit_us`/`wait_us`) were actually measured at
    /// capture time (`perf` was on or a stats capture was armed). The send thread trusts this
    /// instead of re-reading `is_armed()`, so a capture that arms while frames are already in flight
    /// doesn't fold their zeroed splits into the first window's percentiles.
    was_measured: bool,
}

/// The dedicated send thread: it owns the whole [`Session`] (so no socket clone or shared stats are
/// needed) and does FEC+seal + microburst-paced send OFF the capture/encode thread, plus the
/// speed-test probe bursts (which also need the Session). Decoupling the paced send from encoding
/// lets the encode of frame N+1 overlap the transmit of frame N instead of waiting behind its tail.
/// Runs until the encode thread drops the frame channel (end of stream) or `stop` is set.
/// Raise the current thread's OS scheduling priority so a CPU-heavy game can't deschedule our
/// capture/encode/send threads. This matters even though our GPU work is already HIGH priority: the
/// GPU scheduler can only favour commands we've actually SUBMITTED, so if a normal-priority thread is
/// descheduled by the game it submits the convert/encode late and the GPU priority never bites. Apollo
/// does the same (capture thread CRITICAL, encoder ABOVE_NORMAL). The Linux host needs this too: an
/// uncapped GPU-saturating title (e.g. CS2 direct on a virtual output, not capped by gamescope) is
/// also a CPU hog and can deschedule our submit threads. `critical` → highest non-realtime class
/// (the capture+encode loop); otherwise above-normal (the send/relay thread).
pub(crate) fn boost_thread_priority(critical: bool) {
    // Windows host-process/thread session tuning (timer 1ms, DWM MMCSS, HIGH class once; MMCSS +
    // keep-display-awake per thread). No-op off Windows. Both stream threads call us, so this covers
    // capture/encode (critical) and send (non-critical).
    crate::session_tuning::on_hot_thread();
    #[cfg(target_os = "windows")]
    // SAFETY: `GetCurrentThread()` returns the constant pseudo-handle for the calling thread — always
    // valid, thread-local in meaning, and never closed (no leak/double-close). `SetThreadPriority`
    // takes that handle plus a `THREAD_PRIORITY_*` value the windows crate defines (HIGHEST or
    // ABOVE_NORMAL here); it only reprioritizes this OS thread, borrows no Rust memory, and its
    // `Result` is matched (a failure is logged, never UB). No pointers, lifetimes, or aliasing.
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
            THREAD_PRIORITY_HIGHEST,
        };
        let prio = if critical {
            THREAD_PRIORITY_HIGHEST
        } else {
            THREAD_PRIORITY_ABOVE_NORMAL
        };
        match SetThreadPriority(GetCurrentThread(), prio) {
            Ok(()) => tracing::debug!(critical, "thread priority raised"),
            Err(e) => {
                tracing::debug!(critical, error = %format!("{e:?}"), "SetThreadPriority failed")
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Best-effort nice of the CALLING thread. On Linux `setpriority(PRIO_PROCESS, 0, …)` acts on
        // the calling thread (the kernel resolves who==0 to the current task/tid), and both call
        // sites run inside their worker thread — so this nices exactly the capture/encode (critical)
        // and send (non-critical) threads, nothing else. Silently no-ops without CAP_SYS_NICE / a
        // raised RLIMIT_NICE, which is fine. We deliberately do NOT use SCHED_RR/FIFO by default: a
        // realtime CPU class can preempt the compositor AND the game's own render thread, adding the
        // very frame-time we refuse to add (opt-in only — see PUNKTFUNK_SCHED_RR).
        let nice = if critical { -10 } else { -5 };
        // SAFETY: `setpriority` takes three by-value integers and no pointers, so there is nothing to
        // alias or outlive. `PRIO_PROCESS` with `who == 0` targets the calling task on Linux and
        // `nice` is in range; the call only adjusts this thread's scheduling nice value and returns an
        // `int` we inspect. No memory is touched.
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
        if rc == 0 {
            tracing::debug!(critical, nice, "thread nice raised");
        } else {
            tracing::debug!(
                critical,
                "setpriority(nice) no-op (needs CAP_SYS_NICE / RLIMIT_NICE)"
            );
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = critical;
    }
}

/// Everything the send thread needs to emit web-console stats samples at its 2 s aggregation
/// boundary: the shared recorder (whose `is_armed()` gates emission) plus the negotiated
/// mode/codec/client to seed the capture's `CaptureMeta` on the first armed registration.
struct SendStats {
    rec: Arc<StatsRecorder>,
    /// Live session mode, packed w:16|h:16|hz:16 ([`pack_mode`]) — the capture thread updates it
    /// on an accepted mid-stream mode switch (mirroring `bitrate_kbps` below), so a stats capture
    /// registers the mode the stream is ACTUALLY running at, not the session-start latch (H3).
    mode: Arc<AtomicU64>,
    codec: &'static str,
    client: String,
    /// Live encoder bitrate (kbps) — the capture thread updates it on a mid-stream adaptive
    /// bitrate change, so the web-console sample reports what the encoder is ACTUALLY targeting.
    bitrate_kbps: Arc<AtomicU32>,
}

/// Pack a `(width, height, refresh_hz)` mode into one atomic word (w:16|h:16|hz:16) for the live
/// stats-mode slot — one store/load instead of three racy ones. Every dimension fits: the codec
/// max dimension caps w/h well under 2^16 (`validate_dimensions`), refresh likewise.
fn pack_mode(width: u32, height: u32, refresh_hz: u32) -> u64 {
    ((width as u64 & 0xffff) << 32)
        | ((height as u64 & 0xffff) << 16)
        | (refresh_hz as u64 & 0xffff)
}

/// Unpack a [`pack_mode`] word back into `(width, height, refresh_hz)`.
fn unpack_mode(packed: u64) -> (u32, u32, u32) {
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

/// Whether a session on `compositor` (`None` = the synthetic source) with a `per_client_mode`
/// identity policy may LIVE-reconfigure — accept a mid-stream `Reconfigure`
/// (design/midstream-resolution-resize.md H1/H5). Gated OFF for:
///   * **gamescope** (every sub-mode): a resize would respawn the nested game / restart the box's
///     game-mode session — it must never relaunch the title, so the client keeps scaling client-side.
///   * a **per-client-mode identity** policy: the mode is part of the display-identity slot key, so a
///     resize resolves a DIFFERENT slot (a fresh Windows monitor / a differently-named KWin output),
///     defeating the policy — honest downgrade is to reject and let the client scale.
///
/// Every other compositor (and the synthetic protocol-test source) with the default identity accepts.
fn reconfig_allowed(
    compositor: Option<crate::vdisplay::Compositor>,
    per_client_mode: bool,
) -> bool {
    compositor != Some(crate::vdisplay::Compositor::Gamescope) && !per_client_mode
}

#[allow(clippy::too_many_arguments)]
fn send_loop(
    mut session: Session,
    frame_rx: std::sync::mpsc::Receiver<FrameMsg>,
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    stop: Arc<AtomicBool>,
    perf: bool,
    burst_cap: usize,
    fec_target: Arc<AtomicU8>,
    stats: SendStats,
    // `Some` = the client advertised VIDEO_CAP_HOST_TIMING: emit one 0xCF datagram per AU right
    // after its last packet left the socket (capture→sent, the whole host pipeline incl. pacing).
    timing_conn: Option<quinn::Connection>,
    // The client advertised VIDEO_CAP_PROBE_SEQ — mid-session speed-test bursts may run in the
    // probe index space (else they're declined; see `run_probe_burst`).
    probe_seq: bool,
) {
    boost_thread_priority(false); // transmit thread: above-normal (Apollo's encoder-thread level)
    let mut last_perf = std::time::Instant::now();
    let mut last_bytes = 0u64;
    let mut last_send_dropped = 0u64;
    let mut encode_us: Vec<u32> = Vec::new();
    let mut pace_us: Vec<u32> = Vec::new();
    let (mut paced_frames, mut immediate_frames) = (0u64, 0u64);
    // Web-console stats accumulation (active when `perf` OR the recorder is armed): the per-stage
    // split carried on each FrameMsg, the new-vs-repeat frame split, the cached registration id, and
    // the previous window's loss snapshot for delta computation.
    let mut sid: Option<u32> = None;
    let (mut cap_v, mut submit_v, mut wait_v, mut queue_v): (
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut new_frames, mut repeat_frames) = (0u64, 0u64);
    let mut last_frames_dropped = 0u64;
    let mut last_packets_dropped = 0u64;
    let mut last_fec_recovered = 0u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Probes run here (they need the Session); a burst pauses video — the encode thread blocks
        // on the full frame channel meanwhile, which is exactly the intended pause.
        service_probes(&mut session, &stop, &probe_rx, &probe_result_tx, probe_seq);
        // Adaptive FEC: pick up any new recovery target the control task set from client LossReports.
        apply_fec_target(&mut session, &fec_target);
        // Short timeout so we keep re-checking `stop` + probes when no frames are flowing.
        match frame_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(msg) => match paced_submit(
                &mut session,
                &msg.data,
                msg.capture_ns,
                msg.flags,
                msg.frame_index,
                msg.deadline,
                burst_cap,
            ) {
                Ok(stat) => {
                    // Host timing (0xCF): stamped now — the AU's packets have fully left the
                    // socket — against the same capture anchor the wire pts carries, so the
                    // client's per-frame math tiles exactly (network = its host+network − this).
                    // Best-effort like every side-plane datagram; skipped for speed-test filler
                    // (FLAG_PROBE isn't video and its pts is the burst clock).
                    if let Some(tc) = &timing_conn {
                        if msg.flags & FLAG_PROBE as u32 == 0 {
                            let host_us = (now_ns().saturating_sub(msg.capture_ns) / 1000)
                                .min(u32::MAX as u64)
                                as u32;
                            let t = punktfunk_core::quic::HostTiming {
                                pts_ns: msg.capture_ns,
                                host_us,
                            };
                            let _ = tc.send_datagram(
                                punktfunk_core::quic::encode_host_timing_datagram(&t).into(),
                            );
                        }
                    }
                    if perf || stats.rec.is_armed() {
                        // `encode_us`/`pace_us`/fps are valid for every frame (always measured),
                        // including the Windows relay + tail-drain frames. The cap/submit/wait splits
                        // are only real when the frame was measured at capture time — a frame captured
                        // before this capture armed carries zeroed splits, so skip those (an empty
                        // window → `percentile()` returns 0) rather than pull the percentiles down.
                        encode_us.push(msg.encode_us);
                        pace_us.push(stat.spread_us);
                        if msg.was_measured {
                            cap_v.push(msg.cap_us);
                            submit_v.push(msg.submit_us);
                            wait_v.push(msg.wait_us);
                            // Queue age is only meaningful for fresh frames (repeats/tail carry 0
                            // by construction — including those would drag the percentiles down).
                            if !msg.repeat {
                                queue_v.push(msg.queue_us);
                            }
                        }
                        if msg.repeat {
                            repeat_frames += 1;
                        } else {
                            new_frames += 1;
                        }
                        if stat.paced {
                            paced_frames += 1;
                        } else {
                            immediate_frames += 1;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "send failed — stopping stream");
                    break;
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break, // encode thread done
        }
        if last_perf.elapsed() >= std::time::Duration::from_secs(2) {
            let s = session.stats();
            let secs = last_perf.elapsed().as_secs_f64();
            // Attempted (sealed) transmit rate; `send_dropped` is what didn't reach the wire.
            let tx_mbps = (s.bytes_sent - last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
            if perf {
                tracing::info!(
                    tx_mbps = format!("{tx_mbps:.0}"),
                    send_dropped = s.packets_send_dropped - last_send_dropped,
                    send_dropped_total = s.packets_send_dropped,
                    encode_us_p50 = percentile(&mut encode_us, 0.50),
                    encode_us_p99 = percentile(&mut encode_us, 0.99),
                    pace_us_p50 = percentile(&mut pace_us, 0.50),
                    pace_us_p99 = percentile(&mut pace_us, 0.99),
                    pace_us_max = pace_us.last().copied().unwrap_or(0),
                    immediate_frames,
                    paced_frames,
                    "perf"
                );
            }
            // Web-console capture: this thread owns `session.stats()`, so it emits the COMPLETE
            // sample — the cap/submit/encode split carried over from the capture thread plus this
            // window's pacing/goodput/loss. Loss fields are deltas vs the previous window's snapshot.
            if stats.rec.is_armed() {
                let session_id = *sid.get_or_insert_with(|| {
                    // Read the LIVE mode at registration time (H3): a capture armed after a
                    // mid-stream mode switch gets the mode the stream actually runs at.
                    let (w, h, hz) = unpack_mode(stats.mode.load(Ordering::Relaxed));
                    stats
                        .rec
                        .register_session("native", w, h, hz, stats.codec, &stats.client)
                });
                let sample = crate::stats_recorder::StatsSample {
                    t_ms: 0, // stamped by push_sample from the capture's monotonic start
                    session_id,
                    stages: vec![
                        crate::stats_recorder::StageTiming {
                            name: "queue".into(),
                            p50_us: percentile(&mut queue_v, 0.50) as f32,
                            p99_us: percentile(&mut queue_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "capture".into(),
                            p50_us: percentile(&mut cap_v, 0.50) as f32,
                            p99_us: percentile(&mut cap_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "submit".into(),
                            p50_us: percentile(&mut submit_v, 0.50) as f32,
                            p99_us: percentile(&mut submit_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "encode".into(),
                            p50_us: percentile(&mut wait_v, 0.50) as f32,
                            p99_us: percentile(&mut wait_v, 0.99) as f32,
                        },
                        crate::stats_recorder::StageTiming {
                            name: "send".into(),
                            p50_us: percentile(&mut pace_us, 0.50) as f32,
                            p99_us: percentile(&mut pace_us, 0.99) as f32,
                        },
                    ],
                    fps: (new_frames as f64 / secs) as f32,
                    repeat_fps: (repeat_frames as f64 / secs) as f32,
                    mbps: tx_mbps as f32,
                    bitrate_kbps: stats.bitrate_kbps.load(Ordering::Relaxed),
                    frames_dropped: s.frames_dropped.saturating_sub(last_frames_dropped) as u32,
                    packets_dropped: s.packets_dropped.saturating_sub(last_packets_dropped) as u32,
                    send_dropped: s.packets_send_dropped.saturating_sub(last_send_dropped) as u32,
                    fec_recovered: s.fec_recovered_shards.saturating_sub(last_fec_recovered) as u32,
                };
                stats.rec.push_sample(session_id, sample);
            }
            last_perf = std::time::Instant::now();
            last_bytes = s.bytes_sent;
            last_send_dropped = s.packets_send_dropped;
            last_frames_dropped = s.frames_dropped;
            last_packets_dropped = s.packets_dropped;
            last_fec_recovered = s.fec_recovered_shards;
            encode_us.clear();
            pace_us.clear();
            cap_v.clear();
            submit_v.clear();
            wait_v.clear();
            queue_v.clear();
            paced_frames = 0;
            immediate_frames = 0;
            new_frames = 0;
            repeat_frames = 0;
        }
    }
}

/// A mid-stream session change the watcher detected (the box flipped Gaming↔Desktop): the new
/// backend + the [`crate::vdisplay::SessionEnv`] snapshot to retarget at it. The env is applied on
/// the encode thread (not the watcher), so the watcher never does a process-global env write.
struct SessionSwitch {
    kind: crate::vdisplay::ActiveKind,
    compositor: crate::vdisplay::Compositor,
    env: crate::vdisplay::SessionEnv,
}

/// Poll the live graphical session ~1 s and, when its kind changes from what the stream opened with
/// (the user switched Gaming↔Desktop mid-stream) and stays changed for a debounce, send one
/// [`SessionSwitch`] so the encode loop rebuilds the backend in place. Self-baselines on the first
/// read (so no handshake plumbing). Opt-in via `PUNKTFUNK_SESSION_WATCH`; readiness of the new
/// backend is left to the encode thread's `build_pipeline_with_retry` (the watcher never writes
/// env). Exits when `stop` is set or the channel closes.
/// Whether to run the mid-stream session-switch watcher. An explicit `PUNKTFUNK_SESSION_WATCH` wins
/// (truthy → on; `0`/`false`/`no`/`off`/empty → off). When unset it defaults **on** for Steam HTPC
/// platforms (Bazzite / SteamOS) — which flip Gaming↔Desktop and need the host to follow the switch
/// mid-stream — and **off** elsewhere, preserving the opt-in default for plain desktop hosts.
fn session_watch_enabled() -> bool {
    match std::env::var("PUNKTFUNK_SESSION_WATCH") {
        Ok(v) => {
            let v = v.trim();
            !(v.is_empty()
                || v == "0"
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off"))
        }
        Err(_) => is_steam_htpc_platform(),
    }
}

/// True on Bazzite or SteamOS (matched against os-release `ID`/`ID_LIKE`) — the platforms that flip
/// between Steam Gaming Mode and a Desktop session, where following a mid-stream switch is the
/// sensible default. Anything else (incl. non-Linux, where the file is absent) → false.
fn is_steam_htpc_platform() -> bool {
    let Ok(os) = std::fs::read_to_string("/etc/os-release") else {
        return false;
    };
    os.lines().any(|line| {
        let line = line.trim();
        let Some(val) = line
            .strip_prefix("ID=")
            .or_else(|| line.strip_prefix("ID_LIKE="))
        else {
            return false;
        };
        val.trim_matches('"')
            .split_whitespace()
            .any(|tok| tok.eq_ignore_ascii_case("bazzite") || tok.eq_ignore_ascii_case("steamos"))
    })
}

fn session_watcher_loop(tx: std::sync::mpsc::Sender<SessionSwitch>, stop: Arc<AtomicBool>) {
    use crate::vdisplay;
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(3);
    // Baseline = what the stream is currently driving (matches the handshake's resolution).
    let mut current = vdisplay::detect_active_session().kind;
    let mut pending: Option<(vdisplay::ActiveKind, std::time::Instant)> = None;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let active = vdisplay::detect_active_session();
        // A4: bump the session epoch + invalidate the old backend the moment the compositor instance
        // changes (kind change OR same-kind restart) — even for a same-kind restart the watcher won't
        // signal a full SessionSwitch for. Self-dedupes; the debounced SessionSwitch below still drives
        // the in-place rebuild.
        vdisplay::observe_session_instance(&active);
        let cur = active.kind;
        if cur == current {
            pending = None; // back to the current backend before debounce elapsed — no switch
            continue;
        }
        match pending {
            // Stable at the new kind for the debounce window — the switch is real, signal it.
            Some((k, since)) if k == cur && since.elapsed() >= DEBOUNCE => {
                match vdisplay::compositor_for_kind(cur) {
                    Some(comp) => {
                        tracing::info!(from = ?current, to = ?cur, compositor = comp.id(),
                            "session watcher: mid-stream switch — signaling backend rebuild");
                        if tx
                            .send(SessionSwitch {
                                kind: cur,
                                compositor: comp,
                                env: active.env,
                            })
                            .is_err()
                        {
                            break; // encode loop gone
                        }
                        current = cur; // new baseline; don't re-signal until it changes again
                    }
                    // Logout / no usable backend for the new session — keep streaming the old one.
                    None => tracing::debug!(to = ?cur,
                        "session watcher: no usable backend for the new session — staying put"),
                }
                pending = None;
            }
            // Still debouncing this kind.
            Some((k, _)) if k == cur => {}
            // A new (or different) change — start the debounce window.
            _ => pending = Some((cur, std::time::Instant::now())),
        }
    }
}

/// All per-session inputs for [`virtual_stream`], bundled so the session entry
/// is one moved value instead of a 13-positional-argument `#[allow(too_many_arguments)]` signature
/// (Goal-1 stage 4, plan §2.4). Everything is **owned** — the receivers move in (`virtual_stream` is their
/// only consumer) — so the whole context moves into the stream thread and the borrow plumbing disappears.
struct SessionContext {
    /// The hardened data-plane `Session` (Leopard FEC + AES-GCM over UDP); moved into the send thread.
    session: Session,
    /// The client's requested mode — the virtual output is created at exactly this WxH@Hz (no scaling).
    mode: punktfunk_core::Mode,
    /// Stream duration cap (the persistent listener bounds back-to-back sessions).
    seconds: u32,
    /// Session stop flag (set on disconnect / reconnect-preempt).
    stop: Arc<AtomicBool>,
    /// Deliberate-quit flag (set when the client closed with `QUIT_CODE`): the display lease reads it
    /// on teardown to skip the keep-alive linger for a user "stop" (vs. an unwanted disconnect).
    quit: Arc<AtomicBool>,
    /// Accepted mid-stream mode switches — the pipeline is rebuilt at the new mode.
    reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    /// Client decode-recovery keyframe requests.
    keyframe: std::sync::mpsc::Receiver<()>,
    /// Client LTR-RFI recovery requests — the lost-frame range `(first, last)`. The encode loop
    /// prefers `Encoder::invalidate_ref_frames` over a full IDR when the encoder supports it.
    rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    /// Accepted mid-stream bitrate changes (adaptive bitrate, already clamped) — the encoder
    /// alone is rebuilt in place at the new rate; capture + virtual output are untouched.
    bitrate_rx: std::sync::mpsc::Receiver<u32>,
    /// The resolved compositor backend (moot on Windows — `vdisplay::open` ignores it there).
    compositor: crate::vdisplay::Compositor,
    /// Negotiated encoder bitrate (kbps).
    bitrate_kbps: u32,
    /// Negotiated encode bit depth (8, or 10 = HEVC Main10).
    bit_depth: u8,
    /// Negotiated chroma subsampling (4:2:0, or 4:4:4 when the client + host + GPU all support it).
    chroma: crate::encode::ChromaFormat,
    /// Negotiated video codec the encoder emits (HEVC by default; H.264 / AV1 when the client
    /// prefers one the GPU encodes; H.264 for a software host). Also used to rebuild the encoder
    /// at the same codec across a mid-stream mode reconfigure.
    codec: crate::encode::Codec,
    /// Speed-test burst requests (see [`service_probes`]).
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    /// Speed-test results back to the control task.
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    /// Mode-switch outcomes back to the control task (H2): a corrective
    /// `Reconfigured { accepted: true, mode: <actually live> }` when a rebuild failed (stayed at
    /// the old mode) or the backend honored a different refresh than requested.
    reconfig_result_tx: tokio::sync::mpsc::UnboundedSender<Reconfigured>,
    /// Adaptive-FEC target the control task updates from the client's loss reports.
    fec_target: Arc<AtomicU8>,
    /// The QUIC control connection (carries host→client 0xCE source-HDR metadata mid-stream).
    conn: quinn::Connection,
    /// `Some` when the client advertised [`punktfunk_core::quic::VIDEO_CAP_HOST_TIMING`]: the send
    /// thread emits one 0xCF datagram per AU (capture→sent µs) on it, so the client can split its
    /// `host+network` latency stage. `None` = older client, no emission.
    timing_conn: Option<quinn::Connection>,
    /// The client advertised [`punktfunk_core::quic::VIDEO_CAP_PROBE_SEQ`]: speed-test bursts may
    /// run mid-session in the probe index space (its reassembler keeps a separate probe window).
    /// `false` = older client whose single-window reassembler would drop probe-space frames as
    /// stale — mid-session probes are DECLINED for it (a zeroed [`ProbeResult`]) rather than
    /// consuming video frame indexes its gap detectors can't see (the phantom-gap freeze).
    probe_seq: bool,
    /// Shared streaming-stats recorder. The capture loop reads `is_armed()` per frame to decide
    /// whether to measure the per-stage split; the send thread builds + pushes the aggregated
    /// `StatsSample` at its 2 s boundary.
    stats: Arc<StatsRecorder>,
    /// Short client label (cert-fingerprint prefix, else peer IP) seeded into the capture meta on
    /// the first armed stats registration.
    client_label: String,
    /// The session's requested launch, `None` = none. On Windows the store-qualified library id
    /// (spawned into the interactive user session once capture is live); on other hosts the shell
    /// command already resolved against the host's own library — nested into gamescope's bare spawn
    /// via `set_launch_command`, or spawned into the live session once capture is up.
    launch: Option<String>,
    /// The client display's HDR colour volume (`Hello::display_hdr`; `None` = older client / SDR).
    /// Threaded into the vdisplay backend before `create` (→ the pf-vdisplay EDID's CTA HDR block,
    /// so host apps tone-map to the client's real panel) and preferred over the generic baseline
    /// for the 0xCE mastering metadata.
    client_hdr: Option<punktfunk_core::quic::HdrMeta>,
}

fn virtual_stream(ctx: SessionContext) -> Result<()> {
    // This thread runs the capture+encode loop (single-process — the only topology: Linux portal /
    // synthetic, Windows in-process IDD-push). Elevate it so a CPU-heavy game can't deschedule our GPU
    // submission.
    boost_thread_priority(true);
    // Resolve the per-session capture / topology / encoder decision ONCE (Goal-1 stage 3): the deployed
    // path now reads this typed `SessionPlan` instead of re-deriving from config at each dispatch site
    // (the latent "capture and encode disagree on the backend" hazard, plan §2.4). `bit_depth` is the
    // only per-session input — capture/topology/encoder are otherwise pure functions of `HostConfig`.
    let plan = crate::session_plan::SessionPlan::resolve(ctx.bit_depth, ctx.chroma, ctx.codec);
    tracing::info!(?plan, "resolved session plan");
    // Single-process path: unpack the context into the locals the loop below uses (names unchanged, so the
    // body is byte-for-byte the same; the receivers are now owned but `try_recv()` is identical).
    let SessionContext {
        session,
        mode,
        seconds,
        stop,
        quit,
        reconfig,
        keyframe,
        rfi,
        bitrate_rx,
        compositor,
        mut bitrate_kbps,
        bit_depth,
        // The resolved chroma is already captured in `plan` (above); ignore the duplicate here.
        chroma: _,
        // Likewise the codec — `plan.codec` (resolved from `ctx.codec`) is the source of truth below.
        codec: _,
        probe_rx,
        probe_result_tx,
        reconfig_result_tx,
        fec_target,
        conn,
        timing_conn,
        probe_seq,
        stats,
        client_label,
        launch,
        client_hdr,
    } = ctx;
    tracing::info!(
        compositor = compositor.id(),
        ?mode,
        bitrate_kbps,
        bit_depth,
        "punktfunk/1 virtual display"
    );
    // Open the backend FIRST — on Windows this constructs the vdisplay backend, which initialises the
    // host-lifetime VirtualDisplayManager (§2.5). It does NO monitor work, so it must precede the IDD-push
    // preempt below (which reaches the manager) — otherwise `vdm()` is called before init and panics.
    let mut vd = crate::vdisplay::open(compositor)?;
    // Per-client STABLE monitor identity (Phase 2): hand the backend the connecting client's cert
    // fingerprint so a freshly CREATED virtual monitor gets this client's persistent id — Windows then
    // reapplies the client's saved per-monitor config (DPI scaling) on reconnect. No-op on Linux backends
    // and for anonymous/GameStream clients (no fingerprint → the driver auto-allocates).
    vd.set_client_identity(endpoint::peer_fingerprint(&conn));
    // The client display's HDR volume (Hello) → a freshly created virtual monitor's EDID CTA HDR
    // block (pf-vdisplay), so host apps + the OS tone-map to the client's real panel instead of the
    // driver's built-in ~1000-nit placeholder. No-op on Linux backends and for older/SDR clients.
    vd.set_client_hdr(client_hdr);
    // Deliberate-quit wiring (Windows pf-vdisplay; no-op elsewhere): every lease the backend mints —
    // the retry-hold below AND the capturer's — carries the session's quit flag, so a user "stop"
    // (⌘D → the QUIT close code) tears the virtual monitor down the moment the pipeline drops instead
    // of lingering 10 s. The reconnect then finds the manager Idle and does a clean fresh ADD (with
    // the user's think-time as driver settle) rather than the Lingering-preempt's REMOVE→ADD churn.
    // `keep_alive = forever` (gaming-rig) outranks the quit — the monitor pins as before.
    vd.set_quit_flag(quit.clone());
    // Per-session launch (non-Windows): hand the resolved command to the backend instance so
    // gamescope's bare spawn nests it — per-instance, no process-global env, so concurrent sessions
    // can't stomp each other's launch target. The other backends' default `set_launch_command` is a
    // no-op; they get the command spawned into the live session after capture is up (below).
    #[cfg(not(target_os = "windows"))]
    vd.set_launch_command(launch.clone());
    // IDD-push reconnect preempt (the dance now lives in the manager, Goal-1 §2.5): serialize setup so a
    // reconnect FLOOD can't run concurrent monitor create/teardown, STOP the prior session + WAIT for it
    // to release its monitor (instead of tearing a monitor out from under a still-live session), and
    // register THIS session's stop. The returned guard holds the setup lock across the pipeline build;
    // dropping it lets the next reconnect begin (and preempt us). Held BEFORE the monitor is created
    // (build_pipeline → vd.create), so the preempt still precedes this session's monitor creation.
    // SLOT-scoped (Stage W1): the preempt targets only a prior session holding THIS client's slot —
    // a different identity's session is an admission question, never a preempt.
    #[cfg(target_os = "windows")]
    let _idd_setup_guard =
        (plan.capture == crate::session_plan::CaptureBackend::IddPush).then(|| {
            let slot = crate::vdisplay::manager::slot_id_for(
                endpoint::peer_fingerprint(&conn),
                (mode.width, mode.height),
            );
            crate::vdisplay::manager::vdm().begin_idd_setup(slot, stop.clone())
        });
    let (mut capturer, mut enc, mut frame, mut interval, mut cur_node_id, mut cur_display_gen) =
        build_pipeline_with_retry(&mut vd, mode, bitrate_kbps, bit_depth, plan, &quit, &stop)?;
    // Setup done — release the IDD-push setup lock so the next reconnect can begin (and preempt us).
    #[cfg(target_os = "windows")]
    drop(_idd_setup_guard);

    // Capture is live — launch the requested title so it renders onto the streamed output and
    // grabs focus. Windows spawns the library id into the interactive user session; Linux spawns
    // the resolved command into the live session for every backend that didn't already nest it
    // (gamescope's bare spawn ran it inside the fresh gamescope — launching again would start it
    // twice). Best-effort: a launch failure (no recipe, launcher missing, no interactive user)
    // leaves the user on the streamed desktop/session, never tears the stream down. Launched ONCE
    // here — the mid-stream rebuild paths below must not re-spawn it.
    #[cfg(target_os = "windows")]
    if let Some(id) = launch.as_deref() {
        if let Err(e) = crate::library::launch_title(id) {
            tracing::warn!(launch_id = id, error = %e, "could not launch requested library title");
        }
    }
    #[cfg(target_os = "linux")]
    if let Some(cmd) = launch.as_deref() {
        if crate::vdisplay::launch_is_nested(compositor) {
            tracing::info!(command = %cmd, "launch nested into the per-session gamescope");
        } else if let Err(e) = crate::library::launch_session_command(compositor, cmd) {
            tracing::warn!(command = %cmd, error = %e, "could not launch requested title into the session");
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let _ = &launch;

    let perf = crate::config::config().perf;
    // Microburst cap (applied in send_loop/paced_submit): a frame ≤ this bursts out immediately;
    // only a bigger frame's overflow is spread. PUNKTFUNK_PACE_BURST_KB overrides the 128 KB default.
    let burst_cap = std::env::var("PUNKTFUNK_PACE_BURST_KB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(128)
        * 1024;

    // Encode|send split: this thread captures+encodes (the GPU work) + handles reconfig, and hands
    // each AU to a dedicated send thread that owns the Session and does FEC+seal+paced-send — so the
    // encode of frame N+1 overlaps the paced transmit of frame N instead of waiting behind its tail.
    // The bounded channel applies backpressure (the encode thread blocks if the send falls behind,
    // so frames slow down rather than a dropped frame freezing the infinite-GOP stream).
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<FrameMsg>(3);
    // Live encoder bitrate, shared with the send thread's stats sample: a mid-stream adaptive
    // bitrate change (bitrate_rx below) updates it so the console shows the actual target.
    let live_bitrate = Arc::new(AtomicU32::new(bitrate_kbps));
    // Live session mode, same pattern (H3): a mid-stream mode switch (reconfig below) updates it so
    // a stats capture armed after a resize registers the real mode. Seeded with the refresh the
    // initial build actually achieved (`interval_hz`), not the request — KWin may cap a virtual
    // output at 60 Hz.
    let live_mode = Arc::new(AtomicU64::new(pack_mode(
        mode.width,
        mode.height,
        interval_hz(interval),
    )));
    // The send thread emits the web-console stats sample (it owns `session.stats()`); clone the
    // recorder so the capture loop keeps its own handle for the per-frame `is_armed()` gate.
    let send_stats = SendStats {
        rec: stats.clone(),
        mode: live_mode.clone(),
        codec: plan.codec.label(),
        client: client_label,
        bitrate_kbps: live_bitrate.clone(),
    };
    let send_thread = std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn({
            let stop = stop.clone();
            move || {
                send_loop(
                    session,
                    frame_rx,
                    probe_rx,
                    probe_result_tx,
                    stop,
                    perf,
                    burst_cap,
                    fec_target,
                    send_stats,
                    timing_conn,
                    probe_seq,
                )
            }
        })
        .context("spawn send thread")?;

    // Mid-stream session-switch watcher (opt-in via PUNKTFUNK_SESSION_WATCH; never under an explicit
    // PUNKTFUNK_COMPOSITOR pin). It self-baselines and signals the loop below to swap the backend in
    // place when the box flips Gaming↔Desktop. When not spawned, session_rx just stays empty.
    let mut compositor = compositor;
    let (session_tx, session_rx) = std::sync::mpsc::channel::<SessionSwitch>();
    let watch = session_watch_enabled() && crate::config::config().compositor.is_none();
    let _watcher = if watch {
        tracing::info!("session watcher on — following a mid-stream Gaming↔Desktop switch");
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("punktfunk1-watcher".into())
            .spawn(move || session_watcher_loop(session_tx, stop))
            .ok()
    } else {
        None
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
    let mut next = std::time::Instant::now();
    let mut sent: u64 = 0;
    // The session's video frame numbering, owned HERE (the wire `frame_index` of the next AU this
    // loop hands to the send thread; the packetizer seals with exactly this via `seal_frame_at`).
    // A submission's future index is predicted as `au_seq + inflight.len()` — exact because AUs
    // are emitted FIFO, one per submission, and every event that forfeits in-flight frames
    // (reset/rebuild/teardown) clears `inflight` AND the encoder's reference state, so the reused
    // predictions can never meet stale bookkeeping. Passing it to `Encoder::submit_indexed` keeps
    // the RFI backends' frame numbers 1:1 with the client's across encoder rebuilds — an
    // encoder-internal counter desyncs on the first adaptive-bitrate rebuild (NVENC RFI then
    // silently dies; AMF may anchor onto a post-loss LTR).
    let mut au_seq: u32 = 0;
    // Rebuild-in-place on capture loss: track the live mode (a mode switch updates it) so a rebuild
    // targets the CURRENT mode, and cap consecutive rebuilds so a flapping source can't loop the
    // client through endless cold restarts.
    let mut cur_mode = mode;
    const MAX_CAPTURE_REBUILDS: u32 = 5;
    let mut capture_rebuilds: u32 = 0;
    // Encode-stall watchdog: AMF/QSV (and async NVENC) poll non-blocking, so a wedged driver
    // shows up as poll() returning None forever while submits keep succeeding — `inflight` grows,
    // no AU ever reaches the send thread, and the client freezes on the last frame with nothing
    // logged (field reports: AMD/Intel Windows streams freezing after minutes). Track when the
    // encoder last produced an AU and rebuild it in place (bounded, like the capture rebuilds)
    // when it stops. `ENCODE_STALL_WINDOW` also sizes the in-flight backlog bound: a backlog worth
    // more than the window's frames means AUs still trickle (so the gap never trips) but latency
    // is growing without bound — the slow-leak form of the same stall.
    const ENCODE_STALL_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
    const MAX_ENCODER_RESETS: u32 = 5;
    let mut encoder_resets: u32 = 0;
    let mut last_au_at = std::time::Instant::now();
    // Last HDR mastering metadata we forwarded — re-sent as 0xCE on change/keyframe (see below).
    let mut last_hdr_meta: Option<punktfunk_core::quic::HdrMeta> = None;
    // Frames submitted to NVENC but not yet polled (wire pts, submit stamp, pacing deadline). With a
    // capturer that hands a fresh output texture per frame, the loop submits N+1 before polling N
    // (pipeline depth > 1), overlapping the convert/copy of N+1 on the 3D engine with the encode of N
    // on the NVENC ASIC. The wire pts and the submit stamp are carried separately so `encode_us`
    // keeps meaning submit→AU while the wire pts anchors at PipeWire delivery (queue age included).
    let mut inflight: std::collections::VecDeque<(u64, u64, std::time::Instant)> =
        std::collections::VecDeque::new();
    // Diagnostic: distinguish NEW captured frames (the source produced a fresh frame) from REPEATS (the
    // loop re-encoded the last frame because `try_latest` had nothing). A low new-frame rate at a high
    // send rate ⇒ the capture source isn't producing frames (e.g. an IDD virtual display DWM isn't
    // compositing), NOT an encoder problem. Logged every 2 s when `PUNKTFUNK_PERF`.
    let (mut diag_new, mut diag_repeat) = (0u64, 0u64);
    let mut diag_at = std::time::Instant::now();
    // Anchor for the forced-IDR cooldown (see the keyframe-request handling below): the timestamp of
    // the most recent forced/opening IDR. The session's pipeline just opened on an IDR, so start the
    // clock now — that coalesces the keyframe storm a client fires while its decoder wedges on the cold
    // opening GOP, instead of answering it with a redundant second IDR.
    let mut last_forced_idr: Option<std::time::Instant> = Some(std::time::Instant::now());
    // Self-diagnosis for the periodic-stutter class: warns when the served recovery IDRs settle
    // into a stable multi-second rhythm (see [`crate::metronome::Metronome`]).
    let mut recovery_cadence = crate::metronome::Metronome::new();
    // Position within the current intra-refresh wave (frames since the last IDR/wave start). Only
    // meaningful on a `caps().intra_refresh_recovery` encoder; the pump tags every wave-boundary AU
    // with `USER_FLAG_RECOVERY_POINT` so the client can lift its post-loss freeze on a clean
    // re-anchor without a full IDR. Re-phased to 0 at each emitted IDR (which restarts the wave).
    let mut ir_wave_pos: u32 = 0;
    // Per-stage latency breakdown (PUNKTFUNK_PERF): per-call µs for the GPU-bound stages so we see
    // exactly where the capture→encoded latency goes — cap=try_latest (ring read + colour convert),
    // submit=encode_picture launch, wait=lock_bitstream (the scheduling wait + ASIC encode, the one
    // that dominates under a GPU-saturating game).
    let (mut st_cap, mut st_submit, mut st_wait, mut st_queue): (
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
        Vec<u32>,
    ) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
        // Mid-stream session switch (the box flipped Gaming↔Desktop): rebuild the WHOLE backend in
        // place — a different compositor at the SAME client mode — keeping the Session + send thread
        // (and thus the QUIC control + UDP data plane) up. Takes precedence over a queued mode change.
        let mut switch = None;
        while let Ok(s) = session_rx.try_recv() {
            switch = Some(s); // coalesce to the newest
        }
        if let Some(sw) = switch {
            if sw.compositor != compositor {
                tracing::info!(from = compositor.id(), to = sw.compositor.id(), kind = ?sw.kind,
                    "session switch — rebuilding backend in place");
                // Retarget the process env at the new session BEFORE opening the new backend (this
                // thread is the only env writer; the watcher only snapshots).
                crate::vdisplay::apply_session_env(&crate::vdisplay::ActiveSession {
                    kind: sw.kind,
                    env: sw.env,
                    compositor_pid: None,
                });
                // A mid-stream Game↔Desktop switch is not a fresh dedicated launch — route input at the
                // switched-to backend's normal sub-mode.
                crate::vdisplay::apply_input_env(sw.compositor, false);
                // Switching INTO a desktop mid-stream: the xdg portal / systemd-user env may still
                // point at the old session, so input would silently not land until a reconnect.
                // Settle it (env push + KWin portal restart) before the injector reopens against it.
                if matches!(
                    sw.compositor,
                    crate::vdisplay::Compositor::Kwin | crate::vdisplay::Compositor::Mutter
                ) {
                    crate::vdisplay::settle_desktop_portal(sw.compositor);
                }
                // Build the new backend's pipeline BEFORE dropping the old one (retry absorbs the
                // brief compositor-coexistence race during a switch); on failure keep the old.
                let rebuilt =
                    (|| -> Result<(Box<dyn crate::vdisplay::VirtualDisplay>, Pipeline)> {
                        let mut new_vd = crate::vdisplay::open(sw.compositor)?;
                        let pipe = build_pipeline_with_retry(
                            &mut new_vd,
                            cur_mode,
                            bitrate_kbps,
                            bit_depth,
                            plan,
                            &quit,
                            &stop,
                        )?;
                        Ok((new_vd, pipe))
                    })();
                match rebuilt {
                    Ok((
                        new_vd,
                        (new_cap, new_enc, new_frame, new_interval, new_node_id, new_gen),
                    )) => {
                        // Replace the pipeline first (drops the old capturer → old PipeWire stream +
                        // virtual output), then the factory (drops e.g. the old KWin connection).
                        capturer = new_cap;
                        enc = new_enc;
                        frame = new_frame;
                        interval = new_interval;
                        cur_node_id = new_node_id;
                        cur_display_gen = new_gen;
                        vd = new_vd;
                        compositor = sw.compositor;
                        next = std::time::Instant::now();
                        // The owed AUs died with the old encoder — drop their in-flight records
                        // and restart the encode-stall clock for the fresh one.
                        inflight.clear();
                        last_au_at = std::time::Instant::now();
                        encoder_resets = 0;
                        tracing::info!(
                            compositor = compositor.id(),
                            "session switch — backend rebuilt, stream continues"
                        );
                    }
                    Err(e) => {
                        let chain = format!("{e:#}");
                        let kind = if is_permanent_build_error(&chain) {
                            "permanent"
                        } else {
                            "transient"
                        };
                        tracing::error!(error = %chain, kind,
                            "session-switch rebuild failed — staying on the current backend");
                    }
                }
            }
        }
        // Drain to the NEWEST requested mode (a resize drag queues many) so we rebuild once,
        // not once per stale intermediate mode.
        let mut want = None;
        while let Ok(m) = reconfig.try_recv() {
            want = Some(m);
        }
        if let Some(new_mode) = want {
            tracing::info!(?new_mode, "rebuilding pipeline for mode switch");
            // Build the new pipeline BEFORE dropping the old one: the host already acked
            // the switch as accepted, so a rebuild failure must not kill an otherwise
            // healthy session — keep streaming the current mode and log instead.
            match build_pipeline(&mut vd, new_mode, bitrate_kbps, bit_depth, plan, &quit) {
                Ok(next_pipe) => {
                    let old_display_gen = cur_display_gen;
                    // The destructuring assignment drops the OLD capturer (→ its display lease) as
                    // each binding is replaced — the new pipeline is already up (create-before-drop).
                    (capturer, enc, frame, interval, cur_node_id, cur_display_gen) = next_pipe;
                    cur_mode = new_mode;
                    next = std::time::Instant::now();
                    // H4: the old display's lease drop above is indistinguishable from a disconnect
                    // to the keep-alive machinery — under linger/forever policies every resize would
                    // ACCUMULATE kept monitors at stale modes. Retire the superseded entry now (a
                    // no-op when it was already torn down under `immediate`, or off Linux).
                    if let Some(g) = old_display_gen.filter(|g| cur_display_gen != Some(*g)) {
                        crate::vdisplay::registry::retire(g);
                    }
                    // H2/H3: the backend may have honored a different mode than requested — KWin
                    // caps a virtual output's refresh, or Windows pf-vdisplay rejects an in-place
                    // SetMode to a resolution its running monitor doesn't advertise and the host
                    // falls back to the actual display mode. `frame` is the NEW pipeline's first
                    // frame (just rebound above), so its dims are what the client actually decodes.
                    // Publish that ACTUAL mode to the live stats slot, and correct the client's mode
                    // slot when it differs from the accept ack it already got.
                    let actual = delivered_mode(frame.width, frame.height, interval);
                    live_mode.store(
                        pack_mode(actual.width, actual.height, actual.refresh_hz),
                        Ordering::Relaxed,
                    );
                    if actual != new_mode {
                        let _ = reconfig_result_tx.send(Reconfigured {
                            accepted: true,
                            mode: actual,
                        });
                    }
                    // The owed AUs died with the old encoder — drop their in-flight records
                    // and restart the encode-stall clock for the fresh one.
                    inflight.clear();
                    last_au_at = std::time::Instant::now();
                    encoder_resets = 0;
                    last_forced_idr = Some(std::time::Instant::now()); // fresh encoder opens on an IDR — anchor the cooldown
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), ?new_mode,
                        "mode-switch rebuild failed — staying on the current mode");
                    // H2 rollback: the control task acked the switch BEFORE this rebuild, so the
                    // client's mode slot already flipped to `new_mode`. A second accepted ack
                    // carrying the still-live mode corrects it (any accepted ack means "the active
                    // mode is now X" client-side; old clients just log it). `frame` is untouched
                    // here (the destructure only runs on the Ok arm), so it's still the OLD
                    // pipeline's frame — its real dims + interval are exactly what's still on glass.
                    let _ = reconfig_result_tx.send(Reconfigured {
                        accepted: true,
                        mode: delivered_mode(frame.width, frame.height, interval),
                    });
                }
            }
        }
        // Adaptive bitrate: drain to the NEWEST requested rate (the client's controller may step
        // several times while we stream) and rebuild the ENCODER ONLY in place — the mode didn't
        // change, so capture and the virtual output are untouched and the switch costs exactly the
        // IDR the fresh encoder opens with (the same resync discipline as a mode switch, minus the
        // pipeline churn). Rates arrive pre-clamped by the control task (`resolve_bitrate_kbps`).
        let mut want_kbps = None;
        while let Ok(k) = bitrate_rx.try_recv() {
            want_kbps = Some(k);
        }
        if let Some(new_kbps) = want_kbps.filter(|&k| k != bitrate_kbps) {
            // `interval` was built as 1/effective_hz, so the round-trip recovers the integer rate.
            let hz = interval_hz(interval);
            match crate::encode::open_video(
                plan.codec,
                frame.format,
                frame.width,
                frame.height,
                hz,
                new_kbps as u64 * 1000,
                frame.is_cuda(),
                bit_depth,
                plan.chroma,
            ) {
                Ok(new_enc) => {
                    tracing::info!(
                        from_kbps = bitrate_kbps,
                        to_kbps = new_kbps,
                        "encoder rebuilt at new bitrate (adaptive bitrate)"
                    );
                    enc = new_enc;
                    bitrate_kbps = new_kbps;
                    live_bitrate.store(new_kbps, Ordering::Relaxed);
                    // The owed AUs died with the old encoder — same bookkeeping as a mode-switch
                    // rebuild; the fresh encoder opens on an IDR, so anchor the IDR cooldown too.
                    inflight.clear();
                    last_au_at = std::time::Instant::now();
                    encoder_resets = 0;
                    last_forced_idr = Some(std::time::Instant::now());
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), to_kbps = new_kbps,
                        "bitrate-change encoder rebuild failed — keeping the current rate");
                }
            }
        }
        // Client recovery: it asked for a fresh IDR (its decoder wedged on the cold opening
        // GOP). Coalesce the backlog — several requests fire before the IDR lands — and force
        // the next encoded frame to be a keyframe. (A reconfig rebuild above already opens with
        // an IDR, so this is for the steady-state wedge, not mode switches.)
        let mut want_kf = false;
        while keyframe.try_recv().is_ok() {
            want_kf = true;
        }
        // Client LTR-RFI recovery: prefer re-referencing a known-good older frame (a clean recovery
        // P-frame — no 20-40× IDR spike) over a full keyframe when the encoder supports it (native
        // AMF LTR / Windows NVENC). Drain the backlog (the client re-requests until the recovery
        // frame lands) coalesced to the widest lost range. Attempt the invalidate only when a full
        // IDR isn't already queued — an explicit keyframe request means a fully wedged decoder that
        // needs the IDR, which supersedes an RFI recovery. A failure (range older than the encoder's
        // live references, or no RFI backend) falls through to the coalesced keyframe path below.
        let mut rfi_range: Option<(u32, u32)> = None;
        while let Ok((first, last)) = rfi.try_recv() {
            rfi_range = Some(match rfi_range {
                Some((pf, pl)) => (pf.min(first), pl.max(last)),
                None => (first, last),
            });
        }
        if !want_kf {
            if let Some((first, last)) = rfi_range {
                // Sanity-cap the range before consulting the encoder: RFI can only re-reference
                // history the encoder still holds (NVENC: a 5-frame DPB; AMD LTR: ~1 s of marks).
                // A range wider than RFI_MAX_RANGE is either a seconds-long outage (no valid
                // reference anywhere) or a phantom jump from a desynced counter — both belong on
                // the keyframe path, never a force-reference that could ship corruption as a
                // recovery anchor. Wrapping width: frame indexes are u32 counters.
                let width = last.wrapping_sub(first);
                if width > punktfunk_core::packet::RFI_MAX_RANGE {
                    tracing::debug!(first, last, width, "RFI range too wide — keyframe instead");
                    want_kf = true;
                } else if enc.caps().supports_rfi
                    && enc.invalidate_ref_frames(first as i64, last as i64)
                {
                    // The RFI recovered the loss with a clean re-anchor P-frame (no IDR). Anchor the
                    // keyframe cooldown so the client's echo of the SAME loss — its frames_dropped-
                    // driven keyframe request, arriving ~one loss-window later — is coalesced away
                    // instead of emitting a redundant full IDR right after the cheap recovery.
                    last_forced_idr = Some(std::time::Instant::now());
                } else {
                    want_kf = true; // range too old / no RFI backend → coalesced keyframe below
                }
            }
        }
        if want_kf {
            // Clients request a keyframe on EVERY FEC-unrecoverable frame (`frames_dropped` polling)
            // and keep asking until the IDR actually arrives + decodes — a full round-trip on a link
            // that is already behind. Answering each request with a full IDR is a 20-40× bitrate spike
            // that DEEPENS the very loss it is recovering from: a burst of loss → a storm of IDRs →
            // more loss, the periodic double-jolt a Wi-Fi client sees. So coalesce a request storm into
            // at most ONE forced IDR per cooldown, ALWAYS — not only under intra-refresh (the old gate;
            // a full-IDR recovery is exactly where the storm is worst). Serve the first request
            // immediately (a genuinely wedged decoder recovers at once), then suppress for the window.
            //
            // Intra-refresh heals via its own gradual wave (~0.5 s) and can afford a long window; a
            // full-IDR recovery relies on the keyframe itself, so its window is shorter — long enough to
            // swallow the round-trip echo of one recovery event, short enough to re-issue a *lost* IDR
            // promptly.
            const IDR_COOLDOWN_INTRA: std::time::Duration = std::time::Duration::from_secs(2);
            const IDR_COOLDOWN_FULL: std::time::Duration = std::time::Duration::from_millis(750);
            let window = if enc.caps().intra_refresh {
                IDR_COOLDOWN_INTRA
            } else {
                IDR_COOLDOWN_FULL
            };
            let suppress = last_forced_idr.is_some_and(|t| t.elapsed() < window);
            if suppress {
                tracing::debug!("keyframe request coalesced — within the IDR cooldown");
            } else {
                tracing::debug!("forcing keyframe (client decode recovery)");
                enc.request_keyframe();
                let now = std::time::Instant::now();
                last_forced_idr = Some(now);
                if let Some(period) = recovery_cadence.note(now) {
                    tracing::warn!(
                        period_s = format!("{:.1}", period.as_secs_f64()),
                        "client keyframe recoveries are METRONOMIC — a periodic host/display \
                         disturbance (display-topology churn, display-poller software, \
                         virtual-display timing) is the likely cause, not random network loss; \
                         correlate with 'slow display-descriptor poll' / 'display descriptor \
                         changed' / 'IDD-push capture stall' lines"
                    );
                }
            }
        }
        // Measure the per-stage split when `PUNKTFUNK_PERF` is set OR a web-console stats capture is
        // armed (a cheap Relaxed atomic, re-read each frame). The values feed the existing perf log
        // unchanged and ride each FrameMsg to the send thread, which builds the aggregated sample.
        let measure = perf || stats.is_armed();
        let t_cap = std::time::Instant::now();
        let cap_result = capturer.try_latest();
        let cap_us = if measure {
            t_cap.elapsed().as_micros() as u32
        } else {
            0
        };
        if perf {
            st_cap.push(cap_us);
        }
        let mut repeat = false;
        match cap_result {
            Ok(Some(f)) => {
                frame = f;
                diag_new += 1;
                capture_rebuilds = 0; // a delivered frame clears the consecutive-loss counter
            }
            Ok(None) => {
                diag_repeat += 1; // no new frame (static desktop / mid-rebuild) — repeat the last
                repeat = true;
            }
            // The capture source died (PipeWire/compositor thread ended, virtual output gone). Rather
            // than tear the whole session down — the client has no reconnect path and would have to
            // cold-restart the handshake — rebuild the pipeline IN PLACE at the current mode, exactly
            // like a mode/session switch. A genuinely dead source still ends the session once the
            // bounded retry is exhausted; the consecutive cap stops a flapping source from looping the
            // client through endless cold IDRs.
            Err(e) => {
                // B2: a DEDICATED gamescope game session whose gamescope node is gone = the game
                // exited (gamescope is a single-app compositor — it dies with its app). End the session
                // CLEANLY — close with `APP_EXITED_CLOSE_CODE` so a launcher client returns to its
                // library instead of surfacing a failure — rather than the capture-loss rebuild + 40 s
                // timeout. Gated to the dedicated bare-spawn launch (`launch_is_nested`), so a normal
                // Bazzite/desktop capture loss still rebuilds in place.
                // `cur_node_id` (the capture 5-tuple's node id) is read only by the Linux
                // dedicated-game-exit check below; keep it read on other platforms so it isn't a
                // write-only variable under `-D warnings` (the `let _ = &launch` idiom above).
                #[cfg(not(target_os = "linux"))]
                let _ = &cur_node_id;
                #[cfg(target_os = "linux")]
                if launch.is_some()
                    && crate::vdisplay::launch_is_nested(compositor)
                    && crate::vdisplay::dedicated_game_exited(cur_node_id)
                {
                    tracing::info!(
                        "dedicated game session: the game exited — ending the session cleanly"
                    );
                    quit.store(true, Ordering::SeqCst); // skip keep-alive linger — the game is gone
                    conn.close(
                        punktfunk_core::quic::APP_EXITED_CLOSE_CODE.into(),
                        b"game exited",
                    );
                    break;
                }
                capture_rebuilds += 1;
                if capture_rebuilds > MAX_CAPTURE_REBUILDS {
                    return Err(e).context("capture lost — rebuild attempts exhausted");
                }
                tracing::warn!(error = %format!("{e:#}"), rebuild = capture_rebuilds,
                    "capture lost — rebuilding pipeline in place");
                // A Bazzite/SteamOS Gaming↔Desktop switch tears the old compositor down and can take
                // 15s+ to bring the new one up. Don't fail the session over that (the client would
                // have to cold-reconnect, surfacing a "session failed") — keep retrying within a
                // generous budget while the QUIC keepalive (its own thread) holds the connection,
                // RE-DETECTING the live compositor each attempt so we follow the box to whatever
                // session comes up: a fresh instance of the same compositor, OR a different one
                // (the kind-change case the session watcher also handles). The client stays
                // connected, frozen on the last frame, and the stream resumes when the new output
                // appears — no reconnect.
                const REBUILD_BUDGET: std::time::Duration = std::time::Duration::from_secs(40);
                let rebuild_deadline = std::time::Instant::now() + REBUILD_BUDGET;
                let (new_cap, new_enc, new_frame, new_interval, new_node_id, new_display_gen) = loop {
                    // Follow the active session unless an explicit PUNKTFUNK_COMPOSITOR pin forbids
                    // retargeting (then we stick to the pinned backend and just rebuild it).
                    if crate::config::config().compositor.is_none() {
                        let active = crate::vdisplay::detect_active_session();
                        // A4: fold any compositor-instance change into the epoch/invalidation before we
                        // rebuild, so the rebuild's acquire won't reuse a dead-instance node.
                        crate::vdisplay::observe_session_instance(&active);
                        if let Some(c) = crate::vdisplay::compositor_for_kind(active.kind) {
                            crate::vdisplay::apply_session_env(&active);
                            // Capture-loss rebuild follows the live box session, not a fresh dedicated launch.
                            crate::vdisplay::apply_input_env(c, false);
                            if c != compositor {
                                if matches!(
                                    c,
                                    crate::vdisplay::Compositor::Kwin
                                        | crate::vdisplay::Compositor::Mutter
                                ) {
                                    crate::vdisplay::settle_desktop_portal(c);
                                }
                                match crate::vdisplay::open(c) {
                                    Ok(v) => {
                                        tracing::info!(from = compositor.id(), to = c.id(),
                                            "capture loss: active session switched compositor — retargeting");
                                        vd = v;
                                        compositor = c;
                                    }
                                    Err(e2) => tracing::warn!(error = %format!("{e2:#}"),
                                        "capture loss: opening the newly-detected compositor failed — retrying"),
                                }
                            }
                        }
                    }
                    match build_pipeline_with_retry(
                        &mut vd,
                        cur_mode,
                        bitrate_kbps,
                        bit_depth,
                        plan,
                        &quit,
                        &stop,
                    ) {
                        Ok(p) => break p,
                        Err(e2) => {
                            if stop.load(Ordering::SeqCst)
                                || std::time::Instant::now() >= rebuild_deadline
                            {
                                return Err(e2)
                                    .context("capture lost — no compositor came up within the rebuild budget");
                            }
                            tracing::warn!(error = %format!("{e2:#}"),
                                "capture lost — new session not up yet, retrying");
                        }
                    }
                };
                capturer = new_cap;
                enc = new_enc;
                frame = new_frame;
                interval = new_interval;
                cur_node_id = new_node_id;
                cur_display_gen = new_display_gen;
                enc.request_keyframe(); // belt-and-suspenders; a fresh encoder opens on an IDR anyway
                last_forced_idr = Some(std::time::Instant::now()); // anchor the IDR cooldown from the rebuild
                next = std::time::Instant::now();
                // The owed AUs died with the old encoder — drop their in-flight records and
                // restart the encode-stall clock (the rebuild loop above may have eaten seconds,
                // which must not count against the fresh encoder).
                inflight.clear();
                last_au_at = std::time::Instant::now();
                encoder_resets = 0;
                tracing::info!(
                    compositor = compositor.id(),
                    "capture loss: pipeline rebuilt — stream resumes"
                );
            }
        }
        if perf && diag_at.elapsed() >= std::time::Duration::from_secs(2) {
            let secs = diag_at.elapsed().as_secs_f64();
            tracing::info!(
                new_fps = format!("{:.0}", diag_new as f64 / secs),
                repeat_fps = format!("{:.0}", diag_repeat as f64 / secs),
                "capture diag: NEW frames from the source vs REPEATS (low new_fps at high send rate ⇒ \
                 the source isn't producing frames, not an encode stall)"
            );
            let wait_max = st_wait.iter().copied().max().unwrap_or(0);
            tracing::info!(
                queue_us_p50 = percentile(&mut st_queue, 0.50),
                queue_us_p99 = percentile(&mut st_queue, 0.99),
                cap_us_p50 = percentile(&mut st_cap, 0.50),
                cap_us_p99 = percentile(&mut st_cap, 0.99),
                submit_us_p50 = percentile(&mut st_submit, 0.50),
                submit_us_p99 = percentile(&mut st_submit, 0.99),
                wait_us_p50 = percentile(&mut st_wait, 0.50),
                wait_us_p99 = percentile(&mut st_wait, 0.99),
                wait_us_max = wait_max,
                "stage perf (µs/call): queue=delivery→submit cap=try_latest(ring+convert) submit=encode_picture wait=lock_bitstream(sched+ASIC)"
            );
            st_cap.clear();
            st_submit.clear();
            st_wait.clear();
            st_queue.clear();
            diag_new = 0;
            diag_repeat = 0;
            diag_at = std::time::Instant::now();
        }
        // The source's static HDR mastering metadata is the single source of truth: hand it to the
        // encoder (in-band SEI on keyframes) and, when it changes, to the client (0xCE). Re-sent on
        // each keyframe below so a dropped best-effort datagram converges within a GOP. PRESENCE is
        // the capturer's call (Some iff the virtual display is in HDR mode); the VALUE prefers the
        // client's own display volume when it sent one — the virtual display's EDID advertises
        // exactly that volume, so host apps already tone-mapped the content into it and the honest
        // mastering description IS the client's panel. (The IDD capturer only knows the generic
        // baseline; if the driver ever forwards per-content IDDCX_HDR10_METADATA, prefer that here.)
        let hdr_meta = capturer.hdr_meta().map(|m| client_hdr.unwrap_or(m));
        enc.set_hdr_meta(hdr_meta);
        let mut resend_meta = hdr_meta != last_hdr_meta;
        if resend_meta {
            last_hdr_meta = hdr_meta;
        }
        // How deep to pipeline (1 = synchronous submit→poll, the original behaviour). The IDD-push
        // capturer hands a rotating ring of output textures, so it returns >1; other capturers default 1.
        let depth = capturer.pipeline_depth().max(1);
        let submit_ns = now_ns();
        // Wire pts: a fresh frame anchors at its capture-delivery stamp (`CapturedFrame.pts_ns`,
        // stamped when the capture thread handed it over) so client-measured latency covers
        // delivery + queue age, not just submit→glass; `queue_us` splits that age out as its own
        // stage. A re-encoded hold anchors at "now" (its content age is unbounded by design). The
        // stamp must be a recent wall-clock time — a synthetic/index-based or ahead-of-clock stamp
        // (SyntheticCapturer counts from 0, not the epoch) falls back to "now".
        let age_ns = submit_ns.saturating_sub(frame.pts_ns);
        let plausible = frame.pts_ns > 0 && frame.pts_ns <= submit_ns && age_ns < 10_000_000_000;
        let (capture_ns, queue_us) = if !repeat && plausible {
            (frame.pts_ns, (age_ns / 1000) as u32)
        } else {
            (submit_ns, 0)
        };
        if perf && !repeat {
            st_queue.push(queue_us);
        }
        let t_submit = std::time::Instant::now();
        // This submission's future wire frame index (see `au_seq`): AUs are emitted FIFO one per
        // submission, so it lands `inflight.len()` AUs after the `au_seq` the loop is about to
        // assign next. The RFI backends pin their frame numbering to it.
        let wire_index = au_seq.wrapping_add(inflight.len() as u32);
        if let Err(e) = enc.submit_indexed(&frame, wire_index) {
            // The input half of an encode stall: once the driver stops draining AUs, libavcodec's
            // one-frame buffer fills and avcodec_send_frame starts failing (EAGAIN) — the same
            // wedge the watchdog below catches, seen from submit. Rebuild the encoder in place
            // (bounded) instead of killing an otherwise healthy session; a backend without an
            // in-place rebuild keeps today's fail-fast behavior.
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS
                || !reset_stalled_encoder(&mut enc, &mut inflight)
            {
                return Err(e).context("encoder submit");
            }
            tracing::error!(error = %format!("{e:#}"), reset = encoder_resets,
                max = MAX_ENCODER_RESETS,
                "encoder submit failed — encoder rebuilt in place, forcing an IDR");
            last_au_at = std::time::Instant::now();
            // Re-pace from the rebuild and retry this frame next tick (gives the fresh encoder
            // one frame period to come up instead of hammering it in a hot loop).
            next = std::time::Instant::now() + interval;
            std::thread::sleep(interval);
            continue;
        }
        let submit_us = if measure {
            t_submit.elapsed().as_micros() as u32
        } else {
            0
        };
        if perf {
            st_submit.push(submit_us);
        }
        // This frame's pacing deadline (the next frame's due time); the send thread spreads a big frame
        // up to here. Each in-flight frame carries its own (capture_ns, deadline) for when it's polled.
        next += interval;
        inflight.push_back((capture_ns, submit_ns, next));
        // Drain the OLDEST in-flight frames, keeping at most depth-1 deferred. At depth 1 this polls
        // immediately after every submit (synchronous); at depth 2 it polls N right after submitting N+1,
        // so the encode of N overlaps the convert/copy of N+1. NVENC's `pending` is FIFO, so poll() returns
        // the oldest submitted frame's AU — matching `inflight.pop_front()`.
        let mut send_gone = false;
        // A poll error is the explicit form of an encode stall (e.g. a QSV device failure);
        // carry it to the shared stall recovery below instead of killing the session outright.
        let mut poll_err: Option<anyhow::Error> = None;
        while inflight.len() >= depth {
            let t_wait = std::time::Instant::now();
            let polled = enc.poll();
            let wait_us = if measure {
                t_wait.elapsed().as_micros() as u32
            } else {
                0
            };
            if perf {
                st_wait.push(wait_us);
            }
            let au = match polled {
                Ok(Some(au)) => au,
                // No AU ready for a submitted frame. Routine on the non-blocking backends (the
                // libavcodec AMF/QSV wrapper holds ~2 frames; async NVENC drains a ready queue) —
                // the frame stays in flight and the next tick re-polls. The stall watchdog below
                // decides when "not ready yet" has become "the driver is wedged".
                Ok(None) => break,
                Err(e) => {
                    poll_err = Some(e);
                    break;
                }
            };
            // The encoder is alive: feed the stall watchdog, clear the consecutive-reset counter.
            last_au_at = std::time::Instant::now();
            encoder_resets = 0;
            let (cap_ns, sub_ns, deadline) = inflight.pop_front().expect("inflight non-empty");
            let mut flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
            // Intra-refresh recovery marking (inert unless the backend validated its constrained GDR
            // via `intra_refresh_recovery`): tag every wave-boundary AU with USER_FLAG_RECOVERY_POINT
            // so the client lifts its post-loss freeze on the second mark — a proven clean re-anchor —
            // instead of forcing a full IDR. See [`mark_recovery_boundary`] for the cadence.
            let caps = enc.caps();
            if caps.intra_refresh_recovery
                && caps.intra_refresh_period > 0
                && mark_recovery_boundary(&mut ir_wave_pos, au.keyframe, caps.intra_refresh_period)
            {
                flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
            }
            // Reference-frame-invalidation recovery frame (AMD LTR force-reference): a clean P-frame
            // off a known-good reference. Tag it so the client lifts its post-loss freeze on this one
            // AU without an IDR — the definitive single-frame re-anchor (see USER_FLAG_RECOVERY_ANCHOR).
            if au.recovery_anchor {
                flags |= punktfunk_core::packet::USER_FLAG_RECOVERY_ANCHOR;
            }
            // Re-send the HDR mastering metadata (0xCE) on each keyframe (a decoder-resync point) and
            // whenever it changed, so a client that dropped the best-effort datagram re-converges.
            if let Some(m) = last_hdr_meta {
                if au.keyframe || resend_meta {
                    let _ = conn
                        .send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(&m).into());
                    resend_meta = false;
                }
            }
            let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
            let msg = FrameMsg {
                data: au.data,
                capture_ns: cap_ns,
                flags,
                frame_index: au_seq,
                deadline,
                encode_us,
                queue_us,
                cap_us,
                submit_us,
                wait_us,
                repeat,
                was_measured: measure,
            };
            // Hand to the send thread; this blocks (backpressure) if it's behind. An Err means it
            // exited (send failure / stop) — end the encode loop too.
            if frame_tx.send(msg).is_err() {
                send_gone = true;
                break;
            }
            au_seq = au_seq.wrapping_add(1);
            sent += 1;
        }
        if send_gone {
            break;
        }
        // Encode-stall watchdog. Trip on: an explicit poll error; no AU within the window while
        // frames are owed (the full wedge — AMF/QSV's non-blocking poll returns None forever and
        // nothing else ever errors); or an owed backlog worth more than the window's frames (the
        // slow leak — AUs still trickle, so the gap never trips, but latency grows without bound).
        // Recovery rebuilds the encoder in place and forces an IDR — a logged ~one-second hiccup
        // instead of a silent permanent freeze — bounded so a genuinely dead encoder still ends
        // the session with a clear error. The window scales with the frame interval so low-fps
        // modes (where the AMF wrapper's ~2-frame hold spans seconds) can't false-trip.
        let stall_window = ENCODE_STALL_WINDOW.max(interval * 8);
        let stall_backlog =
            depth + (stall_window.as_secs_f64() / interval.as_secs_f64().max(1e-6)).ceil() as usize;
        if poll_err.is_some()
            || (!inflight.is_empty()
                && (last_au_at.elapsed() >= stall_window || inflight.len() > stall_backlog))
        {
            let why = match &poll_err {
                Some(e) => format!("poll failed: {e:#}"),
                None => format!(
                    "no AU for {} ms with {} frame(s) in flight",
                    last_au_at.elapsed().as_millis(),
                    inflight.len()
                ),
            };
            encoder_resets += 1;
            if encoder_resets > MAX_ENCODER_RESETS
                || !reset_stalled_encoder(&mut enc, &mut inflight)
            {
                return Err(poll_err.unwrap_or_else(|| anyhow!("{why}")))
                    .context("encoder stalled — in-place rebuild unavailable or exhausted");
            }
            tracing::error!(reset = encoder_resets, max = MAX_ENCODER_RESETS, %why,
                "encode stall detected — encoder rebuilt in place, forcing an IDR");
            last_au_at = std::time::Instant::now();
        }
        match next.checked_duration_since(std::time::Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = std::time::Instant::now(),
        }
    }
    // Drain the in-flight tail (the depth-1 frames submitted but not yet polled) so the last frames still
    // reach the client instead of being dropped on the way out.
    while let Some((cap_ns, sub_ns, deadline)) = inflight.pop_front() {
        let Ok(Some(au)) = enc.poll() else { break };
        let flags = if au.keyframe {
            (FLAG_PIC | FLAG_SOF) as u32
        } else {
            FLAG_PIC as u32
        };
        let encode_us = (now_ns().saturating_sub(sub_ns) / 1000) as u32;
        // End-of-stream tail drain: the per-stage split isn't measured here (the capture loop has
        // exited), so leave it zero — these last few frames are negligible for the aggregates.
        let msg = FrameMsg {
            data: au.data,
            capture_ns: cap_ns,
            flags,
            frame_index: au_seq,
            deadline,
            encode_us,
            queue_us: 0,
            cap_us: 0,
            submit_us: 0,
            wait_us: 0,
            repeat: false,
            was_measured: false,
        };
        if frame_tx.send(msg).is_err() {
            break;
        }
        au_seq = au_seq.wrapping_add(1);
        sent += 1;
    }
    // Signal the send thread to drain + exit (drop the channel), then join it.
    drop(frame_tx);
    let _ = send_thread.join();
    tracing::info!(sent, "punktfunk/1 virtual stream complete");
    Ok(())
}

/// One mode's capture/encode pipeline: (capturer, encoder, first frame, frame interval).
/// Dropping the capturer tears down the PipeWire stream and the virtual output with it.
type Pipeline = (
    Box<dyn crate::capture::Capturer>,
    Box<dyn crate::encode::Encoder>,
    crate::capture::CapturedFrame,
    std::time::Duration,
    // The virtual output's PipeWire node id — used by the B2 dedicated game-exit probe to check THIS
    // session's own node (scoped), not any gamescope node. `0` for backends without a PipeWire node
    // (Windows IDD-push), which never take the dedicated-gamescope B2 path anyway.
    u32,
    // The display's registry pool generation (Linux keep-alive pool only; `None` on Windows — the
    // manager leases in place — and for non-poolable outputs). A mode-switch rebuild uses it to
    // `registry::retire` the superseded old display, so linger/forever keep-alive policies don't
    // accumulate kept monitors at stale modes (design/midstream-resolution-resize.md H4).
    Option<u64>,
);

/// Build the pipeline, retrying *transient* failures with bounded exponential backoff.
///
/// Bringing a virtual output to first-frame races several async steps — the compositor parenting
/// the output, the portal/RemoteDesktop grant, PipeWire format negotiation — any of which can
/// momentarily time out on a cold session. A single timed-out attempt shouldn't abort the whole
/// punktfunk/1 session. But a *permanent* failure (unsupported compositor/mode, a KWin too old to
/// create virtual outputs, a missing tool) must fail fast instead of burning the budget — so the
/// error chain is classified and permanent ones short-circuit. Each failed attempt drops its
/// capturer, which (via `PortalCapturer::Drop`) tears the PipeWire thread + virtual output down
/// before the next attempt — no leak across retries.
fn build_pipeline_with_retry(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
) -> Result<Pipeline> {
    // ~10s first-frame wait per attempt. 8 gives a ~90s budget for the SLOW case: a host-managed
    // gamescope session cold-starting Steam Big Picture (the SteamOS/Bazzite takeover) can take
    // 30-60s to produce its first frame, and a first-connect timeout would tear down the warm
    // session (forcing another cold start on reconnect). A genuinely permanent failure still fails
    // fast via `is_permanent_build_error`; only transient "no frame yet" retries consume the budget.
    // IDD-push only: HOLD one monitor lease across all build attempts. A failed attempt's capturer
    // drop releases ITS lease, but this held lease keeps the shared monitor Active (refs >= 1), so the
    // next attempt's `vd.create` JOINS it (refcount++) instead of finding it Lingering and tripping the
    // IDD-push reconnect PREEMPT (teardown + recreate). That preempt-per-retry was the REMOVE→ADD churn
    // that exhausts the IddCx monitor-slot pool and wedges ADD at 0x80070490 — one ADD per cold start
    // now, not one per attempt. Non-IDD-push backends (Linux portal, WGC) don't use the refcount manager
    // and aren't churn-wedge-prone, so they keep create-per-attempt (a held lease there would allocate a
    // second virtual output). Dropped when this fn returns — on success the Pipeline's own lease keeps
    // the monitor Active; on failure refs falls to 0 → Lingering → linger-timeout teardown.
    let _retry_hold = if matches!(plan.capture, crate::session_plan::CaptureBackend::IddPush) {
        Some(
            vd.create(mode)
                .context("acquire virtual output for the session (retry-hold lease)")?,
        )
    } else {
        None
    };
    const MAX_ATTEMPTS: u32 = 8;
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 1..=MAX_ATTEMPTS {
        // The client is gone (connection closed → `stop`): every further attempt only churns the
        // box for a session no one is watching — on a Bazzite takeover that means SIGKILLing and
        // relaunching the box's Steam session once per attempt for minutes (the .181 storm
        // 2026-07-07). One in-flight attempt can still overhang; this bounds the damage to it.
        if attempt > 1 && stop.load(Ordering::SeqCst) {
            anyhow::bail!(
                "session ended (client disconnected) during pipeline build — aborting retries \
                 after {} attempt(s)",
                attempt - 1
            );
        }
        match build_pipeline(vd, mode, bitrate_kbps, bit_depth, plan, quit) {
            Ok(pipe) => {
                if attempt > 1 {
                    tracing::info!(attempt, "pipeline up after retry");
                }
                return Ok(pipe);
            }
            Err(e) => {
                let chain = format!("{e:#}");
                let permanent = is_permanent_build_error(&chain);
                if permanent || attempt == MAX_ATTEMPTS {
                    let why = if permanent {
                        "permanent"
                    } else {
                        "out of retries"
                    };
                    return Err(e).with_context(|| {
                        format!("pipeline build failed ({why}) after {attempt} attempt(s)")
                    });
                }
                tracing::warn!(
                    attempt,
                    max = MAX_ATTEMPTS,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %chain,
                    "pipeline build failed — retrying"
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
            }
        }
    }
    unreachable!("the final attempt returns inside the loop")
}

/// Is a pipeline-build error permanent (retrying won't help within this session)? Matches the
/// error chain against signatures that don't change between attempts: unsupported compositor or
/// mode, a KWin too old to expose virtual outputs, a missing/unparseable config, a tool that
/// isn't installed. Everything else — portal/PipeWire negotiation timeouts, "no frame within
/// 10s", transient node races — is treated as transient and retried. Biased toward "transient":
/// a misjudged permanent error only costs a few seconds before it fails anyway.
fn is_permanent_build_error(chain: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "virtual displays require linux",
        "unknown punktfunk_compositor",
        "could not detect compositor",
        "could not find output", // KWin < 6.5.6: createVirtualOutput unsupported
        "must be a node id",     // PUNKTFUNK_GAMESCOPE_NODE not an integer
        "is it installed",       // gamescope / kscreen-doctor not on PATH
        // 4:4:4 NVENC got a CUDA frame — should never happen now the Linux capturer honors gpu=false,
        // but fail fast instead of 8× retry (~90 s) rather than wedge the session if it ever recurs.
        "capture/encoder negotiation mismatch",
    ];
    let lower = chain.to_ascii_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

/// Encode-stall recovery: rebuild the encoder in place (keeping capture + the session up) and
/// discard the owed in-flight frame records — their AUs died with the old encoder instance.
/// Returns `false` when the backend has no in-place rebuild ([`crate::encode::Encoder::reset`]'s
/// default); the caller then surfaces the stall as a session error instead. The forced keyframe
/// makes the rebuilt encoder's first frame an immediate decoder resync point (belt-and-suspenders:
/// a fresh encoder opens on an IDR anyway).
fn reset_stalled_encoder(
    enc: &mut Box<dyn crate::encode::Encoder>,
    inflight: &mut std::collections::VecDeque<(u64, u64, std::time::Instant)>,
) -> bool {
    if !enc.reset() {
        return false;
    }
    inflight.clear();
    enc.request_keyframe();
    true
}

fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
) -> Result<Pipeline> {
    // Acquire through the registry (design/display-management.md): on Linux this pools the display
    // for keep-alive (reuse a kept one, or create + keep the backend's keepalive so it outlives the
    // session per policy); on Windows it delegates to `vd.create` (the manager already leases). The
    // returned `VirtualOutput`'s keepalive is a registry lease — the capturer holds it as before. The
    // `quit` flag rides into the lease so a deliberate-quit teardown skips the keep-alive linger.
    let vout = crate::vdisplay::registry::acquire(vd, mode, quit.clone())
        .context("create virtual output")?;
    // A2: if this was a REUSED kept display and its first frame fails, tear the (dead) pool entry down
    // so the retry loop's next acquire creates fresh instead of re-wedging on the same corpse. Read the
    // gen BEFORE `capture_virtual_output` consumes `vout`. (Linux-only — the pool is Linux.)
    #[cfg(target_os = "linux")]
    let reused_gen = vout.reused_gen;
    // The display's pool generation (fresh AND reused), threaded out so a mode-switch rebuild can
    // `registry::retire` the display this pipeline supersedes (H4). `None` off Linux / non-poolable.
    #[cfg(target_os = "linux")]
    let pool_gen = vout.pool_gen;
    #[cfg(not(target_os = "linux"))]
    let pool_gen = None;
    // The virtual output's PipeWire node id — kept for the B2 dedicated game-exit probe (scoped to
    // this session's own node). Read before `capture_virtual_output` consumes `vout`.
    let node_id = vout.node_id;
    // The backend reports the refresh it actually achieved in `preferred_mode.2` (KWin may cap a
    // virtual output at 60 Hz if the custom-mode install was rejected). Pace the encoder + frame
    // clock to that, not the requested rate, so we don't emit phantom duplicate frames over a
    // slower source. Falls back to the requested rate when a backend reports nothing.
    let effective_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(mode.refresh_hz);
    if effective_hz != mode.refresh_hz {
        tracing::warn!(
            requested = mode.refresh_hz,
            effective = effective_hz,
            "compositor did not honor the requested refresh — encoding at the achieved rate"
        );
    }
    // HDR vs SDR for the IDD-push conversion: a negotiated 10-bit session (client advertised
    // VIDEO_CAP_10BIT + host opted in via PUNKTFUNK_10BIT) is our HDR path → BT.2020 PQ Rgb10a2;
    // otherwise the FP16 IDD frames are converted to 8-bit SDR. (Ignored by non-IDD-push backends,
    // which auto-detect HDR from the monitor state.)
    let mut capturer =
        crate::capture::capture_virtual_output(vout, plan.output_format(), plan.capture)
            .context("capture virtual output")?;
    capturer.set_active(true);
    let frame = match capturer.next_frame().context("first frame") {
        Ok(f) => f,
        Err(e) => {
            // A reused kept display was dead — invalidate it so the next attempt creates fresh (A2).
            #[cfg(target_os = "linux")]
            if let Some(g) = reused_gen {
                crate::vdisplay::registry::mark_failed(g);
            }
            return Err(e);
        }
    };
    // `bit_depth` is the handshake-negotiated value (8, or 10 = HEVC Main10 when the client
    // advertised VIDEO_CAP_10BIT and the host opted in). Threaded down from the Welcome.
    let enc = crate::encode::open_video(
        plan.codec,
        frame.format,
        frame.width,
        frame.height,
        effective_hz,
        bitrate_kbps as u64 * 1000,
        frame.is_cuda(),
        bit_depth,
        plan.chroma,
    )
    .context("open video encoder")?;
    // Post-open cross-check: the Welcome already committed `chroma_format` from the pre-open probe, so
    // warn loudly if the encoder actually opened a different chroma than negotiated (the in-band SPS is
    // authoritative for the decoder, but a mismatch means the probe and the live open disagreed).
    let opened_444 = enc.caps().chroma_444;
    if opened_444 != plan.chroma.is_444() {
        tracing::warn!(
            negotiated_444 = plan.chroma.is_444(),
            opened_444,
            "encoder chroma disagrees with the negotiated Welcome — the client was told the other value"
        );
    }
    let interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    Ok((capturer, enc, frame, interval, node_id, pool_gen))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_pad_route_decision() {
        use GamepadPref::{DualSense, Xbox360};
        // First frame with no device: create in the declared kind's manager, record ownership.
        assert_eq!(
            route_decision(None, DualSense, true),
            (DualSense, Some(DualSense))
        );
        // Subsequent frame: stays in the owning manager even if the declared kind now differs
        // (the arrival-after-first-frame reorder) — never a second device in another manager.
        assert_eq!(
            route_decision(Some(DualSense), Xbox360, true),
            (DualSense, Some(DualSense))
        );
        // Removal (cleared bit): routes to the owner so the RIGHT device is torn down, then clears.
        assert_eq!(
            route_decision(Some(DualSense), Xbox360, false),
            (DualSense, None)
        );
        // Removal with no device is a harmless no-op route (owner stays cleared).
        assert_eq!(route_decision(None, Xbox360, false), (Xbox360, None));
        // A fresh device after a re-plug picks up the newly-declared kind (owner was cleared).
        assert_eq!(
            route_decision(None, Xbox360, true),
            (Xbox360, Some(Xbox360))
        );
    }

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
    fn reconfig_allowed_gates_gamescope_and_per_client_mode() {
        use crate::vdisplay::Compositor::{Gamescope, Hyprland, Kwin, Mutter, Wlroots};
        // gamescope ALWAYS rejects — a resize would respawn the nested game (H1/D3), regardless of
        // the identity policy.
        assert!(!reconfig_allowed(Some(Gamescope), false));
        assert!(!reconfig_allowed(Some(Gamescope), true));
        // A per-client-mode identity policy rejects on every backend — the resize resolves a
        // different display-identity slot (H5).
        assert!(!reconfig_allowed(Some(Kwin), true));
        assert!(!reconfig_allowed(Some(Mutter), true));
        assert!(!reconfig_allowed(None, true));
        // Every other compositor with the default identity ACCEPTS (recreate / re-arrival / in-place).
        for c in [Kwin, Mutter, Wlroots, Hyprland] {
            assert!(
                reconfig_allowed(Some(c), false),
                "{c:?} should allow live reconfigure"
            );
        }
        // The synthetic source (no compositor) is the protocol-test path — always reconfigurable.
        assert!(reconfig_allowed(None, false));
    }

    #[test]
    fn recovery_marks_land_every_period_and_rephase_at_idr() {
        let period = 4;
        let mut pos = 0u32;
        // Frames 1..=3 are mid-wave (no mark), frame 4 is the boundary; then it repeats.
        let marks: Vec<bool> = (0..10)
            .map(|_| mark_recovery_boundary(&mut pos, false, period))
            .collect();
        assert_eq!(
            marks,
            vec![false, false, false, true, false, false, false, true, false, false]
        );

        // An IDR mid-wave re-phases: the counter restarts, so the next boundary is a full period
        // later (an IDR is itself a clean anchor, so it is not additionally marked).
        let mut pos = 0u32;
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 1
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 2
        assert!(!mark_recovery_boundary(&mut pos, true, period)); // IDR → pos 0, no mark
                                                                  // Now a fresh full period is needed, not just the 2 remaining frames.
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 1
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 2
        assert!(!mark_recovery_boundary(&mut pos, false, period)); // pos 3
        assert!(mark_recovery_boundary(&mut pos, false, period)); // pos 4 → mark
    }

    #[test]
    fn pad_snapshot_replaces_state_and_seq_gates() {
        use punktfunk_core::input::{gamepad, GamepadSnapshot};
        let mut state = PadState::default();
        let mut last_seq: Option<u8> = None;

        // Legacy accumulation first (an older client), then a snapshot replaces it wholesale.
        let axis = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: gamepad::AXIS_LT,
            x: 200,
            y: 0,
            flags: 0,
        };
        assert!(state.apply(&axis));
        assert_eq!(state.left_trigger, 200);

        let snap = GamepadSnapshot {
            pad: 0,
            seq: 1,
            buttons: gamepad::BTN_A,
            left_trigger: 255,
            right_trigger: 0,
            ls_x: 100,
            ls_y: -100,
            rs_x: 0,
            rs_y: 0,
        };
        assert!(GamepadSnapshot::seq_newer(snap.seq, last_seq));
        last_seq = Some(snap.seq);
        state.set_snapshot(&snap);
        assert_eq!(state.left_trigger, 255);
        assert_eq!(state.buttons, gamepad::BTN_A);
        assert_eq!((state.ls_x, state.ls_y), (100, -100));

        // A reordered (stale) snapshot must not roll the trigger back.
        let stale = GamepadSnapshot {
            seq: 0,
            left_trigger: 10,
            ..snap
        };
        assert!(!GamepadSnapshot::seq_newer(stale.seq, last_seq));

        // The unchanged-refresh case the input thread skips the frame emit for: identical
        // payload with a newer seq compares equal after apply.
        let refresh = GamepadSnapshot { seq: 2, ..snap };
        assert!(GamepadSnapshot::seq_newer(refresh.seq, last_seq));
        let before = state;
        state.set_snapshot(&refresh);
        assert_eq!(state, before);

        // The snapshot survives the wire roundtrip into the same PadState shape.
        let dec =
            GamepadSnapshot::from_event(&InputEvent::decode(&snap.to_event().encode()).unwrap())
                .unwrap();
        assert_eq!(dec, snap);
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

    #[test]
    fn compositor_resolution_precedence() {
        use crate::vdisplay::Compositor::*;
        // A concrete, available preference is honored.
        assert_eq!(
            pick_compositor(CompositorPref::Gamescope, &[Kwin, Gamescope], Some(Kwin)),
            Some(Gamescope)
        );
        // A concrete but UNavailable preference falls back to the detected default.
        assert_eq!(
            pick_compositor(CompositorPref::Mutter, &[Kwin, Gamescope], Some(Kwin)),
            Some(Kwin)
        );
        // Auto always uses the detected default.
        assert_eq!(
            pick_compositor(CompositorPref::Auto, &[Kwin, Gamescope], Some(Kwin)),
            Some(Kwin)
        );
        // Unavailable preference + nothing detected → None (caller errors the handshake).
        assert_eq!(
            pick_compositor(CompositorPref::Mutter, &[Gamescope], None),
            None
        );
        // Available preference still wins even when nothing was auto-detected.
        assert_eq!(
            pick_compositor(CompositorPref::Gamescope, &[Gamescope], None),
            Some(Gamescope)
        );
        // Wlroots family (D2): the shared `Wlroots` pref resolves to whichever of sway/river
        // (Wlroots) and Hyprland is the live session.
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Hyprland], Some(Hyprland)),
            Some(Hyprland)
        );
        // …and to Wlroots-proper on a sway/river host.
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Wlroots], Some(Wlroots)),
            Some(Wlroots)
        );
        // Family fallback even if detection came back empty but a member is available.
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Hyprland], None),
            Some(Hyprland)
        );
    }

    #[test]
    fn gamepad_resolution_precedence() {
        use GamepadPref::*;
        // Trailing args are (linux, windows).
        // An explicit client choice wins over the env var.
        assert_eq!(
            pick_gamepad(DualSense, Some("xbox360"), true, false),
            DualSense
        );
        assert_eq!(
            pick_gamepad(Xbox360, Some("dualsense"), true, false),
            Xbox360
        );
        // Client Auto defers to the env var.
        assert_eq!(
            pick_gamepad(Auto, Some("dualsense"), true, false),
            DualSense
        );
        assert_eq!(pick_gamepad(Auto, Some("xbox360"), true, false), Xbox360);
        // Auto + no env (or an unparseable one) → X-Box 360.
        assert_eq!(pick_gamepad(Auto, None, true, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("bogus"), true, false), Xbox360);
        // DualSense: honored on Linux (UHID) AND Windows (UMDF minidriver); degrades elsewhere.
        assert_eq!(pick_gamepad(DualSense, None, false, true), DualSense);
        assert_eq!(
            pick_gamepad(Auto, Some("dualsense"), false, true),
            DualSense
        );
        assert_eq!(pick_gamepad(DualSense, None, false, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("dualsense"), false, false), Xbox360);
        // DualShock 4: honored on Linux (UHID) AND Windows (UMDF minidriver); degrades elsewhere.
        assert_eq!(pick_gamepad(DualShock4, None, true, false), DualShock4);
        assert_eq!(pick_gamepad(Auto, Some("ps4"), true, false), DualShock4);
        assert_eq!(pick_gamepad(DualShock4, None, false, true), DualShock4);
        assert_eq!(pick_gamepad(DualShock4, None, false, false), Xbox360);
        // X-Box One: a distinct uinput identity on Linux, folded into the 360 pad on Windows.
        assert_eq!(pick_gamepad(XboxOne, None, true, false), XboxOne);
        assert_eq!(pick_gamepad(Auto, Some("series"), true, false), XboxOne);
        assert_eq!(pick_gamepad(XboxOne, None, false, true), Xbox360);

        // Steam Deck: native on Linux (UHID/usbip/gadget) AND Windows (UMDF device-type 3,
        // Steam-Input-promoted via MI_02 — gamepad-new-types N4); Xbox360 elsewhere.
        assert_eq!(pick_gamepad(SteamDeck, None, true, false), SteamDeck);
        assert_eq!(pick_gamepad(SteamDeck, None, false, true), SteamDeck);
        assert_eq!(pick_gamepad(Auto, Some("deck"), false, true), SteamDeck);
        assert_eq!(pick_gamepad(SteamDeck, None, false, false), Xbox360);
        // Classic Steam Controller: native on Linux (UHID hid-steam); Xbox360 elsewhere.
        assert_eq!(
            pick_gamepad(SteamController, None, true, false),
            SteamController
        );
        assert_eq!(
            pick_gamepad(Auto, Some("steamcontroller"), true, false),
            SteamController
        );
        assert_eq!(pick_gamepad(SteamController, None, false, true), Xbox360);

        // DualSense Edge: native on Linux (UHID) AND Windows (UMDF device-type 2); Xbox360
        // elsewhere.
        assert_eq!(
            pick_gamepad(DualSenseEdge, None, true, false),
            DualSenseEdge
        );
        assert_eq!(
            pick_gamepad(DualSenseEdge, None, false, true),
            DualSenseEdge
        );
        assert_eq!(pick_gamepad(Auto, Some("edge"), true, false), DualSenseEdge);
        assert_eq!(pick_gamepad(DualSenseEdge, None, false, false), Xbox360);
        // Switch Pro: native on Linux (UHID hid-nintendo); Xbox360 on Windows and elsewhere.
        assert_eq!(pick_gamepad(SwitchPro, None, true, false), SwitchPro);
        assert_eq!(
            pick_gamepad(Auto, Some("switchpro"), true, false),
            SwitchPro
        );
        assert_eq!(pick_gamepad(Auto, Some("switch"), true, false), SwitchPro);
        assert_eq!(pick_gamepad(SwitchPro, None, false, true), Xbox360);
        assert_eq!(pick_gamepad(SwitchPro, None, false, false), Xbox360);
    }

    #[test]
    fn permanent_errors_short_circuit_retry() {
        // Permanent: config / version / missing-tool — retrying within a session can't fix these.
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Could not find output"
        ));
        assert!(is_permanent_build_error(
            "unknown PUNKTFUNK_COMPOSITOR 'foo' (kwin|wlroots|mutter|gamescope)"
        ));
        assert!(is_permanent_build_error(
            "spawn gamescope (is it installed? `apt install gamescope`)"
        ));
        assert!(is_permanent_build_error("virtual displays require Linux"));
        // Transient: negotiation/timeout races — exactly what backoff is for.
        assert!(!is_permanent_build_error(
            "first frame: no PipeWire frame within 10s (node 42): format negotiation never completed"
        ));
        assert!(!is_permanent_build_error(
            "create virtual output: timed out creating the KWin virtual output"
        ));
        assert!(!is_permanent_build_error("open NVENC: device busy"));
    }

    fn gp(kind: InputKind, code: u32, x: i32, pad: u32) -> InputEvent {
        InputEvent {
            kind,
            _pad: [0; 3],
            code,
            x,
            y: 0,
            flags: pad,
        }
    }

    /// Incremental wire events accumulate into the full pad frame the virtual xpad applies.
    #[test]
    fn gamepad_accumulator() {
        use punktfunk_core::input::gamepad::*;
        let mut s = PadState::default();
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_LB, 1, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LS_X, -32768, 0)));
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_RT, 255, 0)));
        let f = s.frame(2, 0b0100);
        assert_eq!(f.buttons, BTN_A | BTN_LB);
        assert_eq!((f.ls_x, f.right_trigger), (-32768, 255));
        assert_eq!((f.index, f.active_mask), (2, 0b0100));

        // Release folds out; axis values clamp; unknown axis ids are rejected.
        assert!(s.apply(&gp(InputKind::GamepadButton, BTN_A, 0, 0)));
        assert_eq!(s.frame(0, 1).buttons, BTN_LB);
        assert!(s.apply(&gp(InputKind::GamepadAxis, AXIS_LT, 9_999, 0)));
        assert_eq!(s.left_trigger, 255);
        assert!(!s.apply(&gp(InputKind::GamepadAxis, 42, 1, 0)));
    }

    /// Freeze the gamepad wire contract: every button bit + axis id pinned to its exact value, read
    /// through the GameStream namespace (`crate::gamestream::gamepad`, which re-exports
    /// `punktfunk_core::input::gamepad` — the punktfunk/1 native wire and the GameStream/Limelight
    /// wire are one and the same). Renumbering a bit in core, or dropping one from that re-export,
    /// silently breaks every already-shipped client, so it must fail here first. This is the host
    /// counterpart to the client-side C-ABI cross-checks in the Apple/Android gamepad tests.
    #[test]
    fn gamepad_wire_bits_are_pinned() {
        use crate::gamestream::gamepad as gm;
        use punktfunk_core::input::gamepad as pf;
        // buttonFlags — low 16 bits, named via the GameStream re-export the injectors use.
        assert_eq!(gm::BTN_DPAD_UP, 0x0000_0001);
        assert_eq!(gm::BTN_DPAD_DOWN, 0x0000_0002);
        assert_eq!(gm::BTN_DPAD_LEFT, 0x0000_0004);
        assert_eq!(gm::BTN_DPAD_RIGHT, 0x0000_0008);
        assert_eq!(gm::BTN_START, 0x0000_0010);
        assert_eq!(gm::BTN_BACK, 0x0000_0020);
        assert_eq!(gm::BTN_LS_CLICK, 0x0000_0040);
        assert_eq!(gm::BTN_RS_CLICK, 0x0000_0080);
        assert_eq!(gm::BTN_LB, 0x0000_0100);
        assert_eq!(gm::BTN_RB, 0x0000_0200);
        assert_eq!(gm::BTN_GUIDE, 0x0000_0400);
        assert_eq!(gm::BTN_A, 0x0000_1000);
        assert_eq!(gm::BTN_B, 0x0000_2000);
        assert_eq!(gm::BTN_X, 0x0000_4000);
        assert_eq!(gm::BTN_Y, 0x0000_8000);
        // buttonFlags2 — high 16 bits: back-grip paddles (re-exported), plus the touchpad-click /
        // Share bits the DualSense/DS4 protos consume straight from core.
        assert_eq!(gm::BTN_PADDLE1, 0x0001_0000);
        assert_eq!(gm::BTN_PADDLE2, 0x0002_0000);
        assert_eq!(gm::BTN_PADDLE3, 0x0004_0000);
        assert_eq!(gm::BTN_PADDLE4, 0x0008_0000);
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
