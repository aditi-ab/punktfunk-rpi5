//! Handler + auth tests for the management API, exercised through `app()`. Split out of the
//! `mgmt` facade (plan §W5).

use super::*;
use crate::encode::Codec;
use crate::gamestream::tls::{PeerAddr, PeerCertFingerprint};
use crate::gamestream::{cert::ServerIdentity, Host, LaunchSession, HTTPS_PORT, HTTP_PORT};
use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::Ordering;
use tower::ServiceExt;

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
    };
    let identity = ServerIdentity::ephemeral().expect("ephemeral identity");
    Arc::new(AppState::new(host, identity, test_stats()))
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

    // Session over → back to idle.
    drop(session);
    let (_, body) = send(&app, summary_req()).await;
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);
}

/// The tray's `/local/summary` is unauthenticated for LOOPBACK peers only — a LAN peer is
/// rejected even though the route needs no bearer token, and the body never carries secret
/// material (no PIN values, no fingerprints, no device names — counts/booleans only).
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

    // The carve-outs answer 403 (authenticated but not authorized), not 401.
    for (method, path) in [
        (Method::GET, "/api/v1/hooks"),
        (Method::PUT, "/api/v1/hooks"),
        (Method::GET, "/api/v1/pair"),
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
    ] {
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
    // Every backend the host knows, in stable order.
    let ids: Vec<&str> = arr.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["kwin", "gamescope", "mutter", "wlroots", "hyprland"]);
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

#[tokio::test]
async fn paired_clients_list_and_unpair() {
    let state = test_state();
    let app = test_app(state.clone(), None);

    // Pin the host's own cert DER as a stand-in client.
    let (_, pem) = x509_parser::pem::parse_x509_pem(state.identity.cert_pem.as_bytes()).unwrap();
    let der = pem.contents.clone();
    let fingerprint = hex::encode(Sha256::digest(&der));
    // Isolate from any real paired store on the dev box: AppState::new loads
    // ~/.config/punktfunk/paired.json, so clear it before seeding our stand-in — otherwise
    // a real GameStream-paired client lands at body[0] and this assertion sees its hash.
    {
        let mut p = state.paired.lock().unwrap();
        p.clear();
        p.push(der);
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
    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    assert_eq!(send(&app, del(fingerprint)).await.0, StatusCode::NOT_FOUND);
}

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
    let err = run(test_state(), opts, None, test_stats(), false)
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

/// The OpenAPI document lists every route with a unique operationId (codegen relies
/// on both), and the checked-in copy is current.
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
    // The experimental DDC/CI + PnP-disable axes are acted on (Windows exclusive-isolate path).
    assert!(enforced.contains(&"ddc_power_off"));
    assert!(enforced.contains(&"pnp_disable_monitors"));
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
    assert!(
        listed > 0 || !body["error"].is_null() || body["compositor"].is_null(),
        "an empty list must carry an error or an absent compositor: {body}"
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

    // The ring is a process-wide singleton — start from wherever its cursor currently is.
    let (s, json) = send(&app, get_req("/api/v1/logs")).await;
    assert_eq!(s, StatusCode::OK);
    let start = json["next"].as_u64().unwrap();

    let ring = crate::log_capture::ring();
    ring.push(&tracing::Level::WARN, "mgmt::tests", "first".into());
    ring.push(&tracing::Level::INFO, "mgmt::tests", "second".into());

    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={start}"))).await;
    assert_eq!(s, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["msg"], "first");
    assert_eq!(entries[0]["level"], "WARN");
    assert_eq!(json["next"].as_u64().unwrap(), start + 2);
    assert_eq!(json["dropped"], false);

    // Nothing newer → empty page, cursor unchanged.
    let after = start + 2;
    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={after}"))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["entries"].as_array().unwrap().is_empty());
    assert_eq!(json["next"].as_u64().unwrap(), after);
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

/// The scanner list is platform-shaped and read-only-safe; the toggle rejects unknown ids with
/// 404. (A successful toggle PUT would write the developer's real `library-scanners.json`, so the
/// write path is exercised only through the unknown-id rejection here — the settings round-trip
/// itself is unit-tested in `library::scanners` against pure shapes.)
#[tokio::test]
async fn library_scanner_list_and_unknown_toggle() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/library/scanners")).await;
    assert_eq!(s, StatusCode::OK);
    let scanners = json.as_array().expect("a scanner array");
    assert!(
        scanners
            .iter()
            .any(|sc| sc["id"] == "steam" && sc["label"].is_string() && sc["enabled"].is_boolean()),
        "steam must be a scanner on every platform: {json}"
    );
    // Only platform-available scanners appear (`custom` is a store, never a scanner).
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
        "unknown scanner id must 404: {json}"
    );
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
