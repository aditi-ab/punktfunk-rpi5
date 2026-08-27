//! The GameStream RTSP handshake (TCP 48010). Hand-rolled because GameStream's RTSP is
//! non-standard (streamid= targets, the literal `DEADBEEFCAFE` session, the X-SS-* headers)
//! and off-the-shelf RTSP crates assume standard semantics. Sequence Moonlight drives:
//! OPTIONS → DESCRIBE → SETUP(audio/video/control) → ANNOUNCE → PLAY. ANNOUNCE carries the
//! negotiated stream config; PLAY is where the media stages start (P1.3+).
//!
//! Runs on its own native thread (control-plane setup, not the per-frame hot path), one
//! thread per connection. DESCRIBE offers `SS_ENC_VIDEO` (per-shard AES-128-GCM video, WP7 —
//! on by default, never REQUIRED, `PUNKTFUNK_GS_ENCRYPT=0` opts out) and `SS_ENC_CONTROL_V2`
//! — which gives the ENet control stream a per-direction nonce and lets the client seal RTSP
//! itself (`PUNKTFUNK_GS_ENCRYPT=video` drops just this one). Audio is AES-CBC regardless and
//! `SS_ENC_AUDIO` is still not offered (its layout is absent from the wire reference). See
//! [`EncOffer`].
//!
//! A sealed connection is recognised, not negotiated: see [`ENCRYPTED_MESSAGE_TYPE_BIT`].

use super::audio;
use super::stream::{self, StreamConfig};
use super::{AppState, LaunchSession, AUDIO_PORT, CONTROL_PORT, RTSP_PORT, VIDEO_PORT};
use crate::encode::Codec;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// The RTSP listener is UNAUTHENTICATED (no TLS/pairing) and one-thread-per-connection, so bound
// every attacker-controllable dimension to deny a pre-auth slow-loris / memory-growth DoS: a hard
// cap on concurrent connections, a per-read timeout so a stalled peer can't pin a thread, a
// whole-request deadline so a dribbling one can't either, and size caps on the request headers +
// body (real GameStream RTSP messages are a few hundred bytes).
const MAX_RTSP_CONNS: usize = 8;
const RTSP_READ_TIMEOUT: Duration = Duration::from_secs(15);
// The per-read timeout bounds ONE read, not the request: a peer sending a byte just inside it
// resets the timeout forever and holds one of the eight slots for good. This bounds the whole
// message instead (security-review 2026-08-25) — a real client's request arrives in one segment.
const RTSP_REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const MAX_RTSP_HEADER: usize = 16 * 1024;
const MAX_RTSP_BODY: usize = 64 * 1024;
const MAX_RTSP_MSG: usize = 128 * 1024;

/// Live RTSP connection count, so a flood can't spawn unbounded threads. Decremented by [`ConnGuard`].
static RTSP_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Decrements [`RTSP_ACTIVE`] when a connection thread exits (normally OR on panic).
struct ConnGuard;
impl Drop for ConnGuard {
    fn drop(&mut self) {
        RTSP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Bind 48010 and accept RTSP connections on a dedicated thread.
pub fn spawn(state: Arc<AppState>) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", RTSP_PORT))
        .with_context(|| format!("bind RTSP {RTSP_PORT}"))?;
    tracing::info!(port = RTSP_PORT, "RTSP listening");
    std::thread::Builder::new()
        .name("punktfunk-rtsp".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        // Reserve a slot; over the cap, drop the connection (close) without a thread.
                        if RTSP_ACTIVE.fetch_add(1, Ordering::Relaxed) >= MAX_RTSP_CONNS {
                            RTSP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                            tracing::warn!("RTSP: too many concurrent connections — dropping");
                            continue; // `stream` drops → connection closed
                        }
                        // Construct the slot guard BEFORE spawning and move it into the worker, so the
                        // slot is released even if `thread::spawn` itself panics (OS thread-limit) —
                        // the closure (and its captured guard) is dropped during the unwind.
                        let guard = ConnGuard;
                        let st = state.clone();
                        std::thread::spawn(move || {
                            let _guard = guard; // releases the slot on exit/panic
                            if let Err(e) = handle_conn(stream, st) {
                                tracing::warn!(error = %format!("{e:#}"), "RTSP connection ended");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "RTSP accept failed"),
                }
            }
        })
        .context("spawn RTSP thread")?;
    Ok(())
}

struct Request {
    method: String,
    uri: String,
    cseq: String,
    head: String,
    body: String,
}

fn handle_conn(mut stream: TcpStream, state: Arc<AppState>) -> Result<()> {
    let peer = stream.peer_addr().ok();
    // A per-read timeout so a stalled peer can't pin this thread, plus the whole-request
    // deadline `read_message` enforces — a slow-loris defeats the first with the second absent.
    let _ = stream.set_read_timeout(Some(RTSP_READ_TIMEOUT));
    let deadline = Instant::now() + RTSP_REQUEST_DEADLINE;
    let mut buf: Vec<u8> = Vec::new();
    // Which framing this connection speaks is the client's choice, and the first byte settles it:
    // a sealed message opens with `typeAndLength`, whose MSB is [`ENCRYPTED_MESSAGE_TYPE_BIT`],
    // while a plaintext one opens with an ASCII method name. We answer in kind, so there is no
    // negotiation to get wrong and no state to keep.
    if !fill_at_least(&mut stream, &mut buf, 1, deadline)? {
        return Ok(()); // peer closed without sending anything
    }
    let sealed = buf[0] & 0x80 != 0;
    // Sealed RTSP is keyed by the same `/launch` rikey as everything else, so it cannot precede a
    // launch. Refusing here (rather than mis-parsing) keeps the failure legible.
    let key = state.launch.lock().unwrap().map(|s| s.gcm_key);
    let key = match (sealed, key) {
        (false, _) => None,
        (true, Some(k)) => Some(k),
        (true, None) => {
            anyhow::bail!("sealed RTSP message arrived with no launch session to key it")
        }
    };
    // GameStream RTSP is one request per TCP connection: moonlight-common-c reads the
    // response until EOF, so we answer one message and close the connection (which signals
    // the end of the response). Session state lives in `AppState`, not the connection.
    let req = match key {
        Some(k) => read_sealed_message(&mut stream, &mut buf, deadline, &k)?,
        None => read_message(&mut stream, &mut buf, deadline)?,
    };
    if let Some(req) = req {
        tracing::debug!(
            method = %req.method,
            cseq = %req.cseq,
            sealed,
            headers = %req.head.replace("\r\n", " | "),
            body = %req.body.replace("\r\n", " | "),
            "RTSP request"
        );
        let resp = handle_request(&req, &state, peer);
        let out = match key {
            Some(k) => seal_response(&k, resp.as_bytes()),
            None => resp.into_bytes(),
        };
        stream.write_all(&out).context("RTSP write")?;
        stream.flush().ok();
        // Close (FIN after the flushed response) so the client detects end-of-response.
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(())
}

/// Read one complete RTSP message (headers + any Content-Length body) from the stream,
/// buffering across reads and leaving any pipelined remainder in `buf`. Gives up at `deadline`
/// (the caller's whole-request budget), which the per-read timeout alone does not bound.
fn read_message(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
) -> Result<Option<Request>> {
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("RTSP request deadline exceeded");
        }
        if let Some(end) = find_subslice(buf, b"\r\n\r\n") {
            // Cap the header section even when the terminator IS present (a single oversized header
            // block that fits a `\r\n\r\n` would otherwise skip the no-terminator cap below).
            if end > MAX_RTSP_HEADER {
                anyhow::bail!("RTSP headers exceed limit");
            }
            let head = std::str::from_utf8(&buf[..end]).context("RTSP header utf8")?;
            let content_len = header_value(head, "content-length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            // Reject an absurd Content-Length before waiting to buffer it (allocation amplification).
            if content_len > MAX_RTSP_BODY {
                anyhow::bail!("RTSP Content-Length {content_len} exceeds limit");
            }
            let total = end + 4 + content_len;
            if buf.len() < total {
                // headers complete but body still arriving — read more
            } else {
                let head = head.to_string();
                let body = String::from_utf8_lossy(&buf[end + 4..total]).into_owned();
                buf.drain(..total);
                return Ok(Some(parse_request(&head, body)));
            }
        } else if buf.len() > MAX_RTSP_HEADER {
            // No header terminator within the cap — a slow-loris dribbling headers forever.
            anyhow::bail!("RTSP headers exceed limit");
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).context("RTSP read")?;
        if n == 0 {
            return Ok(None); // peer closed
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_RTSP_MSG {
            anyhow::bail!("RTSP message exceeds limit");
        }
    }
}

fn parse_request(head: &str, body: String) -> Request {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let uri = parts.next().unwrap_or("").to_string();
    let cseq = header_value(head, "cseq").unwrap_or("0").trim().to_string();
    Request {
        method,
        uri,
        cseq,
        head: head.to_string(),
        body,
    }
}

/// Authorize a state-changing RTSP verb against the pairing-gated `/launch` session. The RTSP/UDP
/// media plane is UNAUTHENTICATED, so `ANNOUNCE`/`PLAY`/`TEARDOWN` are honored only for the paired
/// client that completed `/launch` (which set `state.launch`), and — when the launching IP is known
/// — only from that same source IP. An unpaired RTSP peer can therefore neither start a stream,
/// overwrite a paired client's negotiated config, nor tear its active stream down
/// (security-review 2026-06-28 #4; extended from `PLAY`-only to `ANNOUNCE`/`TEARDOWN` 2026-07-17).
/// `nvhttp` gates `/launch` on a pinned client cert. Returns the launch session on success — `PLAY`
/// needs its keys/appid; the other verbs use it as a bool gate.
fn authorized_launch(state: &AppState, peer: Option<SocketAddr>) -> Option<LaunchSession> {
    let ls = (*state.launch.lock().unwrap())?;
    match (ls.peer_ip, peer.map(|p| p.ip())) {
        // Launching IP known on both sides but mismatched → not the owner.
        (Some(want), Some(got)) if want != got => None,
        // Owner IP matches, or the address couldn't be captured on one side → launch-present only.
        _ => Some(ls),
    }
}

// `&Arc<AppState>` (not `&AppState`): PLAY hands the media threads a `'static` session-lost
// callback, which needs an owned clone of the state.
fn handle_request(req: &Request, state: &Arc<AppState>, peer: Option<SocketAddr>) -> String {
    match req.method.as_str() {
        "OPTIONS" => response(
            &req.cseq,
            &[("Public", "OPTIONS DESCRIBE SETUP ANNOUNCE PLAY TEARDOWN")],
            None,
        ),
        "DESCRIBE" => response(
            &req.cseq,
            &[("Content-Type", "application/sdp")],
            Some(&describe_sdp()),
        ),
        "SETUP" => {
            // Gated like its siblings ANNOUNCE and PLAY, and for a sharper reason since the ping
            // payload became a secret: this response is where the payload is handed out, so an
            // ungated SETUP would let any peer that can reach 48010 simply *ask* for the value the
            // media planes verify — and then win the endpoint race it is meant to lose. Real
            // clients SETUP from the same address they launched from, so this costs them nothing.
            if authorized_launch(state, peer).is_none() {
                tracing::warn!(
                    ?peer,
                    "RTSP SETUP — refused: not the paired `/launch` owner"
                );
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            }
            let (port, extra_key) = match stream_type(&req.uri) {
                Some("audio") => (AUDIO_PORT, "X-SS-Ping-Payload"),
                Some("video") => (VIDEO_PORT, "X-SS-Ping-Payload"),
                Some("control") => (CONTROL_PORT, "X-SS-Connect-Data"),
                _ => return response_status("404 Not Found", &req.cseq, &[], None),
            };
            let transport = format!("server_port={port}");
            // This session's payload, minted at `/launch`. The client echoes it as its first
            // datagram on each media port, which is how those planes recognise it.
            let payload = hex::encode(state.av_ping_payload());
            response(
                &req.cseq,
                &[
                    ("Session", "DEADBEEFCAFE;timeout = 90"),
                    ("Transport", &transport),
                    (extra_key, &payload),
                ],
                None,
            )
        }
        "ANNOUNCE" => {
            // ANNOUNCE overwrites the session's negotiated stream/audio config. Gate it to the
            // launch owner so an unpaired RTSP peer can't scribble a paired client's config (or
            // race one in ahead of the owner's own ANNOUNCE).
            if authorized_launch(state, peer).is_none() {
                tracing::warn!(
                    ?peer,
                    "RTSP ANNOUNCE — refused: not the paired `/launch` owner"
                );
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            }
            let map = parse_announce(&req.body);
            match stream_config(&map) {
                Some(cfg) => {
                    tracing::info!(?cfg, "RTSP ANNOUNCE — negotiated stream config");
                    *state.stream.lock().unwrap() = Some(cfg);
                }
                None => tracing::warn!("RTSP ANNOUNCE — missing required video config keys"),
            }
            let ap = audio_params(&map);
            tracing::info!(?ap, "RTSP ANNOUNCE — negotiated audio params");
            *state.audio_params.lock().unwrap() = ap;
            response(&req.cseq, &[], None)
        }
        "PLAY" => {
            // A stream may start only for the paired client that owns the current `/launch`
            // (`authorized_launch` enforces launch-present + matching source IP). `nvhttp` gates
            // `/launch` on a pinned cert, so an unpaired RTSP peer can neither start a stream on an
            // idle host nor ride a paired client's active launch.
            let Some(ls) = authorized_launch(state, peer) else {
                tracing::warn!(?peer, "RTSP PLAY — refused: not the paired `/launch` owner");
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            };
            let cfg = *state.stream.lock().unwrap();
            // Client-unreachable teardown for the media threads: ends the WHOLE session (both
            // planes + launch state), so one plane detecting the dead client can't leave the
            // other streaming at it — or leave a stale launch to wedge the next connect.
            let on_lost: super::OnSessionLost = {
                let st = state.clone();
                Arc::new(move || {
                    st.end_session("client unreachable");
                })
            };
            match cfg {
                Some(cfg) if !state.streaming.swap(true, Ordering::SeqCst) => {
                    // Resolve the launched catalog entry (session recipe) for the stream.
                    let app = super::apps::by_id(ls.appid);
                    tracing::info!(app = ?app.as_ref().map(|a| &a.title), "RTSP PLAY — starting video stream");
                    stream::start(
                        cfg,
                        app,
                        state.streaming.clone(),
                        state.force_idr.clone(),
                        state.rfi_range.clone(),
                        state.loss_stats.clone(),
                        // The rikey reaches the video plane only when SS_ENC_VIDEO was
                        // negotiated (WP7) — no reason for it to travel otherwise.
                        cfg.encrypt_video.then_some(ls.gcm_key),
                        state.video_cap.clone(),
                        state.stats.clone(),
                        on_lost.clone(),
                        state.media_exited.clone(),
                        // The launched game's lifetime wiring. A game *exiting* is a deliberate end
                        // (the player finished) — the same distinction the native plane draws with
                        // its close code, and what the end-game-on-session-end policy keys off at
                        // teardown.
                        stream::GameLifetime {
                            quit: state.quit.clone(),
                            fingerprint: ls.owner_fp.map(hex::encode),
                            owner_ip: ls.peer_ip,
                            av_ping: state.av_ping_payload(),
                            on_game_exit: {
                                let st = state.clone();
                                Arc::new(move || {
                                    st.quit_session("game exited");
                                })
                            },
                        },
                    );
                }
                Some(_) => tracing::info!("RTSP PLAY — stream already running"),
                None => tracing::warn!("RTSP PLAY — no negotiated config (ANNOUNCE missing)"),
            }
            // Audio runs independently (Opus on UDP 48000, stereo or 5.1/7.1 multistream per
            // the ANNOUNCE); it needs the launch key for the AES-CBC payload encryption the
            // client expects.
            if !state.audio_streaming.swap(true, Ordering::SeqCst) {
                tracing::info!("RTSP PLAY — starting audio stream");
                audio::start(
                    state.audio_streaming.clone(),
                    ls.gcm_key,
                    ls.rikeyid,
                    *state.audio_params.lock().unwrap(),
                    state.audio_cap.clone(),
                    on_lost,
                    // Same owner-IP bind as the video plane: only the launching peer's pings are
                    // honored at the audio endpoint. security-review 2026-08-15 finding 1.
                    ls.peer_ip,
                    // ...and the same ping payload, which is what tells this client's first
                    // datagram apart from anything else arriving from that address.
                    state.av_ping_payload(),
                    state.media_exited.clone(),
                );
            }
            response(&req.cseq, &[("Session", "DEADBEEFCAFE;timeout = 90")], None)
        }
        "TEARDOWN" => {
            // Gate to the launch owner so an unpaired RTSP peer can't stop a paired client's media
            // threads (a trivial, repeatable stream DoS).
            if authorized_launch(state, peer).is_none() {
                tracing::warn!(
                    ?peer,
                    "RTSP TEARDOWN — refused: not the paired `/launch` owner"
                );
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            }
            // Signal both stream threads to stop.
            state.streaming.store(false, Ordering::SeqCst);
            state.audio_streaming.store(false, Ordering::SeqCst);
            response(&req.cseq, &[], None)
        }
        other => {
            tracing::warn!(method = other, "RTSP unsupported method");
            response_status("501 Not Implemented", &req.cseq, &[], None)
        }
    }
}

/// moonlight-common-c `LI_FF_PEN_TOUCH_EVENTS`: with this featureFlags bit set, Moonlight
/// clients send native `SS_PEN`/`SS_TOUCH` events (an iPad's Apple Pencil included) instead
/// of synthesizing mouse input client-side.
const SS_FF_PEN_TOUCH_EVENTS: u32 = 0x01;

/// `SS_ENC_VIDEO` — the per-shard AES-128-GCM video mode (moonlight-common-c
/// `Limelight-internal.h`). `SS_ENC_AUDIO` 0x04 is still not offered: the audio-GCM layout is
/// not in the sanctioned wire reference.
const SS_ENC_VIDEO: u32 = 0x02;

/// `SS_ENC_CONTROL_V2` — the V2 control-encryption scheme. What it buys is a **direction byte**
/// in the GCM nonce: `[10..12]` = `b"CC"` client→host, `b"HC"` host→client. The legacy scheme has
/// no such separation — its nonce is just the sender's own `seq` — so host messages and client
/// input share one (key, nonce) space and collide whenever their independent counters cross.
/// That is the single catastrophic AES-GCM failure, and this flag is its documented fix.
///
/// Enabling it also lets the client seal RTSP itself; see [`read_sealed_message`].
const SS_ENC_CONTROL_V2: u32 = 0x01;

/// How this host offers video encryption (WP7), from `PUNKTFUNK_GS_ENCRYPT`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncOffer {
    /// `0` — advertise nothing, the plaintext wire this plane sent before WP7. The escape
    /// hatch for a client that turns out to mis-negotiate, and for measuring the seal's cost.
    Off,
    /// **The default.** Advertise `SS_ENC_VIDEO` and `SS_ENC_CONTROL_V2` as SUPPORTED and let
    /// the client decide — what a Sunshine-class host does. Both verified on glass 2026-08-27
    /// (.173 → Moonlight on macOS, RTX 4090, 2560x1440@240 HEVC Main10 HDR): the client opts into
    /// both of its own accord even on a LAN, decodes the sealed video in hardware, and the host
    /// logs the control scheme it settled on as `V2 { marker: "CC" }` — per-direction nonces, so
    /// the legacy scheme's (key, nonce) reuse is retired for that session.
    Supported,
    /// `video` — video encryption only, dropping `SS_ENC_CONTROL_V2` back to the legacy control
    /// scheme. The granular way out: `Off` would also throw away video encryption, and this plane
    /// serves a spread of client builds of which exactly one has been tested against the V2 offer.
    VideoOnly,
    /// `require` — additionally list everything offered as REQUESTED, which forces any client
    /// that supports it to enable it. **The on-glass test lever**: with `Supported` alone a LAN
    /// session may negotiate plaintext and never exercise a single sealed packet, so a green test
    /// would prove nothing about the path it was meant to validate. Not a shipping mode — a
    /// client that cannot do the offered encryption has nowhere to go from here.
    Required,
}

/// Whether — and how hard — this host offers encryption. **On by default** since the
/// 2026-08-27 on-glass pass: a stock Moonlight client negotiates `SS_ENC_VIDEO` by itself (even
/// on a LAN, where it was not obvious it would) and decodes the sealed stream in hardware, and
/// FEC still recovers through the seal at 5 % injected wire loss — 27 s with zero keyframe
/// re-requests. `SS_ENC_CONTROL_V2` joined the default the same way, in a second on-glass pass
/// later that day. `PUNKTFUNK_GS_ENCRYPT=0` is the escape hatch back to the plaintext wire,
/// `video` keeps video encryption but drops the control offer, and `require` additionally
/// REQUESTS both (the test lever that forces the negotiation when a client would otherwise
/// decline).
fn gs_video_encryption_offer() -> EncOffer {
    static ON: std::sync::OnceLock<EncOffer> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match std::env::var("PUNKTFUNK_GS_ENCRYPT")
            .as_deref()
            .map(str::trim)
        {
            Ok("0") | Ok("off") | Ok("false") | Ok("no") => EncOffer::Off,
            Ok("video") | Ok("video-only") => EncOffer::VideoOnly,
            Ok("require") | Ok("required") => EncOffer::Required,
            // Unset, `1`, `supported`, or anything unrecognized: the default offer.
            _ => EncOffer::Supported,
        }
    })
}

/// The `(encryptionSupported, encryptionRequested)` masks an offer advertises — pure, so the
/// advertisement is unit-testable without touching the process-global env.
fn enc_flags(offer: EncOffer) -> (u32, u32) {
    // REQUESTED is only ever non-zero for the `require` test lever: requiring encryption would
    // refuse every client that cannot do it.
    match offer {
        EncOffer::Off => (0, 0),
        EncOffer::VideoOnly => (SS_ENC_VIDEO, 0),
        EncOffer::Supported => (SS_ENC_VIDEO | SS_ENC_CONTROL_V2, 0),
        EncOffer::Required => (
            SS_ENC_VIDEO | SS_ENC_CONTROL_V2,
            SS_ENC_VIDEO | SS_ENC_CONTROL_V2,
        ),
    }
}

/// `ENCRYPTED_MESSAGE_TYPE_BIT` — the MSB of `typeAndLength`, set on every sealed RTSP message.
///
/// It is also what makes the two framings **self-distinguishing**, which is why this host needs
/// no negotiation state for them: a plaintext RTSP message opens with an ASCII method name
/// (`OPTIONS`, `DESCRIBE`, `PLAY`), whose first byte is always below 0x80. So the client picks the
/// framing per connection and we answer in whatever we were asked in — no `corever` threshold to
/// guess (the sanctioned reference names that field but not its value), and no way for the two
/// sides to disagree.
const ENCRYPTED_MESSAGE_TYPE_BIT: u32 = 0x8000_0000;

/// `encrypted_rtsp_header_t`: `u32 typeAndLength | u32 sequenceNumber | u8 tag[16]`, big-endian,
/// followed by `length` bytes of ciphertext.
const ENC_RTSP_HEADER: usize = 24;

/// Host→client RTSP sequence numbers. PROCESS-global and monotonic, never reset — the same rule
/// WP7 established for the video counter, and for a sharper reason here: GameStream RTSP is one
/// message per TCP connection, so a per-connection counter would restart at 0 for every one of a
/// session's seven messages and reuse (key, nonce) six times over.
static RTSP_HOST_SEQ: AtomicU32 = AtomicU32::new(0);

/// The 12-byte GCM nonce for a sealed RTSP message (NIST SP800-38D 8.2.1): `sequenceNumber`
/// big-endian in `[0..4]`, `[10]` the originating direction (`b'C'` client, `b'H'` host), `[11]`
/// the channel (`b'R'` for RTSP). The direction byte is the entire point — it is what keeps each
/// side's counter in its own nonce space.
fn rtsp_nonce(seq: u32, direction: u8) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_be_bytes());
    iv[10] = direction;
    iv[11] = b'R';
    iv
}

/// Build one sealed frame: `encrypted_rtsp_header_t` followed by the ciphertext. Pure, and
/// direction-parameterised so a test can build the client's side of the wire too.
fn seal_frame(key: &[u8; 16], seq: u32, direction: u8, pt: &[u8]) -> Vec<u8> {
    let ct_tag = super::control::gcm_seal(key, &rtsp_nonce(seq, direction), pt, &[]);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    let mut wire = Vec::with_capacity(ENC_RTSP_HEADER + ct.len());
    wire.extend_from_slice(&(ENCRYPTED_MESSAGE_TYPE_BIT | ct.len() as u32).to_be_bytes());
    wire.extend_from_slice(&seq.to_be_bytes());
    wire.extend_from_slice(tag);
    wire.extend_from_slice(ct);
    wire
}

/// Frame + seal one RTSP response for a client that sealed its request.
fn seal_response(key: &[u8; 16], pt: &[u8]) -> Vec<u8> {
    seal_frame(key, RTSP_HOST_SEQ.fetch_add(1, Ordering::Relaxed), b'H', pt)
}

/// Open one COMPLETE sealed frame and parse the RTSP message inside it. Split out of
/// [`read_sealed_message`] so the half that can be wrong about the wire is reachable without a
/// socket. `frame` must be exactly the header plus its declared payload.
fn open_sealed_frame(key: &[u8; 16], frame: &[u8]) -> Result<Request> {
    anyhow::ensure!(
        frame.len() >= ENC_RTSP_HEADER,
        "sealed RTSP frame too short"
    );
    let seq = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
    // Wire order is tag-first; `aes-gcm` wants `ciphertext || tag`.
    let mut ct_tag = frame[ENC_RTSP_HEADER..].to_vec();
    ct_tag.extend_from_slice(&frame[8..ENC_RTSP_HEADER]);
    let pt = super::control::gcm_open(key, &rtsp_nonce(seq, b'C'), &ct_tag, &[])
        .context("sealed RTSP message failed to authenticate")?;
    // Inside the seal is an ordinary RTSP message. Its end is already known from the frame
    // length, so unlike the plaintext path there is no Content-Length to trust.
    let Some(end) = find_subslice(&pt, b"\r\n\r\n") else {
        anyhow::bail!("sealed RTSP message has no header terminator");
    };
    if end > MAX_RTSP_HEADER {
        anyhow::bail!("RTSP headers exceed limit");
    }
    let head = std::str::from_utf8(&pt[..end]).context("RTSP header utf8")?;
    let body = String::from_utf8_lossy(&pt[end + 4..]).into_owned();
    Ok(parse_request(head, body))
}

/// Read until `buf` holds at least `want` bytes. `Ok(false)` = the peer closed first.
fn fill_at_least(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    want: usize,
    deadline: Instant,
) -> Result<bool> {
    while buf.len() < want {
        if Instant::now() >= deadline {
            anyhow::bail!("RTSP request deadline exceeded");
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).context("RTSP read")?;
        if n == 0 {
            return Ok(false);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_RTSP_MSG {
            anyhow::bail!("RTSP message exceeds limit");
        }
    }
    Ok(true)
}

/// Read one **sealed** RTSP message and return the request inside it. Decrypt input is
/// `tag || ciphertext` on the wire, which is reassembled into the `ciphertext || tag` order
/// `aes-gcm` expects.
fn read_sealed_message(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    key: &[u8; 16],
) -> Result<Option<Request>> {
    if !fill_at_least(stream, buf, ENC_RTSP_HEADER, deadline)? {
        return Ok(None);
    }
    let type_and_length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let len = (type_and_length & !ENCRYPTED_MESSAGE_TYPE_BIT) as usize;
    // This length is attacker-controlled and reaches us before a single byte has authenticated,
    // so it is bounded by the same budget the plaintext path uses rather than trusted enough to
    // reserve against. (`fill_at_least` also caps, but refusing here avoids the read entirely.)
    if len > MAX_RTSP_MSG {
        anyhow::bail!("sealed RTSP payload of {len} bytes exceeds limit");
    }
    if !fill_at_least(stream, buf, ENC_RTSP_HEADER + len, deadline)? {
        anyhow::bail!("sealed RTSP message truncated");
    }
    let req = open_sealed_frame(key, &buf[..ENC_RTSP_HEADER + len])?;
    buf.drain(..ENC_RTSP_HEADER + len);
    Ok(Some(req))
}

/// Host capability SDP returned by DESCRIBE. Advertises HEVC + AV1, the surround configs, and
/// — when offered — `SS_ENC_VIDEO` as SUPPORTED but never REQUESTED: a stock client that
/// wants plaintext (or predates the negotiation) must keep working exactly as before.
fn describe_sdp() -> String {
    // Pen/touch events are advertised only where we can actually inject them (Linux with
    // uinput — the same gate as HOST_CAP_PEN; design/pen-tablet-input.md §4). Elsewhere the
    // flag stays 0 so Moonlight keeps its client-side mouse emulation for touch, exactly as
    // today. PUNKTFUNK_PEN=0 clears it (the operator kill-switch inside pen_supported).
    let feature_flags: u32 = if crate::inject::pen_supported() {
        SS_FF_PEN_TOUCH_EVENTS
    } else {
        0
    };
    // Line-oriented a=key:value, matching what moonlight-common-c scans for.
    let (supported, requested) = enc_flags(gs_video_encryption_offer());
    let mut lines: Vec<String> = vec![
        format!("a=x-ss-general.featureFlags:{feature_flags}"),
        format!("a=x-ss-general.encryptionSupported:{supported}"),
        // REQUESTED stays 0 in every shipping mode: requiring encryption would refuse every
        // client that doesn't do it. Only the `require` test lever sets it.
        format!("a=x-ss-general.encryptionRequested:{requested}"),
        "sprop-parameter-sets=AAAAAU".into(), // HEVC capability indicator
        "a=rtpmap:98 AV1/90000".into(),       // AV1 capability indicator
    ];
    // Opus configs, one line per layout (Sunshine's order): the client scans for the FIRST
    // `surround-params=<channelCount>` match as its normal-quality decoder config and a
    // SECOND match as the high-quality config (which is also what makes it offer HQ at all),
    // so normal must precede HQ per channel count. Stereo lines are emitted for parity with
    // Sunshine but ignored by 2-channel clients (they hardcode 21101). See
    // `audio::surround_params` for the mapping pre-rotation the normal-quality lines carry.
    for (layout, hq) in [
        (&audio::LAYOUT_STEREO, false),
        (&audio::LAYOUT_STEREO, true),
        (&audio::LAYOUT_51, false),
        (&audio::LAYOUT_51_HQ, true),
        (&audio::LAYOUT_71, false),
        (&audio::LAYOUT_71_HQ, true),
    ] {
        lines.push(format!(
            "a=fmtp:97 surround-params={}",
            audio::surround_params(layout, hq)
        ));
    }
    lines.push(String::new());
    lines.join("\r\n")
}

/// Parse an ANNOUNCE SDP body's `a=key:value` lines into a map.
fn parse_announce(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("a=") {
            if let Some((k, v)) = rest.split_once(':') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

/// Map the negotiated ANNOUNCE keys to a [`StreamConfig`] (resolution/packetSize required).
fn stream_config(map: &HashMap<String, String>) -> Option<StreamConfig> {
    let parse_u = |k: &str| map.get(k).and_then(|s| s.trim().parse::<u32>().ok());
    let width = parse_u("x-nv-video[0].clientViewportWd")?;
    let height = parse_u("x-nv-video[0].clientViewportHt")?;
    // packetSize is attacker-controlled and PRE-AUTH (the RTSP listener is unauthenticated). It sets
    // the per-shard payload (`packet_size - 16`); a tiny value underflows / div-by-zeros the video
    // thread, an absurd one amplifies per-shard allocation. Reject anything outside a sane range
    // (real Moonlight uses ~1024) so a malformed ANNOUNCE fails here instead of panicking the stream.
    const PACKET_SIZE_MIN: usize = 64;
    const PACKET_SIZE_MAX: usize = 2048;
    let packet_size = parse_u("x-nv-video[0].packetSize")? as usize;
    if !(PACKET_SIZE_MIN..=PACKET_SIZE_MAX).contains(&packet_size) {
        tracing::warn!(
            packet_size,
            "RTSP ANNOUNCE: out-of-range packetSize — rejecting"
        );
        return None;
    }
    let fps = parse_u("x-nv-video[0].maxFPS")
        .filter(|&f| f > 0)
        .unwrap_or(60);
    // Bitrate: Moonlight caps the legacy `x-nv-vqos[0].bw.*` fields at 100 Mbps for old-GFE
    // compatibility and carries the user's REAL (uncapped) configured bitrate in the moonlight-specific
    // `x-ml-video.configuredBitrateKbps`. Read that first — exactly like Sunshine — so a 500 Mbps client
    // setting isn't silently floored to 100. Fall back to the legacy max for clients that don't send it,
    // then a conservative default; clamp to a sane ceiling (the RTSP ANNOUNCE is attacker-controlled).
    const MAX_BITRATE_KBPS: u32 = 1_000_000; // 1 Gbps — well above Moonlight's 500 Mbps slider
    let bitrate_kbps = parse_u("x-ml-video.configuredBitrateKbps")
        .filter(|&b| b > 0)
        .or_else(|| parse_u("x-nv-vqos[0].bw.maximumBitrateKbps").filter(|&b| b > 0))
        .unwrap_or(20_000)
        .min(MAX_BITRATE_KBPS);
    // Client codec choice (moonlight-common-c SdpGenerator.c): 0=H264, 1=HEVC, 2=AV1.
    let codec = match map.get("x-nv-vqos[0].bitStreamFormat").map(|s| s.trim()) {
        Some("1") => Codec::H265,
        Some("2") => Codec::Av1,
        _ => Codec::H264,
    };
    // 10-bit/HDR request (Moonlight sets `dynamicRangeMode != 0` only when it both saw our Main10 SCM
    // bit AND the user enabled HDR). Honor it only when the host can actually deliver Main10
    // (`host_hdr_capable` — Windows IDD-push, or the Linux GNOME 50+ portal mirror). On Windows,
    // when honored, the video path proactively enables advanced color on the virtual display so a
    // PQ stream flows even from an SDR desktop. On Linux the portal can only deliver PQ while the
    // MIRRORED monitor is in HDR mode, so additionally probe the live colour mode here (one D-Bus
    // round-trip, sync RTSP thread) — an SDR desktop honestly degrades to 8-bit SDR up front
    // instead of running the capture negotiation into its timeout. A request we can't honor
    // degrades to 8-bit SDR (and a Windows desktop that is ALREADY HDR still streams PQ
    // regardless, since the IDD-push capturer follows the display).
    let hdr_requested = parse_u("x-nv-video[0].dynamicRangeMode").unwrap_or(0) != 0;
    // `mut` is load-bearing on Linux only — the GNOME colour-mode probe below clears it. Scope the
    // allow to non-Linux so `unused_mut` still fires here if that probe ever goes away.
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut hdr = hdr_requested && crate::gamestream::host_hdr_capable();
    if hdr_requested && !hdr {
        tracing::warn!(
            "client requested HDR (dynamicRangeMode != 0) but host is not HDR-capable — streaming 8-bit SDR"
        );
    }
    // …and this SESSION's codec must be one of the 10-bit-capable ones. `host_hdr_capable` is
    // host-wide (any codec), and serverinfo advertises the 10-bit bit per codec, so a client
    // that picked the other one has to degrade here rather than be handed a PQ label over an
    // 8-bit stream. H.264 always lands here — there is no 10-bit H.264 encode anywhere.
    if hdr && !crate::encode::can_encode_10bit(codec) {
        tracing::warn!(
            ?codec,
            "client requested HDR but this host cannot encode 10-bit with the codec it \
             negotiated — streaming 8-bit SDR (pick the other codec client-side for HDR)"
        );
        hdr = false;
    }
    // SOURCE-AWARE: the live colour-mode probe belongs to the portal MONITOR mirror, whose HDR
    // depends on a monitor being in BT.2100 right now. A gamescope virtual-output session has no
    // monitor at all — its HDR is a static fact about the binary we spawn, already settled by
    // `host_hdr_capable` — so running the probe there would hard-refuse every gamescope HDR
    // session on a headless box (there is no monitor to be in HDR mode).
    #[cfg(target_os = "linux")]
    let portal_source = pf_host_config::config().video_source.as_deref() == Some("portal");
    #[cfg(target_os = "linux")]
    if hdr && portal_source && !pf_capture::gnome_hdr_monitor_active() {
        tracing::warn!(
            "client requested HDR but no monitor is in BT.2100 (HDR) colour mode — enable HDR in \
             GNOME Settings → Displays (GNOME 50+) to stream it; streaming 8-bit SDR"
        );
        hdr = false;
    }
    // Second half of the same live gate: the process-wide HDR-capture latch. The colour-mode probe
    // above only says the monitor is in BT.2100 RIGHT NOW — it says nothing about whether the
    // portal will actually hand us the 10-bit PQ formats. Once a `want_hdr` negotiation has failed
    // (the monitor left HDR mode between probe and negotiation, NVIDIA EGL not listing LINEAR for
    // XR30, a pre-50 Mutter), `pf_capture::open_portal_monitor` permanently drops the HDR OFFER and
    // captures SDR. Without this check we'd keep telling the client HDR while capturing and
    // encoding SDR: it renders an SDR picture through PQ — washed out, wrong gamut, no error
    // anywhere — and because the latch is sticky until host restart, every reconnect repeats it.
    // Consulted here rather than folded into `host_hdr_capable` for the same reason as the probe
    // above: that fn is the STATIC serverinfo capability, and this is a live per-session fact.
    // The latch is PER SOURCE, so consult the one belonging to the source this session drives —
    // a wedged monitor mirror must not disable a gamescope session's HDR, or vice versa.
    #[cfg(target_os = "linux")]
    let hdr_source = if portal_source {
        pf_capture::HdrSource::PortalMonitor
    } else {
        pf_capture::HdrSource::VirtualOutput
    };
    #[cfg(target_os = "linux")]
    if hdr && pf_capture::hdr_capture_failed(hdr_source) {
        tracing::warn!(
            ?hdr_source,
            "client requested HDR and the host is HDR-capable, but an earlier HDR capture \
             negotiation on this source failed — the capturer offers SDR only for the rest of the \
             process lifetime, so streaming 8-bit SDR (restart the host to retry HDR)"
        );
        hdr = false;
    }
    // The client's requested CSC (moonlight-common-c SdpGenerator.c: `encoderCscMode =
    // (colorspace << 1) | fullRange` — colorspace 0=Rec601, 1=Rec709, 2=Rec2020). Moonlight
    // renderers configure their YUV→RGB from this REQUESTED value (not the bitstream VUI), so a
    // host that encodes something else shifts the client's colours. INSTRUMENTATION ONLY for
    // now: we always encode BT.709 limited for SDR (the IDD VideoConverter / VUI-driven NVENC)
    // and BT.2020 PQ for HDR — log what clients actually ask for so honoring `encoderCscMode`
    // can be scoped from field data rather than guessed. (Absent on very old clients.)
    if let Some(csc) = parse_u("x-nv-video[0].encoderCscMode") {
        let (space, range) = (
            match csc >> 1 {
                0 => "Rec601",
                1 => "Rec709",
                2 => "Rec2020",
                _ => "unknown",
            },
            if csc & 1 != 0 { "full" } else { "limited" },
        );
        let ours = if hdr {
            "Rec2020 limited (PQ)"
        } else {
            "Rec709 limited"
        };
        let matches_ours = (hdr && csc >> 1 == 2 || !hdr && csc >> 1 == 1) && csc & 1 == 0;
        if matches_ours {
            tracing::debug!(
                csc,
                space,
                range,
                "GameStream client requested CSC — matches ours"
            );
        } else {
            tracing::warn!(
                csc,
                requested = format!("{space} {range}"),
                encoding = ours,
                "GameStream client requested a CSC we don't encode — Moonlight renders by its \
                 REQUEST, so its colours will be shifted (honoring encoderCscMode is a known \
                 follow-up; report this log line)"
            );
        }
    }
    // Parity floor the client asks for (protects small frames); clamp to a sane max.
    let min_fec = parse_u("x-nv-vqos[0].fec.minRequiredFecPackets")
        .unwrap_or(2)
        .min(16) as u8;
    // The client's requested per-frame slice count (moonlight-common-c SdpGenerator.c:
    // `videoEncoderSlicesPerFrame`) — 1 for every HARDWARE decoder, 4 only for software
    // decoders (slice-threading). Honor it as the encoder's slicing ceiling: GFE/Sunshine
    // encode what was asked, and hardware TV decoders (Amlogic — Chromecast with Google TV)
    // wedge the whole DEVICE on multi-slice AUs they never requested — the 0.17.0 field
    // regression, where the Linux direct-NVENC 4-slice default (§7 LN1) ignored this key and
    // froze + watchdog-rebooted CCwGTV clients on the first frame. Absent or out-of-range
    // (attacker-controlled pre-auth input) ⇒ 1, the universally-safe single-slice shape.
    let slices = parse_u("x-nv-video[0].videoEncoderSlicesPerFrame")
        .filter(|n| (1..=32).contains(n))
        .unwrap_or(1);
    // The encryption bitmask the client CHOSE, echoed back from what DESCRIBE advertised
    // (WP7). Honor a bit only when we actually offered it — a client cannot turn on a mode the
    // host never advertised, whatever it echoes — so the echo is masked by the offer rather than
    // merely checked against `Off`.
    let (offered, _) = enc_flags(gs_video_encryption_offer());
    let enabled = parse_u("x-ss-general.encryptionEnabled").unwrap_or(0) & offered;
    let encrypt_video = enabled & SS_ENC_VIDEO != 0;
    if encrypt_video {
        tracing::info!("RTSP ANNOUNCE: client enabled SS_ENC_VIDEO — sealing every video shard");
    }
    // Nothing to store for the control bit: both planes it governs recognise the sealed form
    // from the wire itself — the ENet stream detects the V2 nonce on the first packet that
    // authenticates, and RTSP detects the sealed framing from its leading MSB. Worth saying out
    // loud all the same, because it is what retires the legacy scheme's (key, nonce) reuse for
    // this session.
    if enabled & SS_ENC_CONTROL_V2 != 0 {
        tracing::info!(
            "RTSP ANNOUNCE: client enabled SS_ENC_CONTROL_V2 — control nonces are per-direction"
        );
    }
    Some(StreamConfig {
        width,
        height,
        fps,
        packet_size,
        bitrate_kbps,
        codec,
        min_fec,
        hdr,
        slices,
        encrypt_video,
    })
}

/// Map the negotiated ANNOUNCE keys to the session [`audio::AudioParams`]. Attribute names
/// per moonlight-common-c `SdpGenerator.c` (verified 2026-06-10): the client always emits
/// `x-nv-audio.surround.numChannels`/`channelMask` and `x-nv-aqos.packetDuration`;
/// `x-nv-audio.surround.AudioQuality` is 1 only when it saw our second surround-params line
/// and opted into high-quality surround. Unknown channel counts fall back to stereo.
fn audio_params(map: &HashMap<String, String>) -> audio::AudioParams {
    let parse_u = |k: &str| map.get(k).and_then(|s| s.trim().parse::<u32>().ok());
    let requested = parse_u("x-nv-audio.surround.numChannels").unwrap_or(2);
    let channels = match requested {
        2 | 6 | 8 => requested as u8,
        other => {
            tracing::warn!(channels = other, "unsupported channel count — using stereo");
            2
        }
    };
    let high_quality = parse_u("x-nv-audio.surround.AudioQuality") == Some(1);
    // Moonlight uses 5 ms (default) or 10 ms (slow decoder / low-bitrate links). Snap to
    // those two — an in-between value like 7 isn't a legal Opus frame size and would make
    // every encode fail; clamping (not snapping) would let it through.
    let packet_duration_ms = match parse_u("x-nv-aqos.packetDuration") {
        Some(d) if d >= 10 => 10,
        _ => 5,
    };
    audio::AudioParams {
        channels,
        high_quality,
        packet_duration_ms,
    }
}

/// Extract the stream type from a SETUP URI like `…/streamid=video/0/0`.
fn stream_type(uri: &str) -> Option<&str> {
    let after = uri.split("streamid=").nth(1)?;
    let token = after.split('/').next()?;
    match token {
        "audio" | "video" | "control" => Some(token),
        _ => None,
    }
}

fn response(cseq: &str, headers: &[(&str, &str)], body: Option<&str>) -> String {
    response_status("200 OK", cseq, headers, body)
}

fn response_status(
    status: &str,
    cseq: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let body = body.unwrap_or("");
    let mut out = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(body);
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn header_value<'a>(head: &'a str, key_lower: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(key_lower)).then(|| v.trim_start())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announce(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut body = String::from(
            "v=0\r\n\
             a=x-nv-video[0].clientViewportWd:1920\r\n\
             a=x-nv-video[0].clientViewportHt:1080\r\n\
             a=x-nv-video[0].packetSize:1392\r\n\
             a=x-nv-video[0].maxFPS:120\r\n\
             a=x-nv-vqos[0].bw.maximumBitrateKbps:40000\r\n",
        );
        for (k, v) in extra {
            body.push_str(&format!("a={k}:{v}\r\n"));
        }
        parse_announce(&body)
    }

    /// The listener is unauthenticated, so a request that never finishes must not hold one of the
    /// eight connection slots: `read_message` gives up at the caller's deadline instead of resetting
    /// its per-read timeout forever. An already-passed deadline is the deterministic stand-in for a
    /// peer dribbling bytes — it bails without waiting on a socket that will never send.
    #[test]
    fn read_message_gives_up_at_the_request_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect"); // stays open, sends nothing
        let (mut server, _) = listener.accept().expect("accept");
        let mut buf: Vec<u8> = Vec::new();
        // `Request` isn't `Debug`, so match rather than `expect_err`.
        let err = match read_message(&mut server, &mut buf, Instant::now()) {
            Err(e) => e,
            Ok(_) => panic!("an elapsed deadline must end the request"),
        };
        assert!(
            format!("{err:#}").contains("deadline"),
            "unexpected error: {err:#}"
        );
        drop(client);
    }

    /// `x-nv-vqos[0].bitStreamFormat` → codec (0=H264, 1=HEVC, 2=AV1; missing = H264).
    #[test]
    fn announce_codec_selection() {
        for (fmt, codec) in [
            (Some("0"), Codec::H264),
            (Some("1"), Codec::H265),
            (Some("2"), Codec::Av1),
            (None, Codec::H264),
        ] {
            let map = match fmt {
                Some(f) => announce(&[("x-nv-vqos[0].bitStreamFormat", f)]),
                None => announce(&[]),
            };
            let cfg = stream_config(&map).expect("required keys present");
            assert_eq!(cfg.codec, codec, "bitStreamFormat {fmt:?}");
            assert_eq!((cfg.width, cfg.height, cfg.fps), (1920, 1080, 120));
            assert_eq!(cfg.bitrate_kbps, 40_000);
        }
    }

    /// Bitrate precedence: the moonlight-specific `x-ml-video.configuredBitrateKbps` (the user's real,
    /// uncapped setting) wins over the legacy `x-nv-vqos[0].bw.maximumBitrateKbps` (which Moonlight floors
    /// at 100 Mbps for old-GFE compat). Without this a 500 Mbps client streamed at 100.
    #[test]
    fn announce_prefers_configured_bitrate() {
        // Real Moonlight shape: legacy max floored at 100 Mbps, configured carrying the true 500 Mbps.
        let map = announce(&[
            ("x-nv-vqos[0].bw.maximumBitrateKbps", "100000"),
            ("x-ml-video.configuredBitrateKbps", "500000"),
        ]);
        assert_eq!(stream_config(&map).unwrap().bitrate_kbps, 500_000);
        // No configured field (older client) → fall back to the legacy max (the base announce's 40 Mbps).
        assert_eq!(stream_config(&announce(&[])).unwrap().bitrate_kbps, 40_000);
        // A zero configured value is ignored (falls back), and an absurd value is clamped to the ceiling.
        let zero = announce(&[("x-ml-video.configuredBitrateKbps", "0")]);
        assert_eq!(stream_config(&zero).unwrap().bitrate_kbps, 40_000);
        let huge = announce(&[("x-ml-video.configuredBitrateKbps", "9000000")]);
        assert_eq!(stream_config(&huge).unwrap().bitrate_kbps, 1_000_000);
    }

    /// Missing required video keys → no config (the PLAY handler then refuses to stream).
    #[test]
    fn announce_missing_required_keys() {
        let mut map = announce(&[]);
        map.remove("x-nv-video[0].packetSize");
        assert!(stream_config(&map).is_none());
    }

    /// packetSize is attacker-controlled AND pre-auth (the RTSP listener is unauthenticated), so an
    /// out-of-range value must be rejected here rather than panic the video thread (≤16 → div-by-zero
    /// / underflow; absurd → allocation amplification). Sane values (real Moonlight ~1024) pass.
    #[test]
    fn announce_rejects_out_of_range_packet_size() {
        for bad in ["0", "16", "63", "4096", "999999"] {
            let map = announce(&[("x-nv-video[0].packetSize", bad)]);
            assert!(
                stream_config(&map).is_none(),
                "out-of-range packetSize {bad} must be rejected"
            );
        }
        for ok in ["64", "1024", "1392", "2048"] {
            let map = announce(&[("x-nv-video[0].packetSize", ok)]);
            assert!(
                stream_config(&map).is_some(),
                "in-range packetSize {ok} must be accepted"
            );
        }
    }

    /// `videoEncoderSlicesPerFrame` is honored as the encoder's slicing ceiling: Moonlight sends
    /// 1 for every hardware decoder (Amlogic TV SoCs wedge on multi-slice AUs — the 0.17.0
    /// Chromecast regression) and 4 for software decoders. Absent or out-of-range (pre-auth,
    /// attacker-controlled) must fall back to the universally-safe 1, never reject the session.
    #[test]
    fn announce_slices_per_frame() {
        // Absent (very old client) → single-slice.
        assert_eq!(stream_config(&announce(&[])).unwrap().slices, 1);
        // The two real Moonlight values pass through verbatim.
        for want in ["1", "4"] {
            let map = announce(&[("x-nv-video[0].videoEncoderSlicesPerFrame", want)]);
            assert_eq!(
                stream_config(&map).unwrap().slices,
                want.parse::<u32>().unwrap()
            );
        }
        // Garbage / out-of-range degrades to 1 (still streams — this key must never kill a session).
        for bad in ["0", "33", "999999", "-1", "x"] {
            let map = announce(&[("x-nv-video[0].videoEncoderSlicesPerFrame", bad)]);
            let cfg = stream_config(&map).expect("session must still negotiate");
            assert_eq!(cfg.slices, 1, "slicesPerFrame {bad} must degrade to 1");
        }
    }

    /// Audio negotiation: numChannels/AudioQuality/packetDuration, with Moonlight defaults.
    #[test]
    fn announce_audio_params() {
        // Stereo defaults when the attributes are absent (and the legacy path).
        assert_eq!(audio_params(&announce(&[])), audio::AudioParams::default());
        // 5.1 normal quality at 5 ms.
        let ap = audio_params(&announce(&[
            ("x-nv-audio.surround.numChannels", "6"),
            ("x-nv-audio.surround.channelMask", "63"),
            ("x-nv-audio.surround.AudioQuality", "0"),
            ("x-nv-aqos.packetDuration", "5"),
        ]));
        assert_eq!(
            (ap.channels, ap.high_quality, ap.packet_duration_ms),
            (6, false, 5)
        );
        // 7.1 high quality; 10 ms duration honored.
        let ap = audio_params(&announce(&[
            ("x-nv-audio.surround.numChannels", "8"),
            ("x-nv-audio.surround.AudioQuality", "1"),
            ("x-nv-aqos.packetDuration", "10"),
        ]));
        assert_eq!(
            (ap.channels, ap.high_quality, ap.packet_duration_ms),
            (8, true, 10)
        );
        // Bogus channel count falls back to stereo.
        let ap = audio_params(&announce(&[("x-nv-audio.surround.numChannels", "4")]));
        assert_eq!(ap.channels, 2);
    }

    /// The advertisement ladder: `Off` offers nothing (the plaintext wire this plane shipped
    /// before WP7); `video` offers video encryption alone; `Supported` — the default — adds
    /// `SS_ENC_CONTROL_V2`; and only the `require` test lever ever sets REQUESTED, which is the
    /// whole reason it exists (a client that is merely *allowed* to encrypt may decline on a LAN,
    /// and then an on-glass test proves nothing).
    #[test]
    fn encryption_is_offered_but_never_required_in_shipping_modes() {
        assert_eq!(enc_flags(EncOffer::Off), (0, 0));
        assert_eq!(
            enc_flags(EncOffer::VideoOnly),
            (SS_ENC_VIDEO, 0),
            "the granular way out keeps video encryption"
        );
        assert_eq!(
            enc_flags(EncOffer::Supported),
            (SS_ENC_VIDEO | SS_ENC_CONTROL_V2, 0),
            "the default offers both, and requires neither"
        );
        assert_eq!(
            enc_flags(EncOffer::Required),
            (
                SS_ENC_VIDEO | SS_ENC_CONTROL_V2,
                SS_ENC_VIDEO | SS_ENC_CONTROL_V2
            ),
            "the test lever must also REQUEST it, or a client may decline"
        );
        // Whatever the mode, the host never advertises a bit it cannot serve — `SS_ENC_AUDIO`
        // (0x04) most of all, whose layout is not in the sanctioned reference.
        let servable = SS_ENC_VIDEO | SS_ENC_CONTROL_V2;
        for offer in [
            EncOffer::Off,
            EncOffer::VideoOnly,
            EncOffer::Supported,
            EncOffer::Required,
        ] {
            let (sup, req) = enc_flags(offer);
            assert_eq!(sup & !servable, 0, "no unimplemented bits offered");
            assert_eq!(req & !sup, 0, "never request what isn't supported");
        }
        // The default carries the control offer — that is what retires the legacy nonce reuse
        // for a stock client, and a default that quietly lost the bit would be a silent
        // regression rather than a visible one.
        assert_ne!(
            enc_flags(EncOffer::Supported).0 & SS_ENC_CONTROL_V2,
            0,
            "the default must offer control-v2"
        );
    }

    /// The sealed-RTSP round trip, and the property the whole framing rests on: a sealed message
    /// is recognisable from its first byte, and a plaintext one is never mistaken for it.
    #[test]
    fn sealed_rtsp_round_trips_and_is_self_distinguishing() {
        let key = [0x5Au8; 16];
        let msg = b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let wire = seal_response(&key, msg);
        assert!(wire.len() > ENC_RTSP_HEADER);
        // A sealed frame announces itself in the MSB; RTSP methods are ASCII and never do.
        assert!(wire[0] & 0x80 != 0, "sealed frames set the type bit");
        for method in [
            "OPTIONS", "DESCRIBE", "SETUP", "ANNOUNCE", "PLAY", "TEARDOWN",
        ] {
            assert!(
                method.as_bytes()[0] & 0x80 == 0,
                "{method} must not look sealed"
            );
        }
        let type_and_length = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]);
        let len = (type_and_length & !ENCRYPTED_MESSAGE_TYPE_BIT) as usize;
        assert_eq!(
            len,
            wire.len() - ENC_RTSP_HEADER,
            "length covers the payload"
        );
        let seq = u32::from_be_bytes([wire[4], wire[5], wire[6], wire[7]]);
        // Decrypt the way a client does: wire order is tag-first, `aes-gcm` wants tag last.
        let mut ct_tag = wire[ENC_RTSP_HEADER..].to_vec();
        ct_tag.extend_from_slice(&wire[8..ENC_RTSP_HEADER]);
        let pt = super::super::control::gcm_open(&key, &rtsp_nonce(seq, b'H'), &ct_tag, &[])
            .expect("host-sealed RTSP must open under the H/R nonce");
        assert_eq!(pt, msg);
        // The direction byte is the point: the client's nonce must NOT open a host message.
        assert!(
            super::super::control::gcm_open(&key, &rtsp_nonce(seq, b'C'), &ct_tag, &[]).is_none(),
            "a host message must not authenticate under the client direction"
        );
    }

    /// The receive path, end to end: a frame sealed the way a client seals one is opened and
    /// parsed back into the request it carried — headers, CSeq and body intact. This is the half
    /// that decides whether a sealed session works at all.
    #[test]
    fn sealed_rtsp_request_is_opened_and_parsed() {
        let key = [0xC3u8; 16];
        let msg = b"ANNOUNCE rtsp://x RTSP/1.0\r\nCSeq: 6\r\nContent-length: 7\r\n\r\nv=0\r\na=b";
        let frame = seal_frame(&key, 42, b'C', msg);
        let req = open_sealed_frame(&key, &frame).expect("a client-sealed frame must open");
        assert_eq!(req.method, "ANNOUNCE");
        assert_eq!(req.cseq, "6");
        assert_eq!(req.body, "v=0\r\na=b");

        // A frame sealed in the HOST direction must not open as a client request — the direction
        // byte is what separates the two nonce spaces.
        let host_framed = seal_frame(&key, 42, b'H', msg);
        assert!(
            open_sealed_frame(&key, &host_framed).is_err(),
            "host-direction frame must not authenticate as a client request"
        );
        // Neither may a tampered one: GCM is what makes this different from the CBC audio path.
        let mut flipped = frame.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;
        assert!(
            open_sealed_frame(&key, &flipped).is_err(),
            "a flipped ciphertext byte must be rejected, not decrypted"
        );
        // …and a wrong key must not open it either.
        assert!(open_sealed_frame(&[0u8; 16], &frame).is_err(), "wrong key");
    }

    /// Every sealed host message must use a fresh sequence number. RTSP is one message per TCP
    /// connection, so a counter that reset per connection would repeat (key, nonce) across a
    /// session's seven messages — the one catastrophic AES-GCM failure.
    #[test]
    fn host_rtsp_sequence_never_repeats() {
        let key = [1u8; 16];
        let seq_of = |w: &[u8]| u32::from_be_bytes([w[4], w[5], w[6], w[7]]);
        let a = seq_of(&seal_response(&key, b"a\r\n\r\n"));
        let b = seal_response(&key, b"b\r\n\r\n");
        let c = seal_response(&key, b"c\r\n\r\n");
        assert!(seq_of(&b) > a, "sequence must advance");
        assert!(seq_of(&c) > seq_of(&b), "sequence must advance");
    }

    /// The DESCRIBE SDP carries the codec indicators and all six Opus configs, normal
    /// quality before high quality per channel count (the client takes the first match as
    /// its normal config and a second match as HQ).
    #[test]
    fn describe_advertises_codecs_and_surround() {
        let sdp = describe_sdp();
        // The default build OFFERS video encryption AND control-v2 (both verified on glass) but
        // requires neither, so a client that wants the plaintext wire still gets exactly the wire
        // it always got. Asserted against `enc_flags` rather than a literal so this tracks the
        // default instead of restating it — the env can still steer it (`PUNKTFUNK_GS_ENCRYPT`),
        // which is why this reads the offer rather than assuming one.
        let (supported, requested) = enc_flags(gs_video_encryption_offer());
        assert!(sdp.contains(&format!("a=x-ss-general.encryptionSupported:{supported}")));
        assert!(sdp.contains(&format!("a=x-ss-general.encryptionRequested:{requested}")));
        assert!(
            sdp.contains("sprop-parameter-sets=AAAAAU"),
            "HEVC indicator"
        );
        assert!(sdp.contains("a=rtpmap:98 AV1/90000"), "AV1 indicator");
        for params in [
            "21101",       // stereo (clients hardcode this; emitted for Sunshine parity)
            "642012453",   // 5.1 normal — pre-rotated for the client's GFE-order swap
            "660012345",   // 5.1 high quality — verbatim
            "85301245673", // 7.1 normal — pre-rotated over [3, 8)
            "88001234567", // 7.1 high quality — verbatim
        ] {
            assert!(
                sdp.contains(&format!("a=fmtp:97 surround-params={params}")),
                "missing surround-params={params} in:\n{sdp}"
            );
        }
        // Normal precedes HQ for each surround channel count.
        let n51 = sdp.find("surround-params=642").unwrap();
        let h51 = sdp.find("surround-params=660").unwrap();
        let n71 = sdp.find("surround-params=853").unwrap();
        let h71 = sdp.find("surround-params=880").unwrap();
        assert!(n51 < h51 && n71 < h71);
    }
}
