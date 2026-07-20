//! Auth gate for the management API `/api/v1` routes: paired client cert (mTLS, from anywhere)
//! or a bearer token (loopback peers only). Split out of the `mgmt` facade (plan §W5).
//!
//! Three lanes, three authorities:
//! - **paired streaming cert** (mTLS, LAN) — the read-only [`cert_may_access`] allowlist.
//! - **plugin token** (bearer, loopback) — the scripting runner's capability-limited credential:
//!   the admin surface MINUS hook registration and pairing administration
//!   ([`plugin_may_access`]).
//! - **admin token** (bearer, loopback) — everything.

use super::shared::*;
use crate::gamestream::tls::PeerAddr;
use crate::gamestream::tls::PeerCertFingerprint;
use axum::extract::Request;
use axum::http::header;
use axum::http::Method;
use axum::middleware::Next;
use sha2::{Digest, Sha256};

/// Auth gate on the `/api/v1` routes: a paired client cert (mTLS, from anywhere) or the bearer token
/// (from a **loopback** peer only) — required always (the host runs with a token by construction).
/// `/api/v1/health` stays open for probes; `/api/v1/local/summary` is open to loopback peers only
/// (the tray icon's status source). The cert path authorizes only the read-only allowlist
/// ([`cert_may_access`]); the bearer path authorizes the full admin surface and is therefore confined
/// to loopback so it is never LAN-exposed even when the listener binds all interfaces by default.
pub(crate) async fn require_auth(
    State(st): State<Arc<MgmtState>>,
    req: Request,
    next: Next,
) -> Response {
    if req.uri().path() == "/api/v1/health" {
        return next.run(req).await; // liveness probe is always open
    }
    // The tray icon's status source: non-sensitive counts/booleans only, unauthenticated but
    // confined to LOOPBACK peers. The bearer-token file (and cert.pem) are SYSTEM/Administrators-
    // DACL'd on Windows, so the per-user tray process cannot authenticate — this one narrow
    // read-only route is deliberately all it needs. Not on the cert allowlist: LAN mTLS clients
    // already have the richer `/status`. (No PeerAddr ⇒ a unit test → treat as loopback, matching
    // the bearer path below.)
    if req.uri().path() == "/api/v1/local/summary" {
        let from_loopback = req
            .extensions()
            .get::<PeerAddr>()
            .is_none_or(|a| a.0.ip().is_loopback());
        return if from_loopback {
            next.run(req).await
        } else {
            api_error(
                StatusCode::UNAUTHORIZED,
                "the local summary is loopback-only",
            )
        };
    }
    // A paired native client authenticates by its mTLS certificate — the same identity + trust the
    // QUIC data plane uses. But "paired to STREAM" is not "paired to ADMINISTER": a streaming cert
    // authorizes only the safe, read-only status routes, NOT state-changing or pairing-administration
    // routes (which would let one paired client unpair others, read/arm the pairing PIN, stop
    // sessions, or edit the library). Everything outside the allowlist requires the operator's bearer
    // token. The fingerprint is attached by `serve_https` from the verified peer cert.
    if let Some(PeerCertFingerprint(Some(fp))) = req.extensions().get::<PeerCertFingerprint>() {
        if cert_may_access(req.method(), req.uri().path())
            && st.native.as_ref().is_some_and(|n| n.is_paired(fp))
        {
            return next.run(req).await;
        }
    }
    // Otherwise require the bearer token (the web console / admin) — but only from a LOOPBACK peer.
    // The token authorizes the full admin surface, so confining it to loopback keeps that surface off
    // the LAN even though the listener now binds all interfaces by default (so paired clients can
    // browse the library). The web console BFF — the sole token holder — always connects over
    // loopback, so nothing first-party is affected; a LAN caller must use a paired client cert and is
    // limited to the read-only allowlist above. (No PeerAddr ⇒ a non-`serve_https` caller, e.g. a unit
    // test → treat as loopback so handler tests still authenticate by token.)
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
    // `run` always passes a token, so no-token means a misconfigured caller (e.g. a test constructing
    // `app` directly) — deny.
    let Some(expected) = st.token.as_deref() else {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    };
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(token) if token_eq(token, expected) => next.run(req).await,
        // The scripting runner's scoped lane: same loopback confinement as the admin token, but
        // routes that would let a plugin escalate — registering hooks (arbitrary command
        // execution as the host user) or administering pairing (admitting/ejecting devices,
        // reading the PIN) — need the operator's admin token. Checked AFTER the admin token so
        // equal tokens (operator misconfiguration) degrade to full access, never to a lockout.
        Some(token)
            if st
                .plugin_token
                .as_deref()
                .is_some_and(|pt| token_eq(token, pt)) =>
        {
            if plugin_may_access(req.method(), req.uri().path()) {
                next.run(req).await
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

/// Which routes the scripting runner's **plugin token** may reach: the admin surface minus the
/// escalation routes. Exclusion-based (a plugin legitimately reads status/library/events, drives
/// sessions, and registers its UI lease), with these carve-outs:
/// - **hooks** — `hooks.json` runs operator commands on lifecycle events; writing it is arbitrary
///   command execution as the host user, and reading it can expose webhook credentials.
/// - **pairing administration** — arming/approving/denying/unpairing (and PIN visibility) decide
///   *which devices may stream*; a plugin defect must not be able to admit an attacker's device
///   or eject the operator's.
/// - **UI proxy credentials** — a plugin has no business reading another plugin's per-boot UI
///   secret; only the console proxy (admin token) needs it.
/// - **the plugin store** — installing a plugin is running new code with operator privileges, and a
///   plugin that can do that is a persistence/escalation primitive: it could install a helper that
///   isn't constrained the way it is, or switch the runner's own service state. Denied wholesale
///   (reads included — the catalog is not sensitive, but there is no reason a plugin needs it, and
///   a whole-prefix deny can't be defeated by a route added later).
pub(crate) fn plugin_may_access(method: &Method, path: &str) -> bool {
    let denied = path == "/api/v1/hooks"
        || path == "/api/v1/store"
        || path.starts_with("/api/v1/store/")
        || path == "/api/v1/pair"
        || path.starts_with("/api/v1/pair/")
        || path == "/api/v1/native/pair"
        || path.starts_with("/api/v1/native/pair/")
        || path == "/api/v1/native/pending"
        || path.starts_with("/api/v1/native/pending/")
        || (method == Method::DELETE
            && (path.starts_with("/api/v1/clients/")
                || path.starts_with("/api/v1/native/clients/")))
        || (path.starts_with("/api/v1/plugins/") && path.ends_with("/ui-credential"));
    !denied
}

/// Which routes a paired *streaming* cert (mTLS, no bearer token) may reach: a small allowlist of
/// safe, read-only status routes only. Deny-by-default — every state-changing route and every route
/// that exposes a pairing PIN or the pending-approval queue requires the operator's bearer token, so
/// a streaming client can't administer the host (unpair others, arm/read the PIN, stop sessions,
/// edit the library). `/health` is handled separately (always open).
pub(crate) fn cert_may_access(method: &Method, path: &str) -> bool {
    method == Method::GET
        && (matches!(
            path,
            "/api/v1/host"
                | "/api/v1/compositors"
                | "/api/v1/status"
                // The paired-client ROSTERS (`/clients`, `/native/clients`) are deliberately NOT on
                // this lane — they expose every OTHER paired device's name + fingerprint, which one
                // paired streaming client must not be able to enumerate. Only the bearer/loopback
                // console needs them, and no first-party client calls them (security-review 2026-07-17).
                //
                // The native clients browse the game library with their cert (no bearer token); the
                // library MUTATIONS (POST/PUT/DELETE /library/custom) stay token-only via the exact
                // GET-path match above.
                | "/api/v1/library"
        ) || path.starts_with("/api/v1/library/art/"))
}

/// Compare SHA-256 digests instead of the strings — constant-time with respect to the
/// secret without pulling in a ct-eq dependency.
pub(crate) fn token_eq(presented: &str, expected: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(expected.as_bytes())
}
