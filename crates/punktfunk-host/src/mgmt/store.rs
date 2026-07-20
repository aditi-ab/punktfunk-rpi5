//! Plugin **store** API: browse catalogs, install/uninstall, manage sources and the runner
//! (design `plugin-store.md` §4.2). The domain logic lives in [`crate::store`]; this is its HTTP
//! surface.
//!
//! Auth: **admin bearer + loopback**, like every mutation — and explicitly *denied to the plugin
//! token* ([`super::auth::plugin_may_access`]). A plugin that can install plugins is a persistence
//! and escalation primitive: it could install a helper that isn't constrained the way it is. That
//! deny is a carve-out in the exclusion-based allowlist, so it is spelled out there and pinned by a
//! test.
//!
//! (A console *session* can already reach arbitrary command execution via `PUT /hooks`, so the
//! store adds convenience for a session holder rather than a new privilege class. It is still worth
//! keeping the plugin lane away from it — the lanes exist precisely so a plugin defect isn't an
//! operator compromise.)
//!
//! Everything here does blocking work — filesystem scans, network fetches, process spawns — so
//! handlers hop to `spawn_blocking` rather than stalling the runtime.

use super::shared::*;
use crate::store::{self, index, jobs, manifest, sources};

/// Run blocking store work off the async runtime, mapping a joined-thread panic to a 500.
async fn blocking<T, F>(f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!("plugin-store worker panicked: {e}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the plugin store worker failed",
        )
    })
}

// ---------------------------------------------------------------- wire shapes

/// Facts about this host, so the console can grey out rows it can't install.
#[derive(Serialize, ToSchema)]
pub(crate) struct HostFacts {
    pub version: String,
    /// `linux` / `windows` / `macos`.
    pub platform: String,
}

/// A configured catalog source and how its last refresh went.
#[derive(Serialize, ToSchema)]
pub(crate) struct SourceView {
    pub name: String,
    pub url: String,
    /// The built-in `unom` source: not editable, not removable, and the only source whose entries
    /// may carry the "verified" tier.
    pub builtin: bool,
    /// Whether we check a signature on this source's index. An unsigned source still works; the
    /// console marks it.
    pub signed: bool,
    /// The catalog we're serving is older than the last refresh attempt (offline, or the last
    /// fetch failed) — entries still install, because the pin travelled with the entry.
    pub stale: bool,
    /// Unix seconds of the data we hold, when we hold any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    /// Why the last refresh failed, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub entry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

/// One row on the shelf.
#[derive(Serialize, ToSchema)]
pub(crate) struct CatalogEntry {
    pub id: String,
    pub pkg: String,
    pub title: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The one installable version this entry pins.
    pub version: String,
    /// Which source listed it.
    pub source: String,
    /// `verified` (built-in source) or `external` (an operator-added source). Never `unverified`:
    /// unverified installs come from a raw spec and are never listed (D7).
    pub tier: String,
    /// When unom reviewed this exact tarball (built-in source only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<String>,
    pub platforms: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_host: Option<String>,
    /// Can this host install it?
    pub compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incompatible_reason: Option<String>,
    /// The version installed right now, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// Installed, but at a different version than the catalog pins.
    pub update_available: bool,
    /// A revocation covering the catalogued version — do not offer this without shouting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CatalogResponse {
    pub host: HostFacts,
    pub sources: Vec<SourceView>,
    pub plugins: Vec<CatalogEntry>,
    /// True while a package operation is in flight — the console disables install buttons.
    pub busy: bool,
}

/// An installed plugin package, joined with its provenance and whether it's actually running.
#[derive(Serialize, ToSchema)]
pub(crate) struct InstalledView {
    pub pkg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `verified` / `external` / `unverified` / `cli` — remembered from install time, so an
    /// unverified plugin stays visibly unverified long after the dialog is forgotten.
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The catalog entry this maps to, when it is on a shelf we know.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// The plugin id it registers under — the key into `GET /plugins`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    /// Is it registered in the live lease registry right now?
    pub running: bool,
    /// The catalog's version, when it's newer than what's installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_available: Option<String>,
    /// A revocation covering the *installed* version. Reported, never auto-removed: silently
    /// deleting running code is its own hazard, so the operator decides.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// `POST /store/install` — either a catalogued entry, or a raw spec the operator owns.
#[derive(Deserialize, ToSchema)]
pub(crate) struct InstallRequest {
    /// Catalog source name (with [`Self::id`]).
    #[serde(default)]
    pub source: Option<String>,
    /// Catalog entry id (with [`Self::source`]).
    #[serde(default)]
    pub id: Option<String>,
    /// A raw package spec (`@scope/name`, `@scope/name@1.2.3`, an https tarball or git+https URL).
    /// Nothing reviewed it and nothing pins it.
    #[serde(default)]
    pub spec: Option<String>,
    /// Required with [`Self::spec`]: the operator's explicit acknowledgement that this installs
    /// unreviewed code with operator privileges. The console collects it behind a typed
    /// confirmation; the API refuses without it so no other caller can skip the decision.
    #[serde(default)]
    pub accept_unverified: bool,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UninstallRequest {
    pub pkg: String,
}

/// 202 body: where to watch the work.
#[derive(Serialize, ToSchema)]
pub(crate) struct JobRef {
    pub job: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SourceInput {
    pub url: String,
    /// `ed25519:<base64>`. Omitted ⇒ an unsigned source (accepted, flagged everywhere).
    #[serde(default)]
    pub public_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RuntimeView {
    /// Is the runner payload/unit present at all?
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    /// systemd unit or scheduled-task name.
    pub unit: String,
    /// Windows: the account the task runs as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RuntimeRequest {
    pub enabled: bool,
}

// ---------------------------------------------------------------- assembly

fn runtime_view() -> RuntimeView {
    let st = crate::plugins::runtime_status();
    RuntimeView {
        installed: st.installed,
        enabled: st.enabled,
        running: st.running,
        unit: st.unit.to_string(),
        principal: st.principal,
        detail: (!st.detail.is_empty()).then_some(st.detail),
    }
}

/// Build the merged catalog view: every source's shelf, annotated with what this host has and can
/// run. `force` refreshes past the TTL.
fn build_catalog(force: bool) -> CatalogResponse {
    let states = store::catalogs(force);
    let installed = store::installed_packages(&store::plugins_dir());
    let mut plugins = Vec::new();
    let mut source_views = Vec::new();

    for st in &states {
        let count = st.index.as_ref().map(|i| i.plugins.len()).unwrap_or(0);
        source_views.push(SourceView {
            name: st.source.name.clone(),
            url: st.source.url.clone(),
            builtin: st.source.is_official(),
            signed: st.source.is_signed(),
            stale: st.stale,
            fetched_at: st.fetched_at,
            error: st.error.clone(),
            entry_count: count as u32,
            public_key: st.source.public_key.clone(),
        });
        let Some(idx) = &st.index else { continue };
        let verified = st.source.is_official();
        for e in &idx.plugins {
            let installed_version = installed
                .iter()
                .find(|p| p.pkg == e.pkg)
                .and_then(|p| p.version.clone());
            let reason = e.incompatible_reason();
            plugins.push(CatalogEntry {
                id: e.id.clone(),
                pkg: e.pkg.clone(),
                title: e.title.clone(),
                description: e.description.clone(),
                icon: e.icon.clone(),
                author: e.author.clone(),
                homepage: e.homepage.clone(),
                license: e.license.clone(),
                version: e.version.clone(),
                source: st.source.name.clone(),
                // The badge is the built-in source's alone: a third-party curator can pin and
                // publish, but cannot confer unom's review (D6).
                tier: if verified { "verified" } else { "external" }.to_string(),
                reviewed_at: verified
                    .then(|| e.verification.as_ref().map(|v| v.reviewed_at.clone()))
                    .flatten(),
                platforms: e.platforms.clone(),
                min_host: e.min_host.clone(),
                compatible: reason.is_none(),
                incompatible_reason: reason,
                update_available: installed_version.as_deref().is_some_and(|v| v != e.version),
                installed_version,
                blocked: store::advisory_for(&e.pkg, Some(&e.version)).map(|a| a.reason),
            });
        }
    }
    // Stable, useful order: alphabetical by title, then by source so duplicates across shelves
    // don't jitter between polls.
    plugins.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.source.cmp(&b.source))
    });
    CatalogResponse {
        host: HostFacts {
            version: index::host_version().to_string(),
            platform: index::HOST_PLATFORM.to_string(),
        },
        sources: source_views,
        plugins,
        busy: jobs::busy(),
    }
}

fn build_installed(live: &[String]) -> Vec<InstalledView> {
    let dir = store::plugins_dir();
    let records = manifest::load(&dir);
    let catalogs = store::cached_catalogs();
    let mut out: Vec<InstalledView> = store::installed_packages(&dir)
        .into_iter()
        .map(|p| {
            let rec = records.get(&p.pkg);
            // The catalog row for this package, if any shelf carries it — the source of "an update
            // is available" and of a friendly title for CLI-installed packages.
            let entry = catalogs.iter().find_map(|s| {
                s.index
                    .as_ref()?
                    .plugins
                    .iter()
                    .find(|e| e.pkg == p.pkg)
                    .map(|e| (e, s.source.name.clone()))
            });
            let plugin_id = entry
                .as_ref()
                .map(|(e, _)| e.id.clone())
                .or_else(|| store::plugin_id_for_pkg(&p.pkg));
            InstalledView {
                // Absence of a manifest record is meaningful: the CLI put it there (§4.5).
                tier: rec
                    .map(|r| r.tier)
                    .unwrap_or(manifest::Tier::Cli)
                    .as_str()
                    .to_string(),
                source: rec
                    .and_then(|r| r.source.clone())
                    .or_else(|| entry.as_ref().map(|(_, s)| s.clone())),
                entry_id: rec
                    .and_then(|r| r.entry_id.clone())
                    .or_else(|| entry.as_ref().map(|(e, _)| e.id.clone())),
                title: entry.as_ref().map(|(e, _)| e.title.clone()),
                installed_at: rec.and_then(|r| r.installed_at.clone()),
                running: plugin_id
                    .as_deref()
                    .is_some_and(|id| live.iter().any(|l| l == id)),
                update_available: entry.as_ref().and_then(|(e, _)| {
                    (p.version.as_deref() != Some(e.version.as_str())).then(|| e.version.clone())
                }),
                blocked: store::advisory_for(&p.pkg, p.version.as_deref()).map(|a| a.reason),
                plugin_id,
                version: p.version,
                pkg: p.pkg,
            }
        })
        .collect();
    out.sort_by(|a, b| a.pkg.cmp(&b.pkg));
    out
}

// ---------------------------------------------------------------- handlers

/// Browse the plugin catalog
///
/// The merged shelf across every configured source, annotated with what this host already has and
/// what it can run. Sources past their freshness window are refreshed first; a source that can't be
/// reached keeps serving its last good copy, marked `stale` (a LAN-only host still has a working
/// store — an entry's pin travelled with the entry).
#[utoipa::path(
    get,
    path = "/store/catalog",
    tag = "store",
    operation_id = "getPluginCatalog",
    responses(
        (status = OK, description = "The merged catalog", body = CatalogResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn get_catalog() -> Response {
    match blocking(|| build_catalog(false)).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => e,
    }
}

/// Refresh every catalog now
///
/// Bypasses the freshness window and re-fetches all sources, then returns the merged catalog.
#[utoipa::path(
    post,
    path = "/store/refresh",
    tag = "store",
    operation_id = "refreshPluginCatalog",
    responses(
        (status = OK, description = "The freshly-fetched catalog", body = CatalogResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn refresh_catalog() -> Response {
    match blocking(|| build_catalog(true)).await {
        Ok(c) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            Json(c).into_response()
        }
        Err(e) => e,
    }
}

/// List installed plugins
///
/// What's actually in the plugins directory, joined with how it got there (the provenance manifest)
/// and whether it is registered right now. A package with no provenance record was installed with
/// the CLI and reports `tier: "cli"` — absence is the answer, not a gap.
#[utoipa::path(
    get,
    path = "/store/installed",
    tag = "store",
    operation_id = "listInstalledPlugins",
    responses(
        (status = OK, description = "Installed plugin packages", body = [InstalledView]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn list_installed() -> Response {
    // The live set comes from the in-memory lease registry — cheap, and it must be read on this
    // thread rather than inside the blocking closure to keep the borrow simple.
    let live: Vec<String> = super::plugins::live_plugin_ids();
    match blocking(move || build_installed(&live)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e,
    }
}

/// Install a plugin
///
/// Either `{source, id}` — a catalogued entry, installed at its pinned version after its integrity
/// is re-checked against the registry — or `{spec, accept_unverified: true}`, which installs an
/// unreviewed package the operator takes responsibility for. Returns `202` with a job id; watch it
/// at `GET /store/jobs/{id}`.
///
/// One package operation runs at a time (`409` otherwise): `bun` operations share a lockfile and a
/// `node_modules` tree.
#[utoipa::path(
    post,
    path = "/store/install",
    tag = "store",
    operation_id = "installPlugin",
    request_body = InstallRequest,
    responses(
        (status = ACCEPTED, description = "Install job started", body = JobRef),
        (status = BAD_REQUEST, description = "Unknown entry, bad spec, or missing acknowledgement", body = ApiError),
        (status = CONFLICT, description = "Another package operation is in flight", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn install_plugin(ApiJson(req): ApiJson<InstallRequest>) -> Response {
    let plan =
        match (
            req.source.as_deref(),
            req.id.as_deref(),
            req.spec.as_deref(),
        ) {
            (Some(source), Some(id), None) => {
                let found = match blocking({
                    let (source, id) = (source.to_string(), id.to_string());
                    move || store::find_entry(&source, &id)
                })
                .await
                {
                    Ok(f) => f,
                    Err(e) => return e,
                };
                let Some((entry, verified)) = found else {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "no such plugin in that source's catalog",
                    );
                };
                if let Some(reason) = entry.incompatible_reason() {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("this plugin cannot run on this host: {reason}"),
                    );
                }
                match jobs::Plan::from_entry(&entry, source, verified) {
                    Ok(p) => p,
                    Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
                }
            }
            (None, None, Some(spec)) => {
                if !req.accept_unverified {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "installing from a raw package spec runs unreviewed code with operator \
                     privileges — set `accept_unverified` to confirm",
                    );
                }
                match jobs::Plan::from_spec(spec) {
                    Ok(p) => p,
                    Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
                }
            }
            _ => return api_error(
                StatusCode::BAD_REQUEST,
                "provide either {source, id} for a catalogued plugin or {spec} for a raw package",
            ),
        };

    match jobs::spawn_install(plan) {
        Ok(job) => (StatusCode::ACCEPTED, Json(JobRef { job })).into_response(),
        Err(e) => api_error(StatusCode::CONFLICT, &format!("{e:#}")),
    }
}

/// Uninstall a plugin
///
/// Removes the package and forgets its provenance, then restarts the runner. Only names the runner
/// would actually supervise are accepted, so this can't be used to rip a shared dependency out of
/// the tree.
#[utoipa::path(
    post,
    path = "/store/uninstall",
    tag = "store",
    operation_id = "uninstallPlugin",
    request_body = UninstallRequest,
    responses(
        (status = ACCEPTED, description = "Uninstall job started", body = JobRef),
        (status = BAD_REQUEST, description = "Not a plugin package name", body = ApiError),
        (status = CONFLICT, description = "Another package operation is in flight", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn uninstall_plugin(ApiJson(req): ApiJson<UninstallRequest>) -> Response {
    if let Err(e) = store::valid_installed_pkg(&req.pkg) {
        return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}"));
    }
    // The name shape is not enough. `@punktfunk/plugin-kit` — the framework every kit-built plugin
    // *depends on* — matches `@scope/plugin-*` exactly, so a syntactic guard waves it through and
    // the store offers to uninstall a library out from under the plugins using it (accepted on
    // Windows on-glass before this check existed). Only a package the operator actually installed
    // may be removed, which is precisely what `installed_packages` reports: the plugins dir's
    // top-level dependencies, transitive ones excluded.
    let pkg = req.pkg.clone();
    let known = match blocking(move || {
        store::installed_packages(&store::plugins_dir())
            .iter()
            .any(|p| p.pkg == pkg)
    })
    .await
    {
        Ok(k) => k,
        Err(e) => return e,
    };
    if !known {
        return api_error(
            StatusCode::BAD_REQUEST,
            "that package is not an installed plugin — it may be a dependency of one, or already \
             removed",
        );
    }
    match jobs::spawn_uninstall(req.pkg) {
        Ok(job) => (StatusCode::ACCEPTED, Json(JobRef { job })).into_response(),
        Err(e) => api_error(StatusCode::CONFLICT, &format!("{e:#}")),
    }
}

/// List recent package jobs
#[utoipa::path(
    get,
    path = "/store/jobs",
    tag = "store",
    operation_id = "listPluginJobs",
    responses(
        (status = OK, description = "Recent install/uninstall jobs, oldest first", body = [Job]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn list_jobs() -> Json<Vec<jobs::Job>> {
    Json(jobs::list())
}

/// Follow one package job
///
/// Poll this while `state` is `running`; `log` carries the tail of the package manager's output.
#[utoipa::path(
    get,
    path = "/store/jobs/{id}",
    tag = "store",
    operation_id = "getPluginJob",
    params(("id" = String, Path, description = "The job id returned by install/uninstall")),
    responses(
        (status = OK, description = "The job", body = Job),
        (status = NOT_FOUND, description = "No such job (they are kept for a bounded history)", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn get_job(Path(id): Path<String>) -> Response {
    match jobs::get(&id) {
        Some(j) => Json(j).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no such job"),
    }
}

/// List catalog sources
#[utoipa::path(
    get,
    path = "/store/sources",
    tag = "store",
    operation_id = "listPluginSources",
    responses(
        (status = OK, description = "Configured sources, built-in first", body = [SourceView]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn list_sources() -> Response {
    match blocking(|| {
        store::cached_catalogs()
            .into_iter()
            .map(|st| SourceView {
                name: st.source.name.clone(),
                url: st.source.url.clone(),
                builtin: st.source.is_official(),
                signed: st.source.is_signed(),
                stale: st.stale,
                fetched_at: st.fetched_at,
                error: st.error,
                entry_count: st.index.map(|i| i.plugins.len()).unwrap_or(0) as u32,
                public_key: st.source.public_key,
            })
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => e,
    }
}

/// Add or update a catalog source
///
/// Adding a source is a trust decision: its entries become installable on this host. They are
/// attributed to it in the console and never carry the "verified" badge, which belongs to the
/// built-in source alone.
#[utoipa::path(
    put,
    path = "/store/sources/{name}",
    tag = "store",
    operation_id = "putPluginSource",
    params(("name" = String, Path, description = "Source slug (`[a-z][a-z0-9-]*`)")),
    request_body = SourceInput,
    responses(
        (status = NO_CONTENT, description = "Source saved"),
        (status = BAD_REQUEST, description = "Invalid name, url or key — or the reserved built-in name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn put_source(
    Path(name): Path<String>,
    ApiJson(input): ApiJson<SourceInput>,
) -> Response {
    let source = sources::Source {
        name: name.clone(),
        url: input.url,
        public_key: input.public_key.filter(|k| !k.trim().is_empty()),
    };
    let saved = blocking(move || {
        let r = sources::put(source);
        if r.is_ok() {
            // A redefined source must not keep serving what the old definition cached.
            store::drop_source_cache(&name);
        }
        r
    })
    .await;
    match saved {
        Err(e) => e,
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Ok(Ok(())) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Remove a catalog source
#[utoipa::path(
    delete,
    path = "/store/sources/{name}",
    tag = "store",
    operation_id = "deletePluginSource",
    params(("name" = String, Path, description = "Source slug")),
    responses(
        (status = NO_CONTENT, description = "Removed (or already absent)"),
        (status = FORBIDDEN, description = "The built-in source cannot be removed, or the plugin token is not authorized", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn delete_source(Path(name): Path<String>) -> Response {
    if name == sources::OFFICIAL_NAME {
        return api_error(
            StatusCode::FORBIDDEN,
            "the built-in source cannot be removed",
        );
    }
    let removed = blocking(move || {
        let r = sources::remove(&name);
        if matches!(r, Ok(true)) {
            store::drop_source_cache(&name);
        }
        r
    })
    .await;
    match removed {
        Err(e) => e,
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Ok(Ok(_)) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Plugin runner state
///
/// Installed plugins only run while the runner is on, and the runner discovers units at startup —
/// so this is both the "is anything running" answer and the explanation for a freshly installed
/// plugin that hasn't appeared yet.
#[utoipa::path(
    get,
    path = "/store/runtime",
    tag = "store",
    operation_id = "getPluginRuntime",
    responses(
        (status = OK, description = "Runner state", body = RuntimeView),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn get_runtime() -> Response {
    match blocking(runtime_view).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e,
    }
}

/// Turn the plugin runner on or off
#[utoipa::path(
    post,
    path = "/store/runtime",
    tag = "store",
    operation_id = "setPluginRuntime",
    request_body = RuntimeRequest,
    responses(
        (status = OK, description = "The resulting runner state", body = RuntimeView),
        (status = BAD_REQUEST, description = "The runner could not be switched", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn set_runtime(ApiJson(req): ApiJson<RuntimeRequest>) -> Response {
    let switched =
        blocking(move || crate::plugins::set_runtime_enabled(req.enabled).map(|()| runtime_view()))
            .await;
    match switched {
        Err(e) => e,
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Ok(Ok(v)) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            Json(v).into_response()
        }
    }
}

// Re-exported so `routes!` can name the response body types.
pub(crate) use crate::store::jobs::Job;
