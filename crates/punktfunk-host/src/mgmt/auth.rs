//! Auth gate for `/api/v1`: a paired client cert (mTLS, from anywhere) or a bearer token
//! (loopback peers only).
//!
//! Three lanes:
//! - **paired streaming cert** — [`cert_may_access`] (status reads plus two writes: log
//!   upload and host-action invoke).
//! - **plugin token** (bearer, loopback) — [`plugin_may_access`]: admin minus hooks, pairing
//!   admin, host logs, store, and update.
//! - **admin token** (bearer, loopback) — everything.

use super::shared::*;
use crate::gamestream::tls::PeerAddr;
use crate::gamestream::tls::PeerCertFingerprint;
use axum::extract::Request;
use axum::http::header;
use axum::http::Method;
use axum::middleware::Next;
use sha2::{Digest, Sha256};

/// Which credential authorized this request. [`require_auth`] stamps it on every forwarded
/// request; handlers extract `Extension<AuthLane>`. A missing extension is a 500, not a
/// default — a router that forgot the middleware must fail closed.
///
/// [`plugin_may_access`] answers route reachability; this answers field authority. Some
/// payloads carry operator-privileged fields (`prep`, `launch.kind == "command"`) on routes
/// a plugin may otherwise call. See [`crate::library::reject_privileged_fields`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthLane {
    /// Operator admin bearer (loopback): everything, including privileged fields.
    Admin,
    /// Scripting-runner bearer (loopback): [`plugin_may_access`] routes, never privileged fields.
    Plugin,
    /// Paired streaming cert (mTLS): the [`cert_may_access`] set.
    Cert,
    /// Open route (`/health`) or loopback-only tray summary — no credential.
    Public,
}

impl AuthLane {
    /// Fields that become command execution as the host user. Admin only. Still refused
    /// on other lanes if a write that carries `prep` / `launch.kind` is later allowlisted.
    pub(crate) fn may_set_privileged_fields(self) -> bool {
        matches!(self, AuthLane::Admin)
    }

    /// Operator's own lane (the console), as opposed to a paired client or a plugin.
    ///
    /// Same arm as [`Self::may_set_privileged_fields`] today, deliberately a separate
    /// question: command execution vs. "is this the person curating the library". A
    /// read-only operator-only view (hidden titles) is not a privilege escalation;
    /// collapsing the two would leave whichever changes first answering for the other.
    pub(crate) fn is_operator(self) -> bool {
        matches!(self, AuthLane::Admin)
    }
}

/// Paired client cert (mTLS, from anywhere) or a bearer token from a loopback peer.
/// `/health` is always open; `/local/summary` is loopback-only (the tray cannot read the
/// token file). Cert: [`cert_may_access`]. Bearer: full admin, confined to loopback because
/// the listener binds all interfaces by default.
pub(crate) async fn require_auth(
    State(st): State<Arc<MgmtState>>,
    req: Request,
    next: Next,
) -> Response {
    /// Stamp the lane so a handler can refuse privileged fields to a non-operator (see [`AuthLane`]).
    async fn forward(mut req: Request, next: Next, lane: AuthLane) -> Response {
        req.extensions_mut().insert(lane);
        next.run(req).await
    }

    if req.uri().path() == "/api/v1/health" {
        return forward(req, next, AuthLane::Public).await;
    }
    // Tray status: unauthenticated, loopback only. On Windows the token file is
    // SYSTEM/Administrators-DACL'd, so the per-user tray cannot authenticate. Not on the
    // cert allowlist — LAN clients already have `/status`. No PeerAddr ⇒ test ⇒ loopback.
    if req.uri().path() == "/api/v1/local/summary" {
        let from_loopback = req
            .extensions()
            .get::<PeerAddr>()
            .is_none_or(|a| a.0.ip().is_loopback());
        return if from_loopback {
            forward(req, next, AuthLane::Public).await
        } else {
            api_error(
                StatusCode::UNAUTHORIZED,
                "the local summary is loopback-only",
            )
        };
    }
    // Fingerprint is attached by `serve_https` from the verified peer cert. Paired-to-stream
    // is not paired-to-administer: only [`cert_may_access`]; everything else needs the bearer.
    if let Some(PeerCertFingerprint(Some(fp))) = req.extensions().get::<PeerCertFingerprint>() {
        // `effective`, not `is_paired`. The expiry-blind verb answers "is this device listed"
        // (the roster). Authorizing on the listing would leave a lapsed guest on this lane
        // for as long as the record sits in the store.
        if cert_may_access(req.method(), req.uri().path())
            && st
                .native
                .as_ref()
                .is_some_and(|n| n.effective(fp, unix_now()).is_some())
        {
            return forward(req, next, AuthLane::Cert).await;
        }
    }
    // Full admin surface, so loopback only — the listener binds all interfaces so paired
    // clients can browse the library. No PeerAddr ⇒ unit test ⇒ treat as loopback.
    let from_loopback = req
        .extensions()
        .get::<PeerAddr>()
        .is_none_or(|a| a.0.ip().is_loopback());
    if !from_loopback {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "the admin API is loopback-only — a LAN client must present a paired client certificate",
        );
    }
    // `run` always passes a token; no-token is a misconfigured caller (a test building `app`
    // directly) — deny.
    let Some(expected) = st.token.as_deref() else {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if token_eq(token, expected) => forward(req, next, AuthLane::Admin).await,
        // Same loopback confinement. Checked AFTER the admin token so equal tokens
        // (operator misconfiguration) degrade to full access, never to a lockout.
        Some(token)
            if st
                .plugin_token
                .as_deref()
                .is_some_and(|pt| token_eq(token, pt)) =>
        {
            if plugin_may_access(req.method(), req.uri().path()) {
                forward(req, next, AuthLane::Plugin).await
            } else {
                api_error(
                    StatusCode::FORBIDDEN,
                    "this route is not authorized for the plugin token — it requires the \
                     operator's admin token",
                )
            }
        }
        _ => api_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid credentials (a paired client cert, or a bearer token)",
        ),
    }
}

/// Allowlist of routes the plugin token may reach. A later route is denied until classified
/// (`plugin_lane_classifies_every_route` in `mgmt::tests` fails the build otherwise).
///
/// Out of the list: hooks (operator commands + webhook secrets), `GET /logs` (those secrets
/// unredacted), pairing admin, UI-proxy credentials, the plugin store, the update surface.
/// Library writes are in because a provider reconciles its own entries; `prep` and
/// `launch.kind == "command"` are refused in the handlers via [`AuthLane`].
///
/// Route reachability is not launch isolation. `PUT /plugins/{}` lets this lane register any
/// id with the loopback port the host will dial, and `launch.kind == "plugin"` runs whatever
/// that listener answers. The runner is one process on one token, so this gate cannot tell
/// which plugin is calling. See [`crate::library::ask_plugin_launch`].
pub(crate) fn plugin_may_access(method: &Method, path: &str) -> bool {
    // (method, path); `{}` is exactly one segment. Grouped as the route table is.
    const ALLOWED: &[(&Method, &str)] = &[
        (&Method::GET, "/api/v1/health"),
        (&Method::GET, "/api/v1/host"),
        (&Method::GET, "/api/v1/status"),
        (&Method::GET, "/api/v1/local/summary"),
        (&Method::GET, "/api/v1/compositors"),
        (&Method::GET, "/api/v1/events"),
        // Rosters, read-only. DELETE is pairing admin — not listed.
        (&Method::GET, "/api/v1/clients"),
        (&Method::GET, "/api/v1/native/clients"),
        (&Method::GET, "/api/v1/gpus"),
        (&Method::PUT, "/api/v1/gpus/preference"),
        (&Method::GET, "/api/v1/display/settings"),
        (&Method::PUT, "/api/v1/display/settings"),
        (&Method::GET, "/api/v1/display/state"),
        (&Method::GET, "/api/v1/display/monitors"),
        (&Method::PUT, "/api/v1/display/layout"),
        (&Method::POST, "/api/v1/display/release"),
        (&Method::GET, "/api/v1/display/presets"),
        (&Method::POST, "/api/v1/display/presets"),
        (&Method::PUT, "/api/v1/display/presets/{}"),
        (&Method::DELETE, "/api/v1/display/presets/{}"),
        (&Method::DELETE, "/api/v1/session"),
        (&Method::POST, "/api/v1/session/idr"),
        (&Method::GET, "/api/v1/session/settings"),
        (&Method::PUT, "/api/v1/session/settings"),
        (&Method::POST, "/api/v1/game/end"),
        // Library reads + provider reconcile. Privileged fields refused via `AuthLane`.
        (&Method::GET, "/api/v1/library"),
        (&Method::GET, "/api/v1/library/art/{}/{}"),
        (&Method::GET, "/api/v1/library/scanners"),
        (&Method::PUT, "/api/v1/library/scanners/{}"),
        (&Method::POST, "/api/v1/library/custom"),
        (&Method::PUT, "/api/v1/library/custom/{}"),
        (&Method::DELETE, "/api/v1/library/custom/{}"),
        (&Method::PUT, "/api/v1/library/provider/{}"),
        (&Method::DELETE, "/api/v1/library/provider/{}"),
        // Provider liveness for its own titles — mapped through the catalog, no one else's session.
        (&Method::PUT, "/api/v1/library/provider/{}/running"),
        (&Method::POST, "/api/v1/stats/capture/start"),
        (&Method::POST, "/api/v1/stats/capture/stop"),
        (&Method::GET, "/api/v1/stats/capture/status"),
        (&Method::GET, "/api/v1/stats/capture/live"),
        (&Method::GET, "/api/v1/stats/recordings"),
        (&Method::GET, "/api/v1/stats/recordings/{}"),
        (&Method::DELETE, "/api/v1/stats/recordings/{}"),
        (&Method::GET, "/api/v1/plugins"),
        (&Method::POST, "/api/v1/plugins/logs"),
        (&Method::PUT, "/api/v1/plugins/{}"),
        (&Method::DELETE, "/api/v1/plugins/{}"),
    ];
    ALLOWED
        .iter()
        .any(|(m, pat)| *m == method && path_matches(pat, path))
}

/// `{}` is exactly one segment. Never a prefix test: `/api/v1/plugins/{}` must not swallow
/// `/api/v1/plugins/x/ui-credential` the way `starts_with` would.
fn path_matches(pattern: &str, path: &str) -> bool {
    let (mut p, mut a) = (pattern.split('/'), path.split('/'));
    loop {
        match (p.next(), a.next()) {
            (None, None) => return true,
            (Some(pe), Some(ae)) if pe == "{}" || pe == ae => continue,
            _ => return false,
        }
    }
}

/// Allowlist a paired streaming cert may reach. Deny-by-default: pairing PIN, pending queue,
/// and every other mutation need the operator bearer. `/health` is always open, separately.
pub(crate) fn cert_may_access(method: &Method, path: &str) -> bool {
    // Write-only: the device gets an id back and can read nothing, not even its own upload.
    // Size- and quota-capped in the handler/store.
    if method == Method::POST && path == "/api/v1/client-logs" {
        return true;
    }
    // Id-only invoke. The handler re-reads `effective(fp, now)` and demands `GRANT_POWER`;
    // the route being reachable grants nothing by itself. `GET /actions` is on the read list.
    if method == Method::POST && path_matches("/api/v1/actions/{}", path) {
        return true;
    }
    method == Method::GET
        && (matches!(
            path,
            "/api/v1/host"
                | "/api/v1/compositors"
                | "/api/v1/status"
                | "/api/v1/actions"
                // Rosters are not on this lane: they name every other paired device. Library
                // GET is; POST/PUT/DELETE stay token-only via this exact-path match.
                | "/api/v1/library"
        ) || path.starts_with("/api/v1/library/art/"))
}

/// Compare SHA-256 digests, not the strings — constant-time in the secret without a ct-eq
/// dependency.
pub(crate) fn token_eq(presented: &str, expected: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(expected.as_bytes())
}

/// Host wall clock, unix seconds — the clock every stored access deadline is expressed in.
/// Sampled at each check, same as `mgmt::native`'s copy.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /logs` is the `/hooks` carve-out's back door: webhook URLs (the bearer for those
    /// sinks) and spawned command lines, unredacted. A plugin only needs the other direction.
    #[test]
    fn the_plugin_lane_writes_logs_but_never_reads_them() {
        assert!(!plugin_may_access(&Method::GET, "/api/v1/logs"));
        assert!(plugin_may_access(&Method::POST, "/api/v1/plugins/logs"));
    }
}
