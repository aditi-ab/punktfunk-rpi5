//! Game-library client for the host's management REST API (the Apple `LibraryClient`
//! ported): `GET https://<host>:<mgmt>/api/v1/library` plus the per-title art proxy.
//! Authentication is **mTLS** — this client presents its persistent identity (the same
//! cert the host paired over QUIC) and the host authorizes paired certificates for the
//! read-only library routes, no bearer token. The host's self-signed certificate is
//! verified by its pinned SHA-256 fingerprint (`KnownHost::fp_hex`), not a CA chain.

use serde::Deserialize;
use std::collections::VecDeque;
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

    /// Whether this entry has no poster art at all — the condition a launcher's brand mark stands
    /// in for. Separate from `poster_candidates` so a caller asking the question doesn't have to
    /// invent a `base` it has no use for.
    pub fn is_empty(&self) -> bool {
        self.portrait.is_none() && self.header.is_none() && self.hero.is_none()
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
    /// The system the title runs on (`"PC"`, `"PS2"`, …) — free-form display string from the
    /// host's flattened `GameMeta`; the rest of the metadata is not decoded until a UI needs it.
    #[serde(default)]
    pub platform: Option<String>,
    /// `"game"` (the default, and what an older host omits) or `"launcher"` — an entry that opens
    /// the launcher itself (Steam Big Picture, Heroic) rather than a title. A UI may group these
    /// separately; one that doesn't renders them as ordinary tiles, which is the intended
    /// degradation (design D4). Kept a plain string: the host owns the vocabulary, and an unknown
    /// future value must never fail the whole library decode.
    #[serde(default)]
    pub role: Option<String>,
    /// Which brand mark to draw for this entry — `"steam"`, `"heroic"`, `"playnite"` — or `None`
    /// when the host sent none (every older host, and every ordinary title).
    ///
    /// A **token**, not art: the shell resolves it against the marks it ships
    /// (`assets/launcher-icons`) and falls back to naming the launcher for one it doesn't have, so
    /// a plugin can name a mark this client has never heard of without breaking the tile. The host
    /// guarantees the slug shape (`[a-z][a-z0-9-]{0,31}`), which is what makes it safe to
    /// interpolate into a resource name or an asset lookup — but see [`GameEntry::icon_token`],
    /// which re-checks rather than trusting it.
    #[serde(default)]
    pub icon: Option<String>,
}

impl GameEntry {
    /// Whether this entry opens a launcher rather than a game.
    pub fn is_launcher(&self) -> bool {
        self.role.as_deref() == Some("launcher")
    }

    /// The brand-icon token, re-validated here rather than taken on trust.
    ///
    /// The host validates the shape on the way in, so this can only fire for a host that is older
    /// than that check, compromised, or simply not ours. Every caller interpolates the result into
    /// a resource name (`pf-launcher-{t}-symbolic`), an asset-catalog lookup or a file path, and
    /// "the peer promised" is not the standard those deserve — a client re-checks what it is about
    /// to concatenate. Cheap enough to do at the call site.
    pub fn icon_token(&self) -> Option<&str> {
        let t = self.icon.as_deref()?;
        let ok = !t.is_empty()
            && t.len() <= 32
            && t.starts_with(|c: char| c.is_ascii_lowercase())
            && t.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        ok.then_some(t)
    }
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
    // The aws-lc-rs provider, explicitly — the same one core's QUIC endpoints install, so the
    // process never mixes rustls crypto providers.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("tls config", &e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(punktfunk_core::tls::PinVerify::new(pin)));
    let cert = rustls::pki_types::CertificateDer::from_pem_slice(identity.0.as_bytes())
        .map_err(|e| bad("client cert pem", &e))?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(identity.1.as_bytes())
        .map_err(|e| bad("client key pem", &e))?;
    let cfg = builder
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| bad("client auth", &e))?;
    // ureq's own `TlsConfig` has no hook for a custom verifier, so the agent is built around this
    // `ClientConfig` verbatim (punktfunk-core owns that glue — see `tls::ureq_agent`).
    Ok(punktfunk_core::tls::ureq_agent::agent(
        Arc::new(cfg),
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    ))
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
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
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
    let mut resp = if url.starts_with(base) {
        pinned.get(url).call()
    } else {
        // ureq's default agent builds its own rustls config from the process-default provider.
        // Installed here rather than trusting the binary, since several link this crate.
        punktfunk_core::tls::install_default_provider();
        ureq::get(url)
            .config()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .call()
    }
    .map_err(classify)?;
    // `limit` replaces the old `take()` — ureq 3 caps body reads itself, and its default cap is
    // lower than the largest legitimate hero asset.
    resp.body_mut()
        .with_config()
        .limit(ART_MAX_BYTES)
        .read_to_vec()
        .map_err(|e| LibraryError::Unreachable(format!("read image: {e}")))
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

pub(crate) fn classify(e: ureq::Error) -> LibraryError {
    match e {
        ureq::Error::StatusCode(401 | 403) => LibraryError::NotPaired,
        ureq::Error::StatusCode(code) => LibraryError::Http(code),
        // Exactly the rejection `PinVerify` raises on a fingerprint mismatch. ureq 3 carries the
        // typed `rustls::Error`, so this is a real match instead of the substring sniff the 2.x
        // `Transport(t)` string forced — which would also have fired on unrelated cert errors.
        ureq::Error::Rustls(rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        )) => LibraryError::PinMismatch,
        other => LibraryError::Unreachable(other.to_string()),
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
            {"id":"steam:570","store":"steam","title":"Dota 2","platform":"PC",
             "art":{"portrait":"/api/v1/library/art/steam:570/portrait"},
             "launch":{"kind":"steam_appid","value":"570"}},
            {"id":"custom:abc","store":"custom","title":"My Emu","art":{}}
        ]"#;
        let games: Vec<GameEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id, "steam:570");
        assert_eq!(games[0].platform.as_deref(), Some("PC"));
        assert!(games[1].art.portrait.is_none());
        assert!(
            games[1].platform.is_none(),
            "pre-metadata hosts still parse"
        );
    }

    #[test]
    fn ipv6_base_url_is_bracketed() {
        assert_eq!(base_url("fe80::1", 47990), "https://[fe80::1]:47990");
        assert_eq!(base_url("192.168.1.42", 1234), "https://192.168.1.42:1234");
    }
}
