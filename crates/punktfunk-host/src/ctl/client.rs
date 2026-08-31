//! The ctl surface's loopback client — discovery, the **pinned** TLS transport, and the one place
//! a credential is read (implementation plan §§1–2, invariants I1/I2).
//!
//! Three files on disk are the whole configuration; nothing comes from argv or the environment:
//!
//! | file | what | absent ⇒ |
//! |------|------|----------|
//! | `mgmt-endpoint` | the port the host actually bound (`pf_paths::published_mgmt_port`) | fall back to 47990, exactly like the tray and the console |
//! | `native-cert.pem` (else `cert.pem`) | the leaf the mgmt listener presents — **the pin** | [`EXIT_UNREACHABLE`]: no pin, so no token, so no call |
//! | `mgmt-token` | the operator bearer the console already uses | [`EXIT_UNREACHABLE`] ("is the host installed?") |
//!
//! **I2, pin before token.** The agent's rustls verifier is the workspace's canonical
//! [`PinVerify`](punktfunk_core::tls::PinVerify) with the host's own leaf fingerprint. rustls
//! validates the server certificate *during the handshake*, and ureq writes the request line and
//! headers only after the handshake completes — so on a mismatch the `Authorization` header is
//! never serialised, let alone sent. That is a property of the ordering, not of a check we
//! remember to run, which is why the port-squat vector (another local uid binding the mgmt port
//! while the host is down) closes with no server-side change at all (I4).
//!
//! Telling a pin mismatch apart from "nothing is listening" is what [`PinVerify::with_observed`]
//! is for: it records the leaf it saw *before* comparing, so after a failed connect a slot holding
//! a fingerprint that isn't ours means squat/rotation ([`EXIT_PIN`]), and an empty slot means we
//! never got a certificate at all ([`EXIT_UNREACHABLE`]).
//!
//! **I1, no credential on argv/env/logs.** There is deliberately no `--token` flag and no
//! `PUNKTFUNK_MGMT_TOKEN` read here: an operator who had to put the token in ctl's environment
//! would publish it in `/proc/<pid>/environ`, which is exactly the cross-uid leak the config dir's
//! 0700 mode exists to prevent. The cost is stated in the docs: a host handed its token by
//! `--mgmt-token`/env and *never* persisting one is not reachable by ctl. Every packaged host
//! persists (`mgmt_token::load_or_generate`), so this is a dev-box footnote, not a gap.
//!
//! **ctl never mints.** A missing `mgmt-token` is a hard error, never a "let me generate one" —
//! the inverse of the `web-password` silent-adoption finding (security sweep 2026-08-15). The
//! host is the only minter; ctl is a consumer.
//!
//! Responses come back as [`serde_json::Value`], not the in-crate mgmt structs. That is not
//! laziness about drift, it is the *stronger* answer for the contract this ships: `--json` echoes
//! the server's own JSON verbatim, so a field added to a response reaches the plugin with no ctl
//! diff at all, and the OpenAPI drift gate remains the single place the shapes are pinned. Only
//! the human tables name fields, and a table that misses a new one is cosmetic.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Exit codes (implementation plan §4). Distinct because scripts branch on them — most of all
/// [`EXIT_PIN`], which is a security signal and not an ordinary failure.
pub const EXIT_API: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNREACHABLE: i32 = 3;
pub const EXIT_PIN: i32 = 4;

/// The JSON envelope version (I8). Additive: fields may be added, never removed or retyped, and
/// this bumps only if that promise has to break.
pub const SCHEMA_VERSION: u32 = 1;

/// A failed verb: the exit code the process takes, and the line a human reads.
#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self::new(EXIT_UNREACHABLE, message)
    }
    pub fn api(message: impl Into<String>) -> Self {
        Self::new(EXIT_API, message)
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

/// Connect timeout — loopback, so a slow one means "nothing is there", not "the network is far".
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// Whole-call timeout for the one-shot verbs. `watch` passes `None` (it is long-lived by design).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Client {
    agent: ureq::Agent,
    base: String,
    /// `Bearer <token>`, built once from the 0600 file. The ONLY place in this crate where the
    /// operator credential exists outside `mgmt_token` (I1) — grep for `bearer` to audit it.
    bearer: String,
    /// The leaf the last handshake actually presented — the [`EXIT_PIN`] discriminator.
    observed: Arc<Mutex<Option<[u8; 32]>>>,
    pin: [u8; 32],
}

impl Client {
    /// Discover, pin and authenticate. `global_timeout` is `None` for `watch`.
    pub fn connect(global_timeout: Option<Duration>) -> Result<Client> {
        Self::connect_in(&pf_paths::config_dir(), global_timeout)
    }

    /// The IO half, taking the config directory — so the pin-mismatch negative can be exercised
    /// against a real TLS listener without mutating `PUNKTFUNK_CONFIG_DIR` (which needs `unsafe`
    /// since edition 2024, and which this module refuses to need). Same split, same reason, as
    /// `pf_paths::published_mgmt_port_in`.
    pub fn connect_in(dir: &Path, global_timeout: Option<Duration>) -> Result<Client> {
        let pin = load_pin(dir)?;
        let token = load_token(dir)?;
        let port = pf_paths::published_mgmt_port_in(dir).unwrap_or(crate::mgmt::DEFAULT_PORT);
        let observed = Arc::new(Mutex::new(None));
        Ok(Client {
            agent: agent(pin, observed.clone(), global_timeout),
            // Always loopback: the admin surface is honoured from LOOPBACK peers only
            // (`mgmt::auth`), so any other address would be refused by the host anyway.
            base: format!("https://127.0.0.1:{port}"),
            bearer: format!("Bearer {token}"),
            observed,
            pin,
        })
    }

    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .get(self.url(path))
            .header("Authorization", &self.bearer)
            .call();
        self.finish(sent, path)
    }

    pub fn delete(&self, path: &str) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .delete(self.url(path))
            .header("Authorization", &self.bearer)
            .call();
        self.finish(sent, path)
    }

    pub fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .post(self.url(path))
            .header("Authorization", &self.bearer)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        self.finish(sent, path)
    }

    /// Whole-object replace. `PUT /display/settings` is the only route shaped this way, and it is
    /// why `ctl display preset` reads the stored policy before writing one: a PUT built from the
    /// verb's arguments alone would default every axis the caller did not name.
    pub fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .put(self.url(path))
            .header("Authorization", &self.bearer)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        self.finish(sent, path)
    }

    pub fn patch(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .patch(self.url(path))
            .header("Authorization", &self.bearer)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        self.finish(sent, path)
    }

    /// The streaming half, for `watch`: the raw response body of a GET, left unread so the caller
    /// can consume SSE frames as they arrive.
    pub fn stream(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>> {
        let resp = self
            .agent
            .get(self.url(path))
            .header("Authorization", &self.bearer)
            .call()
            .map_err(|e| self.transport_failure(e, path))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let mut resp = resp;
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(Failure::api(http_error(status, &body, path)));
        }
        Ok(Box::new(resp.into_body().into_reader()))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn finish(
        &self,
        sent: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        path: &str,
    ) -> Result<serde_json::Value> {
        let mut resp = sent.map_err(|e| self.transport_failure(e, path))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(Failure::api(http_error(status, &body, path)));
        }
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&body)
            .map_err(|e| Failure::api(format!("{path}: the host sent JSON we can't parse ({e})")))
    }

    /// Classify a transport failure. The observed-fingerprint slot is what separates "somebody
    /// else is on that port" (a security answer, [`EXIT_PIN`]) from "nobody is" ([`EXIT_UNREACHABLE`]).
    fn transport_failure(&self, e: ureq::Error, path: &str) -> Failure {
        if let Some(seen) = *self.observed.lock().unwrap() {
            if seen != self.pin {
                return Failure::new(
                    EXIT_PIN,
                    format!(
                        "certificate pin mismatch on {base} — the process answering there presented \
                         {seen}, but this host's identity is {ours}. No token was sent. Either the \
                         host regenerated its certificate (delete the stale pairing state and \
                         re-pair) or another local process is squatting the management port.",
                        base = self.base,
                        seen = hex::encode(seen),
                        ours = hex::encode(self.pin),
                    ),
                );
            }
        }
        Failure::unreachable(format!(
            "cannot reach the management API at {}{path}: {e}. Is the host running \
             (`systemctl --user status punktfunk-host`)?",
            self.base
        ))
    }
}

/// Turn a non-2xx into the line a human gets, unwrapping the `ApiError` envelope when there is one.
fn http_error(status: u16, body: &str, path: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    match status {
        401 | 403 => format!(
            "{path}: the host rejected our token ({status}). The persisted `mgmt-token` and the \
             running host disagree — restart the host, or delete the file and let it re-mint."
        ),
        404 => format!("{path}: no such thing here ({status} {detail})"),
        503 => format!("{path}: {detail} ({status})"),
        _ if detail.is_empty() => format!("{path}: the host answered {status}"),
        _ => format!("{path}: {detail} ({status})"),
    }
}

/// The mgmt listener presents the **native** identity when one exists and the legacy GameStream
/// identity otherwise (`mgmt::run` takes a `NativeIdentity`; `identity::load_or_adopt` mints
/// `native-cert.pem` or adopts `cert.pem`). Same order the tray and the plugin runner already use —
/// pinning the wrong one of the pair is a guaranteed [`EXIT_PIN`] on a perfectly healthy host.
fn load_pin(dir: &Path) -> Result<[u8; 32]> {
    use rustls::pki_types::pem::PemObject;
    let pem = std::fs::read(dir.join("native-cert.pem"))
        .or_else(|_| std::fs::read(dir.join("cert.pem")))
        .map_err(|_| {
            Failure::unreachable(format!(
                "no host certificate in {} (looked for native-cert.pem, then cert.pem). \
                 Without it there is nothing to pin, and ctl will not send a token unpinned. \
                 Has the host ever run on this machine?",
                dir.display()
            ))
        })?;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(&pem).map_err(|e| {
        Failure::unreachable(format!(
            "the host certificate in {} is not readable as PEM ({e})",
            dir.display()
        ))
    })?;
    Ok(punktfunk_core::tls::cert_fingerprint(der.as_ref()))
}

/// Read the persisted operator token. **Never generates one** — see the module docs.
fn load_token(dir: &Path) -> Result<String> {
    crate::mgmt_token::read_persisted(dir).ok_or_else(|| {
        Failure::unreachable(format!(
            "no management token in {} — ctl reads the one the host persists and never mints its \
             own. Start the host once (`systemctl --user start punktfunk-host`) and retry.",
            dir.join("mgmt-token").display()
        ))
    })
}

/// The pinned agent — the tray's shape (`punktfunk-tray/src/status.rs`), with the pin made
/// mandatory and the observed slot wired up so a mismatch is reportable.
fn agent(
    pin: [u8; 32],
    observed: Arc<Mutex<Option<[u8; 32]>>>,
    global_timeout: Option<Duration>,
) -> ureq::Agent {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(punktfunk_core::tls::PinVerify::with_observed(
            Some(pin),
            observed,
        )))
        .with_no_client_auth();
    // ureq's own `TlsConfig` has roots, a client cert and an off-switch but no hook for a custom
    // verifier, so the agent takes the `ClientConfig` directly through the shared glue.
    punktfunk_core::tls::ureq_agent::agent(
        Arc::new(tls),
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(global_timeout)
            // Let 4xx/5xx come back as responses so the `ApiError` body reaches the operator
            // instead of being flattened into "status 400".
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// **The I2 negative — the reason the pin exists.** A process that is not the host answers on
    /// the management port (the port-squat vector: another local uid binds it while the host is
    /// down). It presents a perfectly valid, perfectly well-formed self-signed certificate that
    /// simply isn't ours.
    ///
    /// Two things must be true, and only the second one is about cryptography:
    ///  1. the verb fails with [`EXIT_PIN`] — a *distinct* code, so a script does not retry into
    ///     the squatter the way it would for "host down";
    ///  2. the squatter receives **zero application bytes**. rustls rejects the certificate during
    ///     the handshake, so ureq never gets to serialise a request line — the `Authorization`
    ///     header is not "sent and ignored", it is never constructed. That ordering is the whole
    ///     security property, and this test is what stops a future refactor (an agent added per
    ///     agent, a retry that disables verification "just to see") from quietly inverting it.
    #[test]
    fn a_squatter_gets_exit_4_and_not_one_byte_of_the_token() {
        punktfunk_core::tls::install_default_provider();
        let dir = std::env::temp_dir().join(format!("pf-ctl-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Two identities that have nothing to do with each other: what the config dir says the
        // host is, and what actually answers on the port.
        let ours = crate::identity::ephemeral().unwrap();
        let squatter = crate::identity::ephemeral().unwrap();
        let server = crate::gamestream::tls::server_config_optional_client(
            &squatter.cert_pem,
            &squatter.key_pem,
        )
        .unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(dir.join("native-cert.pem"), &ours.cert_pem).unwrap();
        std::fs::write(
            dir.join("mgmt-token"),
            "PUNKTFUNK_MGMT_TOKEN=s3kr1t-token\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("mgmt-endpoint"),
            format!("PUNKTFUNK_MGMT_URL=https://127.0.0.1:{port}\n"),
        )
        .unwrap();

        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorder = seen.clone();
        let squat = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("squatter accept");
            let mut conn = rustls::ServerConnection::new(server).expect("squatter tls");
            // The client will send a fatal alert instead of finishing — that is the pass condition,
            // so an error here is expected and deliberately ignored.
            let _ = conn.complete_io(&mut sock);
            let mut plaintext = Vec::new();
            let _ = conn.reader().read_to_end(&mut plaintext);
            *recorder.lock().unwrap() = plaintext;
        });

        let err = Client::connect_in(&dir, Some(Duration::from_secs(10)))
            .and_then(|c| c.get("/api/v1/status"))
            .expect_err("a mismatched certificate must not produce a successful call");
        assert_eq!(err.code, EXIT_PIN, "wrong exit code: {}", err.message);
        assert!(
            err.message.contains("pin mismatch"),
            "the operator must be told WHICH failure this is: {}",
            err.message
        );

        squat.join().unwrap();
        let bytes = seen.lock().unwrap().clone();
        assert!(
            bytes.is_empty(),
            "the squatter read {} application bytes; the token must never leave the process \
             before the pin matches",
            bytes.len()
        );
        assert!(!String::from_utf8_lossy(&bytes).contains("s3kr1t-token"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half of I2: a config dir with no certificate at all is [`EXIT_UNREACHABLE`], not
    /// a quiet fallback to an unverified connection. "No pin available" must never mean "connect
    /// anyway" — that is the shape the tray can afford (it holds no token) and ctl cannot.
    #[test]
    fn no_certificate_means_no_call_at_all() {
        let dir = std::env::temp_dir().join(format!("pf-ctl-nocert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mgmt-token"), "PUNKTFUNK_MGMT_TOKEN=deadbeef\n").unwrap();
        // `.err()` rather than `expect_err`: `Client` deliberately has no `Debug`, because the
        // derived one would print the bearer into any panic message or `{:?}` a future edit adds.
        let err = Client::connect_in(&dir, None)
            .err()
            .expect("no cert, no connection");
        assert_eq!(err.code, EXIT_UNREACHABLE);
        assert!(err.message.contains("native-cert.pem"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// And ctl never mints: a config dir with a certificate but no token fails, and leaves the
    /// directory exactly as it found it (the inverted `web-password` lesson).
    #[test]
    fn a_missing_token_is_an_error_never_a_freshly_minted_one() {
        let dir = std::env::temp_dir().join(format!("pf-ctl-notoken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ours = crate::identity::ephemeral().unwrap();
        std::fs::write(dir.join("native-cert.pem"), &ours.cert_pem).unwrap();
        let err = Client::connect_in(&dir, None)
            .err()
            .expect("no token, no connection");
        assert_eq!(err.code, EXIT_UNREACHABLE);
        assert!(!dir.join("mgmt-token").exists(), "ctl minted a token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_error_unwraps_the_api_envelope() {
        let msg = http_error(
            400,
            r#"{"error":"grants has reserved bits set"}"#,
            "/api/v1/x",
        );
        assert!(msg.contains("grants has reserved bits set"), "{msg}");
        assert!(msg.contains("400"), "{msg}");
    }

    #[test]
    fn http_error_names_the_token_when_auth_fails() {
        let msg = http_error(401, "", "/api/v1/status");
        assert!(msg.contains("mgmt-token"), "{msg}");
    }

    #[test]
    fn exit_codes_are_distinct() {
        // Scripts branch on these; a collision would silently merge "no host" with "squatter".
        let all = [EXIT_API, EXIT_USAGE, EXIT_UNREACHABLE, EXIT_PIN];
        let mut seen: Vec<i32> = all.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), all.len());
        // And none of them is 0 — a failure that exits 0 is worse than a wrong code.
        assert!(all.iter().all(|c| *c != 0));
    }
}
