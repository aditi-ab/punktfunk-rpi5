//! Stats/logs-tagged management endpoints: performance-capture control + time-series and the
//! in-memory log stream. Split out of the `mgmt` facade (plan §W5).

use super::shared::*;
use crate::log_capture::LogPage;
use crate::stats_recorder::Capture;
use crate::stats_recorder::CaptureMeta;
use crate::stats_recorder::StatsStatus;

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
pub(crate) async fn stats_capture_start(State(st): State<Arc<MgmtState>>) -> Json<StatsStatus> {
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
pub(crate) async fn stats_capture_stop(State(st): State<Arc<MgmtState>>) -> Response {
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
pub(crate) async fn stats_capture_status(State(st): State<Arc<MgmtState>>) -> Json<StatsStatus> {
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
pub(crate) async fn stats_capture_live(State(st): State<Arc<MgmtState>>) -> Response {
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
pub(crate) async fn stats_recordings_list(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<CaptureMeta>> {
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
pub(crate) async fn stats_recording_get(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
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
pub(crate) async fn stats_recording_delete(
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
pub(crate) struct LogsQuery {
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
pub(crate) async fn logs_get(Query(q): Query<LogsQuery>) -> Json<LogPage> {
    let limit = q.limit.map_or(crate::log_capture::MAX_PAGE, |l| l as usize);
    Json(crate::log_capture::ring().since(q.after.unwrap_or(0), limit))
}
