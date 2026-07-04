//! Management REST API (plan §4) — the control-plane surface a control pane / CLI talks
//! to: host identity + capabilities, runtime status, paired-client management, the pairing
//! PIN flow, and session control. Control plane only — `tokio`/`axum` are permitted here;
//! the per-frame pipeline never touches this module.
//!
//! The API is versioned under `/api/v1` and described by an OpenAPI 3.1 document generated
//! at compile time with `utoipa` — `punktfunk-host openapi` prints it for client codegen, the
//! running server serves it at `/api/v1/openapi.json` plus interactive docs at `/api/docs`,
//! and a copy is checked in at `api/openapi.json` (a test fails if it drifts, like the
//! cbindgen header).
//!
//! Security: serves HTTPS with the host's identity cert and requires auth on every `/api/v1` route
//! except `/api/v1/health` — **always**, even on loopback. The listener binds **all interfaces by
//! default** so a paired native client can reach the read-only surface (host/status/clients and the
//! **game library**) over the LAN with no operator step — authenticated by its mTLS cert (the
//! `cert_may_access` allowlist). The **bearer-token admin surface** (pairing, unpair, session
//! control, library mutation, stats) is honored **only from a loopback peer**, so it is never
//! LAN-exposed: the web console BFF — the sole token holder (`--mgmt-token` / `PUNKTFUNK_MGMT_TOKEN`,
//! else auto-generated + persisted to `~/.config/punktfunk/mgmt-token`) — always connects over
//! loopback. Restore the old loopback-only listener with `--mgmt-bind 127.0.0.1:47990`. The OpenAPI
//! document and docs UI are served unauthenticated (the spec is public — it lives in this repo).

use crate::encode::Codec;
use crate::gamestream::{
    tls::{serve_https, PeerAddr, PeerCertFingerprint},
    AppState, APP_VERSION, AUDIO_PORT, CONTROL_PORT, GFE_VERSION, RTSP_PORT, VIDEO_PORT,
};
use crate::log_capture::LogPage;
use crate::stats_recorder::{Capture, CaptureMeta, StatsStatus};
use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, Request, State},
    http::{header, Method, StatusCode},
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
    /// Shared streaming-stats recorder — the same handle the streaming loops emit into, so an
    /// operator can arm/stop a capture here and review/list/delete saved recordings.
    stats: Arc<crate::stats_recorder::StatsRecorder>,
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
    stats: Arc<crate::stats_recorder::StatsRecorder>,
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
    let app = app(state, Some(token), opts.bind.port(), native, stats);
    serve_https(opts.bind, app, tls).await
}

/// Compose the full management router (also used directly by the handler tests).
fn app(
    state: Arc<AppState>,
    token: Option<String>,
    port: u16,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
) -> Router {
    let shared = Arc::new(MgmtState {
        app: state,
        native,
        stats,
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
                .routes(routes!(list_gpus))
                .routes(routes!(set_gpu_preference))
                .routes(routes!(get_display_settings))
                .routes(routes!(set_display_settings))
                .routes(routes!(get_status))
                .routes(routes!(get_local_summary))
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
                .routes(routes!(update_custom_game, delete_custom_game))
                .routes(routes!(get_library_art))
                .routes(routes!(stats_capture_start))
                .routes(routes!(stats_capture_stop))
                .routes(routes!(stats_capture_status))
                .routes(routes!(stats_capture_live))
                .routes(routes!(stats_recordings_list))
                .routes(routes!(stats_recording_get, stats_recording_delete))
                .routes(routes!(logs_get)),
        )
        .split_for_parts()
}

/// The OpenAPI document as pretty JSON — what `punktfunk-host openapi` prints and what is
/// checked in at `api/openapi.json` for client codegen.
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
        (name = "gpu", description = "GPU inventory and selection: list the host's GPUs, choose automatic or a preferred GPU, see the one in use"),
        (name = "display", description = "Virtual-display management policy: lifecycle (keep-alive), topology (primary/exclusive), conflict handling, identity, and layout"),
        (name = "clients", description = "Paired Moonlight client management"),
        (name = "pairing", description = "Pairing PIN delivery (the out-of-band half of the GameStream pairing handshake)"),
        (name = "native", description = "Native punktfunk/1 pairing: arm a window, display the host PIN, manage paired devices"),
        (name = "session", description = "Active streaming session control"),
        (name = "library", description = "Game library: installed-store titles (Steam) plus user-curated custom entries"),
        (name = "stats", description = "Streaming performance-stats capture: arm/stop a recording, read the live + saved time-series for graphing"),
        (name = "logs", description = "Host log stream: the newest in-memory log entries, cursor-paged for live following"),
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

/// Non-sensitive host status for the local tray icon: counts and booleans only — no PIN values,
/// no fingerprints, no device names. Served unauthenticated to LOOPBACK peers only (see
/// `require_auth`): the bearer-token file is SYSTEM/Administrators-DACL'd on Windows, so the
/// per-user tray process cannot authenticate — this narrow read-only route is its status source.
#[derive(Serialize, ToSchema)]
struct LocalSummary {
    /// Host version (mirrors `/health`).
    version: String,
    /// True while the video stream thread is running.
    video_streaming: bool,
    /// True while the audio stream thread is running.
    audio_streaming: bool,
    /// The active launch session (set by Moonlight's `/launch`, cleared on cancel/stop).
    session: Option<SessionInfo>,
    /// Number of pinned (paired) GameStream client certificates.
    paired_clients: u32,
    /// Number of paired native (punktfunk/1) devices.
    native_paired_clients: u32,
    /// True while a GameStream pairing handshake is parked waiting for the user's PIN.
    pin_pending: bool,
    /// Native pairing knocks awaiting the operator's approval (count only).
    pending_approvals: u32,
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
    /// Optional: bind the window to ONE device fingerprint (hex SHA-256, e.g. from a pending knock).
    /// When set, only a pairing attempt from that fingerprint consumes the window — so an unpaired
    /// LAN peer can neither pair nor burn a window armed for a specific device (security-review #9).
    /// Omit for an unbound window (any device may use the PIN — trusted-LAN only).
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: Option<String>,
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

/// Auth gate on the `/api/v1` routes: a paired client cert (mTLS, from anywhere) or the bearer token
/// (from a **loopback** peer only) — required always (the host runs with a token by construction).
/// `/api/v1/health` stays open for probes; `/api/v1/local/summary` is open to loopback peers only
/// (the tray icon's status source). The cert path authorizes only the read-only allowlist
/// ([`cert_may_access`]); the bearer path authorizes the full admin surface and is therefore confined
/// to loopback so it is never LAN-exposed even when the listener binds all interfaces by default.
async fn require_auth(State(st): State<Arc<MgmtState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await; // liveness probe is always open
    }
    // The tray icon's status source: non-sensitive counts/booleans only, unauthenticated but
    // confined to LOOPBACK peers. The bearer-token file (and cert.pem) are SYSTEM/Administrators-
    // DACL'd on Windows, so the per-user tray process cannot authenticate — this one narrow
    // read-only route is deliberately all it needs. Not on the cert allowlist: LAN mTLS clients
    // already have the richer `/status`. (No PeerAddr ⇒ a unit test → treat as loopback, matching
    // the bearer path below.)
    if req.uri().path() == "/api/v1/local/summary" {
        let from_loopback = req
            .extensions()
            .get::<PeerAddr>()
            .is_none_or(|a| a.0.ip().is_loopback());
        return if from_loopback {
            next.run(req).await
        } else {
            api_error(
                StatusCode::UNAUTHORIZED,
                "the local summary is loopback-only",
            )
        };
    }
    // A paired native client authenticates by its mTLS certificate — the same identity + trust the
    // QUIC data plane uses. But "paired to STREAM" is not "paired to ADMINISTER": a streaming cert
    // authorizes only the safe, read-only status routes, NOT state-changing or pairing-administration
    // routes (which would let one paired client unpair others, read/arm the pairing PIN, stop
    // sessions, or edit the library). Everything outside the allowlist requires the operator's bearer
    // token. The fingerprint is attached by `serve_https` from the verified peer cert.
    if let Some(PeerCertFingerprint(Some(fp))) = req.extensions().get::<PeerCertFingerprint>() {
        if cert_may_access(req.method(), req.uri().path())
            && st.native.as_ref().is_some_and(|n| n.is_paired(fp))
        {
            return next.run(req).await;
        }
    }
    // Otherwise require the bearer token (the web console / admin) — but only from a LOOPBACK peer.
    // The token authorizes the full admin surface, so confining it to loopback keeps that surface off
    // the LAN even though the listener now binds all interfaces by default (so paired clients can
    // browse the library). The web console BFF — the sole token holder — always connects over
    // loopback, so nothing first-party is affected; a LAN caller must use a paired client cert and is
    // limited to the read-only allowlist above. (No PeerAddr ⇒ a non-`serve_https` caller, e.g. a unit
    // test → treat as loopback so handler tests still authenticate by token.)
    let from_loopback = req
        .extensions()
        .get::<PeerAddr>()
        .is_none_or(|a| a.0.ip().is_loopback());
    if !from_loopback {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "the admin API is loopback-only — a LAN client must present a paired client certificate",
        );
    }
    // `run` always passes a token, so no-token means a misconfigured caller (e.g. a test constructing
    // `app` directly) — deny.
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

/// Which routes a paired *streaming* cert (mTLS, no bearer token) may reach: a small allowlist of
/// safe, read-only status routes only. Deny-by-default — every state-changing route and every route
/// that exposes a pairing PIN or the pending-approval queue requires the operator's bearer token, so
/// a streaming client can't administer the host (unpair others, arm/read the PIN, stop sessions,
/// edit the library). `/health` is handled separately (always open).
fn cert_may_access(method: &Method, path: &str) -> bool {
    method == Method::GET
        && (matches!(
            path,
            "/api/v1/host"
                | "/api/v1/compositors"
                | "/api/v1/status"
                | "/api/v1/clients"
                | "/api/v1/native/clients"
                // The native clients browse the game library with their cert (no bearer token); the
                // library MUTATIONS (POST/PUT/DELETE /library/custom) stay token-only via the exact
                // GET-path match above.
                | "/api/v1/library"
        ) || path.starts_with("/api/v1/library/art/"))
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

/// One hardware GPU on the host (software/WARP adapters are never listed).
#[derive(Serialize, ToSchema)]
struct ApiGpu {
    /// Stable identifier (`vendorid-deviceid-occurrence`, hex PCI ids) — pass to `setGpuPreference`.
    /// Stable across reboots and driver updates, unlike an adapter index or LUID.
    #[schema(example = "10de-2c05-0")]
    id: String,
    /// Adapter/marketing name.
    #[schema(example = "NVIDIA GeForce RTX 5070 Ti")]
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// Dedicated VRAM in MiB (0 where the platform doesn't expose it).
    vram_mb: u64,
}

/// The GPU the **next** session's pipeline will be created on, and why. (A preference change
/// applies to the next session; a running session keeps the GPU it opened on.)
#[derive(Serialize, ToSchema)]
struct ApiSelectedGpu {
    id: String,
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// Why this GPU was selected: `preference` (the manual choice), `env`
    /// (`PUNKTFUNK_RENDER_ADAPTER`), `auto` (max dedicated VRAM / platform default), or
    /// `preference_missing` (a manual choice is set but that GPU is absent — auto-selected
    /// instead so the host keeps streaming).
    source: String,
}

/// The GPU live sessions are encoding on right now.
#[derive(Serialize, ToSchema)]
struct ApiActiveGpu {
    /// Stable id matching an entry of `gpus` (empty for the CPU/software encoder).
    id: String,
    name: String,
    /// `nvidia` | `amd` | `intel` | `other`.
    vendor: String,
    /// The encode backend in use (`nvenc` | `amf` | `qsv` | `vaapi` | `software`).
    backend: String,
    /// Number of live encode sessions on it.
    sessions: u32,
}

/// Full GPU-selection state for the console: inventory, the persisted preference, what the next
/// session will use, and what is in use right now.
#[derive(Serialize, ToSchema)]
struct GpuState {
    /// The host's hardware GPUs.
    gpus: Vec<ApiGpu>,
    /// `auto` or `manual`.
    mode: String,
    /// The manually preferred GPU's stable id, when one is stored (kept while `mode` is `auto` so
    /// a console can offer returning to it). May reference a GPU that is currently absent.
    preferred_id: Option<String>,
    /// The stored name of the preferred GPU (a usable label even when it is absent).
    preferred_name: Option<String>,
    /// Whether the preferred GPU is currently present.
    preferred_available: bool,
    /// `PUNKTFUNK_RENDER_ADAPTER` (the host.env pin), when set — it applies while `mode` is
    /// `auto`; a manual preference overrides it.
    env_override: Option<String>,
    /// The GPU the next session will use.
    selected: Option<ApiSelectedGpu>,
    /// The GPU live sessions use right now (absent while nothing is streaming).
    active: Option<ApiActiveGpu>,
}

/// Request body for `setGpuPreference`.
#[derive(Deserialize, ToSchema)]
struct SetGpuPreference {
    /// `auto` (env pin, else max dedicated VRAM — the default) or `manual`.
    #[schema(example = "manual")]
    mode: String,
    /// Required when `mode` is `manual`: the stable `id` of a currently listed GPU
    /// (see `listGpus`).
    #[schema(example = "10de-2c05-0")]
    gpu_id: Option<String>,
}

/// Build the [`GpuState`] snapshot (shared by the GET and the PUT's response).
fn gpu_state() -> GpuState {
    let gpus = crate::gpu::enumerate();
    let pref = crate::gpu::prefs().get();
    let (preferred_id, preferred_name, preferred_available) = match &pref.gpu {
        Some(want) => {
            let found = crate::gpu::find_preferred(&gpus, want);
            let id = match found {
                // Canonical: the present GPU's id (identity may have matched loosely).
                Some(i) => gpus[i].id.clone(),
                None => format!(
                    "{:04x}-{:04x}-{}",
                    want.vendor_id, want.device_id, want.occurrence
                ),
            };
            let name = match found {
                Some(i) => gpus[i].name.clone(),
                None => want.name.clone(),
            };
            (Some(id), Some(name), found.is_some())
        }
        None => (None, None, false),
    };
    let selected = crate::gpu::selected_gpu().map(|sel| ApiSelectedGpu {
        vendor: sel.info.vendor_tag().into(),
        id: sel.info.id,
        name: sel.info.name,
        source: sel.source.tag().into(),
    });
    let active = crate::gpu::active().and_then(|(g, sessions)| {
        (sessions > 0).then(|| ApiActiveGpu {
            vendor: crate::gpu::vendor_tag(g.vendor_id).into(),
            id: g.id,
            name: g.name,
            backend: g.backend.into(),
            sessions,
        })
    });
    GpuState {
        gpus: gpus
            .into_iter()
            .map(|g| ApiGpu {
                vendor: g.vendor_tag().into(),
                vram_mb: g.vram_bytes / (1024 * 1024),
                id: g.id,
                name: g.name,
            })
            .collect(),
        mode: match pref.mode {
            crate::gpu::GpuMode::Auto => "auto".into(),
            crate::gpu::GpuMode::Manual => "manual".into(),
        },
        preferred_id,
        preferred_name,
        preferred_available,
        env_override: crate::config::config()
            .render_adapter
            .clone()
            .filter(|s| !s.is_empty()),
        selected,
        active,
    }
}

/// GPU inventory and selection
///
/// Lists the host's hardware GPUs, the persisted auto/manual preference, the GPU the next session
/// will use (and why), and the GPU live sessions encode on right now.
#[utoipa::path(
    get,
    path = "/gpus",
    tag = "gpu",
    operation_id = "listGpus",
    responses(
        (status = OK, description = "GPU inventory + selection state", body = GpuState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn list_gpus() -> Json<GpuState> {
    Json(gpu_state())
}

/// Set the GPU preference
///
/// `auto` restores automatic selection (`PUNKTFUNK_RENDER_ADAPTER` pin, else max dedicated VRAM);
/// `manual` pins capture + encode to the given GPU. Persisted across restarts; applies to the
/// **next** session (a running session keeps its GPU). If the preferred GPU is absent at session
/// start the host falls back to automatic selection rather than failing.
#[utoipa::path(
    put,
    path = "/gpus/preference",
    tag = "gpu",
    operation_id = "setGpuPreference",
    request_body = SetGpuPreference,
    responses(
        (status = OK, description = "Preference stored; the new selection state", body = GpuState),
        (status = BAD_REQUEST, description = "Unknown mode, or `gpu_id` missing / not a listed GPU", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Preference could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn set_gpu_preference(ApiJson(req): ApiJson<SetGpuPreference>) -> Response {
    let pref = match req.mode.to_ascii_lowercase().as_str() {
        "auto" => {
            // Keep the stored manual pick so the console can offer switching back to it.
            let mut p = crate::gpu::prefs().get();
            p.mode = crate::gpu::GpuMode::Auto;
            p
        }
        "manual" => {
            let Some(id) = req
                .gpu_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return api_error(StatusCode::BAD_REQUEST, "mode `manual` requires `gpu_id`");
            };
            let Some(g) = crate::gpu::enumerate().into_iter().find(|g| g.id == id) else {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "gpu_id does not match a present GPU (see GET /gpus)",
                );
            };
            crate::gpu::GpuPreference {
                mode: crate::gpu::GpuMode::Manual,
                gpu: Some(crate::gpu::PreferredGpu {
                    vendor_id: g.vendor_id,
                    device_id: g.device_id,
                    occurrence: g.occurrence,
                    name: g.name,
                }),
            }
        }
        other => {
            return api_error(
                StatusCode::BAD_REQUEST,
                &format!("unknown mode {other:?} — use `auto` or `manual`"),
            )
        }
    };
    if let Err(e) = crate::gpu::prefs().set(pref) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist GPU preference: {e:#}"),
        );
    }
    tracing::info!(mode = %req.mode, gpu_id = ?req.gpu_id, "management API: GPU preference updated");
    Json(gpu_state()).into_response()
}

// ---------------------------------------------------------------------------------------
// Display management (design/display-management.md)
// ---------------------------------------------------------------------------------------

/// One preset's human-facing description + the fields it expands to, so the console can render a
/// preset picker with an accurate "what this does" preview without hardcoding the expansion.
#[derive(Serialize, ToSchema)]
struct PresetInfo {
    /// The preset id (`default` | `gaming-rig` | `shared-desktop` | `hotdesk` | `workstation`).
    id: String,
    /// One-line story shown next to the option.
    summary: String,
    /// The effective policy this preset expands to (the same fields a `custom` policy carries).
    fields: crate::vdisplay::policy::EffectivePolicy,
}

/// Full display-management state for the console: the stored policy, every preset's expansion, the
/// resolved effective policy, and which options this build actually enforces yet (Stage 0 wires
/// keep-alive linger + topology; the rest are stored but not yet acted on).
#[derive(Serialize, ToSchema)]
struct DisplaySettingsState {
    /// The stored policy (preset + custom fields), or the built-in default when unconfigured.
    settings: crate::vdisplay::policy::DisplayPolicy,
    /// True once a `display-settings.json` exists (the console has configured this host).
    configured: bool,
    /// The effective (preset-expanded) policy currently in force.
    effective: crate::vdisplay::policy::EffectivePolicy,
    /// Every named preset and what it expands to (for the picker's preview).
    presets: Vec<PresetInfo>,
    /// Option names this build enforces right now (e.g. `keep_alive`, `topology`). The remaining
    /// stored options (`mode_conflict`, `identity`, `layout`) land in later stages — surfaced so the
    /// console can mark them "coming soon" instead of implying they already take effect.
    enforced: Vec<String>,
}

fn preset_summary(id: &str) -> &'static str {
    match id {
        "default" => "Today's behavior: a short linger absorbs reconnects, the streamed output is the sole desktop, extra clients get their own view.",
        "gaming-rig" => "Dedicated couch/headless box: the game and its display survive disconnects; whoever connects takes the box over.",
        "shared-desktop" => "A desktop you also use in person: never blank the real monitors, never keep ghost displays, concurrent viewers each get a view.",
        "hotdesk" => "One user at a time with fast reattach; a second user is told the box is busy; each device+resolution keeps its own scaling.",
        "workstation" => "Multi-monitor daily driver: your displays come back exactly where you arranged them, per-client identity, exclusive.",
        _ => "",
    }
}

fn display_settings_state() -> DisplaySettingsState {
    use crate::vdisplay::policy::{self, Preset};
    let store = policy::prefs();
    let settings = store.get();
    let configured = store.configured().is_some();
    let presets = [
        ("default", Preset::Default),
        ("gaming-rig", Preset::GamingRig),
        ("shared-desktop", Preset::SharedDesktop),
        ("hotdesk", Preset::Hotdesk),
        ("workstation", Preset::Workstation),
    ]
    .into_iter()
    .filter_map(|(id, p)| {
        policy::preset_fields(p).map(|e| PresetInfo {
            id: id.to_string(),
            summary: preset_summary(id).to_string(),
            fields: e,
        })
    })
    .collect();
    DisplaySettingsState {
        effective: settings.effective(),
        settings,
        configured,
        presets,
        enforced: vec!["keep_alive".into(), "topology".into()],
    }
}

/// Display-management policy
///
/// The stored virtual-display policy (lifecycle, topology, conflict handling, identity, layout),
/// every preset's expansion, and which options this build enforces yet. See
/// `design/display-management.md`.
#[utoipa::path(
    get,
    path = "/display/settings",
    tag = "display",
    operation_id = "getDisplaySettings",
    responses(
        (status = OK, description = "Stored policy + preset expansions + enforced options", body = DisplaySettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn get_display_settings() -> Json<DisplaySettingsState> {
    Json(display_settings_state())
}

/// Set the display-management policy
///
/// Persists a new policy (validated + clamped) and applies it from the next connect/teardown — a
/// running session keeps the display it opened on. `keep_alive: forever` is rejected until the
/// display-lifecycle stage ships (it would keep physical monitors dark indefinitely with no release
/// path yet).
#[utoipa::path(
    put,
    path = "/display/settings",
    tag = "display",
    operation_id = "setDisplaySettings",
    request_body = crate::vdisplay::policy::DisplayPolicy,
    responses(
        (status = OK, description = "Policy stored; the new state", body = DisplaySettingsState),
        (status = BAD_REQUEST, description = "An option value is not yet supported (e.g. keep_alive forever)", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Policy could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn set_display_settings(
    ApiJson(policy): ApiJson<crate::vdisplay::policy::DisplayPolicy>,
) -> Response {
    use crate::vdisplay::policy::KeepAlive;
    // Reject options this build can't honor yet, so the console can't promise a behavior that won't
    // happen. `keep_alive: forever` (directly or via the `gaming-rig` preset) needs the Pinned
    // lifecycle + a release path; until then it would strand physical monitors dark.
    if policy.effective().keep_alive == KeepAlive::Forever {
        return api_error(
            StatusCode::BAD_REQUEST,
            "keep_alive `forever` (and the `gaming-rig` preset) is not available yet — it arrives \
             with the display-lifecycle stage. Use a fixed duration for now.",
        );
    }
    if let Err(e) = crate::vdisplay::policy::prefs().set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist display policy: {e:#}"),
        );
    }
    tracing::info!("management API: display policy updated");
    Json(display_settings_state()).into_response()
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

/// Local status summary for the tray icon
///
/// Non-sensitive status (counts and booleans only — no PIN values, no fingerprints, no device
/// names). Unauthenticated, but served to loopback peers only.
#[utoipa::path(
    get,
    path = "/local/summary",
    tag = "host",
    operation_id = "getLocalSummary",
    // Override the document-global bearerAuth: loopback peers are exempt in `require_auth`.
    security(()),
    responses(
        (status = OK, description = "Non-sensitive local host status (loopback peers only)", body = LocalSummary),
        (status = UNAUTHORIZED, description = "Non-loopback peer", body = ApiError),
    )
)]
async fn get_local_summary(State(st): State<Arc<MgmtState>>) -> Json<LocalSummary> {
    let session = st.app.launch.lock().unwrap().map(|l| SessionInfo {
        width: l.width,
        height: l.height,
        fps: l.fps,
    });
    let (native_paired_clients, pending_approvals) = st
        .native
        .as_ref()
        .map(|n| (n.status().paired_clients, n.pending().len() as u32))
        .unwrap_or((0, 0));
    Json(LocalSummary {
        version: env!("PUNKTFUNK_VERSION").into(),
        video_streaming: st.app.streaming.load(Ordering::SeqCst),
        audio_streaming: st.app.audio_streaming.load(Ordering::SeqCst),
        session,
        paired_clients: st.app.paired.lock().unwrap().len() as u32,
        native_paired_clients,
        pin_pending: st.app.pairing.pin.awaiting_pin(),
        pending_approvals,
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
        (status = SERVICE_UNAVAILABLE, description = "Native host not available in this process", body = ApiError),
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
            "native host not available in this process",
        );
    };
    let ttl = req.ttl_secs.unwrap_or(120).clamp(15, 600);
    // A bound window (operator selected a specific device) is DoS-proof: only that fingerprint can
    // consume it (#9). An unbound window (no fingerprint) keeps the legacy any-device behavior.
    let bound = req
        .fingerprint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let bound_to_device = bound.is_some();
    let _pin = np.arm_for(std::time::Duration::from_secs(ttl as u64), bound);
    tracing::info!(
        ttl_secs = ttl,
        bound_to_device,
        "management API: native pairing armed"
    );
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

/// Fetch one cover-art image for a library entry
///
/// Resolves `kind` (`portrait` | `hero` | `logo` | `header`) for the given library id and streams
/// the image bytes. For a Steam title, the host's own local Steam cache is tried first (exact —
/// it's what the user's Steam client already shows for it), the public Steam CDN's flat URL
/// convention as a fallback (newer titles' CDN assets can live at a per-asset-hash path the host
/// can't predict, in which case this 404s and the client falls through to its next art candidate).
/// Only Steam ids are backed today; any other store 404s.
#[utoipa::path(
    get,
    path = "/library/art/{id}/{kind}",
    tag = "library",
    operation_id = "getLibraryArt",
    params(
        ("id" = String, Path, description = "The store-qualified library id, e.g. `steam:570`"),
        ("kind" = String, Path, description = "`portrait` | `hero` | `logo` | `header`"),
    ),
    responses(
        (status = OK, description = "Image bytes", content_type = "image/jpeg"),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = NOT_FOUND, description = "No art of that kind for that id", body = ApiError),
    )
)]
async fn get_library_art(Path((id, kind)): Path<(String, String)>) -> Response {
    let Some(kind) = crate::library::ArtKind::parse(&kind) else {
        return api_error(StatusCode::NOT_FOUND, "unknown art kind");
    };
    let Some(appid) = id
        .strip_prefix("steam:")
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return api_error(StatusCode::NOT_FOUND, "no art proxy for this store");
    };
    match tokio::task::spawn_blocking(move || crate::library::steam_art_bytes(appid, kind)).await {
        Ok(Some((bytes, ctype))) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
        _ => api_error(StatusCode::NOT_FOUND, "no art of that kind for this title"),
    }
}

// ---------------------------------------------------------------------------------------
// Streaming stats capture (design/stats-capture-plan.md §2)
// ---------------------------------------------------------------------------------------

/// Start a stats capture
///
/// Arms a new performance-stats capture. Idempotent: if a capture is already running this returns
/// the current status unchanged. While armed, the streaming loops emit aggregated samples (~ every
/// 1–2 s) into the in-progress capture, readable live via `GET /stats/capture/live`.
#[utoipa::path(
    post,
    path = "/stats/capture/start",
    tag = "stats",
    operation_id = "statsCaptureStart",
    responses(
        (status = OK, description = "Capture armed (or already running)", body = StatsStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn stats_capture_start(State(st): State<Arc<MgmtState>>) -> Json<StatsStatus> {
    let status = st.stats.start();
    tracing::info!(
        started_unix_ms = status.started_unix_ms,
        "management API: stats capture armed"
    );
    Json(status)
}

/// Stop the stats capture
///
/// Disarms the in-progress capture and writes it to disk atomically, returning its summary. If
/// nothing was recording, returns `204 No Content`.
#[utoipa::path(
    post,
    path = "/stats/capture/stop",
    tag = "stats",
    operation_id = "statsCaptureStop",
    responses(
        (status = OK, description = "Capture stopped and saved", body = CaptureMeta),
        (status = NO_CONTENT, description = "Nothing was recording"),
        (status = INTERNAL_SERVER_ERROR, description = "Could not write the recording to disk", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn stats_capture_stop(State(st): State<Arc<MgmtState>>) -> Response {
    match st.stats.stop() {
        Ok(Some(meta)) => {
            tracing::info!(id = %meta.id, samples = meta.sample_count, "management API: stats capture saved");
            (StatusCode::OK, Json(meta)).into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not save capture: {e}"),
        ),
    }
}

/// Stats capture status
///
/// Whether a capture is armed, its sample count, and start time. Poll this (e.g. every 2 s) to
/// drive the capture-control UI.
#[utoipa::path(
    get,
    path = "/stats/capture/status",
    tag = "stats",
    operation_id = "statsCaptureStatus",
    responses(
        (status = OK, description = "In-progress capture status (idle when not armed)", body = StatsStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn stats_capture_status(State(st): State<Arc<MgmtState>>) -> Json<StatsStatus> {
    Json(st.stats.status())
}

/// Live in-progress capture
///
/// The full sample time-series of the capture currently recording, for live graphing. `404` when
/// nothing is armed.
#[utoipa::path(
    get,
    path = "/stats/capture/live",
    tag = "stats",
    operation_id = "statsCaptureLive",
    responses(
        (status = OK, description = "The in-progress capture (meta + samples so far)", body = Capture),
        (status = NOT_FOUND, description = "No capture is currently recording", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn stats_capture_live(State(st): State<Arc<MgmtState>>) -> Response {
    match st.stats.live_snapshot() {
        Some(capture) => Json(capture).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no capture is currently recording"),
    }
}

/// List saved recordings
///
/// Every saved capture's summary (the `meta` head only — not the sample body), newest first.
#[utoipa::path(
    get,
    path = "/stats/recordings",
    tag = "stats",
    operation_id = "statsRecordingsList",
    responses(
        (status = OK, description = "Saved capture summaries, newest first", body = [CaptureMeta]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn stats_recordings_list(State(st): State<Arc<MgmtState>>) -> Json<Vec<CaptureMeta>> {
    Json(st.stats.list())
}

/// Get a saved recording
///
/// The full capture (meta + samples) for `id`, for graphing or download.
#[utoipa::path(
    get,
    path = "/stats/recordings/{id}",
    tag = "stats",
    operation_id = "statsRecordingGet",
    params(("id" = String, Path, description = "The recording id (its filename stem)")),
    responses(
        (status = OK, description = "The full capture", body = Capture),
        (status = NOT_FOUND, description = "No recording with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The recording file is unreadable", body = ApiError),
    )
)]
async fn stats_recording_get(State(st): State<Arc<MgmtState>>, Path(id): Path<String>) -> Response {
    match st.stats.load(&id) {
        Ok(capture) => Json(capture).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            api_error(StatusCode::NOT_FOUND, "no recording with that id")
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not read recording: {e}"),
        ),
    }
}

/// Delete a saved recording
///
/// Removes the recording `id` from disk. `404` if there is no such recording.
#[utoipa::path(
    delete,
    path = "/stats/recordings/{id}",
    tag = "stats",
    operation_id = "statsRecordingDelete",
    params(("id" = String, Path, description = "The recording id (its filename stem)")),
    responses(
        (status = NO_CONTENT, description = "Recording deleted"),
        (status = NOT_FOUND, description = "No recording with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not delete the recording", body = ApiError),
    )
)]
async fn stats_recording_delete(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    match st.stats.delete(&id) {
        Ok(()) => {
            tracing::info!(id, "management API: recording deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            api_error(StatusCode::NOT_FOUND, "no recording with that id")
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not delete recording: {e}"),
        ),
    }
}

/// Query for `GET /logs` — a cursor poll.
#[derive(Deserialize)]
struct LogsQuery {
    after: Option<u64>,
    limit: Option<u32>,
}

/// Host logs
///
/// The host's recent log entries — an in-memory ring of the newest few thousand, captured at
/// DEBUG and above regardless of `RUST_LOG`. Follow live by polling with `after` set to the last
/// response's `next` cursor; a `dropped: true` means entries were evicted between polls (the ring
/// wrapped). Bearer-only: logs can reference client identities and host paths, so this is part of
/// the loopback-only admin surface, never the LAN-readable mTLS one.
#[utoipa::path(
    get,
    path = "/logs",
    tag = "logs",
    operation_id = "logsGet",
    params(
        ("after" = Option<u64>, Query, description = "Return entries with seq greater than this (omitted/0 = oldest retained)"),
        ("limit" = Option<u32>, Query, description = "Max entries per response (default and cap 1000)"),
    ),
    responses(
        (status = OK, description = "Entries after the cursor, oldest first", body = LogPage),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
async fn logs_get(Query(q): Query<LogsQuery>) -> Json<LogPage> {
    let limit = q.limit.map_or(crate::log_capture::MAX_PAGE, |l| l as usize);
    Json(crate::log_capture::ring().since(q.after.unwrap_or(0), limit))
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

    /// A throwaway stats recorder rooted in a unique temp dir (never touches the real config dir).
    fn test_stats() -> Arc<crate::stats_recorder::StatsRecorder> {
        crate::stats_recorder::StatsRecorder::new(std::env::temp_dir().join(format!(
            "pf-mgmt-stats-{}-{:p}",
            std::process::id(),
            &0u8 as *const u8
        )))
    }

    fn test_state() -> Arc<AppState> {
        let host = Host {
            hostname: "test-host".into(),
            uniqueid: "deadbeef".into(),
            local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
        };
        let identity = ServerIdentity::ephemeral().expect("ephemeral identity");
        Arc::new(AppState::new(host, identity, test_stats()))
    }

    // The mgmt API now always requires auth, so the router always has a token. A test that passes
    // `None` gets the default "test-secret" (and `send` auto-attaches the matching bearer); a test
    // that passes an explicit token exercises a mismatch (e.g. `bearer_token_is_enforced`).
    fn test_app(state: Arc<AppState>, token: Option<&str>) -> Router {
        let stats = state.stats.clone();
        app(
            state,
            Some(token.unwrap_or("test-secret").to_string()),
            DEFAULT_PORT,
            None,
            stats,
        )
    }

    fn test_app_native(
        state: Arc<AppState>,
        np: Arc<crate::native_pairing::NativePairing>,
    ) -> Router {
        // Auth required always; the paired-cert tests inject a fingerprint (cert branch wins), the
        // rest authenticate via the `send`-attached default bearer.
        let stats = state.stats.clone();
        app(
            state,
            Some("test-secret".to_string()),
            DEFAULT_PORT,
            Some(np),
            stats,
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

    /// Send a request authenticated ONLY by a paired streaming cert (the `PeerCertFingerprint`
    /// `serve_https` would attach) — no bearer header — so `require_auth`'s cert branch decides.
    async fn send_cert(app: &Router, mut req: axum::http::Request<Body>, fp: &str) -> StatusCode {
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        app.clone().oneshot(req).await.expect("infallible").status()
    }

    /// A paired *streaming* cert (mTLS, no bearer) authorizes only the read-only allowlist; every
    /// state-changing or PIN-exposing route still requires the operator's bearer token (audit #4).
    #[tokio::test]
    async fn cert_auth_is_a_read_only_allowlist() {
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(
                Some(
                    std::env::temp_dir().join(format!("pf-mgmt-cert-{}.json", std::process::id())),
                ),
                None,
                false,
            )
            .unwrap(),
        );
        let fp = "deadbeefcafe";
        np.add("streaming-client", fp).unwrap();
        let app = test_app_native(test_state(), np);

        // Allowlisted read-only GETs → the cert authorizes them (not 401).
        for p in [
            "/api/v1/host",
            "/api/v1/status",
            "/api/v1/compositors",
            "/api/v1/clients",
            "/api/v1/native/clients",
            "/api/v1/library",
        ] {
            assert_ne!(
                send_cert(&app, get_req(p), fp).await,
                StatusCode::UNAUTHORIZED,
                "a paired streaming cert should authorize GET {p}"
            );
        }
        // PIN-exposing GET + state-changing routes → token-only (cert rejected without a bearer).
        assert_eq!(
            send_cert(&app, get_req("/api/v1/native/pair"), fp).await,
            StatusCode::UNAUTHORIZED,
            "GET /native/pair exposes the PIN → must require the bearer token"
        );
        assert_eq!(
            send_cert(
                &app,
                post_json(
                    "/api/v1/native/pair/arm",
                    serde_json::json!({"ttl_secs": 60})
                ),
                fp,
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "arming pairing must require the bearer token"
        );
        assert_eq!(
            send_cert(
                &app,
                axum::http::Request::delete("/api/v1/native/clients/deadbeefcafe")
                    .body(Body::empty())
                    .unwrap(),
                fp,
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "unpair (DELETE) must require the bearer token"
        );
        // An UNPAIRED cert is rejected even on an allowlisted path.
        assert_eq!(
            send_cert(&app, get_req("/api/v1/status"), "not-paired").await,
            StatusCode::UNAUTHORIZED,
            "an unpaired cert must be rejected"
        );
    }

    /// The bearer-token (admin) path is honored only from a LOOPBACK peer: the same token from a LAN
    /// peer is rejected, so binding the listener to all interfaces (so paired clients can browse the
    /// library by default) never LAN-exposes the admin surface. A paired *cert*, by contrast, reaches
    /// the read-only allowlist from anywhere.
    #[tokio::test]
    async fn bearer_admin_is_loopback_only() {
        let lan: SocketAddr = "192.168.1.50:54321".parse().unwrap();
        let loopback: SocketAddr = "127.0.0.1:33333".parse().unwrap();
        let bearer = |peer: SocketAddr| {
            let mut req = get_req("/api/v1/stats/recordings"); // a bearer-only (admin) route
            req.extensions_mut().insert(PeerAddr(peer));
            req.headers_mut().insert(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer test-secret"),
            );
            req
        };

        let app = test_app(test_state(), None);
        // A valid bearer from a LAN peer → rejected on the admin API.
        assert_eq!(
            app.clone()
                .oneshot(bearer(lan))
                .await
                .expect("infallible")
                .status(),
            StatusCode::UNAUTHORIZED,
            "a bearer token from a LAN peer must be rejected on the admin API"
        );
        // The SAME token from a loopback peer (the web console BFF) → accepted.
        assert_ne!(
            app.clone()
                .oneshot(bearer(loopback))
                .await
                .expect("infallible")
                .status(),
            StatusCode::UNAUTHORIZED,
            "the bearer token must be accepted from a loopback peer"
        );

        // A paired cert from a LAN peer still reaches the read-only library (the feature this enables).
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(
                Some(
                    std::env::temp_dir()
                        .join(format!("pf-mgmt-lanlib-{}.json", std::process::id())),
                ),
                None,
                false,
            )
            .unwrap(),
        );
        let fp = "deadbeefcafe";
        np.add("lan-client", fp).unwrap();
        let app = test_app_native(test_state(), np);
        let mut req = get_req("/api/v1/library");
        req.extensions_mut().insert(PeerAddr(lan));
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        assert_ne!(
            app.clone().oneshot(req).await.expect("infallible").status(),
            StatusCode::UNAUTHORIZED,
            "a paired cert must reach the library from a LAN peer"
        );

        // The per-image art proxy (`/api/v1/library/art/{id}/{kind}`) is a prefix match in
        // `cert_may_access`, not an exact one (dynamic id/kind segments) — exercise it directly. An
        // unknown `kind` 404s before any disk/network I/O, so this stays a fast, deterministic check
        // of the auth gate (not of art resolution, which `library::tests` covers).
        let mut req = get_req("/api/v1/library/art/steam:570/not-a-real-kind");
        req.extensions_mut().insert(PeerAddr(lan));
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        assert_eq!(
            app.clone().oneshot(req).await.expect("infallible").status(),
            StatusCode::NOT_FOUND,
            "a paired cert must reach the per-image library art proxy from a LAN peer \
             (and an unknown kind 404s, rather than ever being rejected as unauthorized)"
        );
    }

    #[tokio::test]
    async fn health_is_open_and_versioned() {
        let app = test_app(test_state(), None);
        let (status, body) = send(&app, get_req("/api/v1/health")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["abi_version"], punktfunk_core::ABI_VERSION);
    }

    /// The tray's `/local/summary` is unauthenticated for LOOPBACK peers only — a LAN peer is
    /// rejected even though the route needs no bearer token, and the body never carries secret
    /// material (no PIN values, no fingerprints, no device names — counts/booleans only).
    #[tokio::test]
    async fn local_summary_is_loopback_only_and_non_sensitive() {
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(
                Some(
                    std::env::temp_dir()
                        .join(format!("pf-mgmt-summary-{}.json", std::process::id())),
                ),
                None,
                false,
            )
            .unwrap(),
        );
        np.add("secret-device-name", "deadbeefcafe0123").unwrap();
        let app = test_app_native(test_state(), np);

        // Loopback peer, NO auth header → 200 with the expected shape.
        let mut req = get_req("/api/v1/local/summary");
        req.extensions_mut()
            .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
        let (status, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["video_streaming"], false);
        assert_eq!(body["native_paired_clients"], 1);
        assert_eq!(body["pending_approvals"], 0);
        assert!(body["version"].is_string());
        // No secret material anywhere in the body (paired name / fingerprint must not leak).
        let raw = body.to_string();
        assert!(
            !raw.contains("deadbeefcafe0123") && !raw.contains("secret-device-name"),
            "summary must not leak fingerprints or device names: {raw}"
        );

        // The same request from a LAN peer → rejected (route is loopback-gated, not just tokenless).
        let mut req = get_req("/api/v1/local/summary");
        req.extensions_mut()
            .insert(PeerAddr("192.168.1.50:40000".parse().unwrap()));
        let (status, _) = send(&app, req).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the local summary must be rejected for a LAN peer"
        );

        // IPv6 loopback counts as loopback.
        let mut req = get_req("/api/v1/local/summary");
        req.extensions_mut()
            .insert(PeerAddr("[::1]:40000".parse().unwrap()));
        let (status, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::OK, "::1 is a loopback peer");
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
            peer_ip: None,
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
        let err = run(test_state(), opts, None, test_stats())
            .await
            .unwrap_err();
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
            peer_ip: None,
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

        let checked_in = include_str!("../../../api/openapi.json");
        // Compare STRUCTURALLY with `info.version` normalized on both sides: the served document
        // stamps the live crate version, but a version bump alone must never invalidate the
        // snapshot — the API *surface* is what drift-control protects (the 0.5.0 release tripped
        // on exactly this). Structural comparison also makes line endings a non-issue (git may
        // check the file out CRLF on Windows).
        let mut generated = doc;
        let mut snapshot: serde_json::Value = serde_json::from_str(checked_in).unwrap();
        generated["info"]["version"] = serde_json::json!("<any>");
        snapshot["info"]["version"] = serde_json::json!("<any>");
        assert_eq!(
            generated, snapshot,
            "api/openapi.json is stale — regenerate with: \
             cargo run -p punktfunk-host -- openapi > api/openapi.json"
        );
    }

    fn post_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
        axum::http::Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// The display-management endpoints: GET returns the policy surface (presets + effective +
    /// the Stage-0 enforced list); PUT rejects `keep_alive: forever` (the `gaming-rig` preset)
    /// *before* persisting, so this stays read-only against the global policy store.
    #[tokio::test]
    async fn display_settings_surface_and_forever_rejected() {
        let app = test_app(test_state(), None);

        let (status, body) = send(&app, get_req("/api/v1/display/settings")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["presets"].as_array().map(|a| a.len()),
            Some(5),
            "all five named presets are surfaced for the console picker"
        );
        assert!(
            body["effective"]["keep_alive"].is_object(),
            "the effective policy is echoed"
        );
        let enforced: Vec<&str> = body["enforced"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(enforced.contains(&"keep_alive") && enforced.contains(&"topology"));

        // `gaming-rig` expands to keep_alive: forever → rejected at Stage 0 (before any write).
        let put = axum::http::Request::put("/api/v1/display/settings")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({ "preset": "gaming-rig" }).to_string(),
            ))
            .unwrap();
        let (status, body) = send(&app, put).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["error"].as_str().unwrap_or_default().contains("forever"),
            "the rejection names the unsupported option"
        );
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
        np.note_pending("Enrico's MacBook", "aa11", None);
        np.note_pending("device bb22cc33", "bb22", None);
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

    fn put_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
        axum::http::Request::put(path)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// The GPU endpoints: the inventory GET always answers (an empty list on a GPU-less box —
    /// the schema is platform-independent), and the preference PUT validates mode + gpu_id
    /// BEFORE touching the persisted store, so a bad request can never write.
    #[tokio::test]
    async fn gpu_endpoints_list_and_validate() {
        let app = test_app(test_state(), None);

        let (s, b) = send(&app, get_req("/api/v1/gpus")).await;
        assert_eq!(s, StatusCode::OK);
        assert!(b["gpus"].is_array());
        assert!(b["mode"].is_string());

        // Unknown mode → 400.
        let (s, _) = send(
            &app,
            put_json(
                "/api/v1/gpus/preference",
                serde_json::json!({"mode": "fastest"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // `manual` without a gpu_id → 400.
        let (s, _) = send(
            &app,
            put_json(
                "/api/v1/gpus/preference",
                serde_json::json!({"mode": "manual"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);

        // `manual` with an id that is not a present GPU → 400 (the console only offers listed ids).
        let (s, _) = send(
            &app,
            put_json(
                "/api/v1/gpus/preference",
                serde_json::json!({"mode": "manual", "gpu_id": "ffff-ffff-9"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn logs_endpoint_pages_by_cursor() {
        let app = test_app(test_state(), None);

        // The ring is a process-wide singleton — start from wherever its cursor currently is.
        let (s, json) = send(&app, get_req("/api/v1/logs")).await;
        assert_eq!(s, StatusCode::OK);
        let start = json["next"].as_u64().unwrap();

        let ring = crate::log_capture::ring();
        ring.push(&tracing::Level::WARN, "mgmt::tests", "first".into());
        ring.push(&tracing::Level::INFO, "mgmt::tests", "second".into());

        let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={start}"))).await;
        assert_eq!(s, StatusCode::OK);
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["msg"], "first");
        assert_eq!(entries[0]["level"], "WARN");
        assert_eq!(json["next"].as_u64().unwrap(), start + 2);
        assert_eq!(json["dropped"], false);

        // Nothing newer → empty page, cursor unchanged.
        let after = start + 2;
        let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={after}"))).await;
        assert_eq!(s, StatusCode::OK);
        assert!(json["entries"].as_array().unwrap().is_empty());
        assert_eq!(json["next"].as_u64().unwrap(), after);
    }
}
