//! In-memory, lease-based directory of running plugins and their loopback UI surfaces.
//!
//! Plugins register a port, per-boot secret, title, and icon. The console obtains credentials
//! server-side and proxies only to `127.0.0.1`; registrations are never persisted or health-
//! checked and expire lazily after [`LEASE_TTL`]. Mutation routes require bearer authentication
//! from loopback.
//!
//! The shared `plugin-token` authenticates the runner, not an individual plugin. Any holder can
//! replace an id's registration, and [`crate::library::ask_plugin_launch`] treats that registration
//! as launch authority. Per-plugin ownership therefore requires runner process isolation rather
//! than an additional registry check.

use super::shared::*;
use crate::events::{emit, EventKind};
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

/// How long a registration stays live after its last renewal. The SDK helper renews every 30 s, so
/// this tolerates two missed ticks before a plugin drops out of the listing.
const LEASE_TTL: Duration = Duration::from_secs(90);

/// Lines accepted per `POST /plugins/logs`. The runner batches on a short timer, so a batch this
/// size means a plugin is logging faster than the ring can usefully hold — the shipper drops its
/// own backlog (and says so in a line of its own) rather than letting one chatty plugin evict the
/// whole ring in a single request.
const MAX_LOG_BATCH: usize = 256;

// ---------------------------------------------------------------- wire shapes

/// A plugin's UI surface as it registers it. Carries the secret — this shape is only ever a request
/// body, never a response ([`PluginUiPublic`] is the secret-free view).
#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginUi {
    /// The **loopback** port the plugin serves its UI on. The host and console only ever dial
    /// `127.0.0.1:<port>`; a registration can never carry a hostname.
    pub port: u16,
    /// Per-boot shared secret the console proxy must present (as `Authorization: Bearer`) on every
    /// request to the plugin's UI server. Rotated whenever the plugin restarts.
    pub secret: String,
    /// Optional lucide icon name for the console nav entry (`^[a-z0-9-]{1,48}$`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// Register/renew body for `PUT /plugins/{id}`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginRegistration {
    /// Human-readable title for the console nav entry (1–64 chars; control chars stripped).
    pub title: String,
    /// Optional plugin version, purely informational (≤32 chars).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Present iff the plugin serves a UI surface. A registration with no `ui` is a liveness/phone-book
    /// entry only (e.g. a future runner-management listing) and grows no nav entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<PluginUi>,
    /// What KIND of plugin this is (`^[a-z][a-z0-9-]{0,31}$`), top-level rather than under `ui`
    /// because it describes the plugin, not its surface. The console knows one value today —
    /// `library` — which it filters **out of the nav**: six installed scanner plugins would otherwise
    /// flood the sidebar, and their real entry point is the Game sources surface (design D5). A
    /// library plugin that genuinely wants its own page (rom-manager, which is much more than a
    /// scanner) simply omits the category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// One log line produced by the runner or a plugin inside it (`POST /plugins/logs`).
#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginLogLine {
    /// When the line was produced, unix milliseconds. Kept verbatim — see
    /// [`crate::log_capture::LogRing::push_remote`].
    pub ts_ms: u64,
    /// `ERROR` | `WARN` | `INFO` | `DEBUG` | `TRACE`. Anything else is coerced to `INFO`.
    pub level: String,
    /// Which unit emitted it — a plugin's `definePlugin` name, a package name, or `runner`.
    /// Surfaced in the console's target column as `plugin:<source>`.
    pub source: String,
    pub msg: String,
}

/// A batch of runner log lines.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginLogBatch {
    pub entries: Vec<PluginLogLine>,
}

/// The secret-free view of a plugin's UI surface — what [`list_plugins`] returns to the browser.
#[derive(Serialize, ToSchema)]
pub(crate) struct PluginUiPublic {
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

/// One entry in `GET /plugins`. **Never carries the secret** — the browser learns a plugin exists
/// and has a UI, nothing that lets it reach the plugin directly (it goes through the console proxy).
#[derive(Serialize, ToSchema)]
pub(crate) struct PluginSummary {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui: Option<PluginUiPublic>,
    /// The plugin's kind — see [`PluginRegistration::category`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// `GET /plugins/{id}/ui-credential` — the console proxy's server-side lookup (bearer + loopback).
/// This is the only endpoint that returns a secret; the console BFF denylists it from the browser.
#[derive(Serialize, ToSchema)]
pub(crate) struct UiCredential {
    pub port: u16,
    pub secret: String,
}

// ---------------------------------------------------------------- registry core

/// The stored UI surface (internal — parsed + validated, no wire derives).
#[derive(Clone, PartialEq)]
struct StoredUi {
    port: u16,
    secret: String,
    icon: Option<String>,
}

/// One live registration. `expires_at` is a **monotonic** [`Instant`] (immune to wall-clock jumps).
struct Stored {
    title: String,
    version: Option<String>,
    ui: Option<StoredUi>,
    category: Option<String>,
    expires_at: Instant,
}

impl Stored {
    /// Do the operator-visible fields match (ignoring the lease clock)? A pure lease renewal leaves
    /// these unchanged and emits no event; a restart (new secret) or a re-scan (new title/icon/
    /// category) does.
    fn public_eq(&self, v: &Valid) -> bool {
        self.title == v.title
            && self.version == v.version
            && self.ui == v.ui
            && self.category == v.category
    }
}

/// The process-wide plugin registry.
pub(crate) struct PluginRegistry {
    inner: RwLock<HashMap<String, Stored>>,
}

/// A validated registration ready to store (title trimmed, ui fields checked).
struct Valid {
    title: String,
    version: Option<String>,
    ui: Option<StoredUi>,
    category: Option<String>,
}

impl PluginRegistry {
    fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Insert or renew `id`. Returns `true` when an operator-visible field changed (new plugin,
    /// restart, or re-scan) — the signal the caller emits `plugins.changed` on. A pure renewal
    /// returns `false`.
    fn upsert(&self, id: &str, v: Valid) -> bool {
        let expires_at = Instant::now() + LEASE_TTL;
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let changed = match map.get(id) {
            // An *expired* prior entry counts as a change (it had stopped listing).
            Some(prev) => !prev.is_live() || !prev.public_eq(&v),
            None => true,
        };
        map.insert(
            id.to_string(),
            Stored {
                title: v.title,
                version: v.version,
                ui: v.ui,
                category: v.category,
                expires_at,
            },
        );
        changed
    }

    /// The current listing (sorted by title, then id), plus the ids of entries that had expired and
    /// were pruned by this call — the caller emits `plugins.changed` for those. Prunes under the
    /// write lock so a stale entry is reaped exactly once.
    fn snapshot(&self) -> (Vec<PluginSummary>, Vec<String>) {
        let now = Instant::now();
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let mut expired: Vec<String> = map
            .iter()
            .filter(|(_, s)| now >= s.expires_at)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            map.remove(id);
        }
        expired.sort();
        let mut live: Vec<PluginSummary> = map
            .iter()
            .map(|(id, s)| PluginSummary {
                id: id.clone(),
                title: s.title.clone(),
                version: s.version.clone(),
                ui: s.ui.as_ref().map(|u| PluginUiPublic {
                    port: u.port,
                    icon: u.icon.clone(),
                }),
                category: s.category.clone(),
            })
            .collect();
        live.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        (live, expired)
    }

    /// The `{port, secret}` for a live plugin's UI, or `None` if unknown/expired/UI-less. Does not
    /// prune (a read path) — a stale entry is reaped by the next [`snapshot`](Self::snapshot).
    fn credential(&self, id: &str) -> Option<UiCredential> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let s = map.get(id)?;
        if !s.is_live() {
            return None;
        }
        let ui = s.ui.as_ref()?;
        Some(UiCredential {
            port: ui.port,
            secret: ui.secret.clone(),
        })
    }

    /// The ids of live registrations, without pruning or announcing anything.
    fn live_ids(&self) -> Vec<String> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|(_, s)| s.is_live())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Remove `id`. Returns `true` if a **live** entry existed (a clean deregister); removing an
    /// already-expired or unknown id returns `false` (nothing to announce).
    fn remove(&self, id: &str) -> bool {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.remove(id).is_some_and(|s| s.is_live())
    }
}

impl Stored {
    fn is_live(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

/// The process-wide registry singleton (the [`crate::events::bus`] shape).
pub(crate) fn registry() -> &'static PluginRegistry {
    static REG: OnceLock<PluginRegistry> = OnceLock::new();
    REG.get_or_init(PluginRegistry::new)
}

/// Which plugin ids are registered right now — the plugin store's "running" column
/// ([`super::store::list_installed`]).
///
/// Deliberately **not** [`list_plugins`]: that one prunes expired leases and emits
/// `plugins.changed` for each, which is right for the nav's poll but would turn the store's own
/// polling into an event fire-hose. This is a pure read.
pub(crate) fn live_plugin_ids() -> Vec<String> {
    registry().live_ids()
}

/// The loopback `{port, secret}` a live plugin serves its UI on — the credential the **host itself**
/// presents when it asks a library plugin what to run for one of its `plugin`-kind launch entries
/// ([`crate::library::ask_plugin_launch`]).
///
/// The same lookup the console proxy gets from `GET /plugins/{id}/ui-credential`, exposed in-process
/// so the launch path never round-trips through the management API to reach a port this process
/// already holds. `None` for an unknown, expired, or UI-less plugin — which the launch path reports
/// as "no recipe", exactly like any other unresolvable entry.
pub(crate) fn ui_credential(id: &str) -> Option<UiCredential> {
    registry().credential(id)
}

/// Put a live UI registration in the registry directly — **test only**, so the launch path
/// ([`crate::library::ask_plugin_launch`]) can be driven against a stub server without standing up
/// the whole management router just to reach `PUT /plugins/{id}`.
#[cfg(test)]
pub(crate) fn register_ui_for_test(id: &str, port: u16, secret: &str) {
    registry().upsert(
        id,
        Valid {
            title: id.to_string(),
            version: None,
            ui: Some(StoredUi {
                port,
                secret: secret.to_string(),
                icon: None,
            }),
            category: None,
        },
    );
}

// ---------------------------------------------------------------- validation

/// A plugin id: `definePlugin`'s kebab-case name (`^[a-z][a-z0-9-]*$`, ≤64) — the same regex the SDK
/// enforces, so a plugin's registration id always matches its package name.
fn valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Strip control characters (defense against a title/version smuggling terminal escapes or newlines
/// into a log line or the console nav), then trim.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// The console target for a runner-supplied line: `plugin:<source>`.
///
/// The source is NOT a [`valid_plugin_id`] — the runner names a unit by its `definePlugin` name
/// (`virtualhere`), its package name (`@punktfunk/plugin-virtualhere`), a bare script's file stem,
/// or `runner` for its own supervision lines, and all four are worth telling apart in the log. So
/// this sanitizes rather than validates: control characters go (a log target is rendered in a
/// terminal by `logs download` as readily as in the console), length is capped, and an empty source
/// becomes `runner` so a line is never attributed to nothing.
fn log_target(source: &str) -> String {
    let mut s = sanitize(source);
    if s.is_empty() {
        s = "runner".into();
    }
    // Cap on CHARS, not bytes — truncating a multi-byte name mid-sequence would panic.
    if s.chars().count() > 64 {
        s = s.chars().take(64).collect();
    }
    format!("plugin:{s}")
}

/// Validate a registration body into the internal [`Valid`] form, or a human-readable reason.
fn validate(reg: PluginRegistration) -> Result<Valid, String> {
    let title = sanitize(&reg.title);
    if title.is_empty() {
        return Err("title must not be empty".into());
    }
    if title.chars().count() > 64 {
        return Err("title must be at most 64 characters".into());
    }
    let version = match reg.version {
        Some(v) => {
            let v = sanitize(&v);
            if v.chars().count() > 32 {
                return Err("version must be at most 32 characters".into());
            }
            (!v.is_empty()).then_some(v)
        }
        None => None,
    };
    let ui = match reg.ui {
        Some(u) => Some(validate_ui(u)?),
        None => None,
    };
    // Categories are grouping keys the console switches on — a closed charset, but deliberately not
    // a closed VOCABULARY: an unknown category is stored and simply matches no console rule, so a
    // newer plugin registering against an older host degrades to "shows in the nav", never to a
    // failed registration.
    let category = match reg.category {
        Some(c) => {
            let ok = (1..=32).contains(&c.len())
                && c.starts_with(|ch: char| ch.is_ascii_lowercase())
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(
                    "category must be 1–32 chars of [a-z0-9-], starting with a letter".into(),
                );
            }
            Some(c)
        }
        None => None,
    };
    Ok(Valid {
        title,
        version,
        ui,
        category,
    })
}

fn validate_ui(u: PluginUi) -> Result<StoredUi, String> {
    if u.port < 1024 {
        return Err("ui.port must be a non-privileged port (>= 1024)".into());
    }
    let n = u.secret.len();
    if !(16..=128).contains(&n) {
        return Err("ui.secret must be 16–128 characters".into());
    }
    if !u
        .secret
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("ui.secret must be [A-Za-z0-9_-]".into());
    }
    let icon = match u.icon {
        Some(icon) => {
            let ok = (1..=48).contains(&icon.len())
                && icon
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err("ui.icon must be a lucide name ([a-z0-9-], 1–48 chars)".into());
            }
            Some(icon)
        }
        None => None,
    };
    Ok(StoredUi {
        port: u.port,
        secret: u.secret,
        icon,
    })
}

// ---------------------------------------------------------------- handlers

/// Register or renew a plugin
///
/// Upserts the plugin's directory entry and renews its lease (TTL 90 s). Idempotent: a plugin PUTs
/// this every ~30 s while it runs. The optional `ui` block declares a loopback UI surface the console
/// will proxy and add to its nav. Emits `plugins.changed` when an operator-visible field changed
/// (first registration, restart, or re-scan) — a pure renewal is silent.
#[utoipa::path(
    put,
    path = "/plugins/{id}",
    tag = "plugins",
    operation_id = "registerPlugin",
    params(("id" = String, Path, description = "The plugin id (its `definePlugin` name: `[a-z][a-z0-9-]*`)")),
    request_body = PluginRegistration,
    responses(
        (status = NO_CONTENT, description = "Registered / renewed"),
        (status = BAD_REQUEST, description = "Invalid id or registration", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn register_plugin(
    Path(id): Path<String>,
    ApiJson(reg): ApiJson<PluginRegistration>,
) -> Response {
    if !valid_plugin_id(&id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid plugin id (expected kebab-case `[a-z][a-z0-9-]*`, ≤64)",
        );
    }
    let valid = match validate(reg) {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &e),
    };
    if registry().upsert(&id, valid) {
        tracing::info!(plugin = %id, "plugin registered");
        emit(EventKind::PluginsChanged { id });
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Ingest runner log lines
///
/// The plugin/script runner ships its output here so the console's **Logs** page can show it.
///
/// Plugins are not host child processes — the runner is a separate `bun` process that `import()`s
/// each plugin in-process — so nothing a plugin logs passes through the host's own `tracing`, and
/// before this endpoint the console's log page could not show a single plugin line. On Linux the
/// fallback was `journalctl --user -u punktfunk-scripting`; on Windows the runner task writes no
/// log file at all, so a failing plugin was diagnosable only by stopping the scheduled task and
/// re-running the runner by hand. Both are shell access on the host box, which is exactly what the
/// console exists to avoid.
///
/// Lines land in the same ring as the host's own, sharing one `seq` cursor, targeted
/// `plugin:<source>` — so `GET /logs` needs no second cursor and the console needs no second poll.
#[utoipa::path(
    post,
    path = "/plugins/logs",
    tag = "plugins",
    operation_id = "ingestPluginLogs",
    request_body = PluginLogBatch,
    responses(
        (status = NO_CONTENT, description = "Lines ingested"),
        (status = BAD_REQUEST, description = "Batch too large", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn ingest_plugin_logs(ApiJson(batch): ApiJson<PluginLogBatch>) -> Response {
    if batch.entries.len() > MAX_LOG_BATCH {
        return api_error(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_LOG_BATCH} entries per batch"),
        );
    }
    for line in batch.entries {
        crate::log_capture::ring().push_remote(
            &line.level,
            &log_target(&line.source),
            &sanitize_msg(&line.msg),
            line.ts_ms,
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Strip control characters from an ingested message, keeping tabs.
///
/// Same reasoning as [`sanitize`], one exception wider: a plugin's messages routinely carry a
/// stack trace or a vendor CLI's output, and an embedded newline would let one line forge several
/// in the downloaded log file. Tabs survive because they are load-bearing in that kind of output.
fn sanitize_msg(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\t' || !c.is_control() { c } else { ' ' })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// List registered plugins
///
/// The live plugin directory (lease not expired), sorted by title. **Secret-free**: each entry
/// reports its id, title, optional version, and — for plugins that serve one — a UI descriptor
/// (loopback port + icon). The console renders these as nav entries and proxies to the port; it
/// fetches the secret separately, server-side.
#[utoipa::path(
    get,
    path = "/plugins",
    tag = "plugins",
    operation_id = "listPlugins",
    responses(
        (status = OK, description = "Live plugin registrations", body = [PluginSummary]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_plugins() -> Json<Vec<PluginSummary>> {
    let (plugins, expired) = registry().snapshot();
    // Lazy expiry: an entry that aged out is reaped here (exactly once) and announced, so a consumer
    // watching the event stream sees the departure even though nothing actively deregistered it.
    for id in expired {
        tracing::info!(plugin = %id, "plugin lease expired");
        emit(EventKind::PluginsChanged { id });
    }
    Json(plugins)
}

/// Fetch a plugin UI's proxy credential
///
/// Returns `{port, secret}` for a live plugin's loopback UI — the console proxy's server-side lookup.
/// Bearer + loopback only (like every mutation), and additionally excluded from the console's browser
/// passthrough: the secret never reaches a browser.
#[utoipa::path(
    get,
    path = "/plugins/{id}/ui-credential",
    tag = "plugins",
    operation_id = "getPluginUiCredential",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = OK, description = "The proxy credential", body = UiCredential),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No live plugin with that id, or it serves no UI", body = ApiError),
    )
)]
pub(crate) async fn get_ui_credential(Path(id): Path<String>) -> Response {
    match registry().credential(&id) {
        Some(cred) => Json(cred).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no live plugin UI with that id"),
    }
}

/// Deregister a plugin
///
/// The clean-shutdown path: removes the plugin's directory entry immediately (the SDK helper calls
/// this from its scope finalizer on `SIGTERM`). Emits `plugins.changed` when a live entry was
/// removed. Idempotent — deleting an unknown/expired id is a no-op `204`.
#[utoipa::path(
    delete,
    path = "/plugins/{id}",
    tag = "plugins",
    operation_id = "deregisterPlugin",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = NO_CONTENT, description = "Deregistered (or already absent)"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn delete_plugin(Path(id): Path<String>) -> Response {
    if registry().remove(&id) {
        tracing::info!(plugin = %id, "plugin deregistered");
        emit(EventKind::PluginsChanged { id });
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(title: &str, port: u16, secret: &str) -> PluginRegistration {
        PluginRegistration {
            title: title.into(),
            version: None,
            ui: Some(PluginUi {
                port,
                secret: secret.into(),
                icon: Some("gamepad-2".into()),
            }),
            category: None,
        }
    }

    const SECRET: &str = "abcdefghijklmnop0123"; // 20 chars, valid alphabet

    #[test]
    fn id_validation() {
        assert!(valid_plugin_id("rom-manager"));
        assert!(valid_plugin_id("a"));
        assert!(valid_plugin_id("x9"));
        assert!(!valid_plugin_id("")); // empty
        assert!(!valid_plugin_id("9lives")); // must start with a letter
        assert!(!valid_plugin_id("-lead")); // must start with a letter
        assert!(!valid_plugin_id("Rom")); // no uppercase
        assert!(!valid_plugin_id("rom_manager")); // no underscore
        assert!(!valid_plugin_id(&"a".repeat(65))); // too long
    }

    #[test]
    fn registration_validation() {
        assert!(validate(reg("ROM Manager", 49321, SECRET)).is_ok());
        // control chars stripped from the title
        let v = validate(PluginRegistration {
            title: "Ro\u{7}m\n".into(),
            version: None,
            ui: None,
            category: None,
        })
        .unwrap();
        assert_eq!(v.title, "Rom");
        // Category charset (WP2.7): the console's one known value passes; the shapes that would
        // break a grouping key don't. An UNKNOWN-but-well-formed category is accepted on purpose —
        // a newer plugin must not fail to register against an older host.
        let lib = |c: &str| PluginRegistration {
            title: "X".into(),
            version: None,
            ui: None,
            category: Some(c.into()),
        };
        assert_eq!(
            validate(lib("library")).unwrap().category.as_deref(),
            Some("library")
        );
        assert!(validate(lib("some-future-kind")).is_ok());
        assert!(validate(lib("")).is_err());
        assert!(validate(lib("Library")).is_err()); // no uppercase
        assert!(validate(lib("9lives")).is_err()); // must start with a letter
        assert!(validate(lib("lib_rary")).is_err()); // no underscore
        assert!(validate(lib(&"a".repeat(33))).is_err()); // too long
                                                          // privileged port rejected
        assert!(validate(reg("x", 80, SECRET)).is_err());
        // short secret rejected
        assert!(validate(reg("x", 49321, "tooshort")).is_err());
        // bad secret alphabet rejected
        assert!(validate(reg("x", 49321, "bad secret with spaces!!")).is_err());
        // empty title rejected
        assert!(validate(reg("   ", 49321, SECRET)).is_err());
    }

    #[test]
    fn upsert_reports_change_but_not_renewal() {
        let r = PluginRegistry::new();
        // first registration is a change
        assert!(r.upsert("p", validate(reg("Title", 49321, SECRET)).unwrap()));
        // identical renewal is not
        assert!(!r.upsert("p", validate(reg("Title", 49321, SECRET)).unwrap()));
        // a new secret (restart) is a change
        assert!(r.upsert(
            "p",
            validate(reg("Title", 49321, "ZZZZZZZZZZZZZZZZ")).unwrap()
        ));
        // a new title (re-scan) is a change
        assert!(r.upsert(
            "p",
            validate(reg("New Title", 49321, "ZZZZZZZZZZZZZZZZ")).unwrap()
        ));
    }

    #[test]
    fn snapshot_lists_secret_free_sorted() {
        let r = PluginRegistry::new();
        r.upsert("zeta", validate(reg("Zeta", 50000, SECRET)).unwrap());
        r.upsert("alpha", validate(reg("Alpha", 50001, SECRET)).unwrap());
        let (plugins, expired) = r.snapshot();
        assert!(expired.is_empty());
        assert_eq!(
            plugins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"] // sorted by title
        );
        // the summary type has no secret field at all — check the credential path carries it
        assert_eq!(r.credential("alpha").unwrap().secret, SECRET);
        assert_eq!(r.credential("alpha").unwrap().port, 50001);
    }

    #[test]
    fn credential_absent_for_ui_less_and_unknown() {
        let r = PluginRegistry::new();
        r.upsert(
            "headless",
            validate(PluginRegistration {
                title: "Headless".into(),
                version: None,
                ui: None,
                category: None,
            })
            .unwrap(),
        );
        assert!(r.credential("headless").is_none()); // registered but no UI
        assert!(r.credential("nope").is_none()); // unknown
    }

    #[test]
    fn expired_entries_drop_from_listing_and_credential() {
        let r = PluginRegistry::new();
        r.upsert("p", validate(reg("P", 49321, SECRET)).unwrap());
        // Force the lease into the past.
        {
            let mut map = r.inner.write().unwrap();
            map.get_mut("p").unwrap().expires_at =
                Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        }
        assert!(r.credential("p").is_none()); // expired → no credential
        let (plugins, expired) = r.snapshot();
        assert!(plugins.is_empty());
        assert_eq!(expired, vec!["p".to_string()]); // reaped + announced once
                                                    // second snapshot no longer reports it as freshly-expired
        assert!(r.snapshot().1.is_empty());
    }

    #[test]
    fn remove_reports_live_only() {
        let r = PluginRegistry::new();
        r.upsert("p", validate(reg("P", 49321, SECRET)).unwrap());
        assert!(r.remove("p")); // live → announced
        assert!(!r.remove("p")); // already gone → silent
    }
}
