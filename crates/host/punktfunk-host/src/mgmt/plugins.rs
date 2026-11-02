//! The `/plugins` routes over [`crate::plugin_host::registry`]: register, list and deregister a
//! plugin, the console's credential lookup, and the runner's log ingest.

use super::auth::{AnyId, OwnedId, PluginId};
use super::shared::*;
use crate::events::{emit, EventKind};
use crate::plugin_host::registry::{
    registry, sanitize, validate, PluginRegistration, PluginSummary, UiCredential,
};

/// The runner batches on a timer; past this the shipper drops its own backlog rather than let
/// one plugin fill the ring in a single request.
const MAX_LOG_BATCH: usize = 256;

#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginLogLine {
    /// Unix ms, kept verbatim — see [`crate::log_capture::LogRing::push_remote`].
    pub ts_ms: u64,
    /// Normalized by [`crate::log_capture::LogRing::push_remote`]; unknown becomes INFO.
    pub level: String,
    pub source: String,
    pub msg: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginLogBatch {
    pub entries: Vec<PluginLogLine>,
}

/// Source is not a [`crate::slug::plugin_id`] — the runner names a unit by `definePlugin` name,
/// package name, script stem, or `runner`. Sanitize, do not reject: controls go, length is
/// capped, empty becomes `runner` so a line is never attributed to nothing.
fn log_target(source: &str) -> String {
    let mut s = sanitize(source);
    if s.is_empty() {
        s = "runner".into();
    }
    // Cap on chars, not bytes — truncating a multi-byte name mid-sequence would panic.
    if s.chars().count() > 64 {
        s = s.chars().take(64).collect();
    }
    format!("plugin:{s}")
}

/// Register or renew a plugin
///
/// Idempotent lease renew (~30 s). Emits `plugins.changed` only when an operator-visible field
/// changed; a pure renewal is silent.
#[utoipa::path(
    put,
    path = "/plugins/{id}",
    tag = "plugins",
    operation_id = "registerPlugin",
    params(("id" = String, Path, description = "The plugin id (its `definePlugin` name: `[a-z][a-z0-9-]*`)")),
    request_body = PluginRegistration,
    responses(
        (status = NO_CONTENT, description = "Registered / renewed"),
        (status = BAD_REQUEST, description = "Invalid id or registration", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn register_plugin(
    OwnedId(id, _): OwnedId<PluginId>,
    pipe: Option<axum::Extension<crate::https::PipePlugin>>,
    ApiJson(reg): ApiJson<PluginRegistration>,
) -> Response {
    let valid = match validate(reg, pipe.is_some()) {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &e),
    };
    if registry().upsert(&id, valid) {
        tracing::info!(plugin = %id, "plugin registered");
        emit(EventKind::PluginsChanged { id });
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Ingest runner log lines
///
/// Plugins are not host children — their output never hits the host `tracing` subscriber. Lines
/// share the host ring as `plugin:<source>` so `GET /logs` needs no second cursor.
#[utoipa::path(
    post,
    path = "/plugins/logs",
    tag = "plugins",
    operation_id = "ingestPluginLogs",
    request_body = PluginLogBatch,
    responses(
        (status = NO_CONTENT, description = "Lines ingested"),
        (status = BAD_REQUEST, description = "Batch too large", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn ingest_plugin_logs(ApiJson(batch): ApiJson<PluginLogBatch>) -> Response {
    if batch.entries.len() > MAX_LOG_BATCH {
        return api_error(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_LOG_BATCH} entries per batch"),
        );
    }
    for line in batch.entries {
        crate::log_capture::ring().push_remote(
            &line.level,
            &log_target(&line.source),
            &sanitize_msg(&line.msg),
            line.ts_ms,
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Strip controls from an ingested message, keep tabs.
///
/// A newline would let one line forge several in a downloaded log. Tabs stay — they are
/// load-bearing in stack traces and CLI output.
fn sanitize_msg(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\t' || !c.is_control() { c } else { ' ' })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// List registered plugins
///
/// Live, secret-free directory. The console fetches the secret separately, server-side.
#[utoipa::path(
    get,
    path = "/plugins",
    tag = "plugins",
    operation_id = "listPlugins",
    responses(
        (status = OK, description = "Live plugin registrations", body = [PluginSummary]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_plugins() -> Json<Vec<PluginSummary>> {
    let (plugins, expired) = registry().snapshot();
    // Lazy expiry: reaped here exactly once and announced, so the event stream sees a departure
    // with no DELETE.
    for id in expired {
        tracing::info!(plugin = %id, "plugin lease expired");
        emit(EventKind::PluginsChanged { id });
    }
    Json(plugins)
}

/// Fetch a plugin UI's proxy credential
///
/// Server-side `{port, secret}` for the console proxy. The console BFF denylists this from the
/// browser.
#[utoipa::path(
    get,
    path = "/plugins/{id}/ui-credential",
    tag = "plugins",
    operation_id = "getPluginUiCredential",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = OK, description = "The proxy credential", body = UiCredential),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No live plugin with that id, or it serves no UI", body = ApiError),
    )
)]
pub(crate) async fn get_ui_credential(Path(id): Path<String>) -> Response {
    match registry().credential(&id) {
        Some(cred) => Json(cred).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no live plugin UI with that id"),
    }
}

/// Deregister a plugin
///
/// Immediate remove (SDK finalizer on SIGTERM). Emits `plugins.changed` only for a live entry;
/// unknown/expired is a silent 204.
#[utoipa::path(
    delete,
    path = "/plugins/{id}",
    tag = "plugins",
    operation_id = "deregisterPlugin",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = NO_CONTENT, description = "Deregistered (or already absent)"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn delete_plugin(OwnedId(id, _): OwnedId<AnyId>) -> Response {
    if registry().remove(&id) {
        tracing::info!(plugin = %id, "plugin deregistered");
        emit(EventKind::PluginsChanged { id });
    }
    StatusCode::NO_CONTENT.into_response()
}
