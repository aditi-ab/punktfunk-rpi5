//! Library-tagged management endpoints: installed-store + custom game entries and box art.
//! Split out of the `mgmt` facade (plan §W5).

use super::shared::*;
use axum::http::header;

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
        // Store claims are a reconcile-only concern — the manual CRUD never requests one.
        Ok(MutateOutcome::StoreClaimed { .. }) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected claim outcome",
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
        // Store claims are a reconcile-only concern — the manual CRUD never requests one.
        Ok(MutateOutcome::StoreClaimed { .. }) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected claim outcome",
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

/// Query for `reconcileProviderEntries` — the optional store claim (D2).
#[derive(Deserialize)]
pub(crate) struct ReconcileQuery {
    /// Claim this store for the provider, so its entries take the store's own identity.
    store: Option<String>,
}

/// Replace a provider's library entries (declarative reconcile)
///
/// Atomically replaces the full entry set owned by `{provider}` (RFC §8): the payload is the
/// provider's desired list, keyed by its own stable `external_id` — the host diffs, keeps each
/// surviving title's host id stable across reconciles, drops orphans, and never touches manual
/// entries or other providers'. An empty array removes everything the provider owns. Emits
/// `library.changed` with the provider as `source`.
///
/// `?store=` additionally **claims** that store for the provider: its entries then surface with
/// deterministic `<store>:<external_id>` ids and the store's own badge, instead of opaque
/// `custom:<id>` ones — which is what lets a library plugin reproduce the entries an in-host scanner
/// used to produce, right down to the GameStream app ids and client-side art caches. One provider
/// per store; a second claimant gets 409. While a claim is held the matching built-in scanner is
/// suppressed, so the two never double-list. The claim is released by `DELETE`, not by an empty
/// reconcile (a store can legitimately have zero installed titles).
#[utoipa::path(
    put,
    path = "/library/provider/{provider}",
    tag = "library",
    operation_id = "reconcileProviderEntries",
    params(
        ("provider" = String, Path, description = "The provider id ([a-z0-9._-], `manual` reserved)"),
        ("store" = Option<String>, Query, description = "Claim this store for the provider ([a-z0-9_-], `custom`/`manual` reserved)"),
    ),
    request_body = Vec<crate::library::ProviderEntryInput>,
    responses(
        (status = OK, description = "The provider's resulting entries (host ids assigned/kept)", body = [crate::library::CustomEntry]),
        (status = BAD_REQUEST, description = "Invalid provider id, store id, or payload", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "That store is already claimed by another provider", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the catalog", body = ApiError),
    )
)]
pub(crate) async fn reconcile_provider_entries(
    Path(provider): Path<String>,
    Query(q): Query<ReconcileQuery>,
    ApiJson(inputs): ApiJson<Vec<crate::library::ProviderEntryInput>>,
) -> Response {
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    let store = q.store.filter(|s| !s.is_empty());
    if let Some(store) = &store {
        if let Err(e) = crate::library::validate_store_claim(store) {
            return api_error(StatusCode::BAD_REQUEST, &e);
        }
    }
    if let Err(e) = crate::library::validate_provider_payload(&inputs) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    match crate::library::reconcile_provider(&provider, store.as_deref(), inputs) {
        Ok(crate::library::MutateOutcome::Done(entries)) => {
            tracing::info!(
                provider,
                store = store.as_deref().unwrap_or("-"),
                count = entries.len(),
                "library provider reconciled"
            );
            Json(entries).into_response()
        }
        Ok(crate::library::MutateOutcome::StoreClaimed { store, provider }) => api_error(
            StatusCode::CONFLICT,
            &format!("store `{store}` is already claimed by provider `{provider}`"),
        ),
        Ok(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "unexpected reconcile outcome",
        ),
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
/// the image bytes. Any id stored in the host's catalog (manual entries, provider-synced entries,
/// and a library plugin's claimed-store entries) serves its local art file. A Steam title falls back
/// to the in-host scanner's resolver: the host's own local Steam cache first (exact — it's what the
/// user's Steam client already shows for it), the public Steam CDN's flat URL convention second
/// (newer titles' CDN assets can live at a per-asset-hash path the host can't predict, in which case
/// this 404s and the client falls through to its next art candidate).
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
    // `library.json` FIRST, for ANY id (WP1.2). Stored entries — manual, provider-synced, and (once
    // store claims land) a scanner plugin's `steam:570` — all serve their local art file from here,
    // so the proxy never has to know which store an id belongs to. Steam ids aren't stored today, so
    // this misses and the legacy branch below still answers them.
    let stored = {
        let id = id.clone();
        tokio::task::spawn_blocking(move || crate::library::library_local_art_bytes(&id, kind))
            .await
    };
    if let Ok(Some((bytes, ctype))) = stored {
        return ([(header::CONTENT_TYPE, ctype)], bytes).into_response();
    }
    // Legacy in-host Steam scanner: local Steam cache, then the flat CDN URL. Retired with the
    // scanner itself once the steam plugin claims the store (M6).
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
    api_error(StatusCode::NOT_FOUND, "no art of that kind for this title")
}
