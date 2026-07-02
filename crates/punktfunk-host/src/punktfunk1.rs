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
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::{
    endpoint, io, ClockEcho, ClockProbe, ColorInfo, Hello, LossReport, PairChallenge, PairProof,
    PairRequest, PairResult, ProbeRequest, ProbeResult, Reconfigure, Reconfigured, RequestKeyframe,
    Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::Session;
use rand::RngCore;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
}

/// The native (punktfunk/1) trust store + on-demand arming PIN, shared with the management API.
use crate::native_pairing::{NativePairing, PairingDecision};
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
}

/// Options for the native host when the unified `serve --native` runs it: real virtual capture,
/// persistent (no session/duration cut), pairing armed on demand via the management API (the
/// shared [`NativePairing`] starts disarmed).
/// Default cap on simultaneously-streaming sessions (each holds an NVENC session; high-res
/// split-encode holds two). Conservative — consumer NVENC historically capped concurrent sessions;
/// overflow clients wait in the accept queue. Override with `--max-concurrent`.
pub(crate) const DEFAULT_MAX_CONCURRENT: usize = 4;

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
    let ep = endpoint::server_with_identity(
        ([0, 0, 0, 0], opts.port).into(),
        &identity.cert_pem,
        &identity.key_pem,
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
    let _advert = match crate::gamestream::Host::detect() {
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
    };

    // One audio capturer for the whole host lifetime, handed from session to session
    // (avoids a PipeWire stream setup per session — see AudioCapSlot).
    let audio_cap: AudioCapSlot = Arc::new(std::sync::Mutex::new(None));
    // One pointer/keyboard injector for the whole host lifetime (see InjectorService): the
    // RemoteDesktop-portal grant is established ONCE and reused, instead of a CreateSession per
    // session — which, under rapid client reconnects, raced a prior session's portal teardown and
    // wedged KWin's EIS setup ("EIS setup timed out"). Gamepads stay per-session (uinput).
    let injector = crate::inject::InjectorService::start();
    // One virtual microphone for the whole host lifetime (see MicService): the client's mic uplink
    // (0xCB) is Opus-decoded and fed into a persistent virtual mic host apps record from (Linux
    // PipeWire Audio/Source; Windows a virtual audio device's render endpoint).
    let mic_service = MicService::start();
    // Host-lifetime worker that fires debounced TV-session restores (the managed gamescope path
    // restores the box's autologin gaming session on idle, not per-disconnect — see
    // `vdisplay::restore_managed_session`). Held for serve()'s lifetime; dropping it stops it.
    let _restore_worker = crate::vdisplay::start_restore_worker();
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
            gate_hello.abi_version == punktfunk_core::ABI_VERSION,
            "ABI mismatch: client {} host {}",
            gate_hello.abi_version,
            punktfunk_core::ABI_VERSION
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
            // host from flooding/evicting genuine knocks (#13).
            np.note_pending(&label, &fp_hex, Some(peer.ip()));
            // Free the session slot while a human decides — a parked knock must not hold an NVENC
            // permit (a handful of parked knocks would otherwise block every real session).
            drop(permit);
            let decision = tokio::select! {
                d = np.wait_for_decision(&fp_hex, PENDING_APPROVAL_WAIT) => d,
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
    let handshake = async {
        let hello = Hello::decode(&first).map_err(|e| anyhow!("Hello decode: {e:?}"))?;
        anyhow::ensure!(
            hello.abi_version == punktfunk_core::ABI_VERSION,
            "ABI mismatch: client {} host {}",
            hello.abi_version,
            punktfunk_core::ABI_VERSION
        );
        // The pairing gate (require_pairing → paired? else park for delegated approval) ran above,
        // before this future, so a client reaching here is paired (or the host is `--open`).

        // Codec negotiation: pick the one codec this host will emit (its backend capability ∩ the
        // client's advertised codecs). A GPU-less software host emits H.264, so an HEVC-only client
        // shares nothing with it → refuse honestly rather than send a stream it can't decode.
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

        crate::encode::validate_dimensions(codec, hello.mode.width, hello.mode.height)
            .context("client-requested mode")?;

        // Resolve the client's compositor preference to a concrete backend *now*, so the Welcome
        // can report what we'll actually drive. Only the Virtual source has a compositor; the
        // synthetic source has no virtual output. Blocking probes → spawn_blocking.
        let compositor = match source {
            Punktfunk1Source::Virtual => {
                let pref = hello.compositor;
                Some(
                    tokio::task::spawn_blocking(move || resolve_compositor(pref))
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
        // mgmt/console toggle replaces it.
        let host_wants_10bit = crate::config::config().ten_bit;
        let client_supports_10bit = hello.video_caps & punktfunk_core::quic::VIDEO_CAP_10BIT != 0;
        let bit_depth: u8 = if host_wants_10bit && client_supports_10bit {
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

        // Resolve the chroma subsampling: full-chroma HEVC 4:4:4 only when ALL of — the host opted in
        // (PUNKTFUNK_444), the client advertised VIDEO_CAP_444, the session is single-process (the
        // two-process WGC relay encodes 4:2:0 in v1), and the active GPU/driver actually supports a
        // 4:4:4 encode (probed, cached). The native path always encodes HEVC. We resolve this BEFORE
        // the Welcome so `chroma_format` reflects what we'll really emit — the honest-downgrade
        // channel: if any gate fails the client is told 4:2:0 before it builds its decoder. The probe
        // opens a tiny encoder; it runs only when both opt-ins are set and is cached after the first.
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

        // Reserve a UDP port for the data plane (bind, read it back, rebind in UdpTransport).
        let probe = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let udp_port = probe.local_addr()?.port();
        drop(probe);

        let mut key = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut key);
        let welcome = Welcome {
            abi_version: punktfunk_core::ABI_VERSION,
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
            // ~1452-byte payload keeps the IP datagram within a 1500 MTU (1452 + 40 header + 24
            // crypto + 8 IP/UDP ≈ 1500), vs the old 1200 — ~17% fewer packets for free, and an even
            // size (FEC requires even shards). Negotiated, so the client follows. Jumbo (≈8900) is a
            // future negotiated bump (needs MAX_DATAGRAM_BYTES raised + end-to-end 9000 MTU).
            shard_payload: 1452,
            encrypt: true,
            key,
            salt: *b"pkf1",
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
            // The negotiated codec the encoder will emit (H.264 for a software host, else HEVC). The
            // client builds its decoder from this instead of assuming HEVC.
            codec: codec_bit,
        };
        io::write_msg(&mut send, &welcome.encode()).await?;

        let start = Start::decode(&io::read_msg(&mut recv).await?)
            .map_err(|e| anyhow!("Start decode: {e:?}"))?;
        Ok::<_, anyhow::Error>((hello, welcome, udp_port, start, compositor))
    };
    let (hello, welcome, udp_port, start, compositor) =
        tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| anyhow!("handshake timed out after {HANDSHAKE_TIMEOUT:?}"))??;
    let (mut ctrl_send, mut ctrl_recv) = (send, recv);
    // Negotiated codec (HEVC / H.264), derived from the Welcome. `Copy`, so the control task's
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
    let (probe_tx, probe_rx) = std::sync::mpsc::channel::<ProbeRequest>();
    let (probe_result_tx, mut probe_result_rx) =
        tokio::sync::mpsc::unbounded_channel::<ProbeResult>();
    // Adaptive FEC: the control task maps each client LossReport to a recovery percent and publishes
    // it here; the data-plane send loop reads + applies it per frame. Disabled (pinned) when
    // PUNKTFUNK_FEC_PCT is set. Seeded with the session's starting FEC so it's a no-op until a report.
    let adaptive_fec = fec_static_override().is_none();
    let fec_target = Arc::new(AtomicU8::new(welcome.fec.fec_percent));
    let fec_target_ctl = fec_target.clone();
    tokio::spawn(async move {
        let mut active = hello.mode;
        loop {
            tokio::select! {
                msg = io::read_msg(&mut ctrl_recv) => {
                    let Ok(msg) = msg else { break }; // stream closed
                    if let Ok(req) = Reconfigure::decode(&msg) {
                        let ok = req.mode.refresh_hz > 0
                            && crate::encode::validate_dimensions(
                                codec,
                                req.mode.width,
                                req.mode.height,
                            )
                            .is_ok();
                        if ok {
                            active = req.mode;
                            tracing::info!(mode = ?req.mode, "mode switch accepted");
                        } else {
                            tracing::warn!(mode = ?req.mode, "mode switch rejected (invalid dimensions)");
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
                    } else if let Ok(rep) = LossReport::decode(&msg) {
                        // Adaptive FEC: size recovery to the loss the client is seeing. The data-plane
                        // send loop reads `fec_target_ctl` and applies it per frame. Ignored when FEC
                        // is pinned via PUNKTFUNK_FEC_PCT.
                        if adaptive_fec {
                            let target = adapt_fec(rep.loss_ppm);
                            let prev = fec_target_ctl.swap(target, Ordering::Relaxed);
                            if prev != target {
                                tracing::info!(
                                    loss_ppm = rep.loss_ppm,
                                    fec_pct = target,
                                    prev_fec_pct = prev,
                                    "adaptive FEC adjusted"
                                );
                            }
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
            }
        }
    });

    // Input plane: QUIC datagrams → channel → a native per-session thread. Pointer/keyboard
    // events are forwarded to the host-lifetime [`InjectorService`] (`inj_tx`) so the portal
    // grant persists across sessions; this thread owns the session's virtual gamepads (uinput,
    // per-session) and sends force feedback back over `conn`. It exits when the channel closes
    // (datagram task ends on disconnect) — fresh gamepad state per session.
    let (input_tx, input_rx) = std::sync::mpsc::channel::<InputEvent>();
    let (rich_tx, rich_rx) = std::sync::mpsc::channel::<punktfunk_core::quic::RichInput>();
    let input_handle = {
        let conn = conn.clone();
        let gamepad = welcome.gamepad;
        std::thread::Builder::new()
            .name("punktfunk1-input".into())
            .spawn(move || input_thread(input_rx, rich_rx, conn, inj_tx, gamepad))
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
                if rich_tx.send(rich).is_err() {
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
                if input_tx.send(ev).is_err() {
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
    {
        let stop = stop.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            conn.closed().await;
            stop.store(true, Ordering::SeqCst);
        });
    }

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
        let meta = crate::hdr::generic_hdr10();
        let _ = conn.send_datagram(punktfunk_core::quic::encode_hdr_meta_datagram(&meta).into());
        tracing::info!("sent HDR10 static metadata (0xCE; generic baseline)");
    }

    // Test hook (synthetic source only): a scripted feedback burst on the host→client
    // planes — rumble (0xCA) + DualSense HID-output (0xCD) — so loopback tests can assert
    // the client's feedback path without a real game writing output reports to a real pad.
    if opts.source == Punktfunk1Source::Synthetic
        && std::env::var("PUNKTFUNK_TEST_FEEDBACK").as_deref() == Ok("1")
    {
        use punktfunk_core::quic::HidOutput;
        let d = punktfunk_core::quic::encode_rumble_datagram(0, 0x4000, 0x8000);
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
    let fec_target_dp = fec_target.clone(); // data-plane handle to the adaptive-FEC target
    let conn_stream = conn.clone(); // for sending the source's real HDR metadata (0xCE) mid-stream
    let stats_dp = stats; // data-plane handle to the shared stats recorder
                          // Short label for web-console stats captures: the client's cert-fingerprint prefix, else its
                          // peer IP (no fingerprint = anonymous TOFU/--open client).
    let client_label = endpoint::peer_fingerprint(&conn)
        .map(|fp| fingerprint_hex(&fp)[..12].to_string())
        .unwrap_or_else(|| conn.remote_address().ip().to_string());
    let result: Result<()> = async {
        tokio::task::spawn_blocking(move || -> Result<()> {
            // Wait briefly for the client to hole-punch our data port, then stream to its OBSERVED
            // source — so video traverses a NAT / stateful inter-VLAN firewall (the client and host
            // can be on different subnets; control + side planes ride the client-initiated QUIC, but
            // the raw video UDP needs the client to open the path first). Falls back to the
            // client-reported address for clients that don't punch (flat-LAN, unchanged).
            let (transport, punched) = match UdpTransport::connect_via_punch(
                &format!("0.0.0.0:{udp_port}"),
                &client_udp.to_string(),
                std::time::Duration::from_millis(2500),
            ) {
                Ok(v) => v,
                Err(e) => {
                    // Surface the failure here directly: a data-plane bind error would otherwise be
                    // reported only after teardown (and a teardown stall could swallow it entirely).
                    tracing::error!(error = %e, %client_udp, udp_port, "data-plane socket bind/hole-punch failed");
                    return Err(anyhow::Error::new(e)).context("bind data plane");
                }
            };
            tracing::info!(
                %client_udp,
                punched,
                "data plane bound (punched=true → streaming to the client's observed source; \
                 false → no hole-punch seen, using the reported address)"
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
                ),
                Punktfunk1Source::Virtual => {
                    let compositor = compositor
                        .expect("the Virtual source resolves a compositor during the handshake");
                    virtual_stream(SessionContext {
                        session,
                        mode,
                        seconds,
                        stop: stop_stream,
                        reconfig: reconfig_rx,
                        keyframe: keyframe_rx,
                        compositor,
                        bitrate_kbps,
                        bit_depth,
                        chroma,
                        codec,
                        probe_rx,
                        probe_result_tx,
                        fec_target: fec_target_dp,
                        conn: conn_stream,
                        stats: stats_dp,
                        client_label,
                        launch: launch_for_dp,
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
#[derive(Clone, Copy, Default)]
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

/// Highest pad index addressable on the wire (`flags` field); the uinput manager caps
/// actual pad creation at its own MAX_PADS.
const MAX_WIRE_PADS: usize = 16;

/// Backoff between reopen attempts after a host-lifetime service's backend (the mic source, a
/// capturer) fails to open or its worker dies, so a persistently-unavailable resource isn't hammered.
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// Mic is 48 kHz stereo — matches the Opus stereo decoder and the host→client audio layout.
const MIC_CHANNELS: u32 = 2;
/// Bound for the shared mic frame queue (drop-newest when full). See [`MicService::start`].
const MIC_QUEUE_CAP: usize = 64;

/// Host-lifetime virtual microphone, shared across punktfunk/1 sessions (mirror of
/// [`InjectorService`]). One thread owns the PipeWire `Audio/Source` + an Opus decoder; sessions
/// forward the client's Opus mic frames over a clonable `Send` channel, the thread decodes and
/// feeds the source. Opened lazily on the first frame, the source node persists across sessions
/// (no per-session registration churn), and reopens after a backoff if the source/decoder fails.
struct MicService {
    tx: std::sync::mpsc::SyncSender<Vec<u8>>,
}

impl MicService {
    fn start() -> MicService {
        // Bounded so the host-lifetime mic queue (shared across all concurrent sessions) can't grow
        // without limit under a near-line-rate flood; the producer drops the newest frame when full
        // (audio is lossy by design) rather than buffering unboundedly (security-review 2026-06-28
        // S6). 64 × 5–10 ms frames ≈ 0.3–0.6 s of slack, far more than the decode loop ever lags.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(MIC_QUEUE_CAP);
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk1-mic".into())
            .spawn(move || mic_service_thread(rx))
        {
            tracing::error!(error = %e, "mic service thread spawn failed — mic passthrough disabled");
        }
        MicService { tx }
    }

    /// A sender a session forwards the client's Opus mic frames to. Cloned per session; dropping a
    /// clone does NOT stop the service (it holds the original sender for the host life).
    fn sender(&self) -> std::sync::mpsc::SyncSender<Vec<u8>> {
        self.tx.clone()
    }
}

/// Stub — mic passthrough needs a virtual-mic backend (Linux PipeWire source / Windows virtual audio
/// device); other platforms drain and drop the frames (sessions still count the datagrams).
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn mic_service_thread(rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    tracing::warn!("punktfunk/1 mic passthrough unsupported on this platform — frames dropped");
    for _ in rx {}
}

/// The host-lifetime mic worker: lazily open the virtual mic + decoder, then Opus-decode each
/// forwarded frame and push the PCM into the source. Reopen (after [`INJECTOR_REOPEN_BACKOFF`])
/// only on a backend OPEN failure; a per-frame Opus DECODE error is just a dropped frame (it must
/// not tear down this mic, which is shared across every concurrent session — otherwise one paired
/// client's junk frames would deny everyone's mic; security-review 2026-06-28 S2). Exits when every
/// session sender and the service's own sender drop (host shutdown), tearing the virtual mic down.
/// Linux = PipeWire `Audio/Source`; Windows = a virtual audio device's render endpoint.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn mic_service_thread(rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut mic: Option<Box<dyn crate::audio::VirtualMic>> = None;
    let mut decoder: Option<opus::Decoder> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    let mut decode_fails: u64 = 0;
    let mut pcm = vec![0f32; 5760 * MIC_CHANNELS as usize]; // up to 120 ms scratch
    for opus_frame in rx {
        if opus_frame.is_empty() {
            continue; // DTX silence — the source underruns to silence on its own
        }
        if mic.is_none() || decoder.is_none() {
            if last_failed.is_some_and(|t| t.elapsed() < INJECTOR_REOPEN_BACKOFF) {
                continue; // still within the reopen backoff window
            }
            let opened = crate::audio::open_virtual_mic(MIC_CHANNELS).and_then(|m| {
                let d = opus::Decoder::new(48_000, opus::Channels::Stereo)
                    .map_err(|e| anyhow!("opus decoder: {e}"))?;
                Ok((m, d))
            });
            match opened {
                Ok((m, d)) => {
                    tracing::info!("punktfunk/1 virtual mic ready (host-lifetime)");
                    mic = Some(m);
                    decoder = Some(d);
                    last_failed = None;
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "virtual mic unavailable — will retry");
                    last_failed = Some(std::time::Instant::now());
                    continue;
                }
            }
        }
        let (Some(m), Some(dec)) = (mic.as_ref(), decoder.as_mut()) else {
            continue;
        };
        match dec.decode_float(&opus_frame, &mut pcm, false) {
            Ok(samples_per_ch) => {
                let total = (samples_per_ch * MIC_CHANNELS as usize).min(pcm.len());
                m.push(&pcm[..total]);
                decode_fails = 0;
            }
            Err(e) => {
                // Malformed/garbage frame: drop it and keep the (shared) mic + decoder open. The
                // next valid frame decodes normally; only a backend OPEN failure reopens. Throttle
                // the log (1, 2, 4, … fails) so a junk flood can't spam.
                decode_fails += 1;
                if decode_fails.is_power_of_two() {
                    tracing::warn!(error = %e, fails = decode_fails, "mic opus decode failed — dropping frame");
                }
            }
        }
    }
    tracing::debug!("mic service stopped (host shutting down)");
}

/// The session's virtual-gamepad backend, resolved once per session (sessions run serially).
///
/// - `Xbox360` — uinput X-Box-360 pads on Linux ([`GamepadManager`](crate::inject::gamepad::GamepadManager)),
///   the in-tree XUSB companion driver (classic XInput) on Windows. Also the X-Box One/Series identity
///   (`PUNKTFUNK_GAMEPAD=xboxone`): the same
///   backend with the One/Series USB VID/PID so games show One/Series glyphs (XInput-identical
///   otherwise). The Linux pad carries it as a [`PadIdentity`](crate::inject::gamepad::PadIdentity).
/// - `DualSense` (`PUNKTFUNK_GAMEPAD=dualsense`) — virtual DualSense via UHID + `hid-playstation`,
///   so a game sees a *real* DualSense (adaptive triggers, lightbar, touchpad, motion); feedback
///   flows back over the rich HID-output plane.
/// - `DualShock4` (`PUNKTFUNK_GAMEPAD=ps4`) — virtual DualShock 4 via the same UHID path: lightbar,
///   touchpad, motion, rumble (DualSense minus adaptive triggers / player LEDs / mute).
///
/// DualShock 4 + One/Series are Linux-only; DualSense has both a Linux (UHID) and a Windows (UMDF
/// minidriver) backend. The resolver folds any type a platform can't build into `Xbox360`, so a
/// build never constructs a variant it lacks.
enum PadBackend {
    Xbox360(crate::inject::gamepad::GamepadManager),
    #[cfg(target_os = "linux")]
    DualSense(crate::inject::dualsense::DualSenseManager),
    #[cfg(target_os = "linux")]
    DualShock4(crate::inject::dualshock4::DualShock4Manager),
    #[cfg(target_os = "linux")]
    SteamDeck(crate::inject::steam_controller::SteamControllerManager),
    #[cfg(target_os = "windows")]
    DualSenseWindows(crate::inject::dualsense_windows::DualSenseWindowsManager),
    #[cfg(target_os = "windows")]
    DualShock4Windows(crate::inject::dualshock4_windows::DualShock4WindowsManager),
}

impl PadBackend {
    /// `kind` is the session's resolved backend (see [`resolve_gamepad`] — client preference,
    /// env var, X-Box 360, in that order). Defensive cfg guard: a non-Linux build can only ever
    /// construct the X-Box backend, whatever the resolution said.
    fn select(kind: GamepadPref) -> PadBackend {
        #[cfg(target_os = "linux")]
        match kind {
            GamepadPref::DualSense => {
                tracing::info!("gamepad backend: virtual DualSense (UHID hid-playstation)");
                return PadBackend::DualSense(crate::inject::dualsense::DualSenseManager::new());
            }
            GamepadPref::DualShock4 => {
                tracing::info!("gamepad backend: virtual DualShock 4 (UHID hid-playstation)");
                return PadBackend::DualShock4(crate::inject::dualshock4::DualShock4Manager::new());
            }
            GamepadPref::SteamDeck => {
                tracing::info!("gamepad backend: virtual Steam Deck (UHID hid-steam)");
                return PadBackend::SteamDeck(
                    crate::inject::steam_controller::SteamControllerManager::new(),
                );
            }
            GamepadPref::XboxOne => {
                tracing::info!("gamepad backend: uinput X-Box One/Series pad");
                return PadBackend::Xbox360(crate::inject::gamepad::GamepadManager::with_identity(
                    crate::inject::gamepad::PadIdentity::xbox_one(),
                ));
            }
            _ => {}
        }
        #[cfg(target_os = "windows")]
        match kind {
            GamepadPref::DualSense => {
                tracing::info!("gamepad backend: virtual DualSense (Windows UMDF shm channel)");
                return PadBackend::DualSenseWindows(
                    crate::inject::dualsense_windows::DualSenseWindowsManager::new(),
                );
            }
            GamepadPref::DualShock4 => {
                tracing::info!("gamepad backend: virtual DualShock 4 (Windows UMDF shm channel)");
                return PadBackend::DualShock4Windows(
                    crate::inject::dualshock4_windows::DualShock4WindowsManager::new(),
                );
            }
            _ => {}
        }
        let _ = kind;
        PadBackend::Xbox360(crate::inject::gamepad::GamepadManager::new())
    }

    fn handle(&mut self, ev: &crate::gamestream::gamepad::GamepadEvent) {
        match self {
            PadBackend::Xbox360(m) => m.handle(ev),
            #[cfg(target_os = "linux")]
            PadBackend::DualSense(m) => m.handle(ev),
            #[cfg(target_os = "linux")]
            PadBackend::DualShock4(m) => m.handle(ev),
            #[cfg(target_os = "linux")]
            PadBackend::SteamDeck(m) => m.handle(ev),
            #[cfg(target_os = "windows")]
            PadBackend::DualSenseWindows(m) => m.handle(ev),
            #[cfg(target_os = "windows")]
            PadBackend::DualShock4Windows(m) => m.handle(ev),
        }
    }

    /// Apply a rich client→host event (touchpad / motion). A no-op for the X-Box pad, which has no
    /// equivalent; the DualSense and DualShock 4 pads both carry a touchpad + motion sensors.
    fn apply_rich(&mut self, _rich: punktfunk_core::quic::RichInput) {
        match self {
            PadBackend::Xbox360(_) => {}
            #[cfg(target_os = "linux")]
            PadBackend::DualSense(m) => m.apply_rich(_rich),
            #[cfg(target_os = "linux")]
            PadBackend::DualShock4(m) => m.apply_rich(_rich),
            #[cfg(target_os = "linux")]
            PadBackend::SteamDeck(m) => m.apply_rich(_rich),
            #[cfg(target_os = "windows")]
            PadBackend::DualSenseWindows(m) => m.apply_rich(_rich),
            #[cfg(target_os = "windows")]
            PadBackend::DualShock4Windows(m) => m.apply_rich(_rich),
        }
    }

    /// Service feedback every cycle. `rumble` carries motor force-feedback on the universal plane
    /// (every backend); `hidout` carries rich feedback on the HID-output plane — lightbar (both
    /// UHID pads), plus player LEDs / adaptive triggers (DualSense only). The X-Box pad has no
    /// rich-feedback plane.
    fn pump(
        &mut self,
        rumble: impl FnMut(u16, u16, u16),
        hidout: impl FnMut(punktfunk_core::quic::HidOutput),
    ) {
        match self {
            PadBackend::Xbox360(m) => {
                let _ = hidout; // the X-Box pad has no rich-feedback plane
                m.pump_rumble(rumble)
            }
            #[cfg(target_os = "linux")]
            PadBackend::DualSense(m) => m.pump(rumble, hidout),
            #[cfg(target_os = "linux")]
            PadBackend::DualShock4(m) => m.pump(rumble, hidout),
            #[cfg(target_os = "linux")]
            PadBackend::SteamDeck(m) => m.pump(rumble, hidout),
            #[cfg(target_os = "windows")]
            PadBackend::DualSenseWindows(m) => m.pump(rumble, hidout),
            #[cfg(target_os = "windows")]
            PadBackend::DualShock4Windows(m) => m.pump(rumble, hidout),
        }
    }

    /// Keep a virtual UHID pad alive during input silence: re-emit its current HID report if it's
    /// gone quiet, so the kernel `hid-playstation` driver / SDL don't treat a held-steady pad as
    /// unplugged ("controller disconnected every few seconds"). No-op for the X-Box pad (evdev
    /// holds last-known state with no periodic-report requirement). Called every input-thread tick;
    /// the per-pad gap timer (not the tick rate) governs the actual emit cadence.
    fn heartbeat(&mut self) {
        match self {
            PadBackend::Xbox360(_) => {}
            #[cfg(target_os = "linux")]
            PadBackend::DualSense(m) => m.heartbeat(std::time::Duration::from_millis(8)),
            #[cfg(target_os = "linux")]
            PadBackend::DualShock4(m) => m.heartbeat(std::time::Duration::from_millis(8)),
            #[cfg(target_os = "linux")]
            PadBackend::SteamDeck(m) => m.heartbeat(std::time::Duration::from_millis(8)),
            #[cfg(target_os = "windows")]
            PadBackend::DualSenseWindows(m) => m.heartbeat(std::time::Duration::from_millis(8)),
            #[cfg(target_os = "windows")]
            PadBackend::DualShock4Windows(m) => m.heartbeat(std::time::Duration::from_millis(8)),
        }
    }
}

/// The per-session input thread: route pointer/keyboard events to the host-lifetime injector
/// service (`inj_tx`) and gamepad events to this session's [`PadBackend`] (`gamepad` — the
/// resolved Hello preference: uinput X-Box pads or virtual DualSense pads), with rich
/// client→host input (touchpad / motion, `rich_rx`) merged in and feedback pumped between
/// events — rumble on the universal datagram plane, DualSense LED/trigger feedback on the
/// HID-output plane. The gamepads are created and torn down with the session; the
/// pointer/keyboard injector (and its portal grant) lives in the service, across sessions.
fn input_thread(
    rx: std::sync::mpsc::Receiver<InputEvent>,
    rich_rx: std::sync::mpsc::Receiver<punktfunk_core::quic::RichInput>,
    conn: quinn::Connection,
    inj_tx: std::sync::mpsc::Sender<InputEvent>,
    gamepad: GamepadPref,
) {
    let mut pads = PadBackend::select(gamepad);
    let mut pad_state = [PadState::default(); MAX_WIRE_PADS];
    let mut pad_mask = 0u16;
    // Rumble is idempotent state on a lossy channel (client-side overflow drops datagrams),
    // so re-send the current state of every rumbling-capable pad every 500 ms — a dropped
    // transition (including a stop) heals on the next refresh.
    let mut rumble_state = [(0u16, 0u16); MAX_WIRE_PADS];
    let mut rumble_seen = [false; MAX_WIRE_PADS];
    let mut last_refresh = std::time::Instant::now();
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
            Ok(ev) => match ev.kind {
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
        // Drain rich client→host input (DualSense touchpad / motion) into the pad backend.
        while let Ok(rich) = rich_rx.try_recv() {
            pads.apply_rich(rich);
        }
        // Service feedback every iteration (≤4 ms latency; games block on EVIOCSFF, and the
        // DualSense kernel handshake must be answered promptly). Rumble → the universal 0xCA
        // plane; DualSense rich feedback (lightbar / player LEDs / adaptive triggers) → 0xCD.
        pads.pump(
            |pad, low, high| {
                if let Some(s) = rumble_state.get_mut(pad as usize) {
                    // Log the silent→active transition (once per buzz) so a live test can tell
                    // "host never gets rumble from the game" apart from "client doesn't render it".
                    if *s == (0, 0) && (low != 0 || high != 0) {
                        tracing::info!(pad, low, high, "rumble: forwarding to client (0xCA)");
                    }
                    *s = (low, high);
                    rumble_seen[pad as usize] = true;
                }
                let d = punktfunk_core::quic::encode_rumble_datagram(pad, low, high);
                let _ = conn.send_datagram(d.to_vec().into());
            },
            |h| {
                let _ = conn.send_datagram(h.encode().into());
            },
        );
        // Keep the virtual DualSense from going silent during steady input (no-op for X-Box): a
        // held-steady pad sends no wire events, so without a periodic re-emit the kernel/SDL drop
        // it as unplugged. The 8 ms gap inside heartbeat() governs the rate, not this ≤4 ms tick.
        pads.heartbeat();
        if last_refresh.elapsed() >= std::time::Duration::from_millis(500) {
            last_refresh = std::time::Instant::now();
            for (i, &(low, high)) in rumble_state.iter().enumerate() {
                if rumble_seen[i] {
                    let d = punktfunk_core::quic::encode_rumble_datagram(i as u16, low, high);
                    let _ = conn.send_datagram(d.to_vec().into());
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

fn synthetic_stream(
    session: &mut Session,
    frames: u32,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    fec_target: &AtomicU8,
) -> Result<()> {
    let interval = std::time::Duration::from_millis(1000 / 60);
    for idx in 0..frames {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        apply_fec_target(session, fec_target);
        // Service speed-test probes between synthetic frames (loopback bandwidth tests).
        service_probes(session, stop, probe_rx, probe_result_tx);
        let data = test_frame(idx, 64 * 1024);
        session
            .submit_frame(&data, now_ns(), (FLAG_PIC | FLAG_SOF) as u32)
            .map_err(|e| anyhow!("submit_frame: {e:?}"))?;
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
        // Steam Deck: Linux UHID hid-steam. The classic Steam Controller's backend isn't built yet,
        // so it folds to Xbox360 for now (Windows Steam devices are M7).
        GamepadPref::SteamDeck if linux => GamepadPref::SteamDeck,
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
        GamepadPref::DualSense | GamepadPref::DualShock4 | GamepadPref::SteamDeck
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
#[cfg(target_os = "linux")]
fn physical_steam_controller_present() -> bool {
    let Ok(entries) = std::fs::read_dir("/sys/bus/hid/devices") else {
        return false;
    };
    entries.flatten().any(|e| {
        if !e.file_name().to_string_lossy().contains(":28DE:") {
            return false;
        }
        match std::fs::read_link(e.path()) {
            Ok(target) => !target.to_string_lossy().contains("/virtual/"),
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
    match crate::vdisplay::Compositor::from_pref(pref) {
        Some(want) if available.contains(&want) => Some(want),
        _ => detected,
    }
}

/// Resolve the client's compositor preference to a concrete backend (the I/O shell around
/// [`pick_compositor`]): enumerate what's available, auto-detect the default, pick, and log
/// whether the explicit request was honored or fell back. Runs blocking probes — call off the
/// async reactor (`spawn_blocking`).
fn resolve_compositor(pref: CompositorPref) -> Result<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    // Windows has a single virtual-display backend (SudoVDA); vdisplay::open ignores the compositor
    // arg there, so short-circuit the Linux session-detection state machine with a placeholder.
    #[cfg(target_os = "windows")]
    {
        let _ = pref;
        Ok(Compositor::Kwin)
    }
    #[cfg(not(target_os = "windows"))]
    {
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
            crate::vdisplay::apply_session_env(&active);
            tracing::info!(
                active = ?active.kind,
                wayland = active.env.wayland_display.as_deref().unwrap_or("-"),
                "detected active graphical session"
            );
            crate::vdisplay::compositor_for_kind(active.kind)
        };
        let available = crate::vdisplay::available();
        let chosen = pick_compositor(pref, &available, detected).ok_or_else(|| {
        anyhow!("no usable compositor (no live graphical session for this uid; set PUNKTFUNK_COMPOSITOR or start a desktop/gaming session)")
    })?;
        if !overridden {
            // Point input at the same backend and resolve the gamescope sub-mode (managed where the
            // session infra exists, attach to a foreign gamescope, else per-session bare spawn).
            crate::vdisplay::apply_input_env(chosen);
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
fn run_probe_burst(session: &mut Session, req: ProbeRequest, stop: &AtomicBool) -> ProbeResult {
    let target_kbps = req.target_kbps.min(MAX_PROBE_KBPS);
    let duration_ms = req.duration_ms.min(MAX_PROBE_MS);
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
            // part of what the probe measures (it surfaces as send_dropped), so keep going.
            let _ = session.submit_frame(&filler, now_ns(), FLAG_PROBE as u32);
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
/// Called once per data-plane loop iteration so a probe runs between frames.
fn service_probes(
    session: &mut Session,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
) {
    while let Ok(req) = probe_rx.try_recv() {
        let result = run_probe_burst(session, req, stop);
        let _ = probe_result_tx.send(result);
    }
}

/// Seal one access unit and send its packets PACED over the budget until `deadline` (the next
/// frame's due time), in 16-packet `sendmmsg` chunks — so a high-bitrate frame spreads across the
/// frame interval instead of bursting all at once into the NIC. A real link drops a line-rate burst
/// (the host send buffer EAGAINs), and under infinite GOP a single dropped frame freezes the decode
/// until the next keyframe — the cause of the "freezes over ~150 Mbps, no image at 400 Mbps"
/// symptom. When there's little/no slack (encode ≈ interval at very high fps) the budget collapses
/// to ~0 and every chunk goes out immediately, so this is never slower than the unpaced path.
/// One paced send's outcome: how long the frame's packets took to leave (`spread_us`) and whether
/// any were paced (vs the whole frame fitting the microburst and going out immediately). Fed to the
/// PUNKTFUNK_PERF histogram so the pacing tail is visible per-frame.
struct PaceStat {
    spread_us: u32,
    paced: bool,
}

const PACE_CHUNK: usize = 16;

/// Seal one access unit and send it with MICROBURST pacing: the first `burst_cap` bytes go out
/// immediately (one absorbed burst the NIC / socket tx-buffer can swallow), and only the OVERFLOW
/// beyond that is spread in [`PACE_CHUNK`]-packet chunks across ~90% of the time to `deadline`. So a
/// normal-bitrate frame (≤ cap) leaves in one immediate burst at ~0 added latency, while a genuine
/// IDR / sustained-high-bitrate frame (≫ cap) still spreads — keeping the freeze fix exactly where
/// it's needed (an unpaced line-rate burst overruns the kernel tx buffer → EAGAIN drop → under
/// infinite GOP, a freeze until the next keyframe). With no slack (encode ≈ interval) the budget
/// collapses to 0 and even the overflow goes out immediately, so this is never slower than unpaced.
fn paced_submit(
    session: &mut Session,
    data: &[u8],
    pts_ns: u64,
    flags: u32,
    deadline: std::time::Instant,
    burst_cap: usize,
) -> Result<PaceStat> {
    let wires = session
        .seal_frame(data, pts_ns, flags)
        .map_err(|e| anyhow!("seal_frame: {e:?}"))?;
    let refs: Vec<&[u8]> = wires.iter().map(|w| w.as_slice()).collect();
    let start = std::time::Instant::now();

    // Split at the microburst cap: packets [0..split] burst out immediately, [split..] are paced.
    let mut cum = 0usize;
    let mut split = refs.len();
    for (k, r) in refs.iter().enumerate() {
        cum += r.len();
        if cum >= burst_cap {
            split = k + 1;
            break;
        }
    }
    for chunk in refs[..split].chunks(PACE_CHUNK) {
        session
            .send_sealed(chunk)
            .map_err(|e| anyhow!("send_sealed: {e:?}"))?;
    }
    let paced = split < refs.len();
    if paced {
        let pace_start = std::time::Instant::now();
        let budget = deadline
            .checked_duration_since(pace_start)
            .unwrap_or_default()
            .mul_f32(0.9);
        let m = refs[split..].len().div_ceil(PACE_CHUNK).max(1);
        for (j, chunk) in refs[split..].chunks(PACE_CHUNK).enumerate() {
            session
                .send_sealed(chunk)
                .map_err(|e| anyhow!("send_sealed: {e:?}"))?;
            // Sleep toward this chunk's slice of the budget; skip sub-500µs waits (scheduler jitter).
            let target = pace_start + budget.mul_f64((j + 1) as f64 / m as f64);
            if let Some(ahead) = target.checked_duration_since(std::time::Instant::now()) {
                if ahead > std::time::Duration::from_micros(500) {
                    std::thread::sleep(ahead);
                }
            }
        }
    }
    let spread_us = start.elapsed().as_micros() as u32;
    drop(refs); // release the borrow of `wires` so it can return to the seal pool
    session.reclaim_wires(wires);
    Ok(PaceStat { spread_us, paced })
}

/// Percentile of a slice (sorts it in place first). `q` in 0.0..=1.0.
fn percentile(sorted_or_not: &mut [u32], q: f64) -> u32 {
    if sorted_or_not.is_empty() {
        return 0;
    }
    sorted_or_not.sort_unstable();
    let i = ((sorted_or_not.len() as f64 * q) as usize).min(sorted_or_not.len() - 1);
    sorted_or_not[i]
}

/// One encoded frame handed from the capture/encode thread to the send thread (the encode|send
/// split). The send thread does FEC+seal+paced-send while this thread captures+encodes the next.
struct FrameMsg {
    data: Vec<u8>,
    capture_ns: u64,
    flags: u32,
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
    width: u32,
    height: u32,
    fps: u32,
    codec: &'static str,
    client: String,
    bitrate_kbps: u32,
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
        service_probes(&mut session, &stop, &probe_rx, &probe_result_tx);
        // Adaptive FEC: pick up any new recovery target the control task set from client LossReports.
        apply_fec_target(&mut session, &fec_target);
        // Short timeout so we keep re-checking `stop` + probes when no frames are flowing.
        match frame_rx.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(msg) => match paced_submit(
                &mut session,
                &msg.data,
                msg.capture_ns,
                msg.flags,
                msg.deadline,
                burst_cap,
            ) {
                Ok(stat) => {
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
                    stats.rec.register_session(
                        "native",
                        stats.width,
                        stats.height,
                        stats.fps,
                        stats.codec,
                        &stats.client,
                    )
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
                    bitrate_kbps: stats.bitrate_kbps,
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
    /// Accepted mid-stream mode switches — the pipeline is rebuilt at the new mode.
    reconfig: std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    /// Client decode-recovery keyframe requests.
    keyframe: std::sync::mpsc::Receiver<()>,
    /// The resolved compositor backend (moot on Windows — `vdisplay::open` ignores it there).
    compositor: crate::vdisplay::Compositor,
    /// Negotiated encoder bitrate (kbps).
    bitrate_kbps: u32,
    /// Negotiated encode bit depth (8, or 10 = HEVC Main10).
    bit_depth: u8,
    /// Negotiated chroma subsampling (4:2:0, or 4:4:4 when the client + host + GPU all support it).
    chroma: crate::encode::ChromaFormat,
    /// Negotiated video codec the encoder emits (HEVC by default; H.264 for a software host). Also
    /// used to rebuild the encoder at the same codec across a mid-stream mode reconfigure.
    codec: crate::encode::Codec,
    /// Speed-test burst requests (see [`service_probes`]).
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    /// Speed-test results back to the control task.
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    /// Adaptive-FEC target the control task updates from the client's loss reports.
    fec_target: Arc<AtomicU8>,
    /// The QUIC control connection (carries host→client 0xCE source-HDR metadata mid-stream).
    conn: quinn::Connection,
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
        reconfig,
        keyframe,
        compositor,
        bitrate_kbps,
        bit_depth,
        // The resolved chroma is already captured in `plan` (above); ignore the duplicate here.
        chroma: _,
        // Likewise the codec — `plan.codec` (resolved from `ctx.codec`) is the source of truth below.
        codec: _,
        probe_rx,
        probe_result_tx,
        fec_target,
        conn,
        stats,
        client_label,
        launch,
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
    #[cfg(target_os = "windows")]
    let _idd_setup_guard = (plan.capture == crate::session_plan::CaptureBackend::IddPush)
        .then(|| crate::vdisplay::manager::vdm().begin_idd_setup(stop.clone()));
    let (mut capturer, mut enc, mut frame, mut interval) =
        build_pipeline_with_retry(&mut vd, mode, bitrate_kbps, bit_depth, plan)?;
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
    // The send thread emits the web-console stats sample (it owns `session.stats()`); clone the
    // recorder so the capture loop keeps its own handle for the per-frame `is_armed()` gate.
    let send_stats = SendStats {
        rec: stats.clone(),
        width: mode.width,
        height: mode.height,
        fps: mode.refresh_hz,
        codec: "hevc",
        client: client_label,
        bitrate_kbps,
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
    // Rebuild-in-place on capture loss: track the live mode (a mode switch updates it) so a rebuild
    // targets the CURRENT mode, and cap consecutive rebuilds so a flapping source can't loop the
    // client through endless cold restarts.
    let mut cur_mode = mode;
    const MAX_CAPTURE_REBUILDS: u32 = 5;
    let mut capture_rebuilds: u32 = 0;
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
    // Last client-requested forced IDR — the intra-refresh rate limit window anchor (see below).
    let mut last_forced_idr: Option<std::time::Instant> = None;
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
                });
                crate::vdisplay::apply_input_env(sw.compositor);
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
                        )?;
                        Ok((new_vd, pipe))
                    })();
                match rebuilt {
                    Ok((new_vd, (new_cap, new_enc, new_frame, new_interval))) => {
                        // Replace the pipeline first (drops the old capturer → old PipeWire stream +
                        // virtual output), then the factory (drops e.g. the old KWin connection).
                        capturer = new_cap;
                        enc = new_enc;
                        frame = new_frame;
                        interval = new_interval;
                        vd = new_vd;
                        compositor = sw.compositor;
                        next = std::time::Instant::now();
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
            match build_pipeline(&mut vd, new_mode, bitrate_kbps, bit_depth, plan) {
                Ok(next_pipe) => {
                    (capturer, enc, frame, interval) = next_pipe;
                    cur_mode = new_mode;
                    next = std::time::Instant::now();
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), ?new_mode,
                        "mode-switch rebuild failed — staying on the current mode");
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
        if want_kf {
            // Intra-refresh mode: clients request a keyframe on EVERY FEC-unrecoverable frame
            // (`frames_dropped` polling), but the refresh wave already heals those within ~half a
            // second — answering each with a full IDR is the 20-40× spike cascade the wave exists
            // to avoid. Serve the first request immediately (a genuinely wedged decoder recovers
            // at once), then suppress further requests for one window and let the wave heal.
            const IDR_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
            let suppress = enc.caps().intra_refresh
                && last_forced_idr.is_some_and(|t| t.elapsed() < IDR_WINDOW);
            if suppress {
                tracing::debug!("keyframe request suppressed — intra-refresh wave healing");
            } else {
                tracing::debug!("forcing keyframe (client decode recovery)");
                enc.request_keyframe();
                last_forced_idr = Some(std::time::Instant::now());
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
                let (new_cap, new_enc, new_frame, new_interval) = loop {
                    // Follow the active session unless an explicit PUNKTFUNK_COMPOSITOR pin forbids
                    // retargeting (then we stick to the pinned backend and just rebuild it).
                    if crate::config::config().compositor.is_none() {
                        let active = crate::vdisplay::detect_active_session();
                        if let Some(c) = crate::vdisplay::compositor_for_kind(active.kind) {
                            crate::vdisplay::apply_session_env(&active);
                            crate::vdisplay::apply_input_env(c);
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
                enc.request_keyframe(); // belt-and-suspenders; a fresh encoder opens on an IDR anyway
                next = std::time::Instant::now();
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
        // The source's static HDR mastering metadata (Windows GetDesc1; None on Linux/SDR) is the
        // single source of truth: hand it to the encoder (in-band SEI on keyframes) and, when it
        // changes, to the client (0xCE). Re-sent on each keyframe below so a dropped best-effort
        // datagram converges within a GOP.
        let hdr_meta = capturer.hdr_meta();
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
        enc.submit(&frame).context("encoder submit")?;
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
        while inflight.len() >= depth {
            let t_wait = std::time::Instant::now();
            let polled = enc.poll().context("encoder poll")?;
            let wait_us = if measure {
                t_wait.elapsed().as_micros() as u32
            } else {
                0
            };
            if perf {
                st_wait.push(wait_us);
            }
            let au = match polled {
                Some(au) => au,
                None => break, // no AU ready for a submitted frame (shouldn't happen — poll blocks)
            };
            let (cap_ns, sub_ns, deadline) = inflight.pop_front().expect("inflight non-empty");
            let flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
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
            sent += 1;
        }
        if send_gone {
            break;
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
        match build_pipeline(vd, mode, bitrate_kbps, bit_depth, plan) {
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

fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bit_depth: u8,
    plan: crate::session_plan::SessionPlan,
) -> Result<Pipeline> {
    let vout = vd.create(mode).context("create virtual output")?;
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
    let frame = capturer.next_frame().context("first frame")?;
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
    Ok((capturer, enc, frame, interval))
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // The punktfunk/1 button bits are the GameStream bits — one wire contract end to end.
        assert_eq!(BTN_A, crate::gamestream::gamepad::BTN_A);
        assert_eq!(BTN_GUIDE, crate::gamestream::gamepad::BTN_GUIDE);
        assert_eq!(BTN_DPAD_UP, crate::gamestream::gamepad::BTN_DPAD_UP);
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
    /// enqueues → `punktfunk_connection_close`. Three sequential sessions against ONE host
    /// process prove the persistent listener, and a wrong pin is rejected.
    #[test]
    fn c_abi_connection_roundtrip() {
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
