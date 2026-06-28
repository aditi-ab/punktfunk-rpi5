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
    http::header,
    response::IntoResponse,
    routing::get,
    Extension, Router,
};
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

    let http = axum_server::bind(http_addr).serve(router(state.clone(), false).into_make_service());
    // HTTPS runs the handshake itself (super::tls::serve_https) so handlers see the verified peer
    // cert as a PeerCertFingerprint extension; the post-pair endpoints gate on the paired allow-list.
    tokio::try_join!(
        async { http.await.context("nvhttp HTTP server") },
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

fn router(state: Arc<AppState>, https: bool) -> Router {
    Router::new()
        .route("/serverinfo", get(h_serverinfo))
        .route("/pair", get(h_pair))
        .route("/applist", get(h_applist))
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
    xml(serverinfo::serverinfo_xml(&st.host, https, paired))
}

async fn h_applist(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("applist rejected — client is not paired");
        return xml(error_xml());
    }
    // One app for now: the headless desktop (the wlroots virtual output).
    xml(super::apps::applist_xml())
}

async fn h_launch(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("launch rejected — client is not paired");
        return xml(error_xml());
    }
    match launch(&st, &q) {
        Ok(mut session) => {
            // Bind the (unauthenticated) RTSP/UDP media plane to this paired client's source IP.
            session.peer_ip = addr.map(|Extension(PeerAddr(a))| a.ip());
            *st.launch.lock().unwrap() = Some(session);
            tracing::info!(
                w = session.width,
                h = session.height,
                fps = session.fps,
                rikeyid = session.rikeyid,
                "launch — session created; RTSP at rtsp://{}:{RTSP_PORT}",
                st.host.local_ip
            );
            xml(session_url_xml(&st, "gamesession"))
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "launch failed");
            xml(error_xml())
        }
    }
}

async fn h_resume(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("resume rejected — client is not paired");
        return xml(error_xml());
    }
    if st.launch.lock().unwrap().is_some() {
        xml(session_url_xml(&st, "resume"))
    } else {
        xml(error_xml())
    }
}

async fn h_cancel(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    if !peer_is_paired(&peer, &st) {
        tracing::warn!("cancel rejected — client is not paired");
        return xml(error_xml());
    }
    *st.launch.lock().unwrap() = None;
    // Quit semantics: stop the running media threads (they observe these flags) so the session
    // actually ends — the virtual output/gamescope teardown follows via the capturer's RAII.
    st.streaming
        .store(false, std::sync::atomic::Ordering::SeqCst);
    st.audio_streaming
        .store(false, std::sync::atomic::Ordering::SeqCst);
    tracing::info!("cancel — launch session cleared, streams stopping");
    xml("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><cancel>1</cancel></root>\n".to_string())
}

/// Parse the `/launch` query (rikey/rikeyid/mode) into a [`LaunchSession`].
fn launch(_st: &AppState, q: &HashMap<String, String>) -> Result<LaunchSession> {
    let rikey = q.get("rikey").ok_or_else(|| anyhow!("missing rikey"))?;
    let key_bytes = hex::decode(rikey).context("rikey hex")?;
    if key_bytes.len() < 16 {
        return Err(anyhow!("rikey too short"));
    }
    let mut gcm_key = [0u8; 16];
    gcm_key.copy_from_slice(&key_bytes[..16]);
    // rikeyid is a signed 32-bit int (negative values wrap to a big-endian u32 IV later).
    let rikeyid: i32 = q.get("rikeyid").and_then(|s| s.parse().ok()).unwrap_or(0);
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
        peer_ip: None, // set by `h_launch` from the verified HTTPS peer address
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

fn session_url_xml(st: &AppState, tag: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\n<sessionUrl0>rtsp://{}:{RTSP_PORT}</sessionUrl0>\n<{tag}>1</{tag}>\n</root>\n",
        st.host.local_ip
    )
}

async fn h_pair(
    State(st): State<Arc<AppState>>,
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
        // Reached only over the TLS port with the pinned host cert; the handshake is the
        // proof, so acknowledge success.
        Ok(paired_ok_xml())
    } else if let Some(v) = q.get("clientchallenge") {
        st.pairing.clientchallenge(&st.identity, &uniqueid, v)
    } else if let Some(v) = q.get("serverchallengeresp") {
        st.pairing.serverchallengeresp(&st.identity, &uniqueid, v)
    } else if let Some(v) = q.get("clientpairingsecret") {
        st.pairing.clientpairingsecret(&uniqueid, v, &st.paired)
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
    use std::net::{IpAddr, Ipv4Addr};

    fn test_state() -> Arc<AppState> {
        let host = super::super::Host {
            hostname: "t".into(),
            uniqueid: "id".into(),
            local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
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
}
