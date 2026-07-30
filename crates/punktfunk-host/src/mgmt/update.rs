//! `/api/v1/update/*` — the host update-check surface (design
//! `host-update-from-web-console.md` §4.2, phase U0).
//!
//! Admin lane ONLY: denied to the plugin token (`auth::plugin_may_access`) and absent from
//! the paired-cert allowlist — an update trigger is operator business. U0 exposes `status` +
//! `check`; the `apply` route arrives with the first apply leg (U1) so the API never
//! advertises a capability no code backs.

use super::shared::*;
use crate::update::{self, detect};

/// One channel's manifest facts, as much as the console renders.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateManifestInfo {
    /// The released version this manifest announces.
    pub version: String,
    /// Publish serial (unix seconds) — monotonic per channel.
    pub serial: u64,
    /// RFC-3339 publish time (display only).
    pub published_at: String,
    /// Release-notes link (pinned to our forge by the manifest validator).
    pub notes_url: String,
    /// The last verified manifest is suspiciously old (>45 days) — the freeze/stale hint.
    pub stale: bool,
}

/// The full update-check state for this host.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateStatus {
    /// How this host was installed: `windows-installer` | `sysext` | `rpm-ostree` | `apt` |
    /// `dnf` | `pacman` | `steamos-source` | `nix` | `source`.
    pub install_kind: String,
    /// Release channel this install follows: `stable` | `canary`.
    pub channel: String,
    /// The running host version.
    pub current_version: String,
    /// What the console may offer for this install: `notify` (show the command) — later
    /// phases add `full` (one-click apply) and `staged` (apply + reboot to finish).
    pub apply: String,
    /// The copy-pastable update command for this install kind.
    pub channel_hint: String,
    /// Update checks are disabled on this host (`PUNKTFUNK_UPDATE_CHECK=0`).
    pub check_disabled: bool,
    /// A newer release than `current_version` exists for this channel (definitive
    /// comparisons only — an unparseable version pair never flags).
    pub available: bool,
    /// The last verified manifest, if any check has succeeded.
    pub manifest: Option<UpdateManifestInfo>,
    /// When the last successful check happened (unix seconds).
    pub last_checked_unix: Option<u64>,
    /// Why the last check failed, verbatim, if it did.
    pub last_error: Option<String>,
}

fn status_from(snap: update::Snapshot) -> UpdateStatus {
    let (kind, channel) = detect::detect();
    let current = env!("PUNKTFUNK_VERSION");
    let stale = snap.stale();
    let available = snap
        .checked
        .as_ref()
        .map(|c| detect::is_newer(&c.manifest.version, c.manifest.ci_run, current, channel))
        .unwrap_or(false);
    UpdateStatus {
        install_kind: kind.as_str().into(),
        channel: channel.as_str().into(),
        current_version: current.into(),
        apply: "notify".into(),
        channel_hint: detect::channel_hint(kind).into(),
        check_disabled: update::check_disabled(),
        available,
        manifest: snap.checked.as_ref().map(|c| UpdateManifestInfo {
            version: c.manifest.version.clone(),
            serial: c.manifest.serial,
            published_at: c.manifest.published_at.clone(),
            notes_url: c.manifest.notes_url.clone(),
            stale,
        }),
        last_checked_unix: snap.checked.as_ref().map(|c| c.fetched_unix),
        last_error: snap.last_error,
    }
}

/// Update-check status
///
/// How this host was installed, which channel it follows, whether a newer release is known,
/// and how to update. Reading this may kick a background refresh when the cached check is
/// older than 6 h; the response never blocks on the network.
#[utoipa::path(
    get,
    path = "/update/status",
    tag = "update",
    operation_id = "getUpdateStatus",
    responses(
        (status = OK, description = "Current update-check state", body = UpdateStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_update_status() -> Json<UpdateStatus> {
    Json(status_from(update::snapshot_and_maybe_refresh()))
}

/// Check for updates now
///
/// Forces a manifest fetch + verification and returns the refreshed state. Rate-limited to
/// one forced check per 30 s.
#[utoipa::path(
    post,
    path = "/update/check",
    tag = "update",
    operation_id = "forceUpdateCheck",
    responses(
        (status = OK, description = "Refreshed update-check state (`last_error` carries a failed check)", body = UpdateStatus),
        (status = CONFLICT, description = "Update checks are disabled on this host", body = ApiError),
        (status = TOO_MANY_REQUESTS, description = "A forced check ran less than 30 s ago", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn force_update_check() -> Response {
    match update::force_check().await {
        Ok(snap) => Json(status_from(snap)).into_response(),
        Err(update::ForceError::Disabled) => api_error(
            StatusCode::CONFLICT,
            "update checks are disabled on this host (PUNKTFUNK_UPDATE_CHECK=0)",
        ),
        Err(update::ForceError::TooSoon) => api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "a forced update check ran less than 30 s ago — try again shortly",
        ),
    }
}
