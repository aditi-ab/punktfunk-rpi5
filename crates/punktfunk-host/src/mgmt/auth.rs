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

/// **Which credential authorized this request**, attached to the request extensions by
/// [`require_auth`] on every request it forwards.
///
/// [`plugin_may_access`] answers "may this lane reach this route"; this answers "may this lane set
/// this *field*". Some payloads carry operator-privileged fields on routes a plugin otherwise has
/// every business calling — the library reconcile is the case that matters: a provider plugin owns
/// its entry set, but `prep` and `launch.kind == "command"` are executed verbatim as the host user
/// (`/bin/sh -c` / `cmd.exe /c`), which is the same primitive the `/hooks` carve-out withholds.
/// Route-level authorization cannot express that; a handler holding this can (see
/// [`crate::library::reject_privileged_fields`]).
///
/// Extracted by handlers as `Extension<AuthLane>`. A missing extension is a 500, not a default —
/// a router that forgot the middleware must fail closed, never silently grant admin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthLane {
    /// The operator's admin bearer token (loopback): everything, including the privileged fields.
    Admin,
    /// The scripting runner's scoped bearer token (loopback): [`plugin_may_access`] routes, and
    /// never the operator-privileged fields inside them.
    Plugin,
    /// A paired streaming client certificate (mTLS, LAN): the read-only [`cert_may_access`] set.
    Cert,
    /// An always-open route (`/health`) or the loopback-only tray summary — no credential at all.
    Public,
}

impl AuthLane {
    /// Whether this lane may set fields that become command execution as the host user. Only the
    /// operator's own token may: the console is the surface where the operator types a command, and
    /// typing it there is the trust decision. Everything else is refused, including a paired cert
    /// (which cannot reach a write route anyway — belt and braces if the allowlist ever grows).
    pub(crate) fn may_set_privileged_fields(self) -> bool {
        matches!(self, AuthLane::Admin)
    }

    /// Whether this is the operator's own lane — the console, as opposed to a paired client or a
    /// plugin.
    ///
    /// Same arm as [`may_set_privileged_fields`](Self::may_set_privileged_fields) today, and
    /// deliberately a separate question: that one asks "may this caller cause command execution",
    /// this one asks "is this caller the person curating the library". A read-only view the operator
    /// alone should see (their hidden titles) is not a privilege escalation, and collapsing the two
    /// would leave whichever one changes first silently answering for the other.
    pub(crate) fn is_operator(self) -> bool {
        matches!(self, AuthLane::Admin)
    }
}

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
    /// Stamp the authorizing lane onto the request before it reaches a handler, so a handler can
    /// refuse operator-privileged FIELDS to a non-operator lane (see [`AuthLane`]).
    async fn forward(mut req: Request, next: Next, lane: AuthLane) -> Response {
        req.extensions_mut().insert(lane);
        next.run(req).await
    }

    if req.uri().path() == "/api/v1/health" {
        return forward(req, next, AuthLane::Public).await; // liveness probe is always open
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
            forward(req, next, AuthLane::Public).await
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
            return forward(req, next, AuthLane::Cert).await;
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
        Some(token) if token_eq(token, expected) => forward(req, next, AuthLane::Admin).await,
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

/// The routes the scripting runner's **plugin token** may reach — an explicit **allowlist**, so a
/// route added later is denied until someone classifies it (`plugin_lane_classifies_every_route` in
/// `mgmt::tests` fails the build otherwise).
///
/// This gate used to be a denylist of route prefixes, and that is precisely how the 2026-08-05
/// review's H-1/H-2 arrived: `/api/v1/library` was never enumerated, so the plugin lane inherited
/// two copies of the very "arbitrary command execution as the host user" primitive the `/hooks`
/// carve-out exists to withhold, plus an unconfined file read. Every sibling gate in the system
/// (`cert_may_access`, the QUIC pairing gate, the console's `isPublicPath`) is deny-by-default;
/// this one now is too.
///
/// What stays *out* of the list, and why:
/// - **hooks** — `hooks.json` runs operator commands on lifecycle events; writing it is arbitrary
///   command execution as the host user, and reading it can expose webhook credentials.
/// - **pairing administration** — arming/approving/denying/unpairing (and PIN visibility) decide
///   *which devices may stream*; a plugin defect must not be able to admit an attacker's device
///   or eject the operator's.
/// - **UI proxy credentials** — a plugin has no business reading another plugin's per-boot UI
///   secret; only the console proxy (admin token) needs it.
/// - **the plugin store** — installing a plugin is running new code with operator privileges, and a
///   plugin that can do that is a persistence/escalation primitive: it could install a helper that
///   isn't constrained the way it is, or switch the runner's own service state.
/// - **the update surface** — operator business end to end (`apply` runs an installer / the root
///   helper).
///
/// The library *writes* below are on the list because a provider plugin's whole job is reconciling
/// its own entries — but the two operator-privileged FIELDS inside those payloads (`prep`, and
/// `launch.kind == "command"`) are refused to this lane in the handlers, via [`AuthLane`]. Route
/// reachability and field authority are separate questions and this gate only answers the first.
pub(crate) fn plugin_may_access(method: &Method, path: &str) -> bool {
    // (method, path) pairs, `{}` matching exactly one path segment. Grouped as the route table is.
    const ALLOWED: &[(&Method, &str)] = &[
        // Host / status reads.
        (&Method::GET, "/api/v1/health"),
        (&Method::GET, "/api/v1/host"),
        (&Method::GET, "/api/v1/status"),
        (&Method::GET, "/api/v1/local/summary"),
        (&Method::GET, "/api/v1/compositors"),
        (&Method::GET, "/api/v1/events"),
        (&Method::GET, "/api/v1/logs"),
        // The paired-device rosters: read-only. (DELETE is pairing administration — not listed.)
        (&Method::GET, "/api/v1/clients"),
        (&Method::GET, "/api/v1/native/clients"),
        // GPU + display control: host configuration a plugin may legitimately steer (a room
        // automation plugin swaps the layout with the lights); no privilege boundary crossed.
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
        // Session control: stopping/steering a session is what a launcher plugin exists to do.
        (&Method::DELETE, "/api/v1/session"),
        (&Method::POST, "/api/v1/session/idr"),
        (&Method::GET, "/api/v1/session/settings"),
        (&Method::PUT, "/api/v1/session/settings"),
        (&Method::POST, "/api/v1/game/end"),
        // Library: reads, plus the provider reconcile a scanner plugin is built around. The
        // operator-only FIELDS inside these payloads are refused separately (see `AuthLane`).
        (&Method::GET, "/api/v1/library"),
        (&Method::GET, "/api/v1/library/art/{}/{}"),
        (&Method::GET, "/api/v1/library/scanners"),
        (&Method::PUT, "/api/v1/library/scanners/{}"),
        (&Method::POST, "/api/v1/library/custom"),
        (&Method::PUT, "/api/v1/library/custom/{}"),
        (&Method::DELETE, "/api/v1/library/custom/{}"),
        (&Method::PUT, "/api/v1/library/provider/{}"),
        (&Method::DELETE, "/api/v1/library/provider/{}"),
        // Stats / telemetry.
        (&Method::POST, "/api/v1/stats/capture/start"),
        (&Method::POST, "/api/v1/stats/capture/stop"),
        (&Method::GET, "/api/v1/stats/capture/status"),
        (&Method::GET, "/api/v1/stats/capture/live"),
        (&Method::GET, "/api/v1/stats/recordings"),
        (&Method::GET, "/api/v1/stats/recordings/{}"),
        (&Method::DELETE, "/api/v1/stats/recordings/{}"),
        // The plugin's own directory entry + log ingest (its UI lease registration).
        (&Method::GET, "/api/v1/plugins"),
        (&Method::POST, "/api/v1/plugins/logs"),
        (&Method::PUT, "/api/v1/plugins/{}"),
        (&Method::DELETE, "/api/v1/plugins/{}"),
    ];
    ALLOWED
        .iter()
        .any(|(m, pat)| *m == method && path_matches(pat, path))
}

/// Match a route pattern against a concrete path, `{}` standing for exactly one segment. Segment-
/// wise (never a substring/prefix test), so `/api/v1/plugins/{}` cannot swallow
/// `/api/v1/plugins/x/ui-credential` the way a `starts_with` would.
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
