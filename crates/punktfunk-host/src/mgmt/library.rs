//! Library-tagged management endpoints: installed-store + custom game entries and box art.
//! Split out of the `mgmt` facade (plan §W5).

use super::auth::AuthLane;
use super::shared::*;
use axum::http::header;
use axum::Extension;

/// Refuse a write whose payload carries an operator-privileged field to a lane that may not set one
/// (2026-08-05 review H-1), and refuse any local art path the proxy would not serve back (H-2).
///
/// Both checks belong here rather than in the route gate: `PUT /library/provider/{p}` is a route a
/// provider plugin must be able to call — reconciling its own entry set is the whole point of a
/// scanner plugin — while `prep` / `launch.kind = "command"` inside that payload are the operator's
/// authority alone. Route reachability and field authority are separate questions.
fn check_entry_fields(
    lane: AuthLane,
    art: &crate::library::Artwork,
    launch: Option<&crate::library::LaunchSpec>,
    prep: &[crate::hooks::PrepCmd],
) -> Result<(), Response> {
    if !lane.may_set_privileged_fields() {
        if let Some(field) = crate::library::privileged_field(launch, prep) {
            return Err(api_error(
                StatusCode::FORBIDDEN,
                &format!(
                    "`{field}` is executed as the host user and may only be set with the \
                     operator's admin token — a plugin may publish entries with any host-resolved \
                     launch kind (steam_appid, epic, gog, aumid, lutris_id, heroic) instead"
                ),
            ));
        }
    }
    crate::library::validate_art_paths(art).map_err(|e| api_error(StatusCode::BAD_REQUEST, &e))
}

#[derive(Deserialize)]
pub(crate) struct LibraryQuery {
    /// Only entries owned by this external provider (RFC §8).
    provider: Option<String>,
    /// Only entries on this platform (case-insensitive).
    platform: Option<String>,
}

/// List the game library
///
/// Every installed-store title (Steam, read from the host's local files — no Steam API key)
/// merged with the user's custom entries, sorted by title. Artwork fields are URLs the client
/// fetches directly (the public Steam CDN for Steam titles). `?provider=` narrows to the
/// entries a given external provider owns; `?platform=` to one platform (case-insensitive —
/// installed-store titles are `PC`, custom/provider entries carry whatever was authored).
#[utoipa::path(
    get,
    path = "/library",
    tag = "library",
    operation_id = "getLibrary",
    params(
        ("provider" = Option<String>, Query, description = "Only entries owned by this external provider"),
        ("platform" = Option<String>, Query, description = "Only entries on this platform (case-insensitive, e.g. `PS2`)"),
    ),
    responses(
        (status = OK, description = "Unified library across all stores", body = [crate::library::GameEntry]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_library(
    Extension(lane): Extension<AuthLane>,
    Query(q): Query<LibraryQuery>,
) -> Json<Vec<crate::library::GameEntry>> {
    let mut games = crate::library::all_games();
    if let Some(provider) = q.provider.filter(|p| !p.is_empty()) {
        games.retain(|g| g.provider.as_deref() == Some(provider.as_str()));
    }
    if let Some(platform) = q.platform.filter(|p| !p.is_empty()) {
        games.retain(|g| {
            g.meta
                .platform
                .as_deref()
                .is_some_and(|p| p.eq_ignore_ascii_case(&platform))
        });
    }
    // Rewrite provider entries' local-file art into host art-proxy URLs so a client fetches covers
    // from the host (a provider like Playnite stores on-host paths; the payload stays tiny at any
    // library size, and the client never sees an unreachable `C:\…`).
    for g in &mut games {
        crate::library::proxy_local_art(&g.id, &mut g.art);
    }
    // Redact the operator's command lines for every lane but their own (2026-08-05 review L-1).
    //
    // `cert_may_access` allows `GET /library`, so this response goes to every paired STREAMING
    // client on the LAN — and for a custom entry `launch.value` is the raw shell command or
    // absolute exe path the operator typed. The adjacent `detect` field is `#[serde(skip)]` for
    // exactly this reason; `launch` simply never got the same treatment. Clients don't need it:
    // a client picks a title by ID and the host resolves the recipe itself (`resolve_launch`),
    // which is the invariant that stops a client injecting a command in the first place. The
    // `kind` stays, so "this is launchable, and how" still renders.
    if !lane.may_set_privileged_fields() {
        for g in &mut games {
            if let Some(l) = g.launch.as_mut() {
                if l.kind == "command" {
                    l.value.clear();
                }
            }
        }
    }
    Json(games)
}

/// Request body for `setLibraryScanner`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ScannerToggle {
    /// Whether the scanner should run on this host.
    enabled: bool,
}

/// List the library scanners
///
/// The installed-store scanners this host supports — the list is platform-dependent (Steam
/// everywhere; Lutris + Heroic on Linux; Epic, GOG, and Xbox/Game Pass on Windows), so the console
/// renders a toggle only for scanners that can do anything here. Scanners default to enabled;
/// disabling one hides its titles from every library surface from the next read. The user-curated
/// custom store is not a scanner and is always on.
#[utoipa::path(
    get,
    path = "/library/scanners",
    tag = "library",
    operation_id = "listLibraryScanners",
    responses(
        (status = OK, description = "This host's scanners with their enable state", body = [crate::library::ScannerInfo]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_library_scanners() -> Json<Vec<crate::library::ScannerInfo>> {
    Json(crate::library::list_scanners())
}

/// Enable or disable a library scanner
///
/// Persists the toggle and applies it from the next library read (no restart). Disabling a scanner
/// hides its titles everywhere — the console grid, native clients, and the GameStream app list —
/// and re-enabling brings them straight back (nothing is deleted; the scan just runs again). Emits
/// `library.changed` with the scanner id as `source` when the state changed.
#[utoipa::path(
    put,
    path = "/library/scanners/{id}",
    tag = "library",
    operation_id = "setLibraryScanner",
    params(("id" = String, Path, description = "The scanner id (e.g. `steam`)")),
    request_body = ScannerToggle,
    responses(
        (status = OK, description = "Toggle stored; the full scanner list", body = [crate::library::ScannerInfo]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No such scanner on this platform", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the settings", body = ApiError),
    )
)]
pub(crate) async fn set_library_scanner(
    Path(id): Path<String>,
    ApiJson(toggle): ApiJson<ScannerToggle>,
) -> Response {
    match crate::library::set_scanner_enabled(&id, toggle.enabled) {
        Ok(Some(scanners)) => {
            tracing::info!(
                scanner = %id,
                enabled = toggle.enabled,
                "management API: library scanner toggled"
            );
            Json(scanners).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such scanner on this platform"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
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
    Extension(lane): Extension<AuthLane>,
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    if let Err(denied) = check_entry_fields(lane, &input.art, input.launch.as_ref(), &input.prep) {
        return denied;
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
    Extension(lane): Extension<AuthLane>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    if let Err(denied) = check_entry_fields(lane, &input.art, input.launch.as_ref(), &input.prep) {
        return denied;
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
    Extension(lane): Extension<AuthLane>,
    Path(provider): Path<String>,
    ApiJson(inputs): ApiJson<Vec<crate::library::ProviderEntryInput>>,
) -> Response {
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    if let Err(e) = crate::library::validate_provider_payload(&inputs) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    // Every entry in the payload, not just the first — a reconcile replaces a whole entry set, so
    // one privileged field anywhere in it is one command execution.
    for (i, e) in inputs.iter().enumerate() {
        if let Err(denied) = check_entry_fields(lane, &e.art, e.launch.as_ref(), &e.prep) {
            tracing::warn!(
                provider,
                index = i,
                "library reconcile refused: payload carries a field this lane may not set"
            );
            return denied;
        }
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
    // Steam: CDN / local-cache proxy (id `steam:<appid>`).
    if let Some(appid) = id
        .strip_prefix("steam:")
        .and_then(|s| s.parse::<u32>().ok())
    {
        return match tokio::task::spawn_blocking(move || {
            crate::library::steam_art_bytes(appid, kind)
        })
        .await
        {
            Ok(Some((bytes, ctype))) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
            _ => api_error(StatusCode::NOT_FOUND, "no art of that kind for this title"),
        };
    }
    // Custom/provider entry (id `custom:<id>`): serve its stored LOCAL art file — e.g. the Playnite
    // plugin's covers, reconciled as on-host paths rather than inlined bytes.
    if let Some(cid) = id.strip_prefix("custom:").map(str::to_owned) {
        return match tokio::task::spawn_blocking(move || {
            crate::library::custom_local_art_bytes(&cid, kind)
        })
        .await
        {
            Ok(Some((bytes, ctype))) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
            _ => api_error(StatusCode::NOT_FOUND, "no art of that kind for this title"),
        };
    }
    api_error(StatusCode::NOT_FOUND, "no art proxy for this store")
}
