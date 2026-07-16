//! Hooks management endpoints (scripting-and-hooks RFC §6): read and replace the operator's
//! `hooks.json` — validated on write, applied immediately (the runner reads the store per
//! event, so no restart is needed).

use super::shared::*;

/// Get the hook configuration
///
/// The operator's `hooks.json`: commands and webhooks fired on host lifecycle events. Empty
/// when unconfigured.
#[utoipa::path(
    get,
    path = "/hooks",
    tag = "hooks",
    operation_id = "getHooks",
    responses(
        (status = OK, description = "The stored hook configuration", body = crate::hooks::HooksConfig),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_hooks() -> Json<crate::hooks::HooksConfig> {
    Json(crate::hooks::store().get())
}

/// Replace the hook configuration
///
/// Validates and persists a full `hooks.json` document (this is a whole-document PUT, not a
/// patch). Applies from the next event — no restart. Hook commands run as the host user
/// (interactive user session on Windows): treat this configuration as operator-privileged.
#[utoipa::path(
    put,
    path = "/hooks",
    tag = "hooks",
    operation_id = "setHooks",
    request_body = crate::hooks::HooksConfig,
    responses(
        (status = OK, description = "Configuration stored; the new state", body = crate::hooks::HooksConfig),
        (status = BAD_REQUEST, description = "Structurally invalid configuration", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Configuration could not be persisted", body = ApiError),
    )
)]
pub(crate) async fn set_hooks(ApiJson(cfg): ApiJson<crate::hooks::HooksConfig>) -> Response {
    if let Err(e) = cfg.validate() {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    match crate::hooks::store().set(cfg) {
        Ok(()) => {
            tracing::info!("management API: hook configuration updated");
            Json(crate::hooks::store().get()).into_response()
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist hooks.json: {e:#}"),
        ),
    }
}
