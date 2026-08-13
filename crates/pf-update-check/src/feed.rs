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

/// Why a fetch didn't produce a manifest.
///
/// Exactly one failure mode is an *expected* steady state: a channel nobody has published to
/// answers `manifest.json` with a 404. Collapsing that into the same string as a transport or
/// signature failure is what made an empty stable feed render as "last check failed: feed
/// returned HTTP 404" — telling operators their box is broken when the feed is merely empty.
/// Everything else stays a real failure, loudly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// This channel has no manifest at all. Note the deliberate narrowness: only a 404 on
    /// `manifest.json` itself counts. A 404 on the *signature* means the manifest exists
    /// without its proof — a half-published pair (the publisher's manifest-then-signature
    /// window, or a botched upload), which must stay fail-closed and noisy.
    NotPublished,
    /// A real failure: transport, an HTTP status that isn't the empty-channel 404, the size
    /// cap, or signature/schema rejection.
    Failed(String),
}

impl FeedError {
    /// Is this the benign "nothing published on this channel yet" state?
    pub fn is_not_published(&self) -> bool {
        matches!(self, Self::NotPublished)
    }
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublished => f.write_str("no release has been published on this channel yet"),
            Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for FeedError {}

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
) -> Result<Manifest, FeedError> {
    if keys.is_empty() {
        return Err(FeedError::Failed(
            "no update key is pinned in this build".into(),
        ));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(3)
        .user_agent(user_agent.to_string())
        .build()
        .into();
    let url = format!("{base}/{channel}/manifest.json");
    let sig_url = format!("{url}.sig");

    // Only the MANIFEST leg can report an empty channel; see [`FeedError::NotPublished`].
    let body = read_capped(&mut agent.get(&url).call().map_err(manifest_err)?)?;
    let sig = read_capped(&mut agent.get(&sig_url).call().map_err(fetch_err)?)?;
    let sig_text = String::from_utf8(sig)
        .map_err(|_| FeedError::Failed("signature file is not text".into()))?;

    manifest::verify_and_parse(&body, &sig_text, keys, channel)
        .map_err(|e| FeedError::Failed(format!("{e:#}")))
}

/// The manifest leg: a 404 here means the channel is empty, not broken.
fn manifest_err(e: ureq::Error) -> FeedError {
    match e {
        ureq::Error::StatusCode(404) => FeedError::NotPublished,
        other => fetch_err(other),
    }
}

fn fetch_err(e: ureq::Error) -> FeedError {
    FeedError::Failed(match e {
        ureq::Error::StatusCode(code) => format!("feed returned HTTP {code}"),
        other => format!("feed fetch failed: {other}"),
    })
}

fn read_capped(resp: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, FeedError> {
    // cap+1 so an over-cap body is rejected by the length check rather than silently truncated
    // into something that would then fail signature verification for the wrong reason.
    let buf = resp
        .body_mut()
        .with_config()
        .limit(MAX_MANIFEST_BYTES as u64 + 1)
        .read_to_vec()
        .map_err(|e| FeedError::Failed(format!("read failed: {e}")))?;
    if buf.len() > MAX_MANIFEST_BYTES {
        return Err(FeedError::Failed(
            "response exceeds the manifest size cap".into(),
        ));
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
        assert!(err.to_string().contains("no update key"), "{err}");
        // And it is a real failure — never the benign empty-channel state.
        assert!(!err.is_not_published());
    }

    fn status(code: u16) -> ureq::Error {
        ureq::Error::StatusCode(code)
    }

    /// The whole point of the split: an empty channel is not a broken feed.
    #[test]
    fn manifest_404_is_not_published_but_other_statuses_are_failures() {
        assert_eq!(manifest_err(status(404)), FeedError::NotPublished);
        for code in [403, 500, 502] {
            let e = manifest_err(status(code));
            assert!(!e.is_not_published(), "HTTP {code} must stay a failure");
            assert!(e.to_string().contains(&code.to_string()), "{e}");
        }
    }

    /// A missing SIGNATURE is a half-published pair, not an empty channel — the manifest leg
    /// already answered 200. Treating it as "nothing published yet" would quietly excuse the
    /// one window where a manifest exists without its proof.
    #[test]
    fn signature_404_stays_a_failure() {
        let e = fetch_err(status(404));
        assert!(!e.is_not_published());
        assert_eq!(e.to_string(), "feed returned HTTP 404");
    }

    #[test]
    fn not_published_reads_as_plain_english() {
        assert_eq!(
            FeedError::NotPublished.to_string(),
            "no release has been published on this channel yet"
        );
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
