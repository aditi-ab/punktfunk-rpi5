//! The two routes of a plugin's page channel ([`crate::plugin_host::channel`]): the plugin
//! attaching a connection, and the console's request to the page that goes down one.

use crate::mgmt::auth::PluginIdentity;
use crate::mgmt::shared::{api_error, ApiError};
use crate::plugin_host::channel::{park, send, Unreached};
use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

const UPGRADE_PROTOCOL: &str = "punktfunk-ui";

/// Attach a plugin UI channel
///
/// `Upgrade: punktfunk-ui` from the plugin, on its own pipe. The connection is kept for the
/// console's next request to the plugin's page, which the host sends down it as plain HTTP.
#[utoipa::path(
    get,
    path = "/plugins/{id}/ui/attach",
    tag = "plugins",
    operation_id = "attachPluginUi",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = SWITCHING_PROTOCOLS, description = "The connection is parked for the plugin's page"),
        (status = BAD_REQUEST, description = "Not an `Upgrade: punktfunk-ui` request", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not this plugin's own identity", body = ApiError),
    )
)]
pub(crate) async fn attach(
    Path(id): Path<String>,
    who: Option<Extension<PluginIdentity>>,
    req: Request,
) -> Response {
    if who.map(|w| w.0 .0) != Some(id.clone()) {
        return api_error(StatusCode::FORBIDDEN, "a plugin attaches its own channel");
    }
    let wants = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(UPGRADE_PROTOCOL));
    if !wants {
        return api_error(StatusCode::BAD_REQUEST, "send Upgrade: punktfunk-ui");
    }
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => park(&id, upgraded),
            Err(e) => tracing::debug!(plugin = %id, error = %e, "plugin UI channel not upgraded"),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, UPGRADE_PROTOCOL)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Where a page request is going: the plugin, and the path under its page (empty at the root).
#[derive(serde::Deserialize)]
pub(crate) struct PagePath {
    id: String,
    #[serde(default)]
    rest: String,
}

/// The console's request to a plugin page, any method, under `/plugins/{id}/ui/`. Admin lane:
/// the host adds the plugin's secret, so the console never holds it for a channel plugin.
pub(crate) async fn proxy(Path(PagePath { id, rest }): Path<PagePath>, req: Request) -> Response {
    let Some(cred) = crate::plugin_host::registry::ui_credential(&id).filter(|c| c.port == 0)
    else {
        return api_error(
            StatusCode::NOT_FOUND,
            "no live plugin UI channel with that id",
        );
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let (parts, body) = req.into_parts();
    let target = format!("/{rest}{query}");
    match send(
        &id,
        &cred.secret,
        parts.method,
        &target,
        &parts.headers,
        body,
    )
    .await
    {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Response::from_parts(parts, Body::new(body))
        }
        Err(Unreached::NotAttached) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the plugin UI is not attached",
        ),
        Err(Unreached::Io(e)) => api_error(StatusCode::BAD_GATEWAY, &format!("plugin UI: {e}")),
    }
}
