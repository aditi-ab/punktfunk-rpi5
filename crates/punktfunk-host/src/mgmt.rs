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

mod actions;
mod auth;
mod client_logs;
mod clients;
mod diagnostics;
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
mod update;

/// Lets `library::plugin_launch`'s tests put a stub plugin in the registry (test-only).
#[cfg(test)]
pub(crate) use plugins::register_ui_for_test;
/// The launch path asks a library plugin what to run for its own entries, and needs the loopback
/// credential this process already holds for it. Re-exported (rather than opening the whole
/// `plugins` module crate-wide) so these two are the ONLY things `mgmt` lends to the library side.
pub(crate) use plugins::ui_credential;

/// Default management port — adjacent to the GameStream block (47984…48010), and the same
/// number Sunshine users already associate with "the config UI".
///
/// ⚠ That last part is also why it is the ONE port a Sunshine fork and a GameStream-off Punktfunk
/// still collide on (47990 is their web UI). Moving it is supported — see [`publish_endpoint`] and
/// `PUNKTFUNK_MGMT_BIND` — and every consumer derives the real port rather than assuming this one.
pub const DEFAULT_PORT: u16 = 47990;

/// The file [`publish_endpoint`] writes the effective mgmt URL to, next to `mgmt-token`.
const ENDPOINT_FILE: &str = "mgmt-endpoint";

/// The port the management API actually bound, recorded once by [`publish_endpoint`].
static EFFECTIVE_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// The mgmt port this process is serving on, or `0` when there is no management API at all — the
/// standalone `punktfunk1-host` binary, which never calls [`publish_endpoint`].
///
/// The native handshake reads this to put the port in every session's `Welcome`, so a client learns
/// it over the connection it has already authenticated instead of needing the mDNS advert. Resolved
/// ONCE, from the same value the endpoint file carries, so the wire, the file and the advert cannot
/// disagree — the whole point of this being a lookup rather than a fourth place to compute a port.
///
/// ⚠ `0` matters: advertising 47990 from a host with no mgmt API would point clients at a port
/// nothing is listening on, which is strictly worse than saying nothing and letting them fall back.
pub fn effective_port() -> u16 {
    EFFECTIVE_PORT.get().copied().unwrap_or(0)
}

/// Publish the mgmt API's *effective* loopback URL to `<config-dir>/mgmt-endpoint`, in the same
/// `KEY=VALUE` form as `mgmt-token` so the bundled console can source it directly as a systemd
/// `EnvironmentFile` (and `windows::service::spawn_web` can read it with `read_env_file_value`).
///
/// **Why this exists:** the port used to be a literal `47990` in five places — this constant, the
/// Windows service's console launch, `scripts/punktfunk-web.service`, the NixOS module, and the
/// console's own default. Moving the listener therefore silently broke the console, because nothing
/// downstream had any way to learn the new port. Now the host is the single source of truth and
/// publishes what it actually bound; consumers keep a 47990 fallback purely so an OLD host with a
/// NEW console still works. The plugin runner / SDK (`sdk/src/config.ts::publishedMgmtUrl`) and
/// the tray (`pf_paths::published_mgmt_port`) read the same file — both used to be a sixth and
/// seventh literal 47990, and a moved port left every plugin dialing the old one in silence
/// (field report 2026-08-18).
///
/// Always loopback, never `bind`'s own address: the console proxies over loopback by design (see
/// the module docs — the bearer-token admin surface is confined to loopback peers), so a wide
/// `0.0.0.0` bind must not be echoed here as a LAN URL.
///
/// Best-effort: a console that cannot read this simply falls back to 47990, which is strictly what
/// it did before, so a write failure must not stop the host from serving.
pub fn publish_endpoint(bind: SocketAddr) {
    // Record it for [`effective_port`] BEFORE the write: the native handshake reads that to put the
    // port in every Welcome, and a failed file write must not also cost us the in-band answer.
    let _ = EFFECTIVE_PORT.set(bind.port());
    let dir = pf_paths::config_dir();
    if let Err(e) = pf_paths::create_private_dir(&dir) {
        tracing::warn!(error = %e, "could not create the config dir to publish the mgmt endpoint");
        return;
    }
    match write_endpoint(&dir, bind.port()) {
        Ok(path) => {
            tracing::debug!(path = %path.display(), port = bind.port(), "published mgmt endpoint")
        }
        Err(e) => tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "could not publish the mgmt endpoint — a console on another port will fall back to 47990"
        ),
    }
}

/// The IO half of [`publish_endpoint`], taking the directory so it is testable without touching
/// `PUNKTFUNK_CONFIG_DIR` (which every other test in this process shares).
///
/// Deliberately NOT `pf_paths::write_secret_file`: this is not a secret — the same port is already
/// in the mDNS TXT record — and locking it to SYSTEM/Administrators on Windows would keep a
/// user-session console from reading the very thing it is published for. The 0700 config dir is the
/// access control that matters.
fn write_endpoint(dir: &std::path::Path, port: u16) -> std::io::Result<std::path::PathBuf> {
    let path = dir.join(ENDPOINT_FILE);
    // Write-then-rename rather than a plain truncating write: the console's systemd unit may source
    // this file at any moment, including while the host is restarting and rewriting it. A torn read
    // would hand systemd an EMPTY `PUNKTFUNK_MGMT_URL`, which is worse than a missing file — the
    // built-in default only applies to an UNSET variable, not a set-but-blank one. `rename` over an
    // existing path is atomic on Unix and replaces on Windows, so a reader sees old or new, never
    // half. (The consumers treat blank as unset too — this is the belt to that pair of braces.)
    let tmp = dir.join(format!("{ENDPOINT_FILE}.tmp"));
    std::fs::write(&tmp, endpoint_line(port))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// The published line. Must stay valid as BOTH a systemd `EnvironmentFile` entry and input to
/// `windows::service::read_env_file_value` — i.e. exactly one `KEY=VALUE` line, no quoting, and no
/// `=` inside the value (a URL has none).
fn endpoint_line(port: u16) -> String {
    format!("PUNKTFUNK_MGMT_URL=https://127.0.0.1:{port}\n")
}

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
    /// Log bundles paired clients uploaded (`crate::client_logs`) — constructed here (mgmt-only
    /// state, nothing streams into it) and listed/served back to the console.
    client_logs: Arc<crate::client_logs::ClientLogStore>,
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
    // The identity split (`crate::identity`): the mgmt API must present the SAME identity as
    // the native QUIC plane — paired clients hold ONE pinned fingerprint for both — so the
    // caller resolves it once and hands it to both.
    identity: crate::identity::NativeIdentity,
) -> Result<()> {
    // Close out any update-intent record a previous apply left behind (the update reports its
    // own outcome across its own restart — update/jobs.rs). Once per boot, before serving.
    crate::update::reconcile_at_boot();
    // Keep a status tray alive for as long as this host runs. The tray has no supervisor of its
    // own — the HKLM `Run` value is a sign-in trigger — so every upgrade's `StopTrays` (and every
    // crash) left the box without an icon until the next logon. See `windows::tray::supervise`.
    #[cfg(target_os = "windows")]
    crate::tray::supervise();

    // The mgmt API is HTTPS + token-authenticated ALWAYS (even on loopback): `parse_serve`
    // guarantees a token (CLI flag / env / persisted ~/.config/punktfunk/mgmt-token / generated).
    // A blank token is treated as none — fail loudly rather than ever serve unauthenticated.
    let token = opts
        .token
        .filter(|t| !t.trim().is_empty())
        .context("management API has no token — internal error: parse_serve must provide one")?;
    // Serve over HTTPS with the native identity (the cert clients already pin — see the
    // `identity` parameter) and OPTIONAL client-cert auth: a paired native client presents its
    // cert (authorized by fingerprint, no token), a browser presents none and uses the bearer
    // token. See `require_auth`.
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
        crate::client_logs::default_dir(),
        gamestream_enabled,
    );
    serve_https(opts.bind, app, tls).await
}

/// Compose the full management router (also used directly by the handler tests).
#[allow(clippy::too_many_arguments)] // the composition root wires one state struct; a param per field
fn app(
    state: Arc<AppState>,
    token: Option<String>,
    plugin_token: Option<String>,
    port: u16,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    // Where uploaded client log bundles live — a parameter (not `default_dir()` inline) so the
    // handler tests point it at a temp dir instead of the real config dir.
    client_logs_dir: std::path::PathBuf,
    gamestream_enabled: bool,
) -> Router {
    let shared = Arc::new(MgmtState {
        app: state,
        native,
        stats,
        client_logs: crate::client_logs::ClientLogStore::new(client_logs_dir),
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
    let api_v1 = OpenApiRouter::new()
        .routes(routes!(host::get_health))
        .routes(routes!(host::get_host_info))
        .routes(routes!(host::list_compositors))
        .routes(routes!(gpu::list_gpus))
        .routes(routes!(gpu::set_gpu_preference))
        .routes(routes!(display::get_display_settings))
        .routes(routes!(display::set_display_settings))
        .routes(routes!(display::get_display_state))
        .routes(routes!(display::get_display_monitors))
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
        // Two paths, so two calls — `routes!` merges the METHODS of one path, and two calls naming
        // the same path collide.
        .routes(routes!(diagnostics::get_diagnostics))
        .routes(routes!(diagnostics::refresh_diagnostics))
        // GET and DELETE share the `/clients` path, so they must be ONE `routes!` — utoipa-axum
        // merges the methods of a single call into one route; two calls collide on the path.
        .routes(routes!(
            clients::list_paired_clients,
            clients::unpair_all_clients
        ))
        // DELETE and PATCH share `/clients/{fingerprint}` — one `routes!`, same rule as above.
        .routes(routes!(clients::unpair_client, clients::rename_client));
    // The GameStream PIN flow exists only when the compat planes do (WP19) — a native-only
    // build's API (and its OpenAPI document) simply has no such endpoints.
    #[cfg(feature = "gamestream")]
    let api_v1 = api_v1
        .routes(routes!(clients::get_pairing_status))
        .routes(routes!(clients::submit_pairing_pin));
    let api_v1 = api_v1
        .routes(routes!(native::get_native_pairing))
        .routes(routes!(native::arm_native_pairing))
        .routes(routes!(native::disarm_native_pairing))
        // Same-path pair as `/clients` above — one `routes!` for both methods.
        .routes(routes!(
            native::list_native_clients,
            native::unpair_all_native_clients
        ))
        // DELETE and PATCH share `/native/clients/{fingerprint}` — one `routes!`, same rule.
        .routes(routes!(
            native::unpair_native_client,
            native::update_native_client_access
        ))
        .routes(routes!(native::list_pending_devices))
        .routes(routes!(native::approve_pending_device))
        .routes(routes!(native::deny_pending_device))
        .routes(routes!(session::stop_session))
        .routes(routes!(session::request_idr))
        .routes(routes!(
            session::get_session_settings,
            session::set_session_settings
        ))
        .routes(routes!(session::end_game))
        .routes(routes!(library::get_library))
        .routes(routes!(library::list_library_scanners))
        .routes(routes!(library::set_library_scanner))
        .routes(routes!(library::set_library_entry_hidden))
        .routes(routes!(library::create_custom_game))
        .routes(routes!(
            library::update_custom_game,
            library::delete_custom_game
        ))
        .routes(routes!(
            library::reconcile_provider_entries,
            library::delete_provider_entries
        ))
        .routes(routes!(library::report_provider_running))
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
        .routes(routes!(
            client_logs::client_logs_upload,
            client_logs::client_logs_list
        ))
        .routes(routes!(
            client_logs::client_logs_get,
            client_logs::client_logs_delete
        ))
        .routes(routes!(events::stream_events))
        .routes(routes!(hooks::get_hooks, hooks::set_hooks))
        .routes(routes!(plugins::list_plugins))
        .routes(routes!(plugins::register_plugin, plugins::delete_plugin))
        .routes(routes!(plugins::get_ui_credential))
        .routes(routes!(plugins::ingest_plugin_logs))
        .routes(routes!(store::get_catalog))
        .routes(routes!(store::refresh_catalog))
        .routes(routes!(store::list_installed))
        .routes(routes!(store::install_plugin))
        .routes(routes!(store::uninstall_plugin))
        .routes(routes!(store::list_jobs))
        .routes(routes!(store::get_job))
        .routes(routes!(store::list_sources))
        .routes(routes!(store::put_source, store::delete_source))
        .routes(routes!(store::get_runtime, store::set_runtime))
        .routes(routes!(update::get_update_status))
        .routes(routes!(update::force_update_check))
        .routes(routes!(update::apply_update))
        .routes(routes!(actions::list_actions))
        .routes(routes!(actions::invoke_action));
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .nest("/api/v1", api_v1)
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
        (name = "diagnostics", description = "Host health checks: what is wrong, what it breaks, and how to fix it (admin lane only)"),
        (name = "gpu", description = "GPU inventory and selection: list the host's GPUs, choose automatic or a preferred GPU, see the one in use"),
        (name = "display", description = "Virtual-display management policy: lifecycle (keep-alive), topology (primary/exclusive), conflict handling, identity, and layout"),
        (name = "clients", description = "Paired Moonlight client management"),
        (name = "pairing", description = "Pairing PIN delivery (the out-of-band half of the GameStream pairing handshake)"),
        (name = "native", description = "Native punktfunk/1 pairing: arm a window, display the host PIN, manage paired devices"),
        (name = "session", description = "Active streaming session control"),
        (name = "library", description = "Game library: the titles each installed library plugin syncs, plus user-curated custom entries"),
        (name = "stats", description = "Streaming performance-stats capture: arm/stop a recording, read the live + saved time-series for graphing"),
        (name = "logs", description = "Host log stream: the newest in-memory log entries, cursor-paged for live following"),
        (name = "events", description = "Host lifecycle events: an SSE stream (client/session/stream lifecycle, pairing, displays, library, host) with Last-Event-ID resume and server-side kind filters"),
        (name = "hooks", description = "Operator hooks: commands and webhooks fired on lifecycle events (fire-and-forget — hooks observe, never veto)"),
        (name = "plugins", description = "Plugin directory: running `punktfunk-plugin-*` processes register a lease and, optionally, a loopback UI the web console proxies and adds to its nav"),
        (name = "store", description = "Plugin store: browse signed catalogs (verified first-party entries, attributed third-party sources), install/uninstall as tracked jobs, and switch the plugin runner on"),
        (name = "update", description = "Host update check: install kind + channel, the last verified release manifest, and whether a newer host exists (admin lane only)"),
        (name = "actions", description = "Host actions: discover what this host offers (per-caller availability + permission) and invoke one by id — v1: sleep, restart, shut down the machine, gated per device by the Host power grant"),
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
