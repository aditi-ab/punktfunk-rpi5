//! Library-tagged management endpoints: installed-store + custom game entries and box art.
//! Split out of the `mgmt` facade (plan §W5).

use super::shared::*;
use axum::http::header;

#[derive(Deserialize)]
pub(crate) struct LibraryQuery {
    /// Only entries owned by this external provider (RFC §8).
    provider: Option<String>,
}

/// List the game library
///
/// Every installed-store title (Steam, read from the host's local files — no Steam API key)
/// merged with the user's custom entries, sorted by title. Artwork fields are URLs the client
/// fetches directly (the public Steam CDN for Steam titles). `?provider=` narrows to the
/// entries a given external provider owns.
#[utoipa::path(
    get,
    path = "/library",
    tag = "library",
    operation_id = "getLibrary",
    params(
        ("provider" = Option<String>, Query, description = "Only entries owned by this external provider"),
    ),
    responses(
        (status = OK, description = "Unified library across all stores", body = [crate::library::GameEntry]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_library(
    Query(q): Query<LibraryQuery>,
) -> Json<Vec<crate::library::GameEntry>> {
    let mut games = crate::library::all_games();
    if let Some(provider) = q.provider.filter(|p| !p.is_empty()) {
        games.retain(|g| g.provider.as_deref() == Some(provider.as_str()));
    }
    Json(games)
}

/// Add a custom library entry
///
/// Creates a user-curated title (e.g. a non-Steam game, an emulator, a ROM) with caller-supplied
/// artwork URLs. The host assigns a stable id, returned in the body.
#[utoipa::path(
    post,
    path = "/library/custom",
    tag = "library",
    operation_id = "createCustomGame",
    request_body = crate::library::CustomInput,
    responses(
        (status = CREATED, description = "Entry created", body = crate::library::CustomEntry),
        (status = BAD_REQUEST, description = "Empty title", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn create_custom_game(
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    match crate::library::add_custom(input) {
        Ok(entry) => (StatusCode::CREATED, Json(entry)).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Update a custom library entry
#[utoipa::path(
    put,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "updateCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    request_body = crate::library::CustomInput,
    responses(
        (status = OK, description = "Entry updated", body = crate::library::CustomEntry),
        (status = BAD_REQUEST, description = "Empty title", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn update_custom_game(
    Path(id): Path<String>,
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    use crate::library::MutateOutcome;
    match crate::library::update_custom(&id, input) {
        Ok(MutateOutcome::Done(entry)) => Json(entry).into_response(),
        Ok(MutateOutcome::NotFound) => {
            api_error(StatusCode::NOT_FOUND, "no custom entry with that id")
        }
        Ok(MutateOutcome::ProviderOwned(p)) => api_error(
            StatusCode::CONFLICT,
            &format!("entry is owned by provider `{p}` — update it through its reconcile"),
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Delete a custom library entry
#[utoipa::path(
    delete,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "deleteCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    responses(
        (status = NO_CONTENT, description = "Entry deleted"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn delete_custom_game(Path(id): Path<String>) -> Response {
    use crate::library::MutateOutcome;
    match crate::library::delete_custom(&id) {
        Ok(MutateOutcome::Done(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(MutateOutcome::NotFound) => {
            api_error(StatusCode::NOT_FOUND, "no custom entry with that id")
        }
        Ok(MutateOutcome::ProviderOwned(p)) => api_error(
            StatusCode::CONFLICT,
            &format!(
                "entry is owned by provider `{p}` — remove it there, or DELETE the provider set"
            ),
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The count envelope a provider uninstall returns.
#[derive(Serialize, ToSchema)]
pub(crate) struct ProviderRemoved {
    /// How many entries the provider owned (and were removed).
    removed: usize,
}

/// Replace a provider's library entries (declarative reconcile)
///
/// Atomically replaces the full entry set owned by `{provider}` (RFC §8): the payload is the
/// provider's desired list, keyed by its own stable `external_id` — the host diffs, keeps each
/// surviving title's host id stable across reconciles, drops orphans, and never touches manual
/// entries or other providers'. An empty array removes everything the provider owns. Emits
/// `library.changed` with the provider as `source`.
#[utoipa::path(
    put,
    path = "/library/provider/{provider}",
    tag = "library",
    operation_id = "reconcileProviderEntries",
    params(("provider" = String, Path, description = "The provider id ([a-z0-9._-], `manual` reserved)")),
    request_body = Vec<crate::library::ProviderEntryInput>,
    responses(
        (status = OK, description = "The provider's resulting entries (host ids assigned/kept)", body = [crate::library::CustomEntry]),
        (status = BAD_REQUEST, description = "Invalid provider id or payload", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn reconcile_provider_entries(
    Path(provider): Path<String>,
    ApiJson(inputs): ApiJson<Vec<crate::library::ProviderEntryInput>>,
) -> Response {
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    if let Err(e) = crate::library::validate_provider_payload(&inputs) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    match crate::library::reconcile_provider(&provider, inputs) {
        Ok(entries) => {
            tracing::info!(
                provider,
                count = entries.len(),
                "library provider reconciled"
            );
            Json(entries).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Remove a provider's library entries
///
/// Deletes every entry owned by `{provider}` — the clean-uninstall path for a provider plugin
/// (RFC §8). Emits `library.changed` when anything was removed.
#[utoipa::path(
    delete,
    path = "/library/provider/{provider}",
    tag = "library",
    operation_id = "deleteProviderEntries",
    params(("provider" = String, Path, description = "The provider id")),
    responses(
        (status = OK, description = "How many entries were removed", body = ProviderRemoved),
        (status = BAD_REQUEST, description = "Invalid provider id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn delete_provider_entries(Path(provider): Path<String>) -> Response {
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    match crate::library::delete_provider(&provider) {
        Ok(removed) => {
            if removed > 0 {
                tracing::info!(provider, removed, "library provider entries removed");
            }
            Json(ProviderRemoved { removed }).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Fetch one cover-art image for a library entry
///
/// Resolves `kind` (`portrait` | `hero` | `logo` | `header`) for the given library id and streams
/// the image bytes. For a Steam title, the host's own local Steam cache is tried first (exact —
/// it's what the user's Steam client already shows for it), the public Steam CDN's flat URL
/// convention as a fallback (newer titles' CDN assets can live at a per-asset-hash path the host
/// can't predict, in which case this 404s and the client falls through to its next art candidate).
/// Only Steam ids are backed today; any other store 404s.
#[utoipa::path(
    get,
    path = "/library/art/{id}/{kind}",
    tag = "library",
    operation_id = "getLibraryArt",
    params(
        ("id" = String, Path, description = "The store-qualified library id, e.g. `steam:570`"),
        ("kind" = String, Path, description = "`portrait` | `hero` | `logo` | `header`"),
    ),
    responses(
        (status = OK, description = "Image bytes", content_type = "image/jpeg"),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = NOT_FOUND, description = "No art of that kind for that id", body = ApiError),
    )
)]
pub(crate) async fn get_library_art(Path((id, kind)): Path<(String, String)>) -> Response {
    let Some(kind) = crate::library::ArtKind::parse(&kind) else {
        return api_error(StatusCode::NOT_FOUND, "unknown art kind");
    };
    let Some(appid) = id
        .strip_prefix("steam:")
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return api_error(StatusCode::NOT_FOUND, "no art proxy for this store");
    };
    match tokio::task::spawn_blocking(move || crate::library::steam_art_bytes(appid, kind)).await {
        Ok(Some((bytes, ctype))) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
        _ => api_error(StatusCode::NOT_FOUND, "no art of that kind for this title"),
    }
}
