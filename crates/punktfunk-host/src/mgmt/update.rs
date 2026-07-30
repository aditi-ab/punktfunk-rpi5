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

/// A running apply job (or a spawned installer that hasn't resolved yet).
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateJobInfo {
    /// The version being installed.
    pub target_version: String,
    /// `downloading` | `verifying` | `applying` | `restarting`.
    pub stage: String,
    pub received_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    pub started_unix: u64,
}

/// Durable outcome of the most recent apply attempt (survives the host's own restart).
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateResultInfo {
    pub ok: bool,
    pub from: String,
    pub to: String,
    pub finished_unix: u64,
    /// The stage that failed; absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The installer's own log file on this host, for diagnosis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Applied but activates on the next reboot (rpm-ostree).
    #[serde(default)]
    pub staged: bool,
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
    /// This install could one-click apply, but the operator hasn't opted in yet — the
    /// command to run (Linux: join the `punktfunk-update` group).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_in_hint: Option<String>,
    /// The apply in flight, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<UpdateJobInfo>,
    /// Outcome of the most recent apply attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<UpdateResultInfo>,
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
    // A spawned installer that hasn't resolved shows as a `restarting` job even though the
    // in-process job died with the previous host (the console poller needs continuity here).
    let job = snap
        .job
        .as_ref()
        .map(|j| UpdateJobInfo {
            target_version: j.target_version.clone(),
            stage: j.stage.into(),
            received_bytes: j.received_bytes,
            total_bytes: j.total_bytes,
            started_unix: j.started_unix,
        })
        .or_else(|| {
            snap.applying_from_intent().map(|i| UpdateJobInfo {
                target_version: i.to,
                stage: "restarting".into(),
                received_bytes: 0,
                total_bytes: None,
                started_unix: i.started_unix,
            })
        });
    UpdateStatus {
        install_kind: kind.as_str().into(),
        channel: channel.as_str().into(),
        current_version: current.into(),
        apply: update::apply_support().into(),
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
        opt_in_hint: update::opt_in_hint(),
        job,
        last_result: snap.last_result.as_ref().map(|r| UpdateResultInfo {
            ok: r.ok,
            from: r.from.clone(),
            to: r.to.clone(),
            finished_unix: r.finished_unix,
            stage: r.stage.clone(),
            error: r.error.clone(),
            log_path: r.log_path.clone(),
            staged: r.staged,
        }),
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

#[derive(Default, Deserialize, ToSchema)]
pub(crate) struct ApplyRequest {
    /// Proceed even while a streaming session is live (the stream will drop when the host
    /// restarts — the console warns before sending this).
    #[serde(default)]
    pub force: bool,
}

/// Apply the available update
///
/// Starts the one-click apply for install kinds that support it (Windows installer). The
/// request carries no version or URL — the host installs exactly what its verified manifest
/// announced. Progress is polled via `GET /update/status` (`job`); the host restarts as part
/// of the apply, and the outcome lands in `last_result` after it comes back.
#[utoipa::path(
    post,
    path = "/update/apply",
    tag = "update",
    operation_id = "applyUpdate",
    request_body = ApplyRequest,
    responses(
        (status = ACCEPTED, description = "Apply started — poll `GET /update/status`", body = UpdateStatus),
        (status = CONFLICT, description = "Refused: unsupported install kind, apply disabled (PUNKTFUNK_UPDATE_APPLY=0), a job already running, an active streaming session without `force`, or nothing newer to apply", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn apply_update(
    State(st): State<Arc<MgmtState>>,
    // The body is required (send `{}` for defaults) — axum has no optional-body extractor and
    // the console always posts JSON here anyway.
    ApiJson(req): ApiJson<ApplyRequest>,
) -> Response {
    // Same session-liveness composition as `get_status`: either plane counts.
    let session_active = st.app.streaming.load(std::sync::atomic::Ordering::SeqCst)
        || !crate::session_status::snapshot().is_empty();

    match update::start_apply(req.force, session_active) {
        Ok(()) => {
            let snap = update::snapshot_and_maybe_refresh();
            (StatusCode::ACCEPTED, Json(status_from(snap))).into_response()
        }
        Err(update::ApplyError::Unsupported) => api_error(
            StatusCode::CONFLICT,
            "this install kind has no one-click apply — use the update command shown in status",
        ),
        Err(update::ApplyError::Disabled) => api_error(
            StatusCode::CONFLICT,
            "one-click apply is disabled on this host (PUNKTFUNK_UPDATE_APPLY=0)",
        ),
        Err(update::ApplyError::JobRunning) => api_error(
            StatusCode::CONFLICT,
            "an update is already being applied — poll GET /update/status",
        ),
        Err(update::ApplyError::SessionActive) => api_error(
            StatusCode::CONFLICT,
            "a streaming session is active — pass {\"force\": true} to update anyway (the stream will drop)",
        ),
        Err(update::ApplyError::NothingToApply) => api_error(
            StatusCode::CONFLICT,
            "no newer release is known for this channel — run a check first",
        ),
    }
}
