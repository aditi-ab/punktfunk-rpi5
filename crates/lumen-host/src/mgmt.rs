//! Management REST API (plan §4) — the control-plane surface a control pane / CLI talks
//! to: host identity + capabilities, runtime status, paired-client management, the pairing
//! PIN flow, and session control. Control plane only — `tokio`/`axum` are permitted here;
//! the per-frame pipeline never touches this module.
//!
//! The API is versioned under `/api/v1` and described by an OpenAPI 3.1 document generated
//! at compile time with `utoipa` — `lumen-host openapi` prints it for client codegen, the
//! running server serves it at `/api/v1/openapi.json` plus interactive docs at `/api/docs`,
//! and a copy is checked in at `docs/api/openapi.json` (a test fails if it drifts, like the
//! cbindgen header).
//!
//! Security: binds loopback by default. A bearer token (`--mgmt-token` / `LUMEN_MGMT_TOKEN`)
//! is enforced on every `/api/v1` route except `/api/v1/health`, and is mandatory for
//! non-loopback binds. The OpenAPI document and docs UI are served unauthenticated (the
//! spec is public knowledge — it lives in this repo).

use crate::encode::Codec;
use crate::gamestream::{
    AppState, APP_VERSION, AUDIO_PORT, CONTROL_PORT, GFE_VERSION, RTSP_PORT, VIDEO_PORT,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use utoipa::{Modify, OpenApi, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use utoipa_scalar::{Scalar, Servable};

/// Default management port — adjacent to the GameStream block (47984…48010), and the same
/// number Sunshine users already associate with "the config UI".
pub const DEFAULT_PORT: u16 = 47990;

/// Management server options (CLI: `serve --mgmt-bind ADDR --mgmt-token TOKEN`).
#[derive(Clone, Debug)]
pub struct Options {
    pub bind: SocketAddr,
    /// Bearer token required on `/api/v1` (except `/health`). `None` ⇒ unauthenticated,
    /// which [`run`] only permits on loopback binds.
    pub token: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            token: None,
        }
    }
}

/// Axum state for the management routes: the shared control-plane state + auth config.
struct MgmtState {
    app: Arc<AppState>,
    token: Option<String>,
    /// The port we serve on, echoed in [`PortMap`] so a client can persist a full endpoint map.
    port: u16,
}

/// Run the management API server (control plane; spawned alongside the nvhttp servers).
pub async fn run(state: Arc<AppState>, opts: Options) -> Result<()> {
    // A blank token is no token: it must neither satisfy the non-loopback guard below nor
    // become a credential an empty `Authorization: Bearer ` header would match.
    let token = opts.token.filter(|t| !t.trim().is_empty());
    if token.is_none() && !opts.bind.ip().is_loopback() {
        bail!(
            "management API bind {} is not loopback — set --mgmt-token (or LUMEN_MGMT_TOKEN) \
             to expose it beyond this machine",
            opts.bind
        );
    }
    tracing::info!(
        addr = %opts.bind,
        auth = if token.is_some() { "bearer" } else { "none (loopback)" },
        "management API listening (docs at /api/docs, spec at /api/v1/openapi.json)"
    );
    let app = app(state, token, opts.bind.port());
    axum_server::bind(opts.bind)
        .serve(app.into_make_service())
        .await
        .context("management API server")
}

/// Compose the full management router (also used directly by the handler tests).
fn app(state: Arc<AppState>, token: Option<String>, port: u16) -> Router {
    let shared = Arc::new(MgmtState {
        app: state,
        token,
        port,
    });
    let (api_routes, api) = api_router_parts();
    api_routes
        .route_layer(middleware::from_fn_with_state(shared.clone(), require_auth))
        .with_state(shared)
        .merge(Scalar::with_url("/api/docs", api.clone()))
        .route(
            "/api/v1/openapi.json",
            get(move || {
                let spec = api.clone();
                async move { Json(spec) }
            }),
        )
}

/// The versioned API routes + the OpenAPI document collected from them. Single source of
/// truth for both the live server and the `openapi` subcommand.
fn api_router_parts() -> (Router<Arc<MgmtState>>, utoipa::openapi::OpenApi) {
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .nest(
            "/api/v1",
            OpenApiRouter::new()
                .routes(routes!(get_health))
                .routes(routes!(get_host_info))
                .routes(routes!(get_status))
                .routes(routes!(list_paired_clients))
                .routes(routes!(unpair_client))
                .routes(routes!(get_pairing_status))
                .routes(routes!(submit_pairing_pin))
                .routes(routes!(stop_session))
                .routes(routes!(request_idr)),
        )
        .split_for_parts()
}

/// The OpenAPI document as pretty JSON — what `lumen-host openapi` prints and what is
/// checked in at `docs/api/openapi.json` for client codegen.
pub fn openapi_json() -> String {
    let (_, api) = api_router_parts();
    let mut json = api.to_pretty_json().expect("serialize OpenAPI document");
    json.push('\n');
    json
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "lumen management API",
        description = "Control-plane API for managing a lumen streaming host: host \
                       capabilities, runtime status, paired clients, the pairing PIN flow, \
                       and session control. Authentication: HTTP bearer token, enforced on \
                       every route except `/api/v1/health` when the host is started with a \
                       management token (mandatory for non-loopback binds)."
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "host", description = "Host identity, capabilities, and liveness"),
        (name = "clients", description = "Paired Moonlight client management"),
        (name = "pairing", description = "Pairing PIN delivery (the out-of-band half of the GameStream pairing handshake)"),
        (name = "session", description = "Active streaming session control"),
    )
)]
struct ApiDoc;

/// Registers the `bearerAuth` scheme and applies it globally (utoipa has no first-class
/// "all operations" shorthand, hence a modifier).
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
        openapi.security = Some(vec![utoipa::openapi::security::SecurityRequirement::new(
            "bearerAuth",
            Vec::<String>::new(),
        )]);
    }
}

// ---------------------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------------------

/// Liveness + version probe.
#[derive(Serialize, ToSchema)]
struct Health {
    /// Always `"ok"` when the host responds.
    #[schema(example = "ok")]
    status: String,
    /// `lumen-host` crate version.
    version: String,
    /// `lumen-core` C ABI version.
    abi_version: u32,
}

/// Host identity and advertised capabilities (static for the life of the process).
#[derive(Serialize, ToSchema)]
struct HostInfo {
    hostname: String,
    /// Stable per-host id (persisted across restarts), matched on pairing.
    uniqueid: String,
    /// Best-effort primary LAN IP.
    local_ip: String,
    /// `lumen-host` crate version.
    version: String,
    /// `lumen-core` C ABI version.
    abi_version: u32,
    /// GameStream host version advertised to Moonlight clients.
    app_version: String,
    /// GFE version advertised to Moonlight clients.
    gfe_version: String,
    /// Codecs the host can encode (NVENC).
    codecs: Vec<ApiCodec>,
    ports: PortMap,
}

/// Every port a client integration may need (Moonlight derives the stream ports from the
/// HTTP base; a control pane should not have to).
#[derive(Serialize, ToSchema)]
struct PortMap {
    /// This management API.
    mgmt: u16,
    /// nvhttp plain HTTP (serverinfo, pairing).
    http: u16,
    /// nvhttp mutual-TLS HTTPS (post-pairing).
    https: u16,
    rtsp: u16,
    video: u16,
    control: u16,
    audio: u16,
}

/// Video codec identifier.
#[derive(Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum ApiCodec {
    H264,
    H265,
    Av1,
}

impl From<Codec> for ApiCodec {
    fn from(c: Codec) -> Self {
        match c {
            Codec::H264 => ApiCodec::H264,
            Codec::H265 => ApiCodec::H265,
            Codec::Av1 => ApiCodec::Av1,
        }
    }
}

/// Live host status (changes as clients launch/end sessions).
#[derive(Serialize, ToSchema)]
struct RuntimeStatus {
    /// True while the video stream thread is running.
    video_streaming: bool,
    /// True while the audio stream thread is running.
    audio_streaming: bool,
    /// True while a pairing handshake is parked waiting for the user's PIN
    /// (submit it via `POST /api/v1/pair/pin`).
    pin_pending: bool,
    /// Number of pinned (paired) client certificates.
    paired_clients: u32,
    /// The active launch session (set by Moonlight's `/launch`, cleared on cancel/stop).
    session: Option<SessionInfo>,
    /// The RTSP-negotiated stream parameters (present once a client has completed ANNOUNCE).
    stream: Option<StreamInfo>,
}

/// Client-requested launch parameters (key material is never exposed here).
#[derive(Serialize, ToSchema)]
struct SessionInfo {
    width: u32,
    height: u32,
    fps: u32,
}

/// RTSP-negotiated stream parameters.
#[derive(Serialize, ToSchema)]
struct StreamInfo {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// Video payload size per packet (bytes).
    packet_size: u32,
    /// Client's parity floor per FEC block (`minRequiredFecPackets`).
    min_fec: u8,
    codec: ApiCodec,
}

/// A paired (certificate-pinned) Moonlight client.
#[derive(Serialize, ToSchema)]
struct PairedClient {
    /// Lowercase hex SHA-256 of the client certificate DER — the client's stable id here.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: String,
    /// Certificate subject (e.g. `CN=NVIDIA GameStream Client`), if the DER parses.
    subject: Option<String>,
    /// Certificate validity start (unix seconds).
    not_before_unix: Option<i64>,
    /// Certificate validity end (unix seconds).
    not_after_unix: Option<i64>,
}

/// Pairing-flow status.
#[derive(Serialize, ToSchema)]
struct PairingStatus {
    /// True while a pairing handshake is parked waiting for the user's PIN.
    pin_pending: bool,
}

/// The PIN Moonlight displays during pairing.
#[derive(Deserialize, ToSchema)]
struct SubmitPin {
    /// 1–16 ASCII digits (Moonlight shows 4).
    #[schema(example = "1234")]
    pin: String,
}

/// Error envelope for every non-2xx response.
#[derive(Serialize, Deserialize, ToSchema)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            error: message.to_string(),
        }),
    )
        .into_response()
}

/// `axum::Json` whose rejections (bad JSON → 400/422, wrong content-type → 415) are
/// rewrapped in the [`ApiError`] envelope, keeping "every non-2xx body is `ApiError`" true.
struct ApiJson<T>(T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    Json<T>: axum::extract::FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(api_error(rejection.status(), &rejection.body_text())),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------------------

/// Bearer-token gate on the `/api/v1` routes. No token configured ⇒ open (loopback-only,
/// enforced in [`run`]); `/api/v1/health` stays open for monitoring probes either way.
async fn require_auth(State(st): State<Arc<MgmtState>>, req: Request, next: Next) -> Response {
    let Some(expected) = st.token.as_deref() else {
        return next.run(req).await;
    };
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await;
    }
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if token_eq(token, expected) => next.run(req).await,
        _ => api_error(StatusCode::UNAUTHORIZED, "missing or invalid bearer token"),
    }
}

/// Compare SHA-256 digests instead of the strings — constant-time with respect to the
/// secret without pulling in a ct-eq dependency.
fn token_eq(presented: &str, expected: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(expected.as_bytes())
}

// ---------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------

/// Liveness probe
///
/// Always available without authentication.
#[utoipa::path(
    get,
    path = "/health",
    tag = "host",
    operation_id = "getHealth",
    // Override the document-global bearerAuth: this route is exempt in `require_auth`.
    security(()),
    responses((status = OK, description = "Host is up", body = Health))
)]
async fn get_health() -> Json<Health> {
    Json(Health {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        abi_version: lumen_core::ABI_VERSION,
    })
}

/// Host identity and capabilities
#[utoipa::path(
    get,
    path = "/host",
    tag = "host",
    operation_id = "getHostInfo",
    responses(
        (status = OK, description = "Host identity, versions, codecs, and port map", body = HostInfo),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn get_host_info(State(st): State<Arc<MgmtState>>) -> Json<HostInfo> {
    let h = &st.app.host;
    Json(HostInfo {
        hostname: h.hostname.clone(),
        uniqueid: h.uniqueid.clone(),
        local_ip: h.local_ip.to_string(),
        version: env!("CARGO_PKG_VERSION").into(),
        abi_version: lumen_core::ABI_VERSION,
        app_version: APP_VERSION.into(),
        gfe_version: GFE_VERSION.into(),
        // Everything NVENC encodes here (mirrors SERVER_CODEC_MODE_SUPPORT = 3843).
        codecs: vec![ApiCodec::H264, ApiCodec::H265, ApiCodec::Av1],
        ports: PortMap {
            mgmt: st.port,
            http: h.http_port,
            https: h.https_port,
            rtsp: RTSP_PORT,
            video: VIDEO_PORT,
            control: CONTROL_PORT,
            audio: AUDIO_PORT,
        },
    })
}

/// Live host status
#[utoipa::path(
    get,
    path = "/status",
    tag = "host",
    operation_id = "getStatus",
    responses(
        (status = OK, description = "Streaming/pairing state and the active session, if any", body = RuntimeStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn get_status(State(st): State<Arc<MgmtState>>) -> Json<RuntimeStatus> {
    let session = st.app.launch.lock().unwrap().map(|l| SessionInfo {
        width: l.width,
        height: l.height,
        fps: l.fps,
    });
    let stream = st.app.stream.lock().unwrap().as_ref().map(|c| StreamInfo {
        width: c.width,
        height: c.height,
        fps: c.fps,
        bitrate_kbps: c.bitrate_kbps,
        packet_size: c.packet_size as u32,
        min_fec: c.min_fec,
        codec: c.codec.into(),
    });
    Json(RuntimeStatus {
        video_streaming: st.app.streaming.load(Ordering::SeqCst),
        audio_streaming: st.app.audio_streaming.load(Ordering::SeqCst),
        pin_pending: st.app.pairing.pin.awaiting_pin(),
        paired_clients: st.app.paired.lock().unwrap().len() as u32,
        session,
        stream,
    })
}

/// List paired clients
#[utoipa::path(
    get,
    path = "/clients",
    tag = "clients",
    operation_id = "listPairedClients",
    responses(
        (status = OK, description = "All certificate-pinned clients", body = [PairedClient]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn list_paired_clients(State(st): State<Arc<MgmtState>>) -> Json<Vec<PairedClient>> {
    let ders = st.app.paired.lock().unwrap().clone();
    Json(ders.iter().map(|der| client_info(der)).collect())
}

fn client_info(der: &[u8]) -> PairedClient {
    let fingerprint = hex::encode(Sha256::digest(der));
    match x509_parser::parse_x509_certificate(der) {
        Ok((_, x509)) => PairedClient {
            fingerprint,
            subject: Some(x509.subject().to_string()),
            not_before_unix: Some(x509.validity().not_before.timestamp()),
            not_after_unix: Some(x509.validity().not_after.timestamp()),
        },
        Err(_) => PairedClient {
            fingerprint,
            subject: None,
            not_before_unix: None,
            not_after_unix: None,
        },
    }
}

/// Unpair a client
///
/// Removes the client's certificate from the pairing store. Caveat: the nvhttp TLS layer
/// does not yet reject unlisted certificates (`gamestream/tls.rs` accepts any well-formed
/// client cert — a planned hardening step), so until that lands this removes the client
/// from the listing without severing its ability to reconnect.
#[utoipa::path(
    delete,
    path = "/clients/{fingerprint}",
    tag = "clients",
    operation_id = "unpairClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 fingerprint of the client certificate DER (64 chars, case-insensitive)")
    ),
    responses(
        (status = NO_CONTENT, description = "Client unpaired"),
        (status = BAD_REQUEST, description = "Malformed fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No paired client with that fingerprint", body = ApiError),
    )
)]
async fn unpair_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
) -> Response {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "fingerprint must be the 64-char hex SHA-256 of the client certificate DER",
        );
    }
    let mut paired = st.app.paired.lock().unwrap();
    let before = paired.len();
    paired.retain(|der| !hex::encode(Sha256::digest(der)).eq_ignore_ascii_case(&fingerprint));
    if paired.len() < before {
        tracing::info!(fingerprint, "management API: client unpaired");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "no paired client with that fingerprint",
        )
    }
}

/// Pairing-flow status
///
/// Poll this to know when to prompt the user for the PIN Moonlight displays.
#[utoipa::path(
    get,
    path = "/pair",
    tag = "pairing",
    operation_id = "getPairingStatus",
    responses(
        (status = OK, description = "Whether a pairing handshake is waiting for a PIN", body = PairingStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn get_pairing_status(State(st): State<Arc<MgmtState>>) -> Json<PairingStatus> {
    Json(PairingStatus {
        pin_pending: st.app.pairing.pin.awaiting_pin(),
    })
}

/// Submit the pairing PIN
///
/// Delivers the PIN the Moonlight client is displaying, completing the out-of-band half
/// of the pairing handshake.
#[utoipa::path(
    post,
    path = "/pair/pin",
    tag = "pairing",
    operation_id = "submitPairingPin",
    request_body = SubmitPin,
    responses(
        (status = NO_CONTENT, description = "PIN delivered to the waiting handshake"),
        (status = BAD_REQUEST, description = "Malformed PIN or unparseable JSON body", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No pairing handshake is waiting for a PIN", body = ApiError),
        (status = UNSUPPORTED_MEDIA_TYPE, description = "Body is not application/json", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "JSON body does not match the schema", body = ApiError),
    )
)]
async fn submit_pairing_pin(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<SubmitPin>,
) -> Response {
    let pin = req.pin.trim();
    if pin.is_empty() || pin.len() > 16 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return api_error(StatusCode::BAD_REQUEST, "pin must be 1-16 ASCII digits");
    }
    if !st.app.pairing.pin.awaiting_pin() {
        // Refusing (rather than parking the PIN) prevents a stale PIN from silently
        // satisfying a *future* pairing attempt.
        return api_error(
            StatusCode::CONFLICT,
            "no pairing handshake is waiting for a PIN",
        );
    }
    st.app.pairing.pin.submit(pin.to_string());
    StatusCode::NO_CONTENT.into_response()
}

/// Stop the active session
///
/// Kicks the connected client: stops the video/audio stream threads and clears the launch
/// state. Idempotent — succeeds even when nothing is streaming.
#[utoipa::path(
    delete,
    path = "/session",
    tag = "session",
    operation_id = "stopSession",
    responses(
        (status = NO_CONTENT, description = "Session stopped (or none was active)"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn stop_session(State(st): State<Arc<MgmtState>>) -> StatusCode {
    let was_streaming = st.app.streaming.swap(false, Ordering::SeqCst);
    st.app.audio_streaming.store(false, Ordering::SeqCst);
    *st.app.launch.lock().unwrap() = None;
    *st.app.stream.lock().unwrap() = None;
    tracing::info!(was_streaming, "management API: session stopped");
    StatusCode::NO_CONTENT
}

/// Force a keyframe
///
/// Asks the encoder for an IDR frame on the active video stream (what a client requests
/// after unrecoverable loss — exposed for debugging).
#[utoipa::path(
    post,
    path = "/session/idr",
    tag = "session",
    operation_id = "requestIdr",
    responses(
        (status = ACCEPTED, description = "Keyframe requested"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No active video stream", body = ApiError),
    )
)]
async fn request_idr(State(st): State<Arc<MgmtState>>) -> Response {
    if !st.app.streaming.load(Ordering::SeqCst) {
        return api_error(StatusCode::CONFLICT, "no active video stream");
    }
    st.app.force_idr.store(true, Ordering::SeqCst);
    StatusCode::ACCEPTED.into_response()
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gamestream::{cert::ServerIdentity, Host, LaunchSession, HTTPS_PORT, HTTP_PORT};
    use axum::body::Body;
    use http_body_util::BodyExt;
    use std::net::{IpAddr, Ipv4Addr};
    use tower::ServiceExt;

    fn test_state() -> Arc<AppState> {
        let host = Host {
            hostname: "test-host".into(),
            uniqueid: "deadbeef".into(),
            local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
        };
        let identity = ServerIdentity::ephemeral().expect("ephemeral identity");
        Arc::new(AppState::new(host, identity))
    }

    fn test_app(state: Arc<AppState>, token: Option<&str>) -> Router {
        app(state, token.map(String::from), DEFAULT_PORT)
    }

    async fn send(app: &Router, req: axum::http::Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    fn get_req(path: &str) -> axum::http::Request<Body> {
        axum::http::Request::get(path).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn health_is_open_and_versioned() {
        let app = test_app(test_state(), None);
        let (status, body) = send(&app, get_req("/api/v1/health")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["abi_version"], lumen_core::ABI_VERSION);
    }

    #[tokio::test]
    async fn bearer_token_is_enforced() {
        let app = test_app(test_state(), Some("sekrit"));

        // No/wrong token → 401 with the error envelope.
        let (status, body) = send(&app, get_req("/api/v1/status")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("bearer"));
        let wrong = axum::http::Request::get("/api/v1/status")
            .header("authorization", "Bearer nope")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED);

        // Right token → 200.
        let right = axum::http::Request::get("/api/v1/status")
            .header("authorization", "Bearer sekrit")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, right).await.0, StatusCode::OK);

        // Health + the spec/docs stay open.
        assert_eq!(
            send(&app, get_req("/api/v1/health")).await.0,
            StatusCode::OK
        );
        assert_eq!(
            send(&app, get_req("/api/v1/openapi.json")).await.0,
            StatusCode::OK
        );
        let docs = app.clone().oneshot(get_req("/api/docs")).await.unwrap();
        assert_eq!(docs.status(), StatusCode::OK);
        let html = docs.into_body().collect().await.unwrap().to_bytes();
        assert!(
            html.starts_with(b"<!doctype html>"),
            "Scalar UI should serve HTML"
        );
    }

    #[tokio::test]
    async fn host_info_reports_identity_and_ports() {
        let app = test_app(test_state(), None);
        let (status, body) = send(&app, get_req("/api/v1/host")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["hostname"], "test-host");
        assert_eq!(body["uniqueid"], "deadbeef");
        assert_eq!(body["ports"]["http"], HTTP_PORT);
        assert_eq!(body["ports"]["mgmt"], DEFAULT_PORT);
        assert_eq!(body["codecs"], serde_json::json!(["h264", "h265", "av1"]));
    }

    #[tokio::test]
    async fn status_reflects_runtime_state() {
        let state = test_state();
        let app = test_app(state.clone(), None);

        let (_, body) = send(&app, get_req("/api/v1/status")).await;
        assert_eq!(body["video_streaming"], false);
        assert_eq!(body["session"], serde_json::Value::Null);

        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 1,
            width: 2560,
            height: 1440,
            fps: 120,
        });
        state.streaming.store(true, Ordering::SeqCst);

        let (_, body) = send(&app, get_req("/api/v1/status")).await;
        assert_eq!(body["video_streaming"], true);
        assert_eq!(body["session"]["width"], 2560);
        assert_eq!(body["session"]["fps"], 120);
        // Key material must never appear anywhere in the response.
        assert!(!body.to_string().contains("gcm"));
    }

    #[tokio::test]
    async fn paired_clients_list_and_unpair() {
        let state = test_state();
        let app = test_app(state.clone(), None);

        // Pin the host's own cert DER as a stand-in client.
        let (_, pem) =
            x509_parser::pem::parse_x509_pem(state.identity.cert_pem.as_bytes()).unwrap();
        let der = pem.contents.clone();
        let fingerprint = hex::encode(Sha256::digest(&der));
        state.paired.lock().unwrap().push(der);

        let (status, body) = send(&app, get_req("/api/v1/clients")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["fingerprint"], fingerprint);
        assert_eq!(body[0]["subject"], "CN=lumen");

        // Malformed fingerprint → 400.
        let bad = axum::http::Request::delete("/api/v1/clients/zz")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, bad).await.0, StatusCode::BAD_REQUEST);

        // Unpair (uppercase hex must match too) → 204, list empties, second delete → 404.
        let del = |fp: String| {
            axum::http::Request::delete(format!("/api/v1/clients/{fp}"))
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            send(&app, del(fingerprint.to_uppercase())).await.0,
            StatusCode::NO_CONTENT
        );
        let (_, body) = send(&app, get_req("/api/v1/clients")).await;
        assert_eq!(body, serde_json::json!([]));
        assert_eq!(send(&app, del(fingerprint)).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn submit_pin_validates_and_requires_pending_pairing() {
        let app = test_app(test_state(), None);
        let post = |body: &str| {
            axum::http::Request::post("/api/v1/pair/pin")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap()
        };

        // Malformed PINs → 400.
        assert_eq!(
            send(&app, post(r#"{"pin":""}"#)).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&app, post(r#"{"pin":"12ab"}"#)).await.0,
            StatusCode::BAD_REQUEST
        );

        // Well-formed but nothing waiting → 409 (a parked stale PIN would poison the
        // next pairing attempt).
        assert_eq!(
            send(&app, post(r#"{"pin":"1234"}"#)).await.0,
            StatusCode::CONFLICT
        );

        // axum's own body rejections must still wear the ApiError envelope (ApiJson).
        let (status, body) = send(&app, post("{not json")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string(), "syntax error: {body}");
        let (status, body) = send(&app, post(r#"{"wrong":"shape"}"#)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"].is_string(), "schema mismatch: {body}");
        let no_ct = axum::http::Request::post("/api/v1/pair/pin")
            .body(Body::from(r#"{"pin":"1234"}"#))
            .unwrap();
        let (status, body) = send(&app, no_ct).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(body["error"].is_string(), "media type: {body}");
    }

    /// A blank token must not satisfy the "non-loopback requires a token" guard.
    #[tokio::test]
    async fn blank_token_rejected_for_public_bind() {
        let opts = Options {
            bind: "0.0.0.0:0".parse().unwrap(),
            token: Some("   ".into()),
        };
        let err = run(test_state(), opts).await.unwrap_err();
        assert!(err.to_string().contains("not loopback"), "{err}");
    }

    #[tokio::test]
    async fn stop_session_clears_runtime_state() {
        let state = test_state();
        let app = test_app(state.clone(), None);
        state.streaming.store(true, Ordering::SeqCst);
        state.audio_streaming.store(true, Ordering::SeqCst);
        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
        });

        let del = axum::http::Request::delete("/api/v1/session")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
        assert!(!state.streaming.load(Ordering::SeqCst));
        assert!(!state.audio_streaming.load(Ordering::SeqCst));
        assert!(state.launch.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn idr_requires_an_active_stream() {
        let state = test_state();
        let app = test_app(state.clone(), None);
        let post = || {
            axum::http::Request::post("/api/v1/session/idr")
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(send(&app, post()).await.0, StatusCode::CONFLICT);

        state.streaming.store(true, Ordering::SeqCst);
        assert_eq!(send(&app, post()).await.0, StatusCode::ACCEPTED);
        assert!(state.force_idr.load(Ordering::SeqCst));
    }

    /// The OpenAPI document lists every route with a unique operationId (codegen relies
    /// on both), and the checked-in copy is current.
    #[test]
    fn openapi_document_is_complete_and_checked_in() {
        let json = openapi_json();
        let doc: serde_json::Value = serde_json::from_str(&json).unwrap();

        let paths = doc["paths"].as_object().unwrap();
        for p in [
            "/api/v1/health",
            "/api/v1/host",
            "/api/v1/status",
            "/api/v1/clients",
            "/api/v1/clients/{fingerprint}",
            "/api/v1/pair",
            "/api/v1/pair/pin",
            "/api/v1/session",
            "/api/v1/session/idr",
        ] {
            assert!(paths.contains_key(p), "spec is missing {p}");
        }

        let mut op_ids: Vec<&str> = paths
            .values()
            .flat_map(|ops| ops.as_object().unwrap().values())
            .filter_map(|op| op["operationId"].as_str())
            .collect();
        let total = op_ids.len();
        op_ids.sort_unstable();
        op_ids.dedup();
        assert_eq!(total, op_ids.len(), "duplicate operationIds");
        assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
        // The health probe overrides the document-global bearer requirement (the server
        // exempts it in `require_auth`; the spec must agree).
        assert_eq!(
            doc["paths"]["/api/v1/health"]["get"]["security"],
            serde_json::json!([{}])
        );

        let checked_in = include_str!("../../../docs/api/openapi.json");
        assert_eq!(
            json.trim(),
            checked_in.trim(),
            "docs/api/openapi.json is stale — regenerate with: \
             cargo run -p lumen-host -- openapi > docs/api/openapi.json"
        );
    }
}
