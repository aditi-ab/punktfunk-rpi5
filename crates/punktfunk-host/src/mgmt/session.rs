//! Session-tagged management endpoints: stop the active session, force an IDR. Split out of the
//! `mgmt` facade (plan §W5).

use super::shared::*;
use std::sync::atomic::Ordering;

/// Stop the active session
///
/// Kicks the connected client: stops the video/audio stream threads and clears the launch
/// state. Idempotent — succeeds even when nothing is streaming.
///
/// Counts as a **deliberate** stop, exactly like a client pressing Stop: the display skips its
/// keep-alive linger, and the end-game-on-session-end policy (if the operator enabled one) applies.
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
    // Native plane: the GameStream teardown above doesn't reach it (it runs its own loops off the shared
    // session registry), so signal every live native session to tear down too.
    let native = crate::session_status::count();
    crate::session_status::stop_all_quit();
    tracing::info!(
        was_streaming,
        native_sessions = native,
        "management API: session stopped"
    );
    StatusCode::NO_CONTENT
}

/// End a launched game
///
/// Ends a game whose session has already gone and which is waiting out its reconnect window — the
/// console's "End now" for a game the host is about to close anyway. `app_id` picks one title; omit it
/// to end every waiting game.
///
/// This does **not** touch a game whose session is still live: ending that is session management
/// (`DELETE /session`), and how the game is treated then follows the operator's policy.
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

/// Request body for `endGame`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct EndGameRequest {
    /// Store-qualified library id (`steam:570`) to end; omit to end every waiting game.
    #[serde(default)]
    pub app_id: Option<String>,
}

/// Result of an `endGame`.
#[derive(Serialize, ToSchema)]
pub(crate) struct EndGameResult {
    /// How many waiting games were ended.
    ended: usize,
}

/// The session⇄game lifetime settings, plus which axes this build acts on.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionSettingsState {
    /// The stored settings (or the built-in defaults when this host has never been configured).
    settings: crate::session_settings::SessionSettings,
    /// Whether an operator has ever saved these settings (`false` ⇒ `settings` are the defaults).
    configured: bool,
    /// Which fields this build actually enforces. Empty on a platform with no launch path (macOS),
    /// so the console can say so instead of offering a switch that does nothing.
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

/// Session⇄game lifetime settings
///
/// Whether a launched game's exit ends the streaming session, and whether a session ending ends the
/// game (with the reconnect window that protects a dropped client's unsaved progress). See
/// `design/session-game-lifetime.md`.
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

/// Set the session⇄game lifetime settings
///
/// Persists the settings (clamped) and applies them from the next decision — including to a session
/// that is already streaming, since the policy is read when a session ends rather than when it starts.
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
