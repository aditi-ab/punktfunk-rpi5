//! Fetching the per-channel manifest + its detached signature.
//!
//! Blocking (`ureq`) on purpose: both consumers call it off a background thread, and a sync
//! client with bundled webpki roots avoids depending on a system cert store — which the Deck
//! is exactly the wrong box to depend on.
//!
//! The signature is verified over the FINAL response bytes, after redirects. Our registry
//! answers a file GET with a 303 to object storage; a check that verified the pre-redirect
//! body would be verifying a redirect stub (the sysext-feed lesson, `publish-sysext-feed.sh`).

use crate::manifest::{self, Manifest, MAX_MANIFEST_BYTES};
use crate::sig::PublicKey;
use std::time::Duration;

/// Feed base — `<base>/<channel>/manifest.json` + `.sig`.
pub const DEFAULT_FEED_BASE: &str =
    "https://git.unom.io/api/packages/unom/generic/punktfunk-update";

/// One fetch's wall-clock budget.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The feed base, with a `PUNKTFUNK_UPDATE_FEED` override for tests and dev feeds. This is
/// operator config (an env var on the process), never request-time input; the `https://` (or
/// loopback) requirement keeps a stray value from silently downgrading the transport.
pub fn feed_base() -> String {
    std::env::var("PUNKTFUNK_UPDATE_FEED")
        .ok()
        .filter(|s| s.starts_with("https://") || s.starts_with("http://127.0.0.1"))
        .unwrap_or_else(|| DEFAULT_FEED_BASE.to_string())
}

/// Fetch + verify the channel manifest. `keys` are the caller's pinned Ed25519 keys; an empty
/// list is refused rather than treated as "skip verification".
pub fn fetch_manifest_blocking(
    base: &str,
    channel: &str,
    keys: &[PublicKey],
    user_agent: &str,
) -> Result<Manifest, String> {
    if keys.is_empty() {
        return Err("no update key is pinned in this build".into());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(FETCH_TIMEOUT)
        .redirects(3)
        .user_agent(user_agent)
        .build();
    let url = format!("{base}/{channel}/manifest.json");
    let sig_url = format!("{url}.sig");

    let body = read_capped(agent.get(&url).call().map_err(fetch_err)?)?;
    let sig = read_capped(agent.get(&sig_url).call().map_err(fetch_err)?)?;
    let sig_text = String::from_utf8(sig).map_err(|_| "signature file is not text".to_string())?;

    manifest::verify_and_parse(&body, &sig_text, keys, channel).map_err(|e| format!("{e:#}"))
}

fn fetch_err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("feed returned HTTP {code}"),
        other => format!("feed fetch failed: {other}"),
    }
}

fn read_capped(resp: ureq::Response) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    let mut reader = resp.into_reader().take(MAX_MANIFEST_BYTES as u64 + 1);
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    if buf.len() > MAX_MANIFEST_BYTES {
        return Err("response exceeds the manifest size cap".into());
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_keys_never_fetches() {
        // Fail-closed before any network call: an empty pin list is a broken build, not a
        // licence to trust whatever the feed serves.
        let err =
            fetch_manifest_blocking("https://127.0.0.1:1", "stable", &[], "test").unwrap_err();
        assert!(err.contains("no update key"), "{err}");
    }

    #[test]
    fn feed_override_must_be_https_or_loopback() {
        // Not a full env test (env is process-global and tests run in parallel) — just the
        // predicate the override is filtered by.
        let ok = |s: &str| s.starts_with("https://") || s.starts_with("http://127.0.0.1");
        assert!(ok("https://example.test/feed"));
        assert!(ok("http://127.0.0.1:8080/feed"));
        assert!(!ok("http://example.test/feed"));
        assert!(!ok("file:///tmp/feed"));
    }
}
