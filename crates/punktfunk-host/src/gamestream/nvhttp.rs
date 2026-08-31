//! The nvhttp servers: plain HTTP on 47989 and mutual-TLS on 47984. Serves `/serverinfo`,
//! the `/pair` flow, `/applist`, and `/launch`/`/resume`/`/cancel`. Over HTTPS the client is
//! mutual-TLS-authenticated, so `/serverinfo` reports `PairStatus=1` there.
//!
//! The pairing PIN is delivered out-of-band ONLY through the bearer-authenticated management
//! API (`POST /api/v1/pair/pin`): the operator reads the PIN off the Moonlight client and
//! types it into the host console. There is deliberately NO unauthenticated nvhttp PIN
//! endpoint — one would let a network client submit its own displayed PIN and drive the whole
//! ceremony to a pinned cert with no operator consent (security-review 2026-06-28 #1).

use super::tls::{PeerAddr, PeerCertFingerprint};
use super::{serverinfo, AppState, LaunchSession, HTTPS_PORT, HTTP_PORT, RTSP_PORT};
use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Router,
};
use punktfunk_core::quic::{GRANT_ALL, GRANT_LAUNCH};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// Which listener a request arrived on — HTTPS means a mutual-TLS-authenticated client.
#[derive(Clone, Copy)]
struct Https(bool);

pub async fn run(state: Arc<AppState>) -> Result<()> {
    // Mutual-TLS: request + verify the client cert (Moonlight presents one for the
    // post-pairing pairchallenge + all post-pair endpoints).
    let tls = super::tls::server_config(&state.identity.cert_pem, &state.identity.key_pem)?;

    let http_addr = SocketAddr::from(([0, 0, 0, 0], HTTP_PORT));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], HTTPS_PORT));
    tracing::info!(%http_addr, %https_addr, "nvhttp listening (serverinfo + pair + launch)");

    // Both listeners run the governed acceptor (connection ceilings + header deadlines;
    // security-review 2026-08-31 M-6). HTTPS additionally runs the handshake itself so handlers
    // see the verified peer cert as a PeerCertFingerprint extension; the post-pair endpoints
    // gate on the paired allow-list.
    tokio::try_join!(
        super::tls::serve_plain(http_addr, router(state.clone(), false)),
        super::tls::serve_https(https_addr, router(state, true), tls),
    )?;
    Ok(())
}

/// True iff the request arrived over HTTPS with a client cert whose SHA-256 fingerprint is pinned
/// in the paired allow-list. Plain-HTTP requests carry no client cert and are never paired. This is
/// the post-handshake authorization check (Apollo's `get_verified_cert`) gating the launch surface.
fn peer_is_paired(peer: &Option<Extension<PeerCertFingerprint>>, st: &AppState) -> bool {
    let Some(Extension(PeerCertFingerprint(Some(fp)))) = peer else {
        return false;
    };
    st.paired
        .lock()
        .unwrap()
        .iter()
        .any(|der| hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(der)) == *fp)
}

/// The peer's client-cert fingerprint as raw bytes — the form [`LaunchSession::owner_fp`] stores.
/// `None` when no (or a blank/short) cert was presented.
fn peer_fp(peer: &Option<Extension<PeerCertFingerprint>>) -> Option<[u8; 32]> {
    match peer {
        Some(Extension(PeerCertFingerprint(Some(fp)))) => hex::decode(fp)
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v).ok()),
        _ => None,
    }
}

/// The grant mask the verified HTTPS peer is authorized for *right now*, resolved against the
/// shared grants registry (design/per-client-access.md §8, WP13). `None` = an EXPIRED grants
/// record — the launch surface fails that closed exactly like an unpaired cert. A fingerprint
/// with NO record is ungoverned (`Some(GRANT_ALL)`): the Moonlight plane's pairing authority
/// is its own cert list, so existing pairings keep full control (plan §8 risk table) — but a
/// record that exists (created via the console) governs. Consulted only AFTER
/// [`peer_is_paired`], which is why a certless peer resolves to expired-shaped `None` here:
/// it can never reach this gate with the pairing gate intact, and if it somehow did, failing
/// closed is the right wrong answer.
fn peer_grants(peer: &Option<Extension<PeerCertFingerprint>>, st: &AppState) -> Option<u32> {
    let Some(Extension(PeerCertFingerprint(Some(fp)))) = peer else {
        return None;
    };
    match st.access.get() {
        Some(np) => np.moonlight_effective(fp, super::wall_unix_now()),
        // No registry wired (tests / embedders that never call `serve`): pre-grants behavior.
        None => Some(GRANT_ALL),
    }
}

/// Whether the caller may control (resume/cancel) the current launch session. `true` when there is
/// no session (nothing to protect — keeps cancel idempotent), or the session's owner fingerprint
/// matches the caller's. Only a paired-but-DIFFERENT client with a known, mismatching fingerprint is
/// rejected — so a same-client control action always succeeds, but one paired client can no longer
/// resume or cancel *another* paired client's session (security-review 2026-07-17).
fn peer_may_control_session(peer: &Option<Extension<PeerCertFingerprint>>, st: &AppState) -> bool {
    match st.launch.lock().unwrap().as_ref() {
        None => true,
        Some(session) => match (session.owner_fp, peer_fp(peer)) {
            (Some(owner), Some(caller)) => owner == caller,
            // Owner or caller fingerprint unknown → the `peer_is_paired` gate already applied stands.
            _ => true,
        },
    }
}

fn router(state: Arc<AppState>, https: bool) -> Router {
    Router::new()
        .route("/serverinfo", get(h_serverinfo))
        .route("/pair", get(h_pair))
        .route("/applist", get(h_applist))
        .route("/appasset", get(h_appasset))
        .route("/launch", get(h_launch))
        .route("/resume", get(h_resume))
        .route("/cancel", get(h_cancel))
        .layer(Extension(Https(https)))
        .with_state(state)
}

fn xml(body: String) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/xml")], body)
}

async fn h_serverinfo(
    State(st): State<Arc<AppState>>,
    Extension(Https(https)): Extension<Https>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    // PairStatus=1 only when the HTTPS peer presented a *pinned* client cert; an unpaired client
    // (or plain HTTP) sees 0 and is steered into the pairing flow.
    let paired = https && peer_is_paired(&peer, &st);
    // The running app id, visible to the session OWNER only (WP3 — see `owner_current_game` /
    // the rationale on `serverinfo_xml`). This is what makes Moonlight show Resume/Quit.
    let current_game = if paired {
        owner_current_game(&st.launch.lock().unwrap(), peer_fp(&peer))
    } else {
        0
    };
    xml(serverinfo::serverinfo_xml(
        &st.host,
        https,
        paired,
        current_game,
    ))
}

/// The `currentgame` a given caller may see: the live session's appid iff the caller IS the
/// session owner (fingerprints known on both sides and equal), else 0. Pure — unit-tested.
/// Stricter than [`peer_may_control_session`] on purpose: that gate fails OPEN when a
/// fingerprint is unknown (so a same-client control action can't be locked out), but an
/// ADVERTISEMENT fails closed — an unknown owner is nobody's business to see.
fn owner_current_game(launch: &Option<LaunchSession>, caller: Option<[u8; 32]>) -> u32 {
    match (launch, caller) {
        (Some(s), Some(fp)) if s.owner_fp == Some(fp) => s.appid,
        _ => 0,
    }
}

async fn h_applist(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("applist rejected — client is not paired");
        return xml(error_xml());
    }
    xml(super::apps::applist_xml())
}

/// Box-art cover proxy (`/appasset?appid=N&AssetType=2&AssetIdx=0`). Moonlight fetches per-app covers
/// from the HOST, so we resolve the appid to its library title and proxy the cover image bytes (Steam/
/// Epic CDN, etc.). 404 for Desktop / apps.json entries (no art) or any fetch failure — Moonlight then
/// shows its title-only placeholder. Paired clients only (same gate as `/applist`). The resolve+fetch is
/// blocking (disk + network), so it runs on a blocking thread off the async runtime.
async fn h_appasset(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("appasset rejected — client is not paired");
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(appid) = q.get("appid").and_then(|s| s.parse::<u32>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match tokio::task::spawn_blocking(move || super::apps::appasset_bytes(appid)).await {
        Ok(Some((bytes, ctype))) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn h_launch(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("launch rejected — client is not paired");
        return xml(error_xml()).into_response();
    }
    // Per-client access (WP13, design §8): LAUNCH + expiry beside the pairing gate. An expired
    // grants record fails closed exactly like an unpaired cert; a Controller-only record (no
    // LAUNCH bit) is refused too — on GameStream, launch IS the session, there is no owner-
    // launched session to join. The protocol has no reject vocabulary, so the client just sees
    // the generic error and the story lives in the console (silent enforcement, accepted).
    match peer_grants(&peer, &st) {
        Some(g) if g & GRANT_LAUNCH != 0 => {}
        Some(_) => {
            tracing::warn!("launch rejected — this client's access grants do not include Launch");
            return xml(error_xml()).into_response();
        }
        None => {
            tracing::warn!("launch rejected — this client's access has expired");
            return xml(error_xml()).into_response();
        }
    }
    let req_fp: Option<[u8; 32]> = peer_fp(&peer);

    // Mode-conflict ADMISSION (Stage 4) — GameStream is single-session (`st.launch`), so a DIFFERENT
    // paired client launching while a session is live is governed by `mode_conflict` (see
    // [`gamestream_admission`]). Snapshot the live owner + mode (Copy) so the lock isn't held over it.
    let mut forced_mode: Option<(u32, u32, u32)> = None;
    {
        let live = st
            .launch
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| (s.owner_fp, (s.width, s.height, s.fps)));
        // Same Windows default as the native path (separate → reject; see `effective_conflict`) so a
        // 2nd Moonlight client gets a clean 503 rather than wedging the shared monitor's capture.
        let conflict = crate::vdisplay::admission::effective_conflict();
        match gamestream_admission(live, req_fp, conflict) {
            GsDecision::Serve => {}
            GsDecision::Join((w, h, f)) => {
                forced_mode = Some((w, h, f));
                tracing::info!(
                    "GameStream launch JOIN — admitting at the live session's mode {w}x{h}@{f}"
                );
            }
            GsDecision::Reject => {
                tracing::warn!(
                    "GameStream launch REJECTED — host busy (mode_conflict=reject, session owned by another client)"
                );
                return (StatusCode::SERVICE_UNAVAILABLE, xml(error_xml())).into_response();
            }
        }
    }

    match launch(&st, &q) {
        Ok(mut session) => {
            // Bind the (unauthenticated) RTSP/UDP media plane to this paired client's source IP.
            session.peer_ip = addr.map(|Extension(PeerAddr(a))| a.ip());
            session.owner_fp = req_fp;
            if let Some((w, h, f)) = forced_mode {
                session.width = w;
                session.height = h;
                session.fps = f;
            }
            // A new session starts undecided: whatever ended the last one says nothing about this
            // one (see `AppState::quit`). The game this launch reclaims — if this client left one
            // waiting out its reconnect window — is reprieved by the stream thread, which resolves
            // the title anyway and so needs no second library scan here.
            st.quit.store(false, std::sync::atomic::Ordering::SeqCst);
            // Fresh A/V ping payload for this session, before the client's RTSP SETUP asks for it:
            // it is what the media planes use to tell this client's first datagram apart from any
            // other arriving at the port from the same address.
            st.mint_av_ping();
            *st.launch.lock().unwrap() = Some(session);
            tracing::info!(
                w = session.width,
                h = session.height,
                fps = session.fps,
                rikeyid = session.rikeyid,
                "launch — session created; RTSP at rtsp://{}:{RTSP_PORT}",
                st.host.local_ip()
            );
            xml(session_url_xml(&st, "gamesession")).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "launch failed");
            xml(error_xml()).into_response()
        }
    }
}

async fn h_resume(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("resume rejected — client is not paired");
        return xml(error_xml());
    }
    // Same access gate as `/launch` (WP13): resuming re-attaches the full input/media planes,
    // so it needs the same LAUNCH grant, and expiry fails closed like unpaired.
    match peer_grants(&peer, &st) {
        Some(g) if g & GRANT_LAUNCH != 0 => {}
        Some(_) => {
            tracing::warn!("resume rejected — this client's access grants do not include Launch");
            return xml(error_xml());
        }
        None => {
            tracing::warn!("resume rejected — this client's access has expired");
            return xml(error_xml());
        }
    }
    if !peer_may_control_session(&peer, &st) {
        tracing::warn!("resume rejected — caller does not own the session");
        return xml(error_xml());
    }
    // RESTART the media planes for this connection (WP3). Moonlight resumes with a fresh RTSP
    // handshake, but a PLAY that finds `streaming` still true takes its "already running"
    // branch — the old threads keep streaming at the VANISHED endpoint and the resumed client
    // gets no media. Clear the run flags and WAIT (bounded) for the old threads' full exit:
    // their teardown must complete before the successor's threads start, or the two race over
    // the pooled capturer and the old exit path stomps the new session's flags. A timeout
    // proceeds anyway — worst case is exactly the old (media-less) behavior.
    let before = st.media_exited.load(std::sync::atomic::Ordering::SeqCst);
    let expected = u64::from(
        st.streaming
            .swap(false, std::sync::atomic::Ordering::SeqCst),
    ) + u64::from(
        st.audio_streaming
            .swap(false, std::sync::atomic::Ordering::SeqCst),
    );
    if expected > 0 {
        tracing::info!(
            threads = expected,
            "resume — stopping the previous connection's media threads"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while st.media_exited.load(std::sync::atomic::Ordering::SeqCst) < before + expected {
            if std::time::Instant::now() >= deadline {
                tracing::warn!("resume — old media threads still exiting after 2 s; proceeding");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    // RE-KEY the session with the fresh rikey/rikeyid this resume carries (WP3 — the handler
    // used to read no query params at all). A resuming Moonlight mints NEW session keys and
    // derives its control-GCM and audio-CBC state from them; keeping the original launch's
    // keys meant every post-resume control packet failed decrypt and audio decoded to noise.
    // Keys present → they replace the session's; malformed → refuse the resume (streaming
    // against keys the client doesn't hold is a worse failure than a clean error); absent →
    // keep the current keys (nothing claimed otherwise). `surroundAudioInfo` needs no reading
    // here: the RTSP ANNOUNCE that follows re-negotiates audio for the new connection anyway.
    // The launch may have been cleared by the old threads' client-unreachable teardown while
    // we waited — then there is nothing to resume, and the client falls back to `/launch`.
    {
        let mut launch = st.launch.lock().unwrap();
        let Some(session) = launch.as_mut() else {
            return xml(error_xml());
        };
        if q.contains_key("rikey") {
            match parse_rikey(&q) {
                Ok((gcm_key, rikeyid)) => {
                    session.gcm_key = gcm_key;
                    session.rikeyid = rikeyid;
                    tracing::info!(
                        rikeyid,
                        "resume — session re-keyed with the client's fresh rikey"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "resume rejected — malformed rikey");
                    return xml(error_xml());
                }
            }
        }
        // Re-bind the media/RTSP source-IP filters to where the client resumes FROM — a device
        // that moved networks (Wi-Fi → ethernet) resumes instead of being filtered out.
        if let Some(Extension(PeerAddr(a))) = addr {
            session.peer_ip = Some(a.ip());
        }
    }
    // A resume is a new connection with a new RTSP handshake and new media threads, so it gets a
    // new ping payload too — the old one may have been observed on the wire (RTSP is plaintext
    // until `SS_ENC_CONTROL_V2`), and nothing that learns an endpoint after this point has seen it.
    st.mint_av_ping();
    xml(session_url_xml(&st, "resume"))
}

async fn h_cancel(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("cancel rejected — client is not paired");
        return xml(error_xml());
    }
    // Expiry gates `/cancel` likewise (an expired record fails closed exactly like unpaired) —
    // but the LAUNCH bit deliberately does NOT: cancel is Moonlight's "Quit App", a teardown,
    // and `peer_may_control_session` below already restricts it to the session's owner. Denying
    // a mid-session-downgraded owner its own quit would only wedge the session it is trying to
    // end — ending sessions is what enforcement *wants*.
    if peer_grants(&peer, &st).is_none() {
        tracing::warn!("cancel rejected — this client's access has expired");
        return xml(error_xml());
    }
    if !peer_may_control_session(&peer, &st) {
        tracing::warn!("cancel rejected — caller does not own the session");
        return xml(error_xml());
    }
    // Quit semantics, and now literally so: `/cancel` is Moonlight's "Quit App" — a decision, not a
    // drop. The shared full teardown (launch cleared + both media threads stop on their flags) runs
    // with the session's quit flag set, so the virtual display skips its keep-alive linger and the
    // end-game-on-session-end policy treats this as the operator asking. The virtual
    // output/gamescope teardown follows via the capturer's RAII.
    st.quit_session("client /cancel");
    xml("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><cancel>1</cancel></root>\n".to_string())
}

/// Parse the `rikey`/`rikeyid` pair out of a `/launch` or `/resume` query: the 16-byte AES
/// session key (hex) every media/control crypto derives from, and the signed 32-bit key id
/// (negative values wrap to a big-endian u32 IV later).
fn parse_rikey(q: &HashMap<String, String>) -> Result<([u8; 16], i32)> {
    let rikey = q.get("rikey").ok_or_else(|| anyhow!("missing rikey"))?;
    let key_bytes = hex::decode(rikey).context("rikey hex")?;
    if key_bytes.len() < 16 {
        return Err(anyhow!("rikey too short"));
    }
    let mut gcm_key = [0u8; 16];
    gcm_key.copy_from_slice(&key_bytes[..16]);
    let rikeyid: i32 = q.get("rikeyid").and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((gcm_key, rikeyid))
}

/// Parse the `/launch` query (rikey/rikeyid/mode) into a [`LaunchSession`].
fn launch(_st: &AppState, q: &HashMap<String, String>) -> Result<LaunchSession> {
    let (gcm_key, rikeyid) = parse_rikey(q)?;
    let (width, height, fps) = q
        .get("mode")
        .and_then(|m| parse_mode(m))
        .unwrap_or((1920, 1080, 60));
    let appid = q.get("appid").and_then(|s| s.parse().ok()).unwrap_or(1);
    Ok(LaunchSession {
        gcm_key,
        rikeyid,
        width,
        height,
        fps,
        appid,
        peer_ip: None,  // set by `h_launch` from the verified HTTPS peer address
        owner_fp: None, // set by `h_launch` from the verified HTTPS peer cert fingerprint
    })
}

/// `"1920x1080x60"` → `(1920, 1080, 60)`.
fn parse_mode(mode: &str) -> Option<(u32, u32, u32)> {
    let mut it = mode.split('x');
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    let fps = it.next()?.parse().ok()?;
    Some((w, h, fps))
}

/// A live GameStream session's `(owner cert fingerprint, mode)` snapshot for [`gamestream_admission`].
type LiveGs = (Option<[u8; 32]>, (u32, u32, u32));

/// The outcome of [`gamestream_admission`].
enum GsDecision {
    /// Proceed with the launch (no live session, a same-client re-launch, or `steal`/`separate`
    /// taking over the single session).
    Serve,
    /// Serve at the live session's mode (`join` — honest-downgrade).
    Join((u32, u32, u32)),
    /// Refuse with a 503 (`reject`).
    Reject,
}

/// The GameStream single-session mode-conflict decision (Stage 4, pure so it's unit-tested). `live`
/// is the currently-live session's `(owner_fp, mode)` (`None` ⇒ no session live). No session or a
/// same-client re-launch ⇒ `Serve`; a DIFFERENT client launching applies `policy` — `reject` ⇒
/// `Reject`, `join` ⇒ `Join` the live mode, `steal`/`separate` (GameStream has no separate) ⇒ `Serve`
/// (take over the one session).
fn gamestream_admission(
    live: Option<LiveGs>,
    req_fp: Option<[u8; 32]>,
    policy: crate::vdisplay::policy::ModeConflict,
) -> GsDecision {
    use crate::vdisplay::policy::ModeConflict;
    let Some((owner, mode)) = live else {
        return GsDecision::Serve;
    };
    let different = match (owner, req_fp) {
        (Some(o), Some(r)) => o != r,
        _ => true, // unknown owner or anonymous requester → treat as a different client
    };
    if !different {
        return GsDecision::Serve;
    }
    match policy {
        ModeConflict::Reject => GsDecision::Reject,
        ModeConflict::Join => GsDecision::Join(mode),
        ModeConflict::Steal | ModeConflict::Separate => GsDecision::Serve,
    }
}

fn session_url_xml(st: &AppState, tag: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\n<sessionUrl0>rtsp://{}:{RTSP_PORT}</sessionUrl0>\n<{tag}>1</{tag}>\n</root>\n",
        st.host.local_ip()
    )
}

async fn h_pair(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let uniqueid = q.get("uniqueid").cloned().unwrap_or_default();
    let phrase = q.get("phrase").map(String::as_str);

    let step = phrase
        .filter(|p| *p == "getservercert" || *p == "pairchallenge")
        .or_else(|| {
            [
                "clientchallenge",
                "serverchallengeresp",
                "clientpairingsecret",
            ]
            .into_iter()
            .find(|k| q.contains_key(*k))
        })
        .unwrap_or("?");
    tracing::info!(uniqueid, step, "pair request");

    let result = if phrase == Some("getservercert") {
        match (q.get("salt"), q.get("clientcert")) {
            (Some(salt), Some(cc)) => {
                st.pairing
                    .getservercert(&st.identity, &uniqueid, salt, cc)
                    .await
            }
            _ => Ok(pair_error_xml()),
        }
    } else if phrase == Some("pairchallenge") {
        // The ceremony's last step, which Moonlight makes over the TLS port with the cert phase 4
        // has just pinned — so the pinned handshake is the proof, and only a pinned caller is told
        // it is paired. Anyone else (the plain-HTTP listener, or an HTTPS peer presenting a cert
        // that is not in the allow-list) gets the same answer an unpaired host gives, so this
        // endpoint asserts no pairing the caller does not hold (security-review 2026-08-25).
        if peer_is_paired(&peer, &st) {
            Ok(paired_ok_xml())
        } else {
            tracing::warn!("pairchallenge rejected — client is not paired");
            Ok(pair_error_xml())
        }
    } else if let Some(v) = q.get("clientchallenge") {
        st.pairing.clientchallenge(&st.identity, &uniqueid, v)
    } else if let Some(v) = q.get("serverchallengeresp") {
        st.pairing.serverchallengeresp(&st.identity, &uniqueid, v)
    } else if let Some(v) = q.get("clientpairingsecret") {
        let r = st.pairing.clientpairingsecret(&uniqueid, v, &st.paired);
        // Phase 4 may just have pinned the FIRST pairing — bring the ENet control port up now
        // (idempotent; rust-safety WP0) so this client's imminent /launch finds the control
        // stream listening. Moonlight connects control before video, so "eventually up" would
        // be an aborted session.
        if let Err(e) = super::sync_control(&st) {
            tracing::warn!(error = %format!("{e:#}"), "control port sync after pairing failed");
        }
        r
    } else {
        Ok(pair_error_xml())
    };

    let body = result.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), uniqueid, "pair handler error");
        pair_error_xml()
    });
    xml(body)
}

fn paired_ok_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><paired>1</paired></root>\n"
        .to_string()
}

fn pair_error_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><paired>0</paired></root>\n"
        .to_string()
}

fn error_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"400\"></root>\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<AppState> {
        let host = super::super::Host {
            hostname: "t".into(),
            uniqueid: "id".into(),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let identity = super::super::cert::ServerIdentity::ephemeral().expect("ephemeral identity");
        let stats = crate::stats_recorder::StatsRecorder::new(
            std::env::temp_dir().join(format!("pf-nvhttp-stats-{}", std::process::id())),
        );
        Arc::new(AppState::new(host, identity, stats))
    }

    fn fp_of(der: &[u8]) -> String {
        hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(der))
    }

    /// The launch surface (launch/resume/applist/cancel) must reject any client whose cert
    /// fingerprint is not in the paired allow-list — including a certless (plain-HTTP) peer.
    #[test]
    fn launch_gate_requires_a_pinned_client_cert() {
        let st = test_state();
        let der = b"a-client-cert-der".to_vec();
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_of(&der)))));

        // Empty allow-list: a presented cert, an absent extension, and an explicit None all fail.
        assert!(!peer_is_paired(&peer, &st), "unknown cert must be rejected");
        assert!(
            !peer_is_paired(&None, &st),
            "no client cert must be rejected"
        );
        assert!(
            !peer_is_paired(&Some(Extension(PeerCertFingerprint(None))), &st),
            "certless HTTPS peer must be rejected"
        );

        // After pinning, the same fingerprint is accepted but a different cert still isn't.
        st.paired.lock().unwrap().push(der);
        assert!(peer_is_paired(&peer, &st), "pinned cert must be accepted");
        let other = Some(Extension(PeerCertFingerprint(Some(fp_of(
            b"different-der",
        )))));
        assert!(
            !peer_is_paired(&other, &st),
            "a non-pinned cert stays rejected"
        );
    }

    /// `pairchallenge` is the ceremony's last step and Moonlight makes it over the TLS port with
    /// the cert phase 4 just pinned, so only a pinned caller is told it is paired. A plain-HTTP
    /// scanner (no client cert at all) and an HTTPS peer with an unpinned cert both get the
    /// unpaired answer — the endpoint must not assert a pairing the caller does not hold.
    #[tokio::test]
    async fn pairchallenge_answers_only_a_pinned_client() {
        async fn challenge(
            st: &Arc<AppState>,
            peer: Option<Extension<PeerCertFingerprint>>,
        ) -> String {
            let q = HashMap::from([("phrase".to_string(), "pairchallenge".to_string())]);
            let resp = h_pair(State(st.clone()), peer, Query(q))
                .await
                .into_response();
            let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        }

        let st = test_state();
        let der = b"pairchallenge-client-der".to_vec();
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_of(&der)))));

        // Plain HTTP (no cert) and an unpinned HTTPS cert both answer as an unpaired host.
        let plain = challenge(&st, None).await;
        assert!(plain.contains("<paired>0</paired>"), "plain HTTP: {plain}");
        let unpinned = challenge(&st, peer.clone()).await;
        assert!(
            unpinned.contains("<paired>0</paired>"),
            "unpinned cert: {unpinned}"
        );

        // Once phase 4 has pinned the cert, the real client's last step still succeeds.
        st.paired.lock().unwrap().push(der);
        let pinned = challenge(&st, peer).await;
        assert!(pinned.contains("<paired>1</paired>"), "pinned: {pinned}");
    }

    #[test]
    fn gamestream_admission_policy_matrix() {
        use crate::vdisplay::policy::ModeConflict;
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let live = Some((Some(a), (2560, 1440, 120)));
        // No live session → always Serve.
        assert!(matches!(
            gamestream_admission(None, Some(b), ModeConflict::Reject),
            GsDecision::Serve
        ));
        // Same-client re-launch → Serve regardless of policy.
        assert!(matches!(
            gamestream_admission(live, Some(a), ModeConflict::Reject),
            GsDecision::Serve
        ));
        // A DIFFERENT client applies the policy.
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Reject),
            GsDecision::Reject
        ));
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Join),
            GsDecision::Join((2560, 1440, 120))
        ));
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Steal),
            GsDecision::Serve
        ));
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Separate),
            GsDecision::Serve
        ));
        // Anonymous requester (no cert presented) is treated as a different client.
        assert!(matches!(
            gamestream_admission(live, None, ModeConflict::Reject),
            GsDecision::Reject
        ));
    }

    /// A fresh grants registry backed by a per-test temp store.
    fn test_registry(
        tag: &str,
    ) -> (
        Arc<crate::native_pairing::NativePairing>,
        std::path::PathBuf,
    ) {
        let p = std::env::temp_dir().join(format!(
            "pf-nvhttp-access-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(Some(p.clone()), None, false).unwrap(),
        );
        (np, p)
    }

    /// WP13's resolution rule at the nvhttp gate: no grants record = ungoverned full control
    /// (existing Moonlight pairings keep today's behavior); a record that exists governs; an
    /// expired record resolves `None` — the shape the handlers fail closed exactly like
    /// unpaired. Certless peers resolve `None` too (they never pass `peer_is_paired` anyway).
    #[test]
    fn peer_grants_resolution_rule() {
        use crate::native_pairing::Access;
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let st = test_state();
        let der = b"grants-client-der".to_vec();
        let fp_hex = fp_of(&der);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_hex.clone()))));

        // No registry wired (an AppState that never went through `serve`): pre-grants behavior.
        assert_eq!(peer_grants(&peer, &st), Some(GRANT_ALL));

        let (np, store) = test_registry("rule");
        assert!(st.access.set(np.clone()).is_ok());
        // Registry wired, no record: ungoverned.
        assert_eq!(peer_grants(&peer, &st), Some(GRANT_ALL));
        // A Controller-only record governs — LAUNCH absent.
        np.add_with_access(
            "Guest",
            &fp_hex,
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: None,
            }),
        )
        .unwrap();
        assert_eq!(peer_grants(&peer, &st), Some(GRANT_GAMEPAD));
        assert_eq!(peer_grants(&peer, &st).unwrap() & GRANT_LAUNCH, 0);
        // Expired: the fail-closed shape.
        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_ALL,
                expires_unix: Some(super::super::wall_unix_now() - 5),
            },
        )
        .unwrap();
        assert_eq!(peer_grants(&peer, &st), None);
        // Certless peer: `None` — it can never reach the grants gate past `peer_is_paired`,
        // and failing closed is the right wrong answer if it somehow did.
        assert_eq!(peer_grants(&None, &st), None);
        assert_eq!(
            peer_grants(&Some(Extension(PeerCertFingerprint(None))), &st),
            None
        );
        let _ = std::fs::remove_file(&store);
    }

    /// The WP13 acceptance at handler level: `/resume` (same gate as `/launch`) works for a
    /// paired client with no grants record (stock back-compat), refuses a Controller-only
    /// record (no LAUNCH), refuses an expired record exactly like unpaired — and `/cancel`
    /// gates on expiry only, so a re-granted limited client can still quit its own session.
    #[tokio::test]
    async fn resume_and_cancel_honor_grants_and_expiry() {
        use crate::native_pairing::Access;
        use punktfunk_core::quic::GRANT_GAMEPAD;

        async fn body_of(resp: Response) -> String {
            let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        }

        let st = test_state();
        let der = b"resume-grants-client".to_vec();
        let fp_hex = fp_of(&der);
        let owner_fp = punktfunk_core::quic::endpoint::cert_fingerprint(&der);
        st.paired.lock().unwrap().push(der);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_hex.clone()))));
        let (np, store) = test_registry("resume");
        assert!(st.access.set(np.clone()).is_ok());
        let session = LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(owner_fp),
        };
        *st.launch.lock().unwrap() = Some(session);

        // No grants record: a stock Moonlight pairing resumes exactly as today.
        let ok = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<resume>1</resume>"), "ungoverned resume: {ok}");

        // Controller-only record: the LAUNCH bit is missing — refused.
        let now = super::super::wall_unix_now();
        np.add_with_access(
            "Guest",
            &fp_hex,
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 3600),
            }),
        )
        .unwrap();
        let no = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<resume>1</resume>"), "no-LAUNCH resume: {no}");

        // Expired full record: fails closed exactly like unpaired — for /resume AND /cancel.
        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_ALL,
                expires_unix: Some(now - 5),
            },
        )
        .unwrap();
        let no = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<resume>1</resume>"), "expired resume: {no}");
        let no = body_of(
            h_cancel(State(st.clone()), peer.clone())
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<cancel>1</cancel>"), "expired cancel: {no}");
        assert!(
            st.launch.lock().unwrap().is_some(),
            "a refused cancel must not tear the session down"
        );

        // Re-granted Controller-only (unexpired, still no LAUNCH): /cancel is deliberately NOT
        // LAUNCH-gated — the session's owner may always quit its own app.
        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 3600),
            },
        )
        .unwrap();
        let ok = body_of(
            h_cancel(State(st.clone()), peer.clone())
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<cancel>1</cancel>"), "owner cancel: {ok}");
        assert!(st.launch.lock().unwrap().is_none(), "cancel tears down");
        let _ = std::fs::remove_file(&store);
    }

    /// WP3: `currentgame` is advertised to the session OWNER only. A non-owner (or an unknown
    /// fingerprint on either side) keeps seeing free/0 — which is what keeps it on the `/launch`
    /// path and the reject/join/steal admission, instead of routing into owner-only `/resume`.
    #[test]
    fn current_game_is_owner_scoped() {
        let owner = [7u8; 32];
        let other = [9u8; 32];
        let session = |owner_fp: Option<[u8; 32]>| {
            Some(LaunchSession {
                gcm_key: [0; 16],
                rikeyid: 0,
                width: 1920,
                height: 1080,
                fps: 60,
                appid: 4242,
                peer_ip: None,
                owner_fp,
            })
        };
        // The owner sees the running app.
        assert_eq!(owner_current_game(&session(Some(owner)), Some(owner)), 4242);
        // A different paired client sees a free host.
        assert_eq!(owner_current_game(&session(Some(owner)), Some(other)), 0);
        // Unknown fingerprints — either side — fail CLOSED (unlike the control gate).
        assert_eq!(owner_current_game(&session(None), Some(owner)), 0);
        assert_eq!(owner_current_game(&session(Some(owner)), None), 0);
        // No session: free.
        assert_eq!(owner_current_game(&None, Some(owner)), 0);
    }

    /// WP3: a resume carrying a fresh `rikey`/`rikeyid` RE-KEYS the live session (control GCM +
    /// audio CBC derive from it — stale keys made every post-resume packet undecryptable); a
    /// malformed rikey refuses the resume; a keyless resume keeps the current keys.
    #[tokio::test]
    async fn resume_rekeys_the_session() {
        async fn body_of(resp: Response) -> String {
            let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        }
        let st = test_state();
        let der = b"resume-rekey-client".to_vec();
        let fp_hex = fp_of(&der);
        let owner_fp = punktfunk_core::quic::endpoint::cert_fingerprint(&der);
        st.paired.lock().unwrap().push(der);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_hex))));
        *st.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0x11; 16],
            rikeyid: 1,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(owner_fp),
        });

        // Fresh keys on the query → the session now carries them.
        let mut q = HashMap::new();
        q.insert("rikey".to_string(), "22".repeat(16));
        q.insert("rikeyid".to_string(), "-5".to_string());
        let ok = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(q))
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<resume>1</resume>"), "re-keyed resume: {ok}");
        {
            let launch = st.launch.lock().unwrap();
            let s = launch.as_ref().unwrap();
            assert_eq!(s.gcm_key, [0x22; 16]);
            assert_eq!(s.rikeyid, -5);
        }

        // Malformed rikey → refused, and the session's keys stay what they were.
        let mut bad = HashMap::new();
        bad.insert("rikey".to_string(), "zz".to_string());
        let no = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(bad))
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<resume>1</resume>"), "malformed rikey: {no}");
        assert_eq!(
            st.launch.lock().unwrap().as_ref().unwrap().gcm_key,
            [0x22; 16]
        );

        // No rikey at all → resume succeeds on the current keys (nothing claimed otherwise).
        let ok = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<resume>1</resume>"), "keyless resume: {ok}");
        assert_eq!(st.launch.lock().unwrap().as_ref().unwrap().rikeyid, -5);
    }
}
