//! Catalog **fetch + cache**: pulling a source's signed index over HTTPS and keeping the last good
//! copy on disk (design `plugin-store.md` §4.1).
//!
//! Two properties matter more than freshness here:
//!
//! - **Fail closed on signatures, fail soft on the network.** A source with a pinned key whose
//!   document doesn't verify is an *error* — we keep serving the last good copy and say so, and we
//!   never fall back to "well, unsigned then". But a box that simply can't reach the internet
//!   (LAN-only, common for a streaming host) keeps a working store: the cached shelf still browses,
//!   and installs off it still pin and integrity-check, because the pin travelled with the entry.
//! - **Bounded everything.** Size cap, timeout, redirect cap, https-only, no credentials ever
//!   attached. A source URL is operator config, not request-time input, so there is no lever here
//!   for a browser to aim the host at an arbitrary address.

use super::index::{Index, MAX_INDEX_BYTES};
use super::sources::Source;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Wall-clock budget for one index (or signature) fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// What a refresh attempt produced.
pub(crate) enum Fetched {
    /// A new, verified, parsed document.
    Fresh {
        index: Box<Index>,
        etag: Option<String>,
    },
    /// The source says our cached copy is still current (HTTP 304).
    NotModified,
    /// Nothing usable arrived. The caller keeps whatever it had and marks the source stale.
    Failed(String),
}

/// Fetch, verify and parse a source's index.
///
/// Blocking (`ureq`) — callers run this on a blocking thread, never on the async runtime.
pub(crate) fn fetch(source: &Source, etag: Option<&str>) -> Fetched {
    if !source.url.starts_with("https://") {
        return Fetched::Failed("source url must be https".into());
    }
    let agent = ureq::AgentBuilder::new()
        .timeout(FETCH_TIMEOUT)
        // A signed document doesn't need many hops to reach us; a redirect chain is a good way to
        // waste a host's time.
        .redirects(3)
        .user_agent(&format!("punktfunk-host/{}", super::index::host_version()))
        .build();

    let mut req = agent.get(&source.url);
    if let Some(tag) = etag {
        req = req.set("If-None-Match", tag);
    }
    let resp = match req.call() {
        Ok(r) => r,
        // 304 is the happy "nothing changed" path, not a failure.
        Err(ureq::Error::Status(304, _)) => return Fetched::NotModified,
        Err(ureq::Error::Status(code, _)) => {
            return Fetched::Failed(format!("index fetch returned HTTP {code}"))
        }
        Err(e) => return Fetched::Failed(format!("index fetch failed: {e}")),
    };
    let new_etag = resp.header("etag").map(str::to_string);
    let body = match read_capped(resp) {
        Ok(b) => b,
        Err(e) => return Fetched::Failed(e),
    };

    // Signature FIRST — nothing below may look at a field before this passes (design §6.3).
    let keys = source.keys();
    if !keys.is_empty() {
        let sig = match agent.get(&source.sig_url()).call() {
            Ok(r) => match read_capped(r) {
                Ok(b) => b,
                Err(e) => return Fetched::Failed(format!("signature: {e}")),
            },
            Err(e) => {
                return Fetched::Failed(format!(
                    "this source is signed but its signature could not be fetched: {e}"
                ))
            }
        };
        let sig_text = match String::from_utf8(sig) {
            Ok(s) => s,
            Err(_) => return Fetched::Failed("signature file is not text".into()),
        };
        if let Err(e) = super::index::verify_signature(&body, &sig_text, &keys) {
            return Fetched::Failed(format!("signature check failed: {e}"));
        }
    }

    match Index::parse(&body) {
        Ok(index) => Fetched::Fresh {
            index: Box::new(index),
            etag: new_etag,
        },
        Err(e) => Fetched::Failed(format!("{e:#}")),
    }
}

/// Read a response body, refusing anything past the cap without buffering it.
fn read_capped(resp: ureq::Response) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take((MAX_INDEX_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("reading the response body failed: {e}"))?;
    if buf.len() > MAX_INDEX_BYTES {
        return Err(format!("response exceeds the {MAX_INDEX_BYTES}-byte cap"));
    }
    Ok(buf)
}

// ---------------------------------------------------------------- disk cache

/// `<config_dir>/store-cache` — the last good copy of every source's index, so a host that boots
/// without a network still has a browsable store.
pub(crate) fn cache_dir() -> PathBuf {
    pf_paths::config_dir().join("store-cache")
}

fn body_path(dir: &Path, source: &str) -> PathBuf {
    dir.join(format!("{source}.json"))
}

fn meta_path(dir: &Path, source: &str) -> PathBuf {
    dir.join(format!("{source}.meta.json"))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct CacheMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Unix seconds of the fetch that produced the cached body.
    #[serde(default)]
    pub fetched_at: u64,
}

/// The cached index for a source, if one is on disk and still parses. Re-validated on read: a
/// cache file is just as untrusted as the wire (it lives in a directory an admin can write).
///
/// NOTE the signature is **not** re-checked here — it was checked when the bytes were accepted,
/// and the cache directory is inside the host's own private config tree.
pub(crate) fn read_cache(dir: &Path, source: &str) -> Option<(Index, CacheMeta)> {
    let body = std::fs::read(body_path(dir, source)).ok()?;
    let index = Index::parse(&body).ok()?;
    let meta = std::fs::read(meta_path(dir, source))
        .ok()
        .and_then(|b| serde_json::from_slice::<CacheMeta>(&b).ok())
        .unwrap_or_default();
    Some((index, meta))
}

/// Persist a freshly verified index as the new last-good copy.
pub(crate) fn write_cache(dir: &Path, source: &str, index: &Index, meta: &CacheMeta) {
    if let Err(e) = pf_paths::create_private_dir(dir) {
        tracing::warn!("could not create the store cache dir: {e}");
        return;
    }
    let write = |path: PathBuf, bytes: Vec<u8>| {
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    };
    match serde_json::to_vec_pretty(index) {
        Ok(b) => write(body_path(dir, source), b),
        Err(e) => tracing::warn!("could not serialize the catalog cache: {e}"),
    }
    if let Ok(b) = serde_json::to_vec_pretty(meta) {
        write(meta_path(dir, source), b);
    }
}

/// Drop a removed source's cache files so a later re-add can't be served a stale shelf.
pub(crate) fn drop_cache(dir: &Path, source: &str) {
    let _ = std::fs::remove_file(body_path(dir, source));
    let _ = std::fs::remove_file(meta_path(dir, source));
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> Index {
        Index::parse(
            br#"{"schema":1,"name":"t","plugins":[{"id":"a","pkg":"@p/plugin-a",
                "registry":"https://r.example/","title":"A","version":"1.0.0",
                "integrity":"sha512-AAAA"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let idx = sample_index();
        let meta = CacheMeta {
            etag: Some("\"abc\"".into()),
            fetched_at: 1_700_000_000,
        };
        write_cache(dir.path(), "unom", &idx, &meta);

        let (back, back_meta) = read_cache(dir.path(), "unom").expect("cache should be readable");
        assert_eq!(back.plugins.len(), 1);
        assert_eq!(back.plugins[0].id, "a");
        assert_eq!(back_meta.etag.as_deref(), Some("\"abc\""));
        assert_eq!(back_meta.fetched_at, 1_700_000_000);
    }

    #[test]
    fn missing_or_corrupt_cache_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_cache(dir.path(), "nope").is_none());
        std::fs::write(dir.path().join("bad.json"), b"{ not json").unwrap();
        assert!(read_cache(dir.path(), "bad").is_none());
    }

    #[test]
    fn cache_is_revalidated_on_read() {
        // A tampered cache file (schema bumped by hand) must not be served.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.json"), br#"{"schema":99,"plugins":[]}"#).unwrap();
        assert!(read_cache(dir.path(), "x").is_none());
    }

    #[test]
    fn drop_cache_removes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        write_cache(dir.path(), "s", &sample_index(), &CacheMeta::default());
        assert!(read_cache(dir.path(), "s").is_some());
        drop_cache(dir.path(), "s");
        assert!(read_cache(dir.path(), "s").is_none());
        assert!(!meta_path(dir.path(), "s").exists());
    }

    #[test]
    fn non_https_source_never_reaches_the_network() {
        let s = Source {
            name: "x".into(),
            url: "http://example.org/i.json".into(),
            public_key: None,
        };
        assert!(matches!(fetch(&s, None), Fetched::Failed(_)));
    }
}
