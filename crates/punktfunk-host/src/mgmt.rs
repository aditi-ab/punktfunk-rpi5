//! Management REST API (plan §4) — the control-plane surface a control pane / CLI talks
//! to: host identity + capabilities, runtime status, paired-client management, the pairing
//! PIN flow, and session control. Control plane only — `tokio`/`axum` are permitted here;
//! the per-frame pipeline never touches this module.
//!
//! The API is versioned under `/api/v1` and described by an OpenAPI 3.1 document generated
//! at compile time with `utoipa` — `punktfunk-host openapi` prints it for client codegen, the
//! running server serves it at `/api/v1/openapi.json` plus interactive docs at `/api/docs`,
//! and a copy is checked in at `docs/api/openapi.json` (a test fails if it drifts, like the
//! cbindgen header).
//!
//! Security: binds loopback by default, serves HTTPS with the host's identity cert, and requires
//! auth on every `/api/v1` route except `/api/v1/health` — **always**, even on loopback. A paired
//! native client authenticates by its mTLS cert; everyone else by a bearer token (`--mgmt-token` /
//! `PUNKTFUNK_MGMT_TOKEN`, else auto-generated + persisted to `~/.config/punktfunk/mgmt-token`). The
//! OpenAPI document and docs UI are served unauthenticated (the spec is public — it lives in this repo).

use crate::encode::Codec;
use crate::gamestream::{
    AppState, APP_VERSION, AUDIO_PORT, CONTROL_PORT, GFE_VERSION, RTSP_PORT, VIDEO_PORT,
};
use anyhow::{Context, Result};
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
    /// Native (punktfunk/1) pairing — shared with the QUIC host when the unified `serve --native`
    /// runs it. `None` ⇒ GameStream-only host (the native endpoints report `enabled: false`).
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    token: Option<String>,
    /// The port we serve on, echoed in [`PortMap`] so a client can persist a full endpoint map.
    port: u16,
}

/// Run the management API server (control plane; spawned alongside the nvhttp servers). `native`
/// is the shared punktfunk/1 pairing handle when the unified host runs the native QUIC server.
pub async fn run(
    state: Arc<AppState>,
    opts: Options,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
) -> Result<()> {
    // The mgmt API is HTTPS + token-authenticated ALWAYS (even on loopback): `parse_serve`
    // guarantees a token (CLI flag / env / persisted ~/.config/punktfunk/mgmt-token / generated).
    // A blank token is treated as none — fail loudly rather than ever serve unauthenticated.
    let token = opts
        .token
        .filter(|t| !t.trim().is_empty())
        .context("management API has no token — internal error: parse_serve must provide one")?;
    // Serve over HTTPS with the host's persistent identity (the cert clients already pin) and
    // OPTIONAL client-cert auth: a paired native client presents its cert (authorized by
    // fingerprint, no token), a browser presents none and uses the bearer token. See `require_auth`.
    let identity = crate::gamestream::cert::ServerIdentity::load_or_create()
        .context("load host identity for the management API TLS")?;
    let tls = crate::gamestream::tls::server_config_optional_client(
        &identity.cert_pem,
        &identity.key_pem,
    )
    .context("management API TLS config")?;
    tracing::info!(
        addr = %opts.bind,
        auth = "mTLS (paired cert) or bearer (required)",
        "management API listening over HTTPS (docs at /api/docs, spec at /api/v1/openapi.json)"
    );
    let app = app(state, Some(token), opts.bind.port(), native);
    serve_https(opts.bind, app, tls).await
}

/// SHA-256 of the peer's client certificate (hex), injected per-connection into each request's
/// extensions by [`serve_https`]; `None` when the peer presented no client cert. `require_auth`
/// authorizes a request whose fingerprint is in the paired store.
#[derive(Clone)]
struct PeerCertFingerprint(Option<String>);

/// HTTPS server for the mgmt API. axum-server can't surface the client cert to a handler, so this
/// runs the rustls handshake itself (via tokio-rustls), reads the verified peer certificate, and
/// serves the axum `Router` over hyper with the peer's fingerprint attached to every request.
async fn serve_https(bind: SocketAddr, app: Router, tls: Arc<rustls::ServerConfig>) -> Result<()> {
    use tower::ServiceExt;
    let acceptor = tokio_rustls::TlsAcceptor::from(tls);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind management API {bind}"))?;
    loop {
        let (tcp, _peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "management API accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                // A failed handshake is routine (port scan, a browser bailing on the self-signed
                // cert, a client cert we'd still accept but the peer hung up) — not fatal.
                Err(_) => return,
            };
            // The verified peer cert (the verifier accepts any well-formed one; we authorize by
            // fingerprint in the auth layer) → its SHA-256, matched against the paired store.
            let fp = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(c.as_ref())));
            let peer = PeerCertFingerprint(fp);
            let svc =
                hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let app = app.clone();
                    let peer = peer.clone();
                    async move {
                        let mut req = req.map(axum::body::Body::new);
                        req.extensions_mut().insert(peer);
                        app.oneshot(req).await // Router error is Infallible
                    }
                });
            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let _ =
                hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection_with_upgrades(io, svc)
                    .await;
        });
    }
}

/// Compose the full management router (also used directly by the handler tests).
fn app(
    state: Arc<AppState>,
    token: Option<String>,
    port: u16,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
) -> Router {
    let shared = Arc::new(MgmtState {
        app: state,
        native,
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
                .routes(routes!(list_compositors))
                .routes(routes!(get_status))
                .routes(routes!(list_paired_clients))
                .routes(routes!(unpair_client))
                .routes(routes!(get_pairing_status))
                .routes(routes!(submit_pairing_pin))
                .routes(routes!(get_native_pairing))
                .routes(routes!(arm_native_pairing))
                .routes(routes!(disarm_native_pairing))
                .routes(routes!(list_native_clients))
                .routes(routes!(unpair_native_client))
                .routes(routes!(list_pending_devices))
                .routes(routes!(approve_pending_device))
                .routes(routes!(deny_pending_device))
                .routes(routes!(stop_session))
                .routes(routes!(request_idr))
                .routes(routes!(get_library))
                .routes(routes!(create_custom_game))
                .routes(routes!(update_custom_game, delete_custom_game)),
        )
        .split_for_parts()
}

/// The OpenAPI document as pretty JSON — what `punktfunk-host openapi` prints and what is
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
        title = "punktfunk management API",
        description = "Control-plane API for managing a punktfunk streaming host: host \
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
        (name = "native", description = "Native punktfunk/1 pairing: arm a window, display the host PIN, manage paired devices"),
        (name = "session", description = "Active streaming session control"),
        (name = "library", description = "Game library: installed-store titles (Steam) plus user-curated custom entries"),
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
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
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
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
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

/// Native (punktfunk/1) pairing status. Unlike GameStream, the **host** mints the PIN (the SPAKE2
/// ceremony needs it client-side first), so the console **displays** `pin` for the user to enter on
/// their device — armed on demand for a short window.
#[derive(Serialize, ToSchema)]
struct NativePairStatus {
    /// Whether the native host is running (the unified host started with `--native`).
    enabled: bool,
    /// True while a pairing window is open.
    armed: bool,
    /// The PIN to display while armed (null when disarmed).
    #[schema(example = "1234")]
    pin: Option<String>,
    /// Seconds left in the window (null = disarmed, or armed with no expiry via the CLI flag).
    expires_in_secs: Option<u64>,
    /// Number of paired native clients.
    paired_clients: u32,
}

/// Arm-native-pairing request body.
#[derive(Deserialize, ToSchema)]
struct ArmNativePairing {
    /// Window length in seconds (default 120; clamped to 15–600).
    #[schema(example = 120)]
    ttl_secs: Option<u32>,
}

/// A paired native (punktfunk/1) client.
#[derive(Serialize, ToSchema)]
struct NativeClient {
    /// The name the client supplied when pairing.
    #[schema(example = "Living Room iPad")]
    name: String,
    /// Hex SHA-256 of the client certificate — its stable id here.
    fingerprint: String,
}

/// An unpaired device that tried to connect while the host requires pairing — awaiting
/// **delegated approval** (approve it here instead of fetching the host PIN out of band).
#[derive(Serialize, ToSchema)]
struct PendingDevice {
    /// Id to address approve/deny (per-process; entries expire after ~10 minutes).
    id: u32,
    /// Best-effort device label (the client's own name, else fingerprint-derived).
    #[schema(example = "Enrico's MacBook")]
    name: String,
    /// Hex SHA-256 of the device's certificate — what approval pins.
    fingerprint: String,
    /// Seconds since the device last knocked.
    age_secs: u64,
}

/// Approve-pending-device request body. Send `{}` to keep the device's own name.
#[derive(Deserialize, ToSchema)]
struct ApprovePending {
    /// Operator-chosen label for the device (defaults to the name it knocked with).
    #[schema(example = "Living Room TV")]
    name: Option<String>,
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

/// Auth gate on the `/api/v1` routes: a paired client cert (mTLS) or the bearer token — required
/// always (the host runs with a token by construction). `/api/v1/health` stays open for probes.
async fn require_auth(State(st): State<Arc<MgmtState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await; // liveness probe is always open
    }
    // A paired native client authenticates by its mTLS certificate — the same identity + trust the
    // QUIC data plane uses — so it never needs a bearer token. The fingerprint is attached by
    // `serve_https` from the verified peer cert; we authorize it iff it's in the paired store.
    if let Some(PeerCertFingerprint(Some(fp))) = req.extensions().get::<PeerCertFingerprint>() {
        if st.native.as_ref().is_some_and(|n| n.is_paired(fp)) {
            return next.run(req).await;
        }
    }
    // Otherwise require the bearer token (the web console / admin). `run` always passes a token, so
    // no-token means a misconfigured caller (e.g. a test constructing `app` directly) — deny.
    let Some(expected) = st.token.as_deref() else {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if token_eq(token, expected) => next.run(req).await,
        _ => api_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid credentials (a paired client cert, or a bearer token)",
        ),
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
        version: env!("PUNKTFUNK_VERSION").into(),
        abi_version: punktfunk_core::ABI_VERSION,
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
        version: env!("PUNKTFUNK_VERSION").into(),
        abi_version: punktfunk_core::ABI_VERSION,
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

/// A compositor backend the host can drive a virtual output on, and whether it's usable now.
#[derive(Serialize, ToSchema)]
struct AvailableCompositor {
    /// Stable identifier (`"kwin"` | `"wlroots"` | `"mutter"` | `"gamescope"`) — pass this to a
    /// client's `--compositor` flag.
    id: String,
    /// Human-readable label for UIs.
    label: String,
    /// Usable on this host right now: the live session's own compositor, or gamescope wherever
    /// its binary is installed.
    available: bool,
    /// True for the backend an `Auto` (unspecified) request resolves to right now.
    default: bool,
}

/// Available compositor backends
///
/// Lists every backend the host knows how to drive, flags which are usable right now, and marks
/// the one an unspecified (`Auto`) client request resolves to. Clients pass an `id` to their
/// `--compositor` flag (or `PUNKTFUNK_COMPOSITOR_*` over the C ABI) to request it.
#[utoipa::path(
    get,
    path = "/compositors",
    tag = "host",
    operation_id = "listCompositors",
    responses(
        (status = OK, description = "Compositor backends with availability + the auto-detected default", body = [AvailableCompositor]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn list_compositors() -> Json<Vec<AvailableCompositor>> {
    let available = crate::vdisplay::available();
    let default = crate::vdisplay::detect().ok();
    Json(
        crate::vdisplay::Compositor::all()
            .into_iter()
            .map(|c| AvailableCompositor {
                id: c.id().into(),
                label: c.label().into(),
                available: available.contains(&c),
                default: default == Some(c),
            })
            .collect(),
    )
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

fn native_status(st: &MgmtState) -> NativePairStatus {
    match &st.native {
        Some(np) => {
            let s = np.status();
            NativePairStatus {
                enabled: true,
                armed: s.armed,
                pin: s.pin,
                expires_in_secs: s.expires_in_secs,
                paired_clients: s.paired_clients,
            }
        }
        None => NativePairStatus {
            enabled: false,
            armed: false,
            pin: None,
            expires_in_secs: None,
            paired_clients: 0,
        },
    }
}

/// Native pairing status
///
/// The native (punktfunk/1) pairing window. Poll while armed to show the PIN + countdown.
/// `enabled: false` means this host runs GameStream only (no `--native`).
#[utoipa::path(
    get,
    path = "/native/pair",
    tag = "native",
    operation_id = "getNativePairing",
    responses(
        (status = OK, description = "Native pairing status", body = NativePairStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn get_native_pairing(State(st): State<Arc<MgmtState>>) -> Json<NativePairStatus> {
    Json(native_status(&st))
}

/// Arm native pairing
///
/// Opens a pairing window and mints a fresh PIN to display. The user enters it on their device
/// within `ttl_secs`; the device then appears in the native client list.
#[utoipa::path(
    post,
    path = "/native/pair/arm",
    tag = "native",
    operation_id = "armNativePairing",
    request_body = ArmNativePairing,
    responses(
        (status = OK, description = "Pairing armed; the response carries the PIN to display", body = NativePairStatus),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled (run `serve --native`)", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn arm_native_pairing(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<ArmNativePairing>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "native host not enabled (run `serve --native`)",
        );
    };
    let ttl = req.ttl_secs.unwrap_or(120).clamp(15, 600);
    let _pin = np.arm(std::time::Duration::from_secs(ttl as u64));
    tracing::info!(ttl_secs = ttl, "management API: native pairing armed");
    Json(native_status(&st)).into_response()
}

/// Disarm native pairing
///
/// Closes the pairing window immediately (no new ceremonies accepted).
#[utoipa::path(
    delete,
    path = "/native/pair",
    tag = "native",
    operation_id = "disarmNativePairing",
    responses(
        (status = NO_CONTENT, description = "Pairing disarmed"),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn disarm_native_pairing(State(st): State<Arc<MgmtState>>) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    np.disarm();
    StatusCode::NO_CONTENT.into_response()
}

/// List native paired clients
#[utoipa::path(
    get,
    path = "/native/clients",
    tag = "native",
    operation_id = "listNativeClients",
    responses(
        (status = OK, description = "Paired native clients", body = [NativeClient]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn list_native_clients(State(st): State<Arc<MgmtState>>) -> Json<Vec<NativeClient>> {
    let clients = match &st.native {
        Some(np) => np
            .list()
            .into_iter()
            .map(|c| NativeClient {
                name: c.name,
                fingerprint: c.fingerprint,
            })
            .collect(),
        None => Vec::new(),
    };
    Json(clients)
}

/// Unpair a native client
///
/// Removes a punktfunk/1 client from the native trust store by fingerprint.
#[utoipa::path(
    delete,
    path = "/native/clients/{fingerprint}",
    tag = "native",
    operation_id = "unpairNativeClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 of the client certificate (case-insensitive)")
    ),
    responses(
        (status = NO_CONTENT, description = "Client unpaired"),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = NOT_FOUND, description = "No paired native client with that fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn unpair_native_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.remove(&fingerprint) {
        Ok(true) => {
            tracing::info!(fingerprint, "management API: native client unpaired");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "no paired native client with that fingerprint",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not persist trust store: {e}"),
        ),
    }
}

/// List devices awaiting pairing approval
///
/// Unpaired devices that tried to connect while the host requires pairing. Approve one to pair
/// it without a PIN (delegated approval); entries expire after ~10 minutes.
#[utoipa::path(
    get,
    path = "/native/pending",
    tag = "native",
    operation_id = "listPendingDevices",
    responses(
        (status = OK, description = "Devices awaiting approval (empty when none, or when the \
                                     native host is not enabled)", body = Vec<PendingDevice>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn list_pending_devices(State(st): State<Arc<MgmtState>>) -> Json<Vec<PendingDevice>> {
    let pending = st
        .native
        .as_ref()
        .map(|np| np.pending())
        .unwrap_or_default();
    Json(
        pending
            .into_iter()
            .map(|p| PendingDevice {
                id: p.id,
                name: p.name,
                fingerprint: p.fingerprint,
                age_secs: p.age_secs,
            })
            .collect(),
    )
}

/// Approve a pending device
///
/// Pairs the device's certificate fingerprint — it can connect immediately (no PIN). Optionally
/// relabel it via the body; send `{}` to keep the name it knocked with.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/approve",
    tag = "native",
    operation_id = "approvePendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    request_body = ApprovePending,
    responses(
        (status = OK, description = "Device paired", body = NativeClient),
        (status = NOT_FOUND, description = "No pending request with that id (expired?)", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the trust store", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn approve_pending_device(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<u32>,
    ApiJson(req): ApiJson<ApprovePending>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.approve_pending(id, req.name.as_deref()) {
        Ok(Some(client)) => {
            tracing::info!(name = %client.name, fingerprint = %client.fingerprint,
                "management API: pending device approved (delegated pairing)");
            Json(NativeClient {
                name: client.name,
                fingerprint: client.fingerprint,
            })
            .into_response()
        }
        Ok(None) => api_error(
            StatusCode::NOT_FOUND,
            "no pending request with that id (it may have expired — have the device retry)",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not persist trust store: {e}"),
        ),
    }
}

/// Deny a pending device
///
/// Drops the request. Not a blocklist — the device's next attempt knocks again.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/deny",
    tag = "native",
    operation_id = "denyPendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    responses(
        (status = NO_CONTENT, description = "Request dropped"),
        (status = NOT_FOUND, description = "No pending request with that id", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn deny_pending_device(State(st): State<Arc<MgmtState>>, Path(id): Path<u32>) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    if np.deny_pending(id) {
        tracing::info!(id, "management API: pending device denied");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, "no pending request with that id")
    }
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
// Library
// ---------------------------------------------------------------------------------------

/// List the game library
///
/// Every installed-store title (Steam, read from the host's local files — no Steam API key)
/// merged with the user's custom entries, sorted by title. Artwork fields are URLs the client
/// fetches directly (the public Steam CDN for Steam titles).
#[utoipa::path(
    get,
    path = "/library",
    tag = "library",
    operation_id = "getLibrary",
    responses(
        (status = OK, description = "Unified library across all stores", body = [crate::library::GameEntry]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn get_library() -> Json<Vec<crate::library::GameEntry>> {
    Json(crate::library::all_games())
}

/// Add a custom library entry
///
/// Creates a user-curated title (e.g. a non-Steam game, an emulator, a ROM) with caller-supplied
/// artwork URLs. The host assigns a stable id, returned in the body.
#[utoipa::path(
    post,
    path = "/library/custom",
    tag = "library",
    operation_id = "createCustomGame",
    request_body = crate::library::CustomInput,
    responses(
        (status = CREATED, description = "Entry created", body = crate::library::CustomEntry),
        (status = BAD_REQUEST, description = "Empty title", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
async fn create_custom_game(ApiJson(input): ApiJson<crate::library::CustomInput>) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    match crate::library::add_custom(input) {
        Ok(entry) => (StatusCode::CREATED, Json(entry)).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Update a custom library entry
#[utoipa::path(
    put,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "updateCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    request_body = crate::library::CustomInput,
    responses(
        (status = OK, description = "Entry updated", body = crate::library::CustomEntry),
        (status = BAD_REQUEST, description = "Empty title", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
async fn update_custom_game(
    Path(id): Path<String>,
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    match crate::library::update_custom(&id, input) {
        Ok(Some(entry)) => Json(entry).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no custom entry with that id"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Delete a custom library entry
#[utoipa::path(
    delete,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "deleteCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    responses(
        (status = NO_CONTENT, description = "Entry deleted"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
async fn delete_custom_game(Path(id): Path<String>) -> Response {
    match crate::library::delete_custom(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no custom entry with that id"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
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

    // The mgmt API now always requires auth, so the router always has a token. A test that passes
    // `None` gets the default "test-secret" (and `send` auto-attaches the matching bearer); a test
    // that passes an explicit token exercises a mismatch (e.g. `bearer_token_is_enforced`).
    fn test_app(state: Arc<AppState>, token: Option<&str>) -> Router {
        app(
            state,
            Some(token.unwrap_or("test-secret").to_string()),
            DEFAULT_PORT,
            None,
        )
    }

    fn test_app_native(
        state: Arc<AppState>,
        np: Arc<crate::native_pairing::NativePairing>,
    ) -> Router {
        // Auth required always; the paired-cert tests inject a fingerprint (cert branch wins), the
        // rest authenticate via the `send`-attached default bearer.
        app(
            state,
            Some("test-secret".to_string()),
            DEFAULT_PORT,
            Some(np),
        )
    }

    async fn send(
        app: &Router,
        mut req: axum::http::Request<Body>,
    ) -> (StatusCode, serde_json::Value) {
        // Auto-attach the default bearer unless the test set its own Authorization (e.g. the
        // mismatch cases in `bearer_token_is_enforced`). Open routes ignore it; authed routes
        // accept it against the `test-secret` default token.
        if !req
            .headers()
            .contains_key(axum::http::header::AUTHORIZATION)
        {
            req.headers_mut().insert(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer test-secret"),
            );
        }
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
        assert_eq!(body["abi_version"], punktfunk_core::ABI_VERSION);
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
    async fn compositors_lists_all_backends_with_flags() {
        let app = test_app(test_state(), None);
        let (status, body) = send(&app, get_req("/api/v1/compositors")).await;
        assert_eq!(status, StatusCode::OK);
        let arr = body.as_array().expect("array");
        // Every backend the host knows, in stable order.
        let ids: Vec<&str> = arr.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["kwin", "gamescope", "mutter", "wlroots"]);
        for c in arr {
            assert!(c["available"].is_boolean());
            assert!(c["default"].is_boolean());
            assert!(c["label"].as_str().is_some_and(|s| !s.is_empty()));
        }
        // At most one backend is the auto-detect default (none, if the test env has no desktop).
        assert!(arr.iter().filter(|c| c["default"] == true).count() <= 1);
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
            appid: 1,
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
        // Isolate from any real paired store on the dev box: AppState::new loads
        // ~/.config/punktfunk/paired.json, so clear it before seeding our stand-in — otherwise
        // a real GameStream-paired client lands at body[0] and this assertion sees its hash.
        {
            let mut p = state.paired.lock().unwrap();
            p.clear();
            p.push(der);
        }

        let (status, body) = send(&app, get_req("/api/v1/clients")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["fingerprint"], fingerprint);
        assert_eq!(body[0]["subject"], "CN=punktfunk");

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

    /// A blank token is treated as no token: the mgmt API requires auth always (even on loopback),
    /// so `run` refuses to start unauthenticated rather than serve open.
    #[tokio::test]
    async fn blank_token_rejected() {
        let opts = Options {
            bind: "127.0.0.1:0".parse().unwrap(),
            token: Some("   ".into()),
        };
        let err = run(test_state(), opts, None).await.unwrap_err();
        assert!(err.to_string().contains("no token"), "{err}");
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
            appid: 1,
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
        // Compare content, not line-ending style: the generated `json` is LF (serde_json), but git
        // may check the file out CRLF on Windows.
        assert_eq!(
            json.trim().replace('\r', ""),
            checked_in.trim().replace('\r', ""),
            "docs/api/openapi.json is stale — regenerate with: \
             cargo run -p punktfunk-host -- openapi > docs/api/openapi.json"
        );
    }

    fn post_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
        axum::http::Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn native_pairing_arm_show_and_unpair() {
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(
                Some(std::env::temp_dir().join(format!("pf-mgmt-np-{}.json", std::process::id()))),
                None,
                false,
            )
            .unwrap(),
        );
        let app = test_app_native(test_state(), np.clone());

        // Disarmed: enabled, not armed, no PIN.
        let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["enabled"], true);
        assert_eq!(b["armed"], false);
        assert!(b["pin"].is_null());

        // Arm → a PIN appears and is readable via status.
        let (s, b) = send(
            &app,
            post_json(
                "/api/v1/native/pair/arm",
                serde_json::json!({"ttl_secs": 60}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["armed"], true);
        let pin = b["pin"].as_str().unwrap().to_string();
        assert_eq!(pin.len(), 4);
        let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
        assert_eq!(b["pin"], pin);
        assert!(b["expires_in_secs"].as_u64().unwrap() <= 60);

        // The QUIC side would read the same live PIN.
        assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));

        // Pair a client out-of-band, then it shows in the list + can be unpaired.
        np.add("Test Device", "abc123").unwrap();
        let (s, b) = send(&app, get_req("/api/v1/native/clients")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b[0]["name"], "Test Device");
        assert_eq!(b[0]["fingerprint"], "abc123");
        let del = axum::http::Request::delete("/api/v1/native/clients/ABC123")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
        let missing = axum::http::Request::delete("/api/v1/native/clients/abc123")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, missing).await.0, StatusCode::NOT_FOUND);

        // Disarm clears the window.
        let del = axum::http::Request::delete("/api/v1/native/pair")
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
        let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
        assert_eq!(b["armed"], false);
    }

    #[tokio::test]
    async fn pending_devices_approve_and_deny() {
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(
                Some(
                    std::env::temp_dir()
                        .join(format!("pf-mgmt-pending-{}.json", std::process::id())),
                ),
                None,
                false,
            )
            .unwrap(),
        );
        let app = test_app_native(test_state(), np.clone());

        // Empty queue.
        let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b.as_array().unwrap().len(), 0);

        // Two devices knock (what the QUIC gate records); they appear in the list.
        np.note_pending("Enrico's MacBook", "aa11");
        np.note_pending("device bb22cc33", "bb22");
        let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
        assert_eq!(b.as_array().unwrap().len(), 2);
        assert_eq!(b[0]["name"], "Enrico's MacBook");
        let approve_id = b[0]["id"].as_u64().unwrap();
        let deny_id = b[1]["id"].as_u64().unwrap();

        // Approve the first with an operator label → paired under that name, gone from pending.
        let (s, b) = send(
            &app,
            post_json(
                &format!("/api/v1/native/pending/{approve_id}/approve"),
                serde_json::json!({"name": "Office MacBook"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["name"], "Office MacBook");
        assert_eq!(b["fingerprint"], "aa11");
        assert!(np.is_paired("AA11"), "approval pins the fingerprint");

        // Deny the second → dropped, not paired; a re-deny is 404.
        let deny = post_json(
            &format!("/api/v1/native/pending/{deny_id}/deny"),
            serde_json::json!({}),
        );
        assert_eq!(send(&app, deny).await.0, StatusCode::NO_CONTENT);
        assert!(!np.is_paired("bb22"));
        let (s, _) = send(
            &app,
            post_json(
                &format!("/api/v1/native/pending/{deny_id}/deny"),
                serde_json::json!({}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Queue is empty again; approving a stale id is 404 (keep `{}` = device's own name).
        let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
        assert_eq!(b.as_array().unwrap().len(), 0);
        let (s, _) = send(
            &app,
            post_json("/api/v1/native/pending/123/approve", serde_json::json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn native_endpoints_report_disabled_without_native_host() {
        let app = test_app(test_state(), None);
        let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["enabled"], false);
        // Arming a host that isn't running the native server is a 503.
        let (s, _) = send(
            &app,
            post_json("/api/v1/native/pair/arm", serde_json::json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        // Pending list reads as an empty array (like /native/clients), not a 503.
        let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b.as_array().unwrap().len(), 0);
        // Approve/deny without a native host are 503.
        let (s, _) = send(
            &app,
            post_json("/api/v1/native/pending/0/approve", serde_json::json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        let (s, _) = send(
            &app,
            post_json("/api/v1/native/pending/0/deny", serde_json::json!({})),
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    }
}
