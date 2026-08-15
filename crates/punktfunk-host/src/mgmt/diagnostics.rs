//! Diagnostics endpoints: the host's health verdicts as one structured channel.
//!
//! **Admin lane only, deliberately.** Neither route is on `auth::plugin_may_access` nor
//! `cert_may_access` — both are opt-in allowlists, so a route stays denied until someone classifies
//! it, and these carry usernames, group layout and device-node state. Putting them on the plugin or
//! paired-cert lanes would be a security regression, not a convenience; the unauthenticated
//! loopback summary the tray reads may carry counts at most.

use super::shared::*;
use crate::diagnostics::{CheckSource, DiagnosticsReport};

/// Host health checks
///
/// Every verdict this host computes about its own health — group membership the managed takeover
/// needs, the input device nodes virtual controllers are built on, competing streaming servers —
/// with the impact and a copy-pasteable remedy for each.
///
/// Cached: the probes run once at startup and on demand via `POST /diagnostics/refresh`, so this is
/// cheap to poll. Checks whose status is `ok` and `inapplicable` are included — a troubleshooting
/// page needs to show what is working and to answer "why isn't this check relevant here?".
///
/// `summary`, `impact` and `remedy.text` are always present in English. A console that recognizes
/// the check's `id` replaces them with a localized string interpolated from `params`; one that does
/// not renders the wire text as-is, which is what keeps a console paired with a newer host readable.
#[utoipa::path(
    get,
    path = "/diagnostics",
    tag = "diagnostics",
    operation_id = "getDiagnostics",
    responses(
        (status = OK, description = "The current verdicts, worst-first", body = DiagnosticsReport),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_diagnostics() -> Json<DiagnosticsReport> {
    Json(crate::diagnostics::registry().report())
}

/// Re-run the health checks
///
/// Runs every probe again and returns the refreshed verdicts. Most checks describe state that only
/// changes when an operator changes it (a group membership, an installed udev rule), so this exists
/// for exactly the moment after they have done so — a "did that fix it?" button, not a poll.
#[utoipa::path(
    post,
    path = "/diagnostics/refresh",
    tag = "diagnostics",
    operation_id = "refreshDiagnostics",
    responses(
        (status = OK, description = "The refreshed verdicts, worst-first", body = DiagnosticsReport),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn refresh_diagnostics() -> Json<DiagnosticsReport> {
    // The probes stat sysfs and shell out to `id`, whose NSS lookup can block on a box with a
    // remote directory — so they run on the blocking pool rather than stalling the executor.
    let report = tokio::task::spawn_blocking(|| {
        let reg = crate::diagnostics::registry();
        reg.run_all(CheckSource::Refresh);
        reg.report()
    })
    .await
    .unwrap_or_else(|_| crate::diagnostics::registry().report());
    Json(report)
}
