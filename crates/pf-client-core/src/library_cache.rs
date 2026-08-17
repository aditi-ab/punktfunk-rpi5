//! On-disk cache for a host's library CATALOG — the list of titles, not their art.
//!
//! Cover art has been cached per client for a while; the catalog behind it never was. Every visit
//! to a library refetched `GET /api/v1/library` and showed nothing until that call returned. A host
//! that is asleep, or simply not reachable yet, therefore had an EMPTY library — which is the
//! opposite of what a player wants from the screen they use to decide what to play, and it makes
//! waking a host on library entry pointless: there would be nothing to look at while it boots.
//!
//! So the catalog is cached per host and rendered immediately, marked stale, and reconciled when the
//! host answers. The Rust half of the Apple client's `LibraryCache` (PR #276), shared by every
//! client that fetches through [`crate::library`].
//!
//! Cache directory, not config: every byte is re-derivable from the host, so the system is welcome
//! to evict it. Unlike art, a catalog is small (a few hundred KB for a big library), so there is no
//! size budget here — one file per host, replaced wholesale.
//!
//! The key is the host's **pinned certificate fingerprint**, not its address: a host that moves to a
//! new DHCP lease is the same host with the same library, and keying on `addr:port` would silently
//! lose the cache exactly when a cold-booted box needs it most.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::library::GameEntry;

/// A host's library as last seen, with when that was.
#[derive(Debug, Deserialize, Serialize)]
pub struct CachedLibrary {
    pub games: Vec<GameEntry>,
    /// Unix seconds. A plain integer rather than a `SystemTime` so the file stays readable and a
    /// clock that moved backwards yields a silly age rather than a decode failure.
    pub fetched_at: u64,
}

impl CachedLibrary {
    /// How old this snapshot is. A UI uses it to word the staleness note, never to decide whether
    /// to show the catalog: a year-old library is still a far better answer than an empty one, and
    /// the live fetch is always in flight behind it anyway. `None` when the stamp is in the future
    /// (a clock that has since been corrected), which is not a reason to hide the titles.
    pub fn age(&self) -> Option<Duration> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        now.checked_sub(self.fetched_at).map(Duration::from_secs)
    }
}

/// `~/.cache/punktfunk/library` (`%LOCALAPPDATA%\punktfunk\cache\library` on Windows), honouring
/// `XDG_CACHE_HOME` where the platform defines it.
fn cache_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA").ok()?;
        Some(
            PathBuf::from(local)
                .join("punktfunk")
                .join("cache")
                .join("library"),
        )
    }
    #[cfg(not(windows))]
    {
        let base = std::env::var("XDG_CACHE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".cache"))
            })?;
        Some(base.join("punktfunk").join("library"))
    }
}

/// The file this host's catalog lives in, or `None` for a key that isn't a fingerprint.
///
/// `fp_hex` reaches a path component, so it is re-validated rather than trusted: it must be exactly
/// the 64 lowercase hex characters of a SHA-256 digest. That leaves no way for a `..` or a
/// separator to appear in a filename, and it costs one scan of a short string. A host with no pin
/// (TOFU, never paired) has no key and simply runs without a cache — which is correct anyway,
/// since an unpinned host is not an identity we should be remembering a game list against.
fn path_for(fp_hex: &str) -> Option<PathBuf> {
    let ok = fp_hex.len() == 64
        && fp_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| cache_dir().map(|d| d.join(format!("{fp_hex}.json"))))
        .flatten()
}

/// This host's last-known catalog, or `None` if there is no usable one.
///
/// A catalog written by an older build whose `GameEntry` had different fields decodes to nothing
/// rather than failing: a miss costs one fetch, which is what would have happened anyway. Never
/// surfaced as an error.
pub fn load(fp_hex: &str) -> Option<CachedLibrary> {
    let raw = std::fs::read_to_string(path_for(fp_hex)?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Remember this host's catalog. Best-effort: a cache that can't write is a slower client, not a
/// broken one, so every failure here is swallowed.
pub fn store(fp_hex: &str, games: &[GameEntry]) {
    // An empty catalog is not worth remembering: it is indistinguishable from "never fetched" when
    // read back, and caching it would pin a blank library over a host that has titles.
    if games.is_empty() {
        return;
    }
    let Some(path) = path_for(fp_hex) else { return };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return;
    };
    let snapshot = CachedLibrary {
        games: games.to_vec(),
        fetched_at: now.as_secs(),
    };
    let Ok(json) = serde_json::to_vec(&snapshot) else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Write-then-rename: a client killed mid-write (a Deck going to sleep, a TV pulling power)
    // must not leave a half-file that the next launch reads as a corrupt catalog.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Drop a host's catalog — part of forgetting the host, so a removed host leaves no list of what
/// somebody plays behind on disk.
pub fn forget(fp_hex: &str) {
    if let Some(path) = path_for(fp_hex) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_fingerprint_becomes_a_path() {
        // The whole point of the check: nothing user- or host-supplied can escape the directory
        // or name a file outside it.
        assert!(path_for("../../etc/passwd").is_none());
        assert!(path_for("").is_none());
        assert!(path_for(&"a".repeat(63)).is_none(), "too short");
        assert!(
            path_for(&"A".repeat(64)).is_none(),
            "uppercase is not our hex"
        );
        assert!(path_for(&"g".repeat(64)).is_none(), "not hex at all");
        // A real digest does, provided the platform gives us a cache directory at all.
        if cache_dir().is_some() {
            let p = path_for(&"0123456789abcdef".repeat(4)).expect("64 lowercase hex is a key");
            assert!(p.to_string_lossy().ends_with(".json"));
        }
    }

    #[test]
    fn a_future_stamp_yields_no_age_rather_than_panicking() {
        let ahead = CachedLibrary {
            games: Vec::new(),
            fetched_at: u64::MAX,
        };
        assert!(ahead.age().is_none());
    }
}
