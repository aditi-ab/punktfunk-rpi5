//! Library-tagged management endpoints: the game catalog (plugin-synced + custom entries), the
//! source toggles, the provider reconcile API and box art. Split out of the `mgmt` facade (plan §W5).

use super::auth::AuthLane;
use super::shared::*;
use axum::http::header;
use axum::Extension;

/// Refuse a write whose payload carries an operator-privileged field to a lane that may not set one
/// (2026-08-05 review H-1), and refuse any local art path the proxy would not serve back (H-2).
///
/// The **single-entry writes** — the operator creating or editing one custom entry. The provider
/// reconcile takes [`check_privileged_fields`] and sanitizes art instead; the split is the whole
/// point, and [`crate::library::sanitize_art_paths`] carries the reasoning.
///
/// Both checks belong here rather than in the route gate: `PUT /library/provider/{p}` is a route a
/// provider plugin must be able to call — reconciling its own entry set is the whole point of a
/// scanner plugin — while `prep` / `launch.kind = "command"` inside that payload are the operator's
/// authority alone. Route reachability and field authority are separate questions.
///
/// `Some((reason, response))` is the refusal to return; `None` means the payload may proceed.
/// Deliberately not `Result<(), Response>`: the "error" here IS the response the handler sends, so
/// there is no error value to propagate, and a 128-byte `Response` in an `Err` variant is what
/// `clippy::result_large_err` objects to.
///
/// `reason` is the caller's log line. It exists because these are TWO different refusals — an
/// operator-privileged field (403) and an unservable art path (400) — and logging both as "carries
/// a field this lane may not set" sent the Lutris/Steam `file://` art rejection looking like an
/// auth problem.
fn check_entry_fields(
    lane: AuthLane,
    art: &crate::library::Artwork,
    launch: Option<&crate::library::LaunchSpec>,
    prep: &[crate::hooks::PrepCmd],
    icon: Option<&str>,
) -> Option<(String, Response)> {
    check_privileged_fields(lane, launch, prep, icon).or_else(|| {
        crate::library::validate_art_paths(art)
            .err()
            .map(|e| (e.clone(), api_error(StatusCode::BAD_REQUEST, &e)))
    })
}

/// The half of [`check_entry_fields`] that is about *authority* rather than about art: an
/// operator-privileged field this lane may not set (403), or an unrepresentable icon token (400).
///
/// Split out for the provider reconcile, which must apply exactly these two and NOT the art check —
/// it sanitizes unservable covers instead of refusing the payload
/// ([`crate::library::sanitize_art_paths`] explains why the two callers want different answers).
fn check_privileged_fields(
    lane: AuthLane,
    launch: Option<&crate::library::LaunchSpec>,
    prep: &[crate::hooks::PrepCmd],
    icon: Option<&str>,
) -> Option<(String, Response)> {
    if !lane.may_set_privileged_fields() {
        if let Some(field) = crate::library::privileged_field(launch, prep) {
            return Some((
                format!("payload carries `{field}`, which this lane may not set"),
                api_error(
                    StatusCode::FORBIDDEN,
                    &format!(
                        "`{field}` is executed as the host user and may only be set with the \
                         operator's admin token — a plugin may publish entries with any host-resolved \
                         launch kind (steam_appid, steam_ui, launcher_ui, epic, gog, aumid, xbox, lutris_id, \
                         heroic, playnite) \
                         instead"
                    ),
                ),
            ));
        }
    }
    // Shape-only, and on every lane: an icon token names no resource the host owns, so there is
    // nothing here for an operator token to unlock — it is refused for being unrepresentable as a
    // slug, not for being privileged. Clients interpolate the value, so the guard belongs upstream
    // of all of them (`crate::library::validate_icon`).
    if let Err(e) = crate::library::validate_icon(icon) {
        return Some((e.clone(), api_error(StatusCode::BAD_REQUEST, &e)));
    }
    None
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
/// Every title this host knows about, sorted by title: the entries each installed library plugin
/// has synced (Steam, Lutris, Heroic, Epic, GOG, Xbox, Playnite, ROM managers, …) plus the user's
/// own custom entries. Artwork fields are URLs the client fetches directly, except local files on
/// the host, which are rewritten to this API's own art proxy. `?provider=` narrows to the entries a
/// given external provider owns; `?platform=` to one platform (case-insensitive — whatever the
/// source authored, conventionally `PC` for desktop stores).
///
/// **The operator's own lane additionally sees the titles they have HIDDEN**, each carrying
/// `hidden: true`; every other lane gets them filtered out upstream and cannot tell they exist. The
/// console needs them to offer "un-hide", and it is the only surface that does.
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
        (status = OK, description = "Unified library across all stores (the operator's lane also gets hidden entries, flagged)", body = [crate::library::OperatorGameEntry]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_library(
    Extension(lane): Extension<AuthLane>,
    Query(q): Query<LibraryQuery>,
) -> Response {
    // The operator's list is a DIFFERENT TYPE, not the same one with a flag set — which is what
    // makes "a hidden title never reaches a paired client" structural rather than a filter someone
    // has to remember. The redaction below is skipped here because this arm is the operator's own
    // token: the command line being redacted is the one they typed.
    if lane.is_operator() {
        let mut rows = crate::library::all_games_for_operator();
        rows.retain(|r| matches_query(&r.entry, &q));
        for r in &mut rows {
            crate::library::proxy_local_art(&r.entry.id, &mut r.entry.art);
        }
        return Json(rows).into_response();
    }
    let mut games = crate::library::all_games();
    games.retain(|g| matches_query(g, &q));
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
    //
    // Unconditional now: the operator's lane returned above, so reaching here IS "some lane but
    // theirs". Leaving the old `if !lane.may_set_privileged_fields()` would read as though an
    // unredacted path still existed here, and would quietly stop redacting if that early return
    // ever moved.
    for g in &mut games {
        if let Some(l) = g.launch.as_mut() {
            if l.kind == "command" {
                l.value.clear();
            }
        }
    }
    Json(games).into_response()
}

/// The `?provider=` / `?platform=` narrowing, shared by both lane arms so they cannot drift.
fn matches_query(g: &crate::library::GameEntry, q: &LibraryQuery) -> bool {
    if let Some(provider) = q.provider.as_deref().filter(|p| !p.is_empty()) {
        if g.provider.as_deref() != Some(provider) {
            return false;
        }
    }
    if let Some(platform) = q.platform.as_deref().filter(|p| !p.is_empty()) {
        if !g
            .meta
            .platform
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(platform))
        {
            return false;
        }
    }
    true
}

/// Request body for `setLibraryEntryHidden`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct HiddenToggle {
    /// Whether this title should be hidden from every play surface.
    hidden: bool,
}

/// What `setLibraryEntryHidden` echoes back.
#[derive(Serialize, ToSchema)]
pub(crate) struct HiddenState {
    /// The entry id the call addressed.
    id: String,
    /// Its visibility after the call.
    hidden: bool,
}

/// Hide or un-hide one library title
///
/// Curation, not access control: a hidden title disappears from every play surface — the console
/// grid on a client, native clients, the GameStream app list, and launch resolution — while nothing
/// is deleted and un-hiding restores it immediately. The operator's own console still lists it
/// (flagged `hidden`) so it can be brought back.
///
/// Keyed by the entry's stable `<store>:<external_id>` id, which survives re-scans and reconciles by
/// construction (D2). The id is **not** validated against the current library on purpose: a title
/// can be legitimately absent at this moment (launcher closed, plugin mid-sync, drive unmounted),
/// and refusing the operator's choice in that window would be worse than storing an id that
/// currently matches nothing. Emits `library.changed` (source = the store) only on a real change.
#[utoipa::path(
    put,
    path = "/library/hidden/{id}",
    tag = "library",
    operation_id = "setLibraryEntryHidden",
    params(("id" = String, Path, description = "The library entry id (e.g. `steam:70`)")),
    request_body = HiddenToggle,
    responses(
        (status = OK, description = "Stored; the entry's visibility after the call", body = HiddenState),
        (status = BAD_REQUEST, description = "Empty entry id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Could not persist the settings", body = ApiError),
    )
)]
pub(crate) async fn set_library_entry_hidden(
    Path(id): Path<String>,
    ApiJson(toggle): ApiJson<HiddenToggle>,
) -> Response {
    if id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "entry id must not be empty");
    }
    match crate::library::set_entry_hidden(&id, toggle.hidden) {
        Ok(hidden) => {
            tracing::info!(entry = %id, hidden, "management API: library entry visibility set");
            Json(HiddenState { id, hidden }).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Request body for `setLibraryScanner`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ScannerToggle {
    /// Whether this source should contribute titles on this host.
    enabled: bool,
}

/// List the library sources
///
/// Every game source on this host with its enable state — one row per installed library plugin
/// (Steam, Lutris, Heroic, Epic, GOG, Xbox, Playnite, ROM managers, …), so the list reflects what
/// the operator has actually installed rather than what this build happens to support. Sources
/// default to enabled; disabling one hides its titles from every library surface from the next
/// read. The user-curated custom store is not a source and is always on.
///
/// Older hosts (≤ v0.27.x) also listed the six scanners built into the host binary, with
/// `origin: "builtin"`. Those are gone; every row now reports `origin: "plugin"`.
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

/// Enable or disable a library source
///
/// Persists the toggle and applies it from the next library read (no restart). Disabling a source
/// hides its titles everywhere — the console grid, native clients, and the GameStream app list —
/// and re-enabling brings them straight back. Nothing is deleted: the plugin may keep reconciling
/// while its source is off, and those entries simply aren't surfaced. Emits `library.changed` with
/// the source id as `source` when the state changed.
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
    if let Some((_, denied)) = check_entry_fields(
        lane,
        &input.art,
        input.launch.as_ref(),
        &input.prep,
        input.icon.as_deref(),
    ) {
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
    if let Some((_, denied)) = check_entry_fields(
        lane,
        &input.art,
        input.launch.as_ref(),
        &input.prep,
        input.icon.as_deref(),
    ) {
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
/// `custom:<id>` ones — which is what let a library plugin reproduce the entries the in-host scanner
/// used to produce, right down to the GameStream app ids and client-side art caches, and is why
/// removing those scanners changed nothing downstream. One provider per store; a second claimant
/// gets 409. The claim is released by `DELETE`, not by an empty reconcile (a store can legitimately
/// have zero installed titles).
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
    Extension(lane): Extension<AuthLane>,
    Path(provider): Path<String>,
    Query(q): Query<ReconcileQuery>,
    ApiJson(mut inputs): ApiJson<Vec<crate::library::ProviderEntryInput>>,
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
    // Every entry in the payload, not just the first — a reconcile replaces a whole entry set, so
    // one privileged field anywhere in it is one command execution.
    //
    // Art is deliberately NOT part of this refusal. A privileged field is the plugin overreaching
    // and must fail the write; an unservable cover is a path mismatch between where a launcher keeps
    // its art and where the host is allowed to read, and failing the payload over one of those threw
    // away a working library to save a thumbnail. Those covers are stripped below instead, which
    // holds the same "no unservable path is ever persisted" invariant.
    for (i, e) in inputs.iter().enumerate() {
        if let Some((reason, denied)) =
            check_privileged_fields(lane, e.launch.as_ref(), &e.prep, e.icon.as_deref())
        {
            tracing::warn!(
                provider,
                index = i,
                title = %e.title,
                reason = %reason,
                "library reconcile refused"
            );
            return denied;
        }
    }
    // One aggregated line, not one per entry: a root mismatch misses EVERY cover in the payload, and
    // a per-entry warn would bury the rest of the log under a thousand copies of one fact.
    let mut dropped_art = 0usize;
    let mut first_dropped: Option<(String, &'static str, String)> = None;
    for e in inputs.iter_mut() {
        for (field, value) in crate::library::sanitize_art_paths(&mut e.art) {
            dropped_art += 1;
            first_dropped.get_or_insert_with(|| (e.title.clone(), field, value));
        }
    }
    if let Some((title, field, path)) = first_dropped {
        tracing::warn!(
            provider,
            dropped = dropped_art,
            example_title = %title,
            example_field = field,
            example_path = %path,
            "library reconcile: dropped local art the proxy may not serve — these entries still \
             sync, but their covers will be blank. The path must be an image file (jpg/png/webp/\
             gif/bmp/ico/tga) inside an allowed art root; set PUNKTFUNK_LIBRARY_ART_ROOTS if this \
             library's art lives outside the defaults"
        );
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
/// and a library plugin's claimed-store entries) serves its local art file; anything else 404s and
/// the client falls through to its next art candidate.
///
/// The host fetches nothing here. Art a plugin published as an `http(s)` URL is fetched by the
/// client directly — this proxy exists for the *local* files a plugin finds on the host's own disk
/// (a launcher's cover cache), which a client has no way to read.
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
    // `library.json`, for ANY id (WP1.2): manual entries, provider-synced entries and a library
    // plugin's claimed-store `steam:570` all serve their local art file from here, so the proxy never
    // has to know which store an id belongs to. This was one of two branches — the second resolved a
    // `steam:` id through the in-host Steam scanner's own cache/CDN ladder, and was retired with that
    // scanner (M6). Steam ids now arrive here like every other claimed store's.
    let stored = {
        let id = id.clone();
        tokio::task::spawn_blocking(move || crate::library::library_local_art_bytes(&id, kind))
            .await
    };
    if let Ok(Some((bytes, ctype))) = stored {
        return ([(header::CONTENT_TYPE, ctype)], bytes).into_response();
    }
    api_error(StatusCode::NOT_FOUND, "no art of that kind for this title")
}
