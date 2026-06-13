//! M3 — the `punktfunk/1` native host: QUIC control plane + the hardened M1 data plane over UDP.
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
//! `punktfunk-host m3-host [--port 9777] [--source synthetic|virtual] [--seconds 30]
//!  [--frames 300]` serves sessions back to back (one at a time — the virtual output and
//!  encoder are single-tenant); `punktfunk-client-rs --connect host:9777` is the counterpart.
//!  The data plane runs on native threads (no async on the frame path).
//!
//! Alongside video + input, a session carries **audio** (desktop Opus, 5 ms frames, host →
//! client QUIC datagrams tagged [`punktfunk_core::quic::AUDIO_MAGIC`]) and **gamepads** (client
//! GamepadButton/GamepadAxis datagrams accumulated into per-pad state for the virtual xpad;
//! force feedback flows back as [`punktfunk_core::quic::RUMBLE_MAGIC`] datagrams).
//!
//! Trust: the host serves with its persistent identity (`~/.config/punktfunk/cert.pem`, shared
//! with GameStream pairing) and logs the SHA-256 fingerprint clients pin.

use anyhow::{anyhow, Context, Result};
use punktfunk_core::config::{CompositorPref, FecConfig, FecScheme, GamepadPref, Role};
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::packet::{FLAG_PIC, FLAG_PROBE, FLAG_SOF};
use punktfunk_core::quic::{
    endpoint, io, ClockEcho, ClockProbe, Hello, PairChallenge, PairProof, PairRequest, PairResult,
    ProbeRequest, ProbeResult, Reconfigure, Reconfigured, RequestKeyframe, Start, Welcome,
};
use punktfunk_core::transport::UdpTransport;
use punktfunk_core::Session;
use rand::RngCore;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum M3Source {
    /// Deterministic test frames (protocol verification; the client byte-checks them).
    Synthetic,
    /// Real capture: virtual display at the client's requested mode → NVENC.
    Virtual,
}

pub struct M3Options {
    pub port: u16,
    pub source: M3Source,
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
use crate::native_pairing::NativePairing;

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

pub fn run(opts: M3Options) -> Result<()> {
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
    rt.block_on(serve(opts, np))
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
}

/// Options for the native host when the unified `serve --native` runs it: real virtual capture,
/// persistent (no session/duration cut), pairing armed on demand via the management API (the
/// shared [`NativePairing`] starts disarmed).
/// Default cap on simultaneously-streaming sessions (each holds an NVENC session; high-res
/// split-encode holds two). Conservative — consumer NVENC historically capped concurrent sessions;
/// overflow clients wait in the accept queue. Override with `--max-concurrent`.
pub(crate) const DEFAULT_MAX_CONCURRENT: usize = 4;

pub(crate) fn native_serve_opts(cfg: &NativeServe) -> M3Options {
    M3Options {
        port: cfg.port,
        source: M3Source::Virtual,
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

pub(crate) async fn serve(opts: M3Options, np: Arc<NativePairing>) -> Result<()> {
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
    let injector = InjectorService::start();
    // One virtual microphone for the whole host lifetime (see MicService): the client's mic uplink
    // (0xCB) is Opus-decoded and fed into a persistent PipeWire Audio/Source host apps record from.
    let mic_service = MicService::start();
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
        let inj_tx = injector.sender();
        let mic_tx = mic_service.sender();
        sessions.spawn(async move {
            let _permit = permit; // held for the session's lifetime; frees a slot on completion
            match serve_session(
                conn,
                &opts,
                &audio_cap,
                inj_tx,
                mic_tx,
                &fingerprint,
                &np,
                &last_pairing,
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

/// FEC recovery percent for the session's Welcome. Default 20% (Sunshine's default too); a clean
/// wired LAN can lower it (every recovery shard is wire bytes + packets), so `PUNKTFUNK_FEC_PCT`
/// overrides it — e.g. `0` disables FEC entirely, `10` halves the overhead. Clamped to ≤ 90.
fn fec_percent_from_env() -> u8 {
    std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .map(|p| p.min(90))
        .unwrap_or(20)
}

/// Persistent audio-capturer slot, reused across sessions (same pattern as the GameStream
/// path): keeps one warm PipeWire capture stream instead of a connect/negotiate cycle —
/// and a daemon-side node churn — per session. (Drop now tears a capturer down cleanly.)
type AudioCapSlot = Arc<std::sync::Mutex<Option<Box<dyn crate::audio::AudioCapturer>>>>;

/// Pairing needs a human in the loop (reading the PIN off the host, typing it into the
/// client), so its budget is far larger than the machine-speed session handshake.
const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
    opts: &M3Options,
    audio_cap: &AudioCapSlot,
    inj_tx: std::sync::mpsc::Sender<InputEvent>,
    mic_tx: std::sync::mpsc::Sender<Vec<u8>>,
    host_fp: &[u8; 32],
    np: &NativePairing,
    last_pairing: &std::sync::Mutex<Option<std::time::Instant>>,
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
        // Read the live arming PIN per attempt, so a window that lapsed no longer pairs.
        let pin = np
            .current_pin()
            .context("pairing not armed (arm it in the console, or start with --allow-pairing)")?;
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
        if opts.require_pairing {
            let fp = endpoint::peer_fingerprint(&conn);
            let known = fp
                .as_ref()
                .map(|fp| np.is_paired(&fingerprint_hex(fp)))
                .unwrap_or(false);
            if !known {
                // Delegated approval (§8b-1): an identified-but-unpaired knock becomes a pending
                // request the operator can approve from the console — no PIN fetched out of band.
                // The label is the client's Hello name, else fingerprint-derived. An anonymous
                // client (no certificate) has no identity to approve, so nothing is recorded.
                if let Some(fp) = &fp {
                    let fp_hex = fingerprint_hex(fp);
                    // Sanitize the wire-supplied name before it reaches the log (untrusted: an
                    // unpaired device could embed terminal escapes / bidi overrides); note_pending
                    // stores the same sanitized form and derives a fingerprint label when empty.
                    let label = crate::native_pairing::sanitize_device_name(
                        hello.name.as_deref().unwrap_or(""),
                        &fp_hex,
                    );
                    tracing::info!(name = %label, fingerprint = %fp_hex,
                        "unpaired device knocked — held for approval in the console");
                    np.note_pending(&label, &fp_hex);
                }
                anyhow::bail!(
                    "unpaired client rejected (this host requires pairing — approve the device \
                     in the console, or run the PIN ceremony)"
                );
            }
        }
        crate::encode::validate_dimensions(
            crate::encode::Codec::H265,
            hello.mode.width,
            hello.mode.height,
        )
        .context("client-requested mode")?;

        // Resolve the client's compositor preference to a concrete backend *now*, so the Welcome
        // can report what we'll actually drive. Only the Virtual source has a compositor; the
        // synthetic source has no virtual output. Blocking probes → spawn_blocking.
        let compositor = match source {
            M3Source::Virtual => {
                let pref = hello.compositor;
                Some(
                    tokio::task::spawn_blocking(move || resolve_compositor(pref))
                        .await
                        .context("resolve compositor task")??,
                )
            }
            M3Source::Synthetic => None,
        };

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
                fec_percent: fec_percent_from_env(),
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
                M3Source::Synthetic => frames,
                M3Source::Virtual => 0, // unbounded — client streams until we close
            },
            // Report the resolved backends back to the client (compositor: Auto for the
            // synthetic source).
            compositor: compositor
                .map(|c| c.as_pref())
                .unwrap_or(CompositorPref::Auto),
            gamepad,
            bitrate_kbps,
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
    tokio::spawn(async move {
        let mut active = hello.mode;
        loop {
            tokio::select! {
                msg = io::read_msg(&mut ctrl_recv) => {
                    let Ok(msg) = msg else { break }; // stream closed
                    if let Ok(req) = Reconfigure::decode(&msg) {
                        let ok = req.mode.refresh_hz > 0
                            && crate::encode::validate_dimensions(
                                crate::encode::Codec::H265,
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
            .name("punktfunk-m3-input".into())
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
                // Host-lifetime mic service; a send error just means the host is shutting down.
                let _ = mic_tx.send(opus.to_vec());
            } else if let Some(rich) = punktfunk_core::quic::RichInput::decode(&d) {
                rich_count += 1;
                if rich_tx.send(rich).is_err() {
                    break;
                }
            } else if let Some(ev) = InputEvent::decode(&d) {
                input_count += 1;
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
    let audio_handle = if opts.source == M3Source::Virtual {
        let conn = conn.clone();
        let stop = stop.clone();
        let cap = audio_cap.clone();
        std::thread::Builder::new()
            .name("punktfunk-m3-audio".into())
            .spawn(move || audio_thread(conn, stop, cap))
            .map_err(|e| tracing::error!(error = %e, "audio thread spawn failed — session continues without audio"))
            .ok()
    } else {
        None
    };

    // Test hook (synthetic source only): a scripted feedback burst on the host→client
    // planes — rumble (0xCA) + DualSense HID-output (0xCD) — so loopback tests can assert
    // the client's feedback path without a real game writing output reports to a real pad.
    if opts.source == M3Source::Synthetic
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
    let bitrate_kbps = welcome.bitrate_kbps; // resolved encoder bitrate (Hello clamped, or default)
    let stop_stream = stop.clone();
    let result: Result<()> = async {
        tokio::task::spawn_blocking(move || -> Result<()> {
            // Wait briefly for the client to hole-punch our data port, then stream to its OBSERVED
            // source — so video traverses a NAT / stateful inter-VLAN firewall (the client and host
            // can be on different subnets; control + side planes ride the client-initiated QUIC, but
            // the raw video UDP needs the client to open the path first). Falls back to the
            // client-reported address for clients that don't punch (flat-LAN, unchanged).
            let (transport, punched) = UdpTransport::connect_via_punch(
                &format!("0.0.0.0:{udp_port}"),
                &client_udp.to_string(),
                std::time::Duration::from_millis(2500),
            )
            .context("bind data plane")?;
            tracing::info!(
                %client_udp,
                punched,
                "data plane bound (punched=true → streaming to the client's observed source; \
                 false → no hole-punch seen, using the reported address)"
            );
            let mut session = Session::new(cfg, Box::new(transport))
                .map_err(|e| anyhow!("host session: {e:?}"))?;
            match source {
                M3Source::Synthetic => synthetic_stream(
                    &mut session,
                    frames,
                    &stop_stream,
                    &probe_rx,
                    &probe_result_tx,
                ),
                M3Source::Virtual => {
                    let compositor = compositor
                        .expect("the Virtual source resolves a compositor during the handshake");
                    virtual_stream(
                        session,
                        mode,
                        seconds,
                        stop_stream,
                        &reconfig_rx,
                        &keyframe_rx,
                        compositor,
                        bitrate_kbps,
                        probe_rx,
                        probe_result_tx,
                    )
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

/// Host-lifetime pointer/keyboard injector, shared across punktfunk/1 sessions.
///
/// The injector backend (libei/RemoteDesktop on KWin/GNOME, gamescope's EIS, wlr, uinput) owns
/// compositor resources and is `!Send`, so — unlike the audio capturer — it can't be handed
/// between per-session threads through a slot. Instead one host-lifetime thread *owns* it and
/// injects events forwarded over a clonable `Send` channel. Opening it ONCE means the privileged
/// RemoteDesktop-portal grant is established once and held for the whole run, eliminating the
/// per-session `CreateSession` churn that wedged KWin's EIS setup (rapid client reconnects raced
/// a prior session's portal teardown — "EIS setup timed out"). The service opens lazily on the
/// first event and reopens, after a backoff, if injection fails — so a transient portal hiccup,
/// or a gamescope EIS socket that respawns with its nested session, self-heals.
struct InjectorService {
    tx: std::sync::mpsc::Sender<InputEvent>,
}

impl InjectorService {
    fn start() -> InjectorService {
        let (tx, rx) = std::sync::mpsc::channel::<InputEvent>();
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-m3-injector".into())
            .spawn(move || injector_service_thread(rx))
        {
            tracing::error!(error = %e, "injector service thread spawn failed — pointer/keyboard input disabled");
        }
        InjectorService { tx }
    }

    /// A sender a session forwards its pointer/keyboard events to. Cloned per session; dropping a
    /// clone does NOT stop the service (the service holds the original sender for the host life).
    fn sender(&self) -> std::sync::mpsc::Sender<InputEvent> {
        self.tx.clone()
    }
}

/// Backoff between reopen attempts after the injector backend fails to open or its worker dies,
/// so a persistently-unavailable portal isn't hammered once per event.
const INJECTOR_REOPEN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

/// The host-lifetime injector worker: lazily open the pointer/keyboard backend, then inject every
/// forwarded event into it. Reopen (after [`INJECTOR_REOPEN_BACKOFF`]) on open failure or if the
/// backend's worker dies mid-stream. Exits only when every session sender *and* the service's own
/// sender have dropped (host shutdown), which drops the injector and closes its portal session.
fn injector_service_thread(rx: std::sync::mpsc::Receiver<InputEvent>) {
    let mut injector: Option<Box<dyn crate::inject::InputInjector>> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    for ev in rx {
        if injector.is_none() {
            // Open on the first event; after a failure wait out the backoff before retrying (a
            // few events drop during setup — acceptable, input is lossy).
            let ready = last_failed.is_none_or(|t| t.elapsed() >= INJECTOR_REOPEN_BACKOFF);
            if ready {
                let backend = crate::inject::default_backend();
                match crate::inject::open(backend) {
                    Ok(i) => {
                        tracing::info!(
                            ?backend,
                            "punktfunk/1 input injector ready (host-lifetime)"
                        );
                        injector = Some(i);
                        last_failed = None;
                    }
                    Err(e) => {
                        tracing::error!(error = %format!("{e:#}"), "pointer/keyboard injection unavailable — will retry");
                        last_failed = Some(std::time::Instant::now());
                    }
                }
            }
        }
        if let Some(inj) = injector.as_mut() {
            if let Err(e) = inj.inject(&ev) {
                // The backend's worker (portal session / EIS socket) died — drop it and reopen on
                // a later event (covers a gamescope EIS socket that respawns with its session).
                tracing::warn!(error = %format!("{e:#}"), "inject failed — reopening injector");
                injector = None;
                last_failed = Some(std::time::Instant::now());
            }
        }
    }
    tracing::debug!("injector service stopped (host shutting down)");
}

/// Mic is 48 kHz stereo — matches the Opus stereo decoder and the host→client audio layout.
const MIC_CHANNELS: u32 = 2;

/// Host-lifetime virtual microphone, shared across punktfunk/1 sessions (mirror of
/// [`InjectorService`]). One thread owns the PipeWire `Audio/Source` + an Opus decoder; sessions
/// forward the client's Opus mic frames over a clonable `Send` channel, the thread decodes and
/// feeds the source. Opened lazily on the first frame, the source node persists across sessions
/// (no per-session registration churn), and reopens after a backoff if the source/decoder fails.
struct MicService {
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl MicService {
    fn start() -> MicService {
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-m3-mic".into())
            .spawn(move || mic_service_thread(rx))
        {
            tracing::error!(error = %e, "mic service thread spawn failed — mic passthrough disabled");
        }
        MicService { tx }
    }

    /// A sender a session forwards the client's Opus mic frames to. Cloned per session; dropping a
    /// clone does NOT stop the service (it holds the original sender for the host life).
    fn sender(&self) -> std::sync::mpsc::Sender<Vec<u8>> {
        self.tx.clone()
    }
}

/// Stub — mic passthrough needs Linux (PipeWire source + libopus); non-Linux dev builds
/// drain and drop the frames (sessions still count the datagrams), same as when the
/// source fails to open.
#[cfg(not(target_os = "linux"))]
fn mic_service_thread(rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    tracing::warn!(
        "punktfunk/1 mic passthrough requires Linux (PipeWire + libopus) — frames dropped"
    );
    for _ in rx {}
}

/// The host-lifetime mic worker: lazily open the virtual mic + decoder, then Opus-decode each
/// forwarded frame and push the PCM into the source. Reopen (after [`INJECTOR_REOPEN_BACKOFF`])
/// on open failure or a decode error. Exits when every session sender and the service's own
/// sender drop (host shutdown), tearing the PipeWire source down.
#[cfg(target_os = "linux")]
fn mic_service_thread(rx: std::sync::mpsc::Receiver<Vec<u8>>) {
    let mut mic: Option<Box<dyn crate::audio::VirtualMic>> = None;
    let mut decoder: Option<opus::Decoder> = None;
    let mut last_failed: Option<std::time::Instant> = None;
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
            }
            Err(e) => {
                tracing::warn!(error = %e, "mic opus decode failed — reopening");
                mic = None;
                decoder = None;
                last_failed = Some(std::time::Instant::now());
            }
        }
    }
    tracing::debug!("mic service stopped (host shutting down)");
}

/// The session's virtual-gamepad backend. Default = uinput X-Box-360 pads
/// ([`GamepadManager`](crate::inject::gamepad::GamepadManager)); `PUNKTFUNK_GAMEPAD=dualsense`
/// switches to virtual DualSense pads (UHID + the kernel `hid-playstation` driver) so a game sees
/// a *real* DualSense — adaptive triggers, lightbar, touchpad, motion — and a game's feedback
/// flows back over the rich HID-output plane. Selected once per session (sessions run serially).
enum PadBackend {
    Xbox360(crate::inject::gamepad::GamepadManager),
    #[cfg(target_os = "linux")]
    DualSense(crate::inject::dualsense::DualSenseManager),
}

impl PadBackend {
    /// `kind` is the session's resolved backend (see [`resolve_gamepad`] — client preference,
    /// env var, X-Box 360, in that order). Defensive cfg guard: a non-Linux build can only
    /// ever construct the X-Box backend, whatever the resolution said.
    fn select(kind: GamepadPref) -> PadBackend {
        #[cfg(target_os = "linux")]
        if kind == GamepadPref::DualSense {
            tracing::info!("gamepad backend: virtual DualSense (UHID hid-playstation)");
            return PadBackend::DualSense(crate::inject::dualsense::DualSenseManager::new());
        }
        let _ = kind;
        PadBackend::Xbox360(crate::inject::gamepad::GamepadManager::new())
    }

    fn handle(&mut self, ev: &crate::gamestream::gamepad::GamepadEvent) {
        match self {
            PadBackend::Xbox360(m) => m.handle(ev),
            #[cfg(target_os = "linux")]
            PadBackend::DualSense(m) => m.handle(ev),
        }
    }

    /// Apply a rich client→host event (DualSense touchpad / motion). A no-op for the X-Box pad,
    /// which has no equivalent.
    fn apply_rich(&mut self, _rich: punktfunk_core::quic::RichInput) {
        #[cfg(target_os = "linux")]
        if let PadBackend::DualSense(m) = self {
            m.apply_rich(_rich);
        }
    }

    /// Service feedback every cycle. `rumble` carries motor force-feedback on the universal plane
    /// (both backends); `hidout` carries DualSense-only rich feedback (lightbar / player LEDs /
    /// adaptive triggers — DualSense backend only).
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
    let mut held_buttons: Vec<u32> = Vec::new();
    let mut held_keys: Vec<u32> = Vec::new();
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
                        InputKind::MouseButtonDown if !held_buttons.contains(&ev.code) => {
                            held_buttons.push(ev.code)
                        }
                        InputKind::MouseButtonUp => held_buttons.retain(|&c| c != ev.code),
                        InputKind::KeyDown if !held_keys.contains(&ev.code) => {
                            held_keys.push(ev.code)
                        }
                        InputKind::KeyUp => held_keys.retain(|&c| c != ev.code),
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

/// The audio thread: desktop capture → Opus (48 kHz stereo, 5 ms, CBR — same tuning as the
/// GameStream path) → `AUDIO_MAGIC` datagrams. QUIC already encrypts; no extra layer.
/// The capturer comes from (and returns to) the persistent slot — see [`AudioCapSlot`].
#[cfg(target_os = "linux")]
fn audio_thread(conn: quinn::Connection, stop: Arc<AtomicBool>, audio_cap: AudioCapSlot) {
    use crate::audio::{CHANNELS, SAMPLE_RATE};
    const FRAME_MS: usize = 5;
    const SAMPLES_PER_FRAME: usize = SAMPLE_RATE as usize * FRAME_MS / 1000; // 240

    let mut capturer = match audio_cap.lock().unwrap().take() {
        Some(mut c) => {
            c.drain(); // discard audio captured between sessions
            c
        }
        None => match crate::audio::open_audio_capture(CHANNELS as u32) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "punktfunk/1 audio unavailable — session continues without it");
                return;
            }
        },
    };
    let mut enc = match opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::LowDelay,
    ) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(error = %e, "opus encoder");
            *audio_cap.lock().unwrap() = Some(capturer);
            return;
        }
    };
    enc.set_bitrate(opus::Bitrate::Bits(128_000)).ok();
    enc.set_vbr(false).ok();

    let frame_len = SAMPLES_PER_FRAME * CHANNELS;
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut opus_buf = vec![0u8; 1500];
    let mut seq: u32 = 0;
    let mut capture_dead = false;
    tracing::info!("punktfunk/1 audio streaming (Opus 48 kHz stereo, 5 ms datagrams)");
    'session: while !stop.load(Ordering::SeqCst) {
        let chunk = match capturer.next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture ended");
                capture_dead = true;
                break;
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
    // Return the live capturer for the next session; a dead one is dropped so the next
    // session reopens fresh.
    if !capture_dead {
        *audio_cap.lock().unwrap() = Some(capturer);
    }
}

/// Stub — punktfunk/1 audio needs Linux (PipeWire capture + libopus); non-Linux dev builds
/// run sessions without it, same as when the capturer fails to open.
#[cfg(not(target_os = "linux"))]
fn audio_thread(_conn: quinn::Connection, _stop: Arc<AtomicBool>, _audio_cap: AudioCapSlot) {
    tracing::warn!(
        "punktfunk/1 audio requires Linux (PipeWire + libopus) — session continues without it"
    );
}

fn synthetic_stream(
    session: &mut Session,
    frames: u32,
    stop: &AtomicBool,
    probe_rx: &std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: &tokio::sync::mpsc::UnboundedSender<ProbeResult>,
) -> Result<()> {
    let interval = std::time::Duration::from_millis(1000 / 60);
    for idx in 0..frames {
        if stop.load(Ordering::SeqCst) {
            break;
        }
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
/// then the host's `PUNKTFUNK_GAMEPAD` env var (under a client `Auto`), then X-Box 360. The
/// DualSense backend needs Linux UHID — when unavailable any DualSense wish degrades to
/// X-Box 360 (never an error: a session without rich pads still streams).
fn pick_gamepad(pref: GamepadPref, env: Option<&str>, dualsense_available: bool) -> GamepadPref {
    let want = match pref {
        GamepadPref::Auto => env
            .and_then(GamepadPref::from_name)
            .unwrap_or(GamepadPref::Auto),
        explicit => explicit,
    };
    match want {
        GamepadPref::DualSense if dualsense_available => GamepadPref::DualSense,
        _ => GamepadPref::Xbox360,
    }
}

/// Resolve the client's gamepad-backend preference (the env/logging shell around
/// [`pick_gamepad`]). Always concrete — the `Welcome` reports what the session will drive.
fn resolve_gamepad(pref: GamepadPref) -> GamepadPref {
    let env = std::env::var("PUNKTFUNK_GAMEPAD").ok();
    let chosen = pick_gamepad(pref, env.as_deref(), cfg!(target_os = "linux"));
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
    if let Some(want) = crate::vdisplay::Compositor::from_pref(pref) {
        if available.contains(&want) {
            return Some(want);
        }
    }
    detected
}

/// Resolve the client's compositor preference to a concrete backend (the I/O shell around
/// [`pick_compositor`]): enumerate what's available, auto-detect the default, pick, and log
/// whether the explicit request was honored or fell back. Runs blocking probes — call off the
/// async reactor (`spawn_blocking`).
fn resolve_compositor(pref: CompositorPref) -> Result<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    let available = crate::vdisplay::available();
    let detected = crate::vdisplay::detect().ok();
    let chosen = pick_compositor(pref, &available, detected).ok_or_else(|| {
        anyhow!("no usable compositor (set PUNKTFUNK_COMPOSITOR or run inside a supported desktop)")
    })?;
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
        };
    }
    // kbps -> bytes/s (x1000/8).
    let bytes_per_sec = target_kbps as u64 * 125;
    // ~240 AUs/s for smooth pacing, each capped so one submit_frame stays a bounded burst (a large
    // AU fragments into many UDP shards via sendmmsg).
    let chunk = (bytes_per_sec / 240).clamp(1200, 256 * 1024) as usize;
    let filler = vec![0u8; chunk];
    // Host send-buffer drops over the burst — at high target rates this is where the native
    // single-send()-per-packet path first loses, so report it alongside what we offered.
    let send_dropped0 = session.stats().packets_send_dropped;
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_millis(duration_ms as u64);
    let mut bytes_sent = 0u64;
    let mut packets_sent = 0u32;
    while std::time::Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
        let allowed = (start.elapsed().as_secs_f64() * bytes_per_sec as f64) as u64;
        if bytes_sent < allowed {
            // A full send buffer drops on WouldBlock (UdpTransport returns Ok) — that loss is part
            // of what the probe measures, so count what we offered and keep going.
            let _ = session.submit_frame(&filler, now_ns(), FLAG_PROBE as u32);
            bytes_sent += chunk as u64;
            packets_sent += 1;
        } else {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
    }
    let actual_ms = start.elapsed().as_millis() as u32;
    let send_dropped = session.stats().packets_send_dropped - send_dropped0;
    tracing::info!(
        target_kbps,
        duration_ms = actual_ms,
        bytes_sent,
        packets_sent,
        send_dropped,
        "speed-test probe burst complete"
    );
    ProbeResult {
        bytes_sent,
        packets_sent,
        duration_ms: actual_ms,
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
    /// capture→encoded latency (µs), measured on the encode thread, carried for the perf histogram.
    encode_us: u32,
}

/// The dedicated send thread: it owns the whole [`Session`] (so no socket clone or shared stats are
/// needed) and does FEC+seal + microburst-paced send OFF the capture/encode thread, plus the
/// speed-test probe bursts (which also need the Session). Decoupling the paced send from encoding
/// lets the encode of frame N+1 overlap the transmit of frame N instead of waiting behind its tail.
/// Runs until the encode thread drops the frame channel (end of stream) or `stop` is set.
fn send_loop(
    mut session: Session,
    frame_rx: std::sync::mpsc::Receiver<FrameMsg>,
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    stop: Arc<AtomicBool>,
    perf: bool,
    burst_cap: usize,
) {
    let mut last_perf = std::time::Instant::now();
    let mut last_bytes = 0u64;
    let mut last_send_dropped = 0u64;
    let mut encode_us: Vec<u32> = Vec::new();
    let mut pace_us: Vec<u32> = Vec::new();
    let (mut paced_frames, mut immediate_frames) = (0u64, 0u64);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        // Probes run here (they need the Session); a burst pauses video — the encode thread blocks
        // on the full frame channel meanwhile, which is exactly the intended pause.
        service_probes(&mut session, &stop, &probe_rx, &probe_result_tx);
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
                    if perf {
                        encode_us.push(msg.encode_us);
                        pace_us.push(stat.spread_us);
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
        if perf && last_perf.elapsed() >= std::time::Duration::from_secs(2) {
            let s = session.stats();
            let secs = last_perf.elapsed().as_secs_f64();
            // Attempted (sealed) transmit rate; `send_dropped` is what didn't reach the wire.
            let tx_mbps = (s.bytes_sent - last_bytes) as f64 * 8.0 / secs / 1_000_000.0;
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
            last_perf = std::time::Instant::now();
            last_bytes = s.bytes_sent;
            last_send_dropped = s.packets_send_dropped;
            encode_us.clear();
            pace_us.clear();
            paced_frames = 0;
            immediate_frames = 0;
        }
    }
}

/// Real capture→encode→punktfunk/1: a native virtual output at the client's mode, NVENC AUs
/// stamped with the capture wall clock (the client derives per-frame pipeline latency).
///
/// `reconfig` delivers accepted mid-stream mode switches: the capture/encode pipeline is
/// rebuilt at the new mode (capturer drop tears down the PipeWire stream and, via its
/// keepalive, the virtual output) while the data-plane `session` continues untouched —
/// the rebuilt encoder opens with an IDR + in-band parameter sets. `probe_rx`/`probe_result_tx`
/// carry speed-test bursts (see [`service_probes`]).
#[allow(clippy::too_many_arguments)]
fn virtual_stream(
    session: Session,
    mode: punktfunk_core::Mode,
    seconds: u32,
    stop: Arc<AtomicBool>,
    reconfig: &std::sync::mpsc::Receiver<punktfunk_core::Mode>,
    keyframe: &std::sync::mpsc::Receiver<()>,
    compositor: crate::vdisplay::Compositor,
    bitrate_kbps: u32,
    probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
) -> Result<()> {
    tracing::info!(
        compositor = compositor.id(),
        ?mode,
        bitrate_kbps,
        "punktfunk/1 virtual display"
    );
    let mut vd = crate::vdisplay::open(compositor)?;
    let (mut capturer, mut enc, mut frame, mut interval) =
        build_pipeline_with_retry(&mut vd, mode, bitrate_kbps)?;

    let perf = std::env::var("PUNKTFUNK_PERF").is_ok();
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
                )
            }
        })
        .context("spawn send thread")?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds as u64);
    let mut next = std::time::Instant::now();
    let mut sent: u64 = 0;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
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
            match build_pipeline(&mut vd, new_mode, bitrate_kbps) {
                Ok(next_pipe) => {
                    (capturer, enc, frame, interval) = next_pipe;
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
            tracing::debug!("forcing keyframe (client decode recovery)");
            enc.request_keyframe();
        }
        if let Some(f) = capturer.try_latest().context("capture")? {
            frame = f;
        }
        let capture_ns = now_ns();
        enc.submit(&frame).context("encoder submit")?;
        // The deadline for this frame's packets (the next frame's due time); the send thread paces
        // up to here so a high-bitrate frame spreads over the interval instead of bursting.
        next += interval;
        let mut send_gone = false;
        while let Some(au) = enc.poll().context("encoder poll")? {
            let flags = if au.keyframe {
                (FLAG_PIC | FLAG_SOF) as u32
            } else {
                FLAG_PIC as u32
            };
            let encode_us = (now_ns().saturating_sub(capture_ns) / 1000) as u32;
            let msg = FrameMsg {
                data: au.data,
                capture_ns,
                flags,
                deadline: next,
                encode_us,
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
) -> Result<Pipeline> {
    const MAX_ATTEMPTS: u32 = 4;
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 1..=MAX_ATTEMPTS {
        match build_pipeline(vd, mode, bitrate_kbps) {
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
    ];
    let lower = chain.to_ascii_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
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
    let mut capturer =
        crate::capture::capture_virtual_output(vout).context("capture virtual output")?;
    capturer.set_active(true);
    let frame = capturer.next_frame().context("first frame")?;
    let enc = crate::encode::open_video(
        crate::encode::Codec::H265,
        frame.format,
        frame.width,
        frame.height,
        effective_hz,
        bitrate_kbps as u64 * 1000,
        frame.is_cuda(),
    )
    .context("open NVENC")?;
    let interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    Ok((capturer, enc, frame, interval))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // An explicit client choice wins over the env var.
        assert_eq!(pick_gamepad(DualSense, Some("xbox360"), true), DualSense);
        assert_eq!(pick_gamepad(Xbox360, Some("dualsense"), true), Xbox360);
        // Client Auto defers to the env var.
        assert_eq!(pick_gamepad(Auto, Some("dualsense"), true), DualSense);
        assert_eq!(pick_gamepad(Auto, Some("xbox360"), true), Xbox360);
        // Auto + no env (or an unparseable one) → X-Box 360.
        assert_eq!(pick_gamepad(Auto, None, true), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("bogus"), true), Xbox360);
        // DualSense degrades to X-Box 360 where the backend doesn't exist (non-Linux).
        assert_eq!(pick_gamepad(DualSense, None, false), Xbox360);
        assert_eq!(pick_gamepad(Auto, Some("dualsense"), false), Xbox360);
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
        let mut frame = unsafe { std::mem::zeroed() };
        while got < count {
            match unsafe {
                punktfunk_core::abi::punktfunk_connection_next_au(conn, &mut frame, 2000)
            } {
                PunktfunkStatus::Ok => {
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
            run(M3Options {
                port: 19777,
                source: M3Source::Synthetic,
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
        assert_eq!(
            unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) },
            PunktfunkStatus::Ok
        );
        assert_eq!((w, h, hz), (1280, 720, 60));

        // Mid-stream renegotiation: request a new mode, the host acks on the control
        // stream, and punktfunk_connection_mode reflects the switch.
        assert_eq!(
            unsafe {
                punktfunk_core::abi::punktfunk_connection_request_mode(conn, 1920, 1080, 144)
            },
            PunktfunkStatus::Ok
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            assert_eq!(
                unsafe { punktfunk_connection_mode(conn, &mut w, &mut h, &mut hz) },
                PunktfunkStatus::Ok
            );
            if (w, h, hz) == (1920, 1080, 144) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "mode switch not acked (still {w}x{h}@{hz})"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        unsafe { pull_verified(conn, 25) };

        let ev = punktfunk_core::input::InputEvent {
            kind: punktfunk_core::input::InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        assert_eq!(
            unsafe { punktfunk_connection_send_input(conn, &ev) },
            PunktfunkStatus::Ok
        );
        unsafe { punktfunk_connection_close(conn) };

        // Session 2 (same host process — the listener survived): pin the fingerprint.
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
        unsafe { pull_verified(conn2, 25) };
        unsafe { punktfunk_connection_close(conn2) };

        // Session 3: a wrong pin must be rejected by the handshake.
        let bad = [0xAAu8; 32];
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
        unsafe { pull_verified(conn4, 25) };
        unsafe { punktfunk_connection_close(conn4) };

        host.join().unwrap().unwrap();
    }

    fn test_paired_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("punktfunk-paired-test-{}.json", std::process::id()))
    }

    /// Delegated approval (§8b-1) end to end in-process: an identified-but-unpaired client's
    /// knock on a pairing-required host is held as a pending request (fingerprint-derived label —
    /// the connector sends no Hello name); approving it pairs the fingerprint, and the same
    /// identity then gets a session with no PIN ceremony.
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
                M3Options {
                    port: 19779,
                    source: M3Source::Synthetic,
                    seconds: 0,
                    frames: 25,
                    max_sessions: 2, // the knock + the post-approval session
                    max_concurrent: 1,
                    require_pairing: true,
                    allow_pairing: false,
                    pairing_pin: None,
                    paired_store: None, // unused: the shared `np` IS the store handle
                },
                np_host,
            ))
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        let timeout = std::time::Duration::from_secs(10);
        let (cert, key) = endpoint::generate_identity().unwrap();
        let mode = punktfunk_core::Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };

        // 1: the knock — an identified-but-unpaired connect is rejected, but lands in pending.
        assert!(
            NativeClient::connect(
                "127.0.0.1",
                19779,
                mode,
                CompositorPref::Auto,
                GamepadPref::Auto,
                0,
                None,
                Some((cert.clone(), key.clone())),
                timeout
            )
            .is_err(),
            "unpaired knock must still be rejected"
        );
        let expected_fp = fingerprint_hex(&endpoint::fingerprint_of_pem(&cert).unwrap());
        let pend = np.pending();
        assert_eq!(pend.len(), 1, "the knock must be held for approval");
        assert_eq!(pend[0].fingerprint, expected_fp);
        assert!(
            pend[0].name.starts_with("device "),
            "no Hello name → fingerprint-derived label, got {:?}",
            pend[0].name
        );

        // 2: approve (with an operator label) → the same identity now gets a session, no PIN.
        let approved = np
            .approve_pending(pend[0].id, Some("Approved Device"))
            .unwrap()
            .expect("pending id must approve");
        assert_eq!(approved.fingerprint, expected_fp);
        let client = NativeClient::connect(
            "127.0.0.1",
            19779,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
            None,
            Some((cert, key)),
            timeout,
        )
        .expect("approved identity gets a session");
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
            run(M3Options {
                port: 19778,
                source: M3Source::Synthetic,
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

        // 1: wrong PIN → Crypto, nothing stored.
        let err = NativeClient::pair("127.0.0.1", 19778, identity, "0000", "imposter", timeout)
            .unwrap_err();
        assert!(
            matches!(err, punktfunk_core::PunktfunkError::Crypto),
            "{err:?}"
        );

        // 2: anonymous session on a pairing-required host → rejected (connect fails).
        assert!(
            NativeClient::connect(
                "127.0.0.1",
                19778,
                mode,
                CompositorPref::Auto,
                GamepadPref::Auto,
                0,
                None,
                None,
                timeout
            )
            .is_err(),
            "anonymous session must be rejected"
        );

        // 3: correct PIN → paired, host fingerprint returned. Space past the pairing
        // cooldown that the wrong-PIN attempt above just triggered (a real retry is slower).
        std::thread::sleep(PAIRING_COOLDOWN + std::time::Duration::from_millis(200));
        let host_fp =
            NativeClient::pair("127.0.0.1", 19778, identity, "4321", "test-client", timeout)
                .expect("pairing with the right PIN");
        assert!(test_paired_path().exists());
        let _ = std::fs::remove_file(test_paired_path()); // already loaded; tidy /tmp

        // 4: the paired identity gets a session — pinned to the ceremony's fingerprint.
        let client = NativeClient::connect(
            "127.0.0.1",
            19778,
            mode,
            CompositorPref::Auto,
            GamepadPref::Auto,
            0,
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

        host.join().unwrap().unwrap();
    }
}
