//! Host **update check** (design `host-update-from-web-console.md`, phase U0).
//!
//! This module answers one question for the console: *does a newer host release exist for
//! this box's channel* — by fetching the per-channel signed manifest and verifying it against
//! the Ed25519 keys pinned below. It deliberately contains **no apply code**: U0 ships check
//! everywhere; apply legs land per-channel (U1 Windows, U2 Linux helper) behind the same
//! status surface.
//!
//! Shape: a process-wide cache + a lazy refresh. `GET /update/status` returns the cache and,
//! when it is older than [`AUTO_REFRESH_AFTER`], kicks a background refresh — the console
//! polls status anyway, so freshness needs no timer of its own. `POST /update/check` forces a
//! refresh, rate-limited to one per [`FORCE_MIN_INTERVAL`].
//!
//! Trust and failure rules live in [`manifest`]; the serial floor persisted here
//! (`update-state.json`) is what makes a replayed older manifest an *error*, not a silent
//! downgrade of our knowledge. `PUNKTFUNK_UPDATE_CHECK=0` disables all network activity —
//! status then reports `check_disabled` and carries whatever identity facts need no network.

pub(crate) mod detect;
pub(crate) mod manifest;

use crate::store::index::PublicKey;
use manifest::Manifest;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The Ed25519 public keys this binary trusts for update manifests — two slots so a key
/// rotation is "sign with the new one, ship a host trusting both, retire the old" (the
/// plugin-store `OFFICIAL_KEYS` drill). The private half is the `UPDATE_MANIFEST_KEY` CI
/// secret; it also lives in the operator's offline backup (plan U0.1 DoD).
pub(crate) const UPDATE_KEYS: [&str; 2] = [
    "ed25519:6rmlLg1aQ55cgB6icpC5BEpbMJxwPKdGaDQtDcJ0yLI=",
    "", // rotation slot
];

/// Feed base — `<base>/<channel>/manifest.json` + `.sig`. Override for tests/dev feeds via
/// `PUNKTFUNK_UPDATE_FEED` (a base URL, not request-time input: env is operator config).
const DEFAULT_FEED_BASE: &str = "https://git.unom.io/api/packages/unom/generic/punktfunk-update";

/// A cache older than this is refreshed in the background on the next status read.
const AUTO_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// Forced checks (`POST /update/check`) are rate-limited to one per this interval.
pub(crate) const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// A manifest whose publish serial is older than this is flagged stale in status — the
/// freeze-detection hint (design §3.2), not an error.
const STALE_AFTER: Duration = Duration::from_secs(45 * 24 * 60 * 60);

/// One fetch's wall-clock budget (mirrors the store catalog fetch).
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Update checks disabled by operator config (env or `host.env`).
pub(crate) fn check_disabled() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_UPDATE_CHECK").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

fn feed_base() -> String {
    std::env::var("PUNKTFUNK_UPDATE_FEED")
        .ok()
        .filter(|s| s.starts_with("https://") || s.starts_with("http://127.0.0.1"))
        .unwrap_or_else(|| DEFAULT_FEED_BASE.to_string())
}

fn pinned_keys() -> Vec<PublicKey> {
    UPDATE_KEYS
        .iter()
        .filter(|k| !k.is_empty())
        .filter_map(|k| PublicKey::parse(k).ok())
        .collect()
}

// ---------------------------------------------------------------- runtime state

/// What the last successful refresh produced.
#[derive(Clone)]
pub(crate) struct Checked {
    pub manifest: Manifest,
    pub fetched_unix: u64,
}

#[derive(Default)]
struct Runtime {
    checked: Option<Checked>,
    last_error: Option<String>,
    /// Refresh in flight (status kicks at most one).
    refreshing: bool,
    /// Wall-clock guard for the forced-check rate limit.
    last_forced: Option<Instant>,
    /// Last attempt of any kind — drives the auto-refresh cadence.
    last_attempt: Option<Instant>,
    /// The manifest version an `update.available` event was already emitted for, so a
    /// steady-state "newer exists" doesn't re-announce every 6 h.
    announced: Option<String>,
}

fn runtime() -> &'static Mutex<Runtime> {
    static RT: OnceLock<Mutex<Runtime>> = OnceLock::new();
    RT.get_or_init(|| Mutex::new(Runtime::default()))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- serial floor

/// Persisted anti-rollback state: the highest manifest serial ever accepted per channel.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FloorFile {
    #[serde(default)]
    serial_floor: std::collections::BTreeMap<String, u64>,
}

fn state_path() -> PathBuf {
    pf_paths::config_dir().join("update-state.json")
}

fn load_floor(path: &Path, channel: &str) -> u64 {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<FloorFile>(&b).ok())
        .and_then(|f| f.serial_floor.get(channel).copied())
        .unwrap_or(0)
}

/// Raise (never lower) the floor; atomic tmp+rename so a power cut can't half-write it.
fn store_floor(path: &Path, channel: &str, serial: u64) {
    let mut file: FloorFile = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let slot = file.serial_floor.entry(channel.to_string()).or_insert(0);
    if serial <= *slot {
        return;
    }
    *slot = serial;
    let Ok(bytes) = serde_json::to_vec_pretty(&file) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

// ---------------------------------------------------------------- refresh

/// Fetch + verify the channel manifest. Blocking (`ureq`) — call from a blocking thread.
fn fetch_manifest_blocking(channel: &str) -> Result<Manifest, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(FETCH_TIMEOUT)
        // Follow the registry's 303-to-object-storage redirect; the signature is verified
        // over the FINAL bytes (the sysext-feed lesson).
        .redirects(3)
        .user_agent(&format!(
            "punktfunk-host/{} (update-check)",
            env!("PUNKTFUNK_VERSION")
        ))
        .build();
    let base = feed_base();
    let url = format!("{base}/{channel}/manifest.json");
    let sig_url = format!("{url}.sig");

    let body = read_capped(agent.get(&url).call().map_err(fetch_err)?)?;
    let sig = read_capped(agent.get(&sig_url).call().map_err(fetch_err)?)?;
    let sig_text = String::from_utf8(sig).map_err(|_| "signature file is not text".to_string())?;

    let keys = pinned_keys();
    if keys.is_empty() {
        // Both slots empty would mean a build with the feature disarmed; refuse rather than
        // silently skipping verification.
        return Err("no update key is pinned in this build".into());
    }
    manifest::verify_and_parse(&body, &sig_text, &keys, channel).map_err(|e| format!("{e:#}"))
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
    let mut reader = resp
        .into_reader()
        .take(manifest::MAX_MANIFEST_BYTES as u64 + 1);
    reader
        .read_to_end(&mut buf)
        .map_err(|e| format!("read failed: {e}"))?;
    if buf.len() > manifest::MAX_MANIFEST_BYTES {
        return Err("response exceeds the manifest size cap".into());
    }
    Ok(buf)
}

/// One full refresh: fetch, verify, enforce + raise the serial floor, update the cache,
/// announce a newly available release on the event bus. Returns the user-facing error string
/// on failure (also cached for status).
pub(crate) fn refresh_blocking() -> Result<Checked, String> {
    let (kind, channel) = detect::detect();
    let result = fetch_manifest_blocking(channel.as_str()).and_then(|m| {
        let path = state_path();
        let floor = load_floor(&path, channel.as_str());
        if m.serial < floor {
            return Err(format!(
                "manifest serial {} is older than the last accepted {} — refusing rollback",
                m.serial, floor
            ));
        }
        store_floor(&path, channel.as_str(), m.serial);
        Ok(m)
    });

    let mut rt = runtime().lock().unwrap();
    rt.last_attempt = Some(Instant::now());
    rt.refreshing = false;
    match result {
        Ok(m) => {
            let checked = Checked {
                manifest: m,
                fetched_unix: now_unix(),
            };
            let newer = detect::is_newer(
                &checked.manifest.version,
                checked.manifest.ci_run,
                env!("PUNKTFUNK_VERSION"),
                channel,
            );
            if newer && rt.announced.as_deref() != Some(checked.manifest.version.as_str()) {
                rt.announced = Some(checked.manifest.version.clone());
                crate::events::emit(crate::events::EventKind::UpdateAvailable {
                    version: checked.manifest.version.clone(),
                    channel: channel.as_str().to_string(),
                    install_kind: kind.as_str().to_string(),
                });
            }
            rt.last_error = None;
            rt.checked = Some(checked.clone());
            Ok(checked)
        }
        Err(e) => {
            rt.last_error = Some(e.clone());
            Err(e)
        }
    }
}

/// The status handler's read: current cache + errors, kicking a background refresh when the
/// cache is cold and checks are enabled.
pub(crate) fn snapshot_and_maybe_refresh() -> Snapshot {
    let mut kick = false;
    let snap = {
        let mut rt = runtime().lock().unwrap();
        let cold = rt
            .last_attempt
            .map(|t| t.elapsed() >= AUTO_REFRESH_AFTER)
            .unwrap_or(true);
        if cold && !rt.refreshing && !check_disabled() {
            rt.refreshing = true;
            rt.last_attempt = Some(Instant::now());
            kick = true;
        }
        Snapshot {
            checked: rt.checked.clone(),
            last_error: rt.last_error.clone(),
        }
    };
    if kick {
        // Fire-and-forget; the console's next poll reads the outcome.
        tokio::task::spawn_blocking(|| {
            let _ = refresh_blocking();
        });
    }
    snap
}

/// A forced check (`POST /update/check`): rate-limited, blocking until the refresh finishes.
pub(crate) async fn force_check() -> Result<Snapshot, ForceError> {
    if check_disabled() {
        return Err(ForceError::Disabled);
    }
    {
        let mut rt = runtime().lock().unwrap();
        if let Some(t) = rt.last_forced {
            if t.elapsed() < FORCE_MIN_INTERVAL {
                return Err(ForceError::TooSoon);
            }
        }
        rt.last_forced = Some(Instant::now());
        rt.refreshing = true;
    }
    let _ = tokio::task::spawn_blocking(refresh_blocking).await;
    let rt = runtime().lock().unwrap();
    Ok(Snapshot {
        checked: rt.checked.clone(),
        last_error: rt.last_error.clone(),
    })
}

pub(crate) enum ForceError {
    Disabled,
    TooSoon,
}

/// What status hands to the API layer.
pub(crate) struct Snapshot {
    pub checked: Option<Checked>,
    pub last_error: Option<String>,
}

impl Snapshot {
    /// The stale-feed hint: last successful check is fine but the manifest itself was
    /// published suspiciously long ago (freeze detection, design §3.2).
    pub(crate) fn stale(&self) -> bool {
        self.checked
            .as_ref()
            .map(|c| now_unix().saturating_sub(c.manifest.serial) > STALE_AFTER.as_secs())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_roundtrip_and_monotonicity() {
        let dir = std::env::temp_dir().join(format!("pf-update-floor-{}", std::process::id()));
        let path = dir.join("update-state.json");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(load_floor(&path, "stable"), 0);
        store_floor(&path, "stable", 100);
        assert_eq!(load_floor(&path, "stable"), 100);
        // Lowering is a no-op.
        store_floor(&path, "stable", 50);
        assert_eq!(load_floor(&path, "stable"), 100);
        // Channels are independent.
        store_floor(&path, "canary", 7);
        assert_eq!(load_floor(&path, "canary"), 7);
        assert_eq!(load_floor(&path, "stable"), 100);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_floor_file_reads_as_zero() {
        let dir = std::env::temp_dir().join(format!("pf-update-floor2-{}", std::process::id()));
        let path = dir.join("update-state.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load_floor(&path, "stable"), 0);
        // And writing over it recovers.
        store_floor(&path, "stable", 5);
        assert_eq!(load_floor(&path, "stable"), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pinned_keys_skip_empty_rotation_slot() {
        let keys = pinned_keys();
        assert_eq!(keys.len(), 1, "one live key, one empty rotation slot");
    }

    #[test]
    fn stale_math() {
        let mk = |serial| Snapshot {
            checked: Some(Checked {
                manifest: manifest::parse_verified(
                    serde_json::to_vec(&serde_json::json!({
                        "schema": 1, "channel": "stable", "serial": serial,
                        "version": "0.23.0",
                        "notes_url": "https://git.unom.io/unom/punktfunk/releases",
                    }))
                    .unwrap()
                    .as_slice(),
                    "stable",
                )
                .unwrap(),
                fetched_unix: now_unix(),
            }),
            last_error: None,
        };
        assert!(!mk(now_unix()).stale());
        assert!(mk(now_unix() - STALE_AFTER.as_secs() - 10).stale());
    }
}
