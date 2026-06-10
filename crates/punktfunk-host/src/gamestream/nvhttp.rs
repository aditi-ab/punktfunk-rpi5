//! The nvhttp servers: plain HTTP on 47989 and mutual-TLS on 47984. Serves `/serverinfo`,
//! the `/pair` flow, `/applist`, and `/launch`/`/resume`/`/cancel`, plus a punktfunk-only
//! `/pin` endpoint to deliver the Moonlight-displayed PIN. Over HTTPS the client is
//! mutual-TLS-authenticated, so `/serverinfo` reports `PairStatus=1` there.

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
    let tls = axum_server::tls_rustls::RustlsConfig::from_config(super::tls::server_config(
        &state.identity.cert_pem,
        &state.identity.key_pem,
    )?);

    let http_addr = SocketAddr::from(([0, 0, 0, 0], HTTP_PORT));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], HTTPS_PORT));
    tracing::info!(%http_addr, %https_addr, "nvhttp listening (serverinfo + pair + launch)");

    let http = axum_server::bind(http_addr).serve(router(state.clone(), false).into_make_service());
    let https =
        axum_server::bind_rustls(https_addr, tls).serve(router(state, true).into_make_service());
    tokio::try_join!(async { http.await.context("nvhttp HTTP server") }, async {
        https.await.context("nvhttp HTTPS server")
    },)?;
    Ok(())
}

fn router(state: Arc<AppState>, https: bool) -> Router {
    Router::new()
        .route("/serverinfo", get(h_serverinfo))
        .route("/pair", get(h_pair))
        .route("/pin", get(h_pin))
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
) -> impl IntoResponse {
    // Over the mutual-TLS port the peer is an authenticated (paired) client → PairStatus=1.
    xml(serverinfo::serverinfo_xml(&st.host, https))
}

async fn h_pin(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match q.get("pin").filter(|p| !p.is_empty()) {
        Some(pin) => {
            st.pairing.pin.submit(pin.clone());
            "PIN accepted\n".to_string()
        }
        None => "usage: GET /pin?pin=NNNN\n".to_string(),
    }
}

async fn h_applist(State(_st): State<Arc<AppState>>) -> impl IntoResponse {
    // One app for now: the headless desktop (the wlroots virtual output).
    xml(super::apps::applist_xml())
}

async fn h_launch(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match launch(&st, &q) {
        Ok(session) => {
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

async fn h_resume(State(st): State<Arc<AppState>>) -> impl IntoResponse {
    if st.launch.lock().unwrap().is_some() {
        xml(session_url_xml(&st, "resume"))
    } else {
        xml(error_xml())
    }
}

async fn h_cancel(State(st): State<Arc<AppState>>) -> impl IntoResponse {
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
