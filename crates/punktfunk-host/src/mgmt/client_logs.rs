//! Client log bundles: the upload endpoint paired devices POST to, and the admin list/fetch/
//! delete surface the web console reads. See `crate::client_logs` for why this is a file store.
//!
//! Lane split (see `auth`): the UPLOAD is the one write a paired streaming cert may perform —
//! it is write-only (a device can never read anything back, not even its own bundle), size-capped,
//! and quota-bounded per device. Listing/fetching/deleting are operator business: bundles can
//! contain whatever the client logged (addresses, host names), so reading them stays on the
//! loopback-only bearer lane with the host's own logs.

use super::shared::*;
use crate::client_logs::{ClientLogMeta, MAX_BUNDLE_BYTES};
use crate::gamestream::tls::PeerCertFingerprint;
use axum::body::Bytes;
use axum::Extension;

/// Response to a successful upload.
#[derive(Serialize, ToSchema)]
pub(crate) struct ClientLogUploaded {
    /// The stored bundle's id.
    pub id: String,
}

/// Upload a client log bundle
///
/// A PAIRED DEVICE posts its recent client log as plain text, authenticated by its streaming
/// certificate (the same mTLS identity it pairs and streams with) — no bearer token. Bundles are
/// capped at 1 MiB and only the newest few per device are kept. The operator downloads them from
/// the console's Logs page. This is deliberately write-only for devices: uploading grants no read.
#[utoipa::path(
    post,
    path = "/client-logs",
    tag = "logs",
    operation_id = "clientLogsUpload",
    request_body(content = String, content_type = "text/plain", description = "The client's log text"),
    responses(
        (status = CREATED, description = "Bundle stored", body = ClientLogUploaded),
        (status = BAD_REQUEST, description = "No paired-device certificate on the connection", body = ApiError),
        (status = FORBIDDEN, description = "The device's access has expired (per-client access)", body = ApiError),
        (status = PAYLOAD_TOO_LARGE, description = "Bundle exceeds the size cap", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "Empty body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not store the bundle", body = ApiError),
    )
)]
pub(crate) async fn client_logs_upload(
    State(st): State<Arc<MgmtState>>,
    fp: Option<Extension<PeerCertFingerprint>>,
    body: Bytes,
) -> Response {
    // The auth middleware admits this route for paired certs AND (like everything) the admin
    // bearer token — but an upload without a device identity has no owner to file it under, so
    // the bearer path is a caller error, not a second way in.
    let Some(Extension(PeerCertFingerprint(Some(fp)))) = fp else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "client log upload requires a paired device certificate",
        );
    };
    if body.is_empty() {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "empty log bundle");
    }
    if body.len() > MAX_BUNDLE_BYTES {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "log bundle exceeds the 1 MiB cap — send the tail",
        );
    }
    // Per-client access (design/per-client-access.md): the auth gate's `is_paired` is
    // EXPIRY-BLIND by design — right for the read-only status GETs (an expired guest still
    // appears in rosters), wrong for this lane's one WRITE. `effective` is the authorization
    // verb: `None` = unpaired or expired ⇒ a lapsed guest can't keep writing bundles to the
    // operator's disk. No specific grant BIT is required — uploading one's own logs is not an
    // input capability, and a view-only guest mid-session is exactly who a debug bundle is
    // wanted from.
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if st
        .native
        .as_ref()
        .and_then(|n| n.effective(&fp, now_unix))
        .is_none()
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "this device's access has expired — ask the host's operator to approve it again",
        );
    }
    // Resolve the device's display name from the paired roster (the auth gate already proved
    // membership; a race with an unpair between the gate and here just falls back to the prefix).
    let device_name = st
        .native
        .as_ref()
        .and_then(|n| {
            n.list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(&fp))
                .map(|c| c.name)
        })
        .unwrap_or_else(|| fp.chars().take(16).collect());
    match st.client_logs.save(&fp, &device_name, &body) {
        Ok(id) => {
            tracing::info!(
                device = %device_name,
                id = %id,
                bytes = body.len(),
                "client log bundle received — listed on the console's Logs page"
            );
            (StatusCode::CREATED, Json(ClientLogUploaded { id })).into_response()
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not store the bundle: {e}"),
        ),
    }
}

/// List uploaded client log bundles
///
/// Every stored bundle's metadata, newest first.
#[utoipa::path(
    get,
    path = "/client-logs",
    tag = "logs",
    operation_id = "clientLogsList",
    responses(
        (status = OK, description = "Stored bundles, newest first", body = [ClientLogMeta]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn client_logs_list(State(st): State<Arc<MgmtState>>) -> Json<Vec<ClientLogMeta>> {
    Json(st.client_logs.list())
}

/// Download a client log bundle
///
/// The bundle body as plain text, for saving or attaching to a report.
#[utoipa::path(
    get,
    path = "/client-logs/{id}",
    tag = "logs",
    operation_id = "clientLogsGet",
    params(("id" = String, Path, description = "The bundle id (its filename stem)")),
    responses(
        (status = OK, description = "The bundle body", body = String, content_type = "text/plain"),
        (status = NOT_FOUND, description = "No bundle with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The bundle file is unreadable", body = ApiError),
    )
)]
pub(crate) async fn client_logs_get(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    match st.client_logs.load(&id) {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            api_error(StatusCode::NOT_FOUND, "no bundle with that id")
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not read the bundle: {e}"),
        ),
    }
}

/// Delete a client log bundle
///
/// Removes the bundle `id` from disk. `404` if there is no such bundle.
#[utoipa::path(
    delete,
    path = "/client-logs/{id}",
    tag = "logs",
    operation_id = "clientLogsDelete",
    params(("id" = String, Path, description = "The bundle id (its filename stem)")),
    responses(
        (status = NO_CONTENT, description = "Bundle deleted"),
        (status = NOT_FOUND, description = "No bundle with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not delete the bundle", body = ApiError),
    )
)]
pub(crate) async fn client_logs_delete(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    match st.client_logs.delete(&id) {
        Ok(()) => {
            tracing::info!(id, "management API: client log bundle deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            api_error(StatusCode::NOT_FOUND, "no bundle with that id")
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not delete the bundle: {e}"),
        ),
    }
}
