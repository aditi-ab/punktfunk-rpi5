//! Game-library client for the host's management REST API (the Apple `LibraryClient`
//! ported): `GET https://<host>:<mgmt>/api/v1/library` plus the per-title art proxy.
//! Authentication is **mTLS** — this client presents its persistent identity (the same
//! cert the host paired over QUIC) and the host authorizes paired certificates for the
//! read-only library routes, no bearer token. The host's self-signed certificate is
//! verified by its pinned SHA-256 fingerprint (`KnownHost::fp_hex`), not a CA chain.

use serde::Deserialize;
use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The management API's default port — matches `mgmt::DEFAULT_PORT` on the host. A
/// discovered host may override it via its mDNS `mgmt` TXT (`DiscoveredHost::mgmt_port`);
/// saved-but-not-advertising hosts fall back here (Apple parity).
pub const DEFAULT_MGMT_PORT: u16 = 47990;

/// Cover-art URLs, mirroring the host's `library::Artwork`: absolute CDN URLs for custom
/// entries, host-relative proxy paths (`/api/v1/library/art/...`) for Steam titles. The
/// wire shape also carries a `logo` (a transparent title logo) — not a poster kind, so
/// serde just skips it here.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Artwork {
    #[serde(default)]
    pub portrait: Option<String>,
    #[serde(default)]
    pub hero: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
}

impl Artwork {
    /// Poster candidates in the Apple client's fallback order — portrait (the 600×900
    /// capsule) → header (near-universal) → hero — with host-relative paths resolved
    /// against `base` so the loader only ever sees absolute URLs.
    pub fn poster_candidates(&self, base: &str) -> Vec<String> {
        [&self.portrait, &self.header, &self.hero]
            .into_iter()
            .flatten()
            .map(|u| {
                if u.starts_with('/') {
                    format!("{base}{u}")
                } else {
                    u.clone()
                }
            })
            .collect()
    }
}

/// One title in the host's unified library. `id` is store-qualified (`steam:<appid>`,
/// `custom:<id>`) and is also the launch handle the Hello carries when a session is
/// started from the library. The host's `launch` spec field is deliberately not
/// deserialized — launching goes by id, the host resolves the spec itself.
#[derive(Clone, Debug, Deserialize)]
pub struct GameEntry {
    pub id: String,
    /// Which store surfaced it (`"steam"`, `"custom"`, future `"heroic"`/`"gog"`/…) —
    /// drives the poster's store badge.
    pub store: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
}

/// Errors surfaced to the UI so it can guide setup (the common case is "not paired yet").
#[derive(Debug)]
pub enum LibraryError {
    /// The host rejected our certificate — this device isn't on its paired list.
    NotPaired,
    /// The host's certificate didn't hash to the pinned fingerprint (impostor/rotated cert).
    PinMismatch,
    Http(u16),
    Unreachable(String),
}

impl std::fmt::Display for LibraryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryError::NotPaired => f.write_str(
                "The host didn't recognize this device. Pair with the host first — the \
                 library is authorized by this device's certificate (no token needed).",
            ),
            LibraryError::PinMismatch => f.write_str(
                "The host's certificate doesn't match the pinned fingerprint. \
                 Re-pair with a PIN to re-establish trust.",
            ),
            LibraryError::Http(code) => {
                write!(f, "The management API returned HTTP {code}.")
            }
            LibraryError::Unreachable(why) => write!(
                f,
                "Couldn't reach the host's management API: {why}. Check the host is \
                 updated and reachable (a host pinned to --mgmt-bind 127.0.0.1 is \
                 loopback-only and can't be browsed remotely)."
            ),
        }
    }
}

/// `https://addr:port`, IPv6 literals bracketed.
pub fn base_url(addr: &str, mgmt_port: u16) -> String {
    if addr.contains(':') {
        format!("https://[{addr}]:{mgmt_port}")
    } else {
        format!("https://{addr}:{mgmt_port}")
    }
}

/// An HTTPS agent presenting `identity` via TLS client auth and verifying the server by
/// `pin` (`None` = accept any cert, the TOFU special case — same semantics as the QUIC
/// connect). Reused across a whole grid's worth of poster loads.
pub fn agent(
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<ureq::Agent, LibraryError> {
    use rustls::pki_types::pem::PemObject;
    let bad =
        |what: &str, e: &dyn std::fmt::Display| LibraryError::Unreachable(format!("{what}: {e}"));
    // The ring provider, explicitly — the same one core's QUIC endpoints install, so the
    // process never mixes rustls crypto providers.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("tls config", &e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerify { pin }));
    let cert = rustls::pki_types::CertificateDer::from_pem_slice(identity.0.as_bytes())
        .map_err(|e| bad("client cert pem", &e))?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(identity.1.as_bytes())
        .map_err(|e| bad("client key pem", &e))?;
    let cfg = builder
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| bad("client auth", &e))?;
    Ok(ureq::AgentBuilder::new()
        .tls_config(Arc::new(cfg))
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build())
}

/// Fetch the host's unified library. Errors are pre-classified for the UI (401/403 →
/// [`LibraryError::NotPaired`], a pin-verifier rejection → [`LibraryError::PinMismatch`]).
pub fn fetch_games(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Vec<GameEntry>, LibraryError> {
    let agent = agent(identity, pin)?;
    let url = format!("{}/api/v1/library", base_url(addr, mgmt_port));
    let body = match agent.get(&url).call() {
        Ok(resp) => resp
            .into_string()
            .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?,
        Err(e) => return Err(classify(e)),
    };
    serde_json::from_str(&body).map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// Poster-art byte fetch cap — largest Steam hero assets run a few MB; anything bigger is
/// not an image we want to hand to the texture decoder.
const ART_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Fetch one cover-art image. URLs on the host itself (under `base`) go through the
/// pinned mTLS agent (the host's art proxy requires the paired cert); any other origin —
/// a public CDN URL on a custom entry — uses ureq's default agent with normal webpki
/// trust and no client cert (Apple's `LibraryTLSDelegate` does the same split).
pub fn fetch_art(pinned: &ureq::Agent, base: &str, url: &str) -> Result<Vec<u8>, LibraryError> {
    let resp = if url.starts_with(base) {
        pinned.get(url).call()
    } else {
        ureq::get(url).timeout(Duration::from_secs(10)).call()
    }
    .map_err(classify)?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(ART_MAX_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|e| LibraryError::Unreachable(format!("read image: {e}")))?;
    Ok(bytes)
}

/// Concurrent poster fetches — a handful is plenty for a LAN art proxy without turning a
/// big library into a connection burst.
const ART_WORKERS: usize = 3;

/// Fetch poster bytes for `jobs` (entry id → candidate URLs, walked in order until one
/// loads) on a small worker pool; results stream on the returned channel as they land.
/// Dropping the receiver (the consuming page popped) winds the workers down. Shared by
/// the touch grid and the gamepad launcher — the consumer does its own texture decode on
/// the main loop.
pub fn spawn_art_fetch(
    base: String,
    identity: (String, String),
    pin: Option<[u8; 32]>,
    jobs: VecDeque<(String, Vec<String>)>,
) -> async_channel::Receiver<(String, Vec<u8>)> {
    let queue = Arc::new(Mutex::new(jobs));
    let (tx, rx) = async_channel::unbounded::<(String, Vec<u8>)>();
    for _ in 0..ART_WORKERS {
        let queue = queue.clone();
        let tx = tx.clone();
        let base = base.clone();
        let identity = identity.clone();
        std::thread::Builder::new()
            .name("punktfunk-lib-art".into())
            .spawn(move || {
                let Ok(agent) = agent(&identity, pin) else {
                    return;
                };
                loop {
                    let job = queue.lock().unwrap().pop_front();
                    let Some((id, candidates)) = job else { break };
                    for url in &candidates {
                        match fetch_art(&agent, &base, url) {
                            Ok(bytes) => {
                                // Receiver gone (page popped) — stop fetching.
                                if tx.send_blocking((id, bytes)).is_err() {
                                    return;
                                }
                                break;
                            }
                            // 404 on a guessed CDN path is routine — try the next kind.
                            Err(e) => tracing::debug!(%id, url, error = %e, "poster miss"),
                        }
                    }
                }
            })
            .expect("spawn art thread");
    }
    rx
}

fn classify(e: ureq::Error) -> LibraryError {
    match e {
        ureq::Error::Status(401 | 403, _) => LibraryError::NotPaired,
        ureq::Error::Status(code, _) => LibraryError::Http(code),
        ureq::Error::Transport(t) => {
            // A pin rejection surfaces as a TLS alert wrapped in a transport error; the
            // verifier's error kind survives in the message.
            let msg = t.to_string();
            if msg.contains("ApplicationVerificationFailure") || msg.contains("InvalidCertificate")
            {
                LibraryError::PinMismatch
            } else {
                LibraryError::Unreachable(msg)
            }
        }
    }
}

/// Fingerprint-pinning verifier — the client-HTTP twin of core's (private) QUIC
/// `PinVerify`: trust is the SHA-256 of the host's self-signed leaf cert. The handshake
/// signatures MUST still be verified for real: CertificateVerify is what proves the peer
/// *holds the pinned cert's private key* — skip it and an active MITM can replay the
/// host's (public) certificate, match the pin, and complete the handshake with its own key.
#[derive(Debug)]
struct PinVerify {
    pin: Option<[u8; 32]>,
}

impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if let Some(expected) = self.pin {
            let fp = punktfunk_core::quic::endpoint::cert_fingerprint(end_entity.as_ref());
            if fp != expected {
                return Err(rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ));
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_candidates_order_and_resolution() {
        // Fallback order is portrait → header → hero, host-relative paths resolved.
        let art = Artwork {
            portrait: Some("/api/v1/library/art/steam:570/portrait".into()),
            hero: Some("https://cdn.example/hero.jpg".into()),
            header: Some("/api/v1/library/art/steam:570/header".into()),
        };
        assert_eq!(
            art.poster_candidates("https://192.168.1.42:47990"),
            vec![
                "https://192.168.1.42:47990/api/v1/library/art/steam:570/portrait",
                "https://192.168.1.42:47990/api/v1/library/art/steam:570/header",
                "https://cdn.example/hero.jpg",
            ]
        );
        assert!(Artwork::default()
            .poster_candidates("https://h:47990")
            .is_empty());
    }

    #[test]
    fn game_entry_decodes_the_wire_shape() {
        // The exact shape mgmt.rs serializes (optional art fields omitted, launch ignored).
        let json = r#"[
            {"id":"steam:570","store":"steam","title":"Dota 2",
             "art":{"portrait":"/api/v1/library/art/steam:570/portrait"},
             "launch":{"kind":"steam_appid","value":"570"}},
            {"id":"custom:abc","store":"custom","title":"My Emu","art":{}}
        ]"#;
        let games: Vec<GameEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id, "steam:570");
        assert!(games[1].art.portrait.is_none());
    }

    #[test]
    fn ipv6_base_url_is_bracketed() {
        assert_eq!(base_url("fe80::1", 47990), "https://[fe80::1]:47990");
        assert_eq!(base_url("192.168.1.42", 1234), "https://192.168.1.42:1234");
    }
}
