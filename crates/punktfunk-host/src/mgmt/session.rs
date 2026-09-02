//! Session-tagged management HTTP handlers: stop, IDR, end-game, session⇄game lifetime.
//!
//! `DELETE /session` is a deliberate stop: skip keep-alive linger and apply
//! `game_on_session_end`. Policy: `design/session-game-lifetime.md`.

use super::shared::*;
use std::sync::atomic::Ordering;

/// Deliberate stop: skip keep-alive linger and apply `game_on_session_end`.
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
    let was_streaming = st.app.quit_session("management API stop");
    // Native sessions run off the GameStream registry; `quit_session` does not reach them.
    let native = crate::session_status::count();
    crate::session_status::stop_all_quit();
    tracing::info!(
        was_streaming,
        native_sessions = native,
        "management API: session stopped"
    );
    StatusCode::NO_CONTENT
}

/// End games waiting out the reconnect window. Does not touch a live session
/// (`DELETE /session` plus `game_on_session_end`).
#[utoipa::path(
    post,
    path = "/game/end",
    tag = "session",
    operation_id = "endGame",
    request_body = EndGameRequest,
    responses(
        (status = OK, description = "How many waiting games were ended", body = EndGameResult),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No game is waiting to be ended", body = ApiError),
    )
)]
pub(crate) async fn end_game(ApiJson(req): ApiJson<EndGameRequest>) -> Response {
    let ended = crate::gamelease::end_pending(req.app_id.as_deref());
    if ended == 0 {
        return api_error(StatusCode::CONFLICT, "no game is waiting to be ended");
    }
    tracing::info!(app_id = ?req.app_id, ended, "management API: game ended");
    Json(EndGameResult { ended }).into_response()
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct EndGameRequest {
    /// Store-qualified id (`steam:570`); omit to end every waiting game.
    #[serde(default)]
    pub app_id: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EndGameResult {
    ended: usize,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionSettingsState {
    settings: crate::session_settings::SessionSettings,
    /// `false` means `settings` are the built-in defaults.
    configured: bool,
    /// Axes this build enforces. Empty with no launch path (macOS) so the console
    /// does not offer a no-op switch.
    enforced: Vec<String>,
}

fn session_settings_state() -> SessionSettingsState {
    let store = crate::session_settings::store();
    SessionSettingsState {
        settings: store.get(),
        configured: store.configured(),
        enforced: crate::session_settings::enforced(),
    }
}

#[utoipa::path(
    get,
    path = "/session/settings",
    tag = "session",
    operation_id = "getSessionSettings",
    responses(
        (status = OK, description = "Stored settings + which axes this build enforces", body = SessionSettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_session_settings() -> Json<SessionSettingsState> {
    Json(session_settings_state())
}

/// Persist (clamped). Takes effect on the next decision, including a session
/// already streaming — policy is read at session end, not start.
#[utoipa::path(
    put,
    path = "/session/settings",
    tag = "session",
    operation_id = "setSessionSettings",
    request_body = crate::session_settings::SessionSettings,
    responses(
        (status = OK, description = "Settings stored; the new state", body = SessionSettingsState),
        (status = BAD_REQUEST, description = "Malformed settings body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Settings could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_session_settings(
    ApiJson(settings): ApiJson<crate::session_settings::SessionSettings>,
) -> Response {
    if let Err(e) = crate::session_settings::store().set(settings) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("persist session settings: {e:#}"),
        );
    }
    let state = session_settings_state();
    tracing::info!(
        game_on_session_end = state.settings.game_on_session_end.as_str(),
        session_on_game_exit = state.settings.session_on_game_exit,
        grace_s = state.settings.disconnect_grace_seconds,
        "management API: session⇄game lifetime settings updated"
    );
    Json(state).into_response()
}

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
    // Native sessions take IDR from the registry flag, not `app.force_idr`.
    crate::session_status::force_idr_all();
    StatusCode::ACCEPTED.into_response()
}
