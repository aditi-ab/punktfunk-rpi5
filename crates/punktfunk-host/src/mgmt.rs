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

use crate::gamestream::{tls::serve_https, AppState};
use anyhow::{Context, Result};
use axum::{middleware, routing::get, Json, Router};
use std::net::SocketAddr;
use std::sync::Arc;
use utoipa::{Modify, OpenApi};
use utoipa_axum::{router::OpenApiRouter, routes};
use utoipa_scalar::{Scalar, Servable};

mod auth;
mod clients;
mod display;
mod events;
mod gpu;
mod hooks;
mod host;
mod library;
mod native;
mod plugins;
mod session;
mod shared;
mod stats;
mod store;
#[cfg(test)]
mod tests;

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
    /// The scripting runner's capability-limited bearer token (`plugin-token`): authorizes the
    /// plugin surface only, never hook registration or pairing administration
    /// (`auth::plugin_may_access`). Optional — `None` simply disables the lane.
    pub plugin_token: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            token: None,
            plugin_token: None,
        }
    }
}

/// Axum state for the management routes: the shared control-plane state + auth config.
pub(crate) struct MgmtState {
    app: Arc<AppState>,
    /// Native (punktfunk/1) pairing — shared with the QUIC host when the unified `serve --native`
    /// runs it. `None` ⇒ GameStream-only host (the native endpoints report `enabled: false`).
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    /// Shared streaming-stats recorder — the same handle the streaming loops emit into, so an
    /// operator can arm/stop a capture here and review/list/delete saved recordings.
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    /// Whether this host runs the GameStream/Moonlight-compat planes (`--gamestream`). Surfaced in
    /// [`HostInfo`] so the web console can hide the Moonlight-only pairing UI on the secure default
    /// (native-only) host, where a Moonlight PIN can never arrive.
    gamestream_enabled: bool,
    token: Option<String>,
    /// The plugin lane's token (see [`Options::plugin_token`]). Checked only after the admin token
    /// mismatches, and gated by `auth::plugin_may_access` per route.
    plugin_token: Option<String>,
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
    gamestream_enabled: bool,
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
    let app = app(
        state,
        Some(token),
        opts.plugin_token.filter(|t| !t.trim().is_empty()),
        opts.bind.port(),
        native,
        stats,
        gamestream_enabled,
    );
    serve_https(opts.bind, app, tls).await
}

/// Compose the full management router (also used directly by the handler tests).
fn app(
    state: Arc<AppState>,
    token: Option<String>,
    plugin_token: Option<String>,
    port: u16,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    gamestream_enabled: bool,
) -> Router {
    let shared = Arc::new(MgmtState {
        app: state,
        native,
        stats,
        gamestream_enabled,
        token,
        plugin_token,
        port,
    });
    let (api_routes, api) = api_router_parts();
    api_routes
        .route_layer(middleware::from_fn_with_state(
            shared.clone(),
            auth::require_auth,
        ))
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
                .routes(routes!(host::get_health))
                .routes(routes!(host::get_host_info))
                .routes(routes!(host::list_compositors))
                .routes(routes!(gpu::list_gpus))
                .routes(routes!(gpu::set_gpu_preference))
                .routes(routes!(display::get_display_settings))
                .routes(routes!(display::set_display_settings))
                .routes(routes!(display::get_display_state))
                .routes(routes!(display::release_display))
                .routes(routes!(display::set_display_layout))
                .routes(routes!(
                    display::list_custom_presets,
                    display::create_custom_preset
                ))
                .routes(routes!(
                    display::update_custom_preset,
                    display::delete_custom_preset
                ))
                .routes(routes!(host::get_status))
                .routes(routes!(host::get_local_summary))
                .routes(routes!(clients::list_paired_clients))
                .routes(routes!(clients::unpair_client))
                .routes(routes!(clients::get_pairing_status))
                .routes(routes!(clients::submit_pairing_pin))
                .routes(routes!(native::get_native_pairing))
                .routes(routes!(native::arm_native_pairing))
                .routes(routes!(native::disarm_native_pairing))
                .routes(routes!(native::list_native_clients))
                .routes(routes!(native::unpair_native_client))
                .routes(routes!(native::list_pending_devices))
                .routes(routes!(native::approve_pending_device))
                .routes(routes!(native::deny_pending_device))
                .routes(routes!(session::stop_session))
                .routes(routes!(session::request_idr))
                .routes(routes!(library::get_library))
                .routes(routes!(library::list_library_scanners))
                .routes(routes!(library::set_library_scanner))
                .routes(routes!(library::create_custom_game))
                .routes(routes!(
                    library::update_custom_game,
                    library::delete_custom_game
                ))
                .routes(routes!(
                    library::reconcile_provider_entries,
                    library::delete_provider_entries
                ))
                .routes(routes!(library::get_library_art))
                .routes(routes!(stats::stats_capture_start))
                .routes(routes!(stats::stats_capture_stop))
                .routes(routes!(stats::stats_capture_status))
                .routes(routes!(stats::stats_capture_live))
                .routes(routes!(stats::stats_recordings_list))
                .routes(routes!(
                    stats::stats_recording_get,
                    stats::stats_recording_delete
                ))
                .routes(routes!(stats::logs_get))
                .routes(routes!(events::stream_events))
                .routes(routes!(hooks::get_hooks, hooks::set_hooks))
                .routes(routes!(plugins::list_plugins))
                .routes(routes!(plugins::register_plugin, plugins::delete_plugin))
                .routes(routes!(plugins::get_ui_credential))
                .routes(routes!(store::get_catalog))
                .routes(routes!(store::refresh_catalog))
                .routes(routes!(store::list_installed))
                .routes(routes!(store::install_plugin))
                .routes(routes!(store::uninstall_plugin))
                .routes(routes!(store::list_jobs))
                .routes(routes!(store::get_job))
                .routes(routes!(store::list_sources))
                .routes(routes!(store::put_source, store::delete_source))
                .routes(routes!(store::get_runtime, store::set_runtime)),
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
        (name = "events", description = "Host lifecycle events: an SSE stream (client/session/stream lifecycle, pairing, displays, library, host) with Last-Event-ID resume and server-side kind filters"),
        (name = "hooks", description = "Operator hooks: commands and webhooks fired on lifecycle events (fire-and-forget — hooks observe, never veto)"),
        (name = "plugins", description = "Plugin directory: running `punktfunk-plugin-*` processes register a lease and, optionally, a loopback UI the web console proxies and adds to its nav"),
        (name = "store", description = "Plugin store: browse signed catalogs (verified first-party entries, attributed third-party sources), install/uninstall as tracked jobs, and switch the plugin runner on"),
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
