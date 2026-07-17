//! Session-tagged management endpoints: stop the active session, force an IDR. Split out of the
//! `mgmt` facade (plan §W5).

use super::shared::*;
use std::sync::atomic::Ordering;

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
pub(crate) async fn stop_session(State(st): State<Arc<MgmtState>>) -> StatusCode {
    let was_streaming = st.app.streaming.swap(false, Ordering::SeqCst);
    st.app.audio_streaming.store(false, Ordering::SeqCst);
    *st.app.launch.lock().unwrap_or_else(|e| e.into_inner()) = None;
    *st.app.stream.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // Native plane: the GameStream flags above don't reach it (it runs its own loops off the shared
    // session registry), so signal every live native session to tear down too.
    let native = crate::session_status::count();
    crate::session_status::stop_all();
    tracing::info!(
        was_streaming,
        native_sessions = native,
        "management API: session stopped"
    );
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
pub(crate) async fn request_idr(State(st): State<Arc<MgmtState>>) -> Response {
    let gs = st.app.streaming.load(Ordering::SeqCst);
    let native = crate::session_status::count();
    if !gs && native == 0 {
        return api_error(StatusCode::CONFLICT, "no active video stream");
    }
    if gs {
        st.app.force_idr.store(true, Ordering::SeqCst);
    }
    // Native sessions get the keyframe request through their registry flag (see `session_status`).
    crate::session_status::force_idr_all();
    StatusCode::ACCEPTED.into_response()
}
