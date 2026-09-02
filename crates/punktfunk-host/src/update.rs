//! Host update check and apply for the console (`design/host-update-from-web-console.md`).
//!
//! Fetches the per-channel signed manifest and verifies it against the Ed25519
//! keys pinned in `pf-update-check`. `GET /update/status` returns a process-wide
//! cache and, when older than [`AUTO_REFRESH_AFTER`], kicks a background refresh —
//! the console already polls, so this module has no timer of its own.
//! `POST /update/check` forces a refresh, rate-limited to one per [`FORCE_MIN_INTERVAL`].
//!
//! Apply takes no version, URL, or channel from the request; those come from the
//! verified cache. Trust and failure rules live in [`manifest`]. The serial floor
//! in `update-state.json` makes a replayed older manifest an error, not a silent
//! downgrade. `PUNKTFUNK_UPDATE_CHECK=0` disables network activity; status then
//! reports `check_disabled`. `PUNKTFUNK_UPDATE_APPLY=0` leaves check intact and
//! 409s apply.

pub(crate) mod detect;
pub(crate) mod jobs;
#[cfg(target_os = "linux")]
mod linux;
// Schema and validation live in `pf-update-check` (same crate the client uses).
pub(crate) use pf_update_check::manifest;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

use manifest::Manifest;
use pf_update_check::{FeedError, PublicKey};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Same Ed25519 signers the client trusts. A host that pinned a different set
/// would accept a feed the client rejects (or the reverse).
pub(crate) use pf_update_check::OFFICIAL_UPDATE_KEYS as UPDATE_KEYS;

/// 6 h: long enough not to hammer the signed feed; status polls kick refresh.
const AUTO_REFRESH_AFTER: Duration = Duration::from_secs(6 * 60 * 60);

/// 30 s: operator mash of `POST /update/check` must not stampede the feed.
pub(crate) const FORCE_MIN_INTERVAL: Duration = Duration::from_secs(30);

/// 45 d: freeze-detection hint in status, not an error. Serial is publish time.
const STALE_AFTER: Duration = Duration::from_secs(45 * 24 * 60 * 60);

pub(crate) fn check_disabled() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_UPDATE_CHECK").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Operator kill switch: apply 409s and status reports `notify` even when a
/// one-click leg exists. Check is unaffected.
pub(crate) fn apply_disabled() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_UPDATE_APPLY").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// `full` (one-click), `staged` (apply then reboot — rpm-ostree), or `notify`
/// (show the command). Linux `full`/`staged` also need the packaged root helper
/// and the operator's group; pacman also the root-owned full-sysupgrade config.
pub(crate) fn apply_support() -> &'static str {
    if apply_disabled() {
        return "notify";
    }
    // Omarchy owns `pacman` (snapper snapshot, then sysupgrade). A one-click
    // apply here would hit their `pacman -Syu` guard or skip that snapshot.
    // Packages ride their transaction once the repo is configured.
    #[cfg(target_os = "linux")]
    if crate::osinfo::is_omarchy() {
        return "notify";
    }
    let (kind, _) = detect::detect();
    match kind {
        detect::InstallKind::WindowsInstaller => "full",
        #[cfg(target_os = "linux")]
        detect::InstallKind::Apt | detect::InstallKind::Dnf | detect::InstallKind::Sysext
            if linux::helper_installed() && linux::opted_in() =>
        {
            "full"
        }
        #[cfg(target_os = "linux")]
        detect::InstallKind::RpmOstree if linux::helper_installed() && linux::opted_in() => {
            "staged"
        }
        #[cfg(target_os = "linux")]
        detect::InstallKind::Pacman
            if linux::helper_installed() && linux::opted_in() && linux::pacman_opted_in() =>
        {
            "full"
        }
        // SteamOS source rebuild is user-owned: no helper, no group.
        #[cfg(target_os = "linux")]
        detect::InstallKind::SteamosSource => "full",
        _ => "notify",
    }
}

/// Status copy when the helper is installed but the operator is not in
/// `punktfunk-update`.
pub(crate) fn opt_in_hint() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // Apply is notify-only on Omarchy; the group would buy no button.
        if crate::osinfo::is_omarchy() {
            return None;
        }
        let (kind, _) = detect::detect();
        let capable = matches!(
            kind,
            detect::InstallKind::Apt
                | detect::InstallKind::Dnf
                | detect::InstallKind::Sysext
                | detect::InstallKind::RpmOstree
                | detect::InstallKind::Pacman
        );
        if capable && !apply_disabled() && linux::helper_installed() && !linux::opted_in() {
            return Some(linux::opt_in_hint());
        }
    }
    None
}

fn pinned_keys() -> Vec<PublicKey> {
    UPDATE_KEYS
        .iter()
        .filter(|k| !k.is_empty())
        .filter_map(|k| PublicKey::parse(k).ok())
        .collect()
}

#[derive(Clone)]
pub(crate) struct Checked {
    pub manifest: Manifest,
    pub fetched_unix: u64,
}

#[derive(Default)]
struct Runtime {
    checked: Option<Checked>,
    last_error: Option<String>,
    /// Empty feed, never seen a manifest. Kept out of `last_error` so status
    /// does not paint "nothing published yet" as a broken host.
    not_published: bool,
    /// At most one background refresh at a time.
    refreshing: bool,
    last_forced: Option<Instant>,
    /// Any attempt (forced or auto); drives [`AUTO_REFRESH_AFTER`].
    last_attempt: Option<Instant>,
    /// Version already emitted as `update.available`; a still-newer cache must
    /// not re-announce every auto-refresh.
    announced: Option<String>,
    job: Option<jobs::JobSnapshot>,
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

/// Highest accepted manifest serial per channel. Persist this or a replayed
/// older manifest becomes a silent downgrade of knowledge.
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

/// Raise (never lower) the floor. Atomic tmp+rename so a power cut cannot
/// half-write it.
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

/// Blocking feed fetch; call from a blocking thread.
fn fetch_manifest_blocking(channel: &str) -> Result<Manifest, FeedError> {
    pf_update_check::feed::fetch_manifest_blocking(
        &pf_update_check::feed::feed_base(),
        channel,
        &pinned_keys(),
        &format!(
            "punktfunk-host/{} (update-check)",
            env!("PUNKTFUNK_VERSION")
        ),
    )
}

pub(crate) fn refresh_blocking() -> Result<Checked, FeedError> {
    let (kind, channel) = detect::detect();
    let result = fetch_manifest_blocking(channel.as_str()).and_then(|m| {
        let path = state_path();
        let floor = load_floor(&path, channel.as_str());
        if m.serial < floor {
            return Err(FeedError::Failed(format!(
                "manifest serial {} is older than the last accepted {} — refusing rollback",
                m.serial, floor
            )));
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
            rt.not_published = false;
            rt.checked = Some(checked.clone());
            Ok(checked)
        }
        Err(e) => {
            let (last_error, not_published) = classify_failure(&e, rt.checked.is_some());
            rt.last_error = last_error;
            rt.not_published = not_published;
            Err(e)
        }
    }
}

/// `(last_error, not_published)` — never both. A 404 is benign only until a
/// manifest has been seen; after that the feed lost a document and must stay
/// a loud error.
fn classify_failure(e: &FeedError, had_manifest: bool) -> (Option<String>, bool) {
    if e.is_not_published() && !had_manifest {
        (None, true)
    } else {
        (Some(e.to_string()), false)
    }
}

/// Cache + errors; kick a background refresh when older than [`AUTO_REFRESH_AFTER`].
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
            not_published: rt.not_published,
            job: rt.job.clone(),
            last_result: jobs::read_result(&jobs::result_path()),
        }
    };
    if kick {
        // Outcome lands in the cache; the console's next poll reads it.
        tokio::task::spawn_blocking(|| {
            let _ = refresh_blocking();
        });
    }
    snap
}

/// Rate-limited `POST /update/check`. Blocks until the refresh finishes.
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
        not_published: rt.not_published,
        job: rt.job.clone(),
        last_result: jobs::read_result(&jobs::result_path()),
    })
}

pub(crate) enum ForceError {
    Disabled,
    TooSoon,
}

/// Refusal mapped to HTTP 409 by the API layer.
pub(crate) enum ApplyError {
    /// No one-click leg; the console shows the command instead.
    Unsupported,
    Disabled,
    /// In-process job, or a spawned installer that has not resolved yet.
    JobRunning,
    /// A stream is live and the request did not pass `force`.
    SessionActive,
    /// No verified newer manifest, or the Windows installer asset is missing.
    NothingToApply,
}

/// Apply from the verified cache only. The request carries no version, URL,
/// or channel.
pub(crate) fn start_apply(force: bool, session_active: bool) -> Result<(), ApplyError> {
    if apply_disabled() {
        return Err(ApplyError::Disabled);
    }
    let (kind, channel) = detect::detect();
    let windows_leg = kind == detect::InstallKind::WindowsInstaller;
    let linux_leg = matches!(
        kind,
        detect::InstallKind::Apt
            | detect::InstallKind::Dnf
            | detect::InstallKind::Sysext
            | detect::InstallKind::RpmOstree
            | detect::InstallKind::Pacman
            | detect::InstallKind::SteamosSource
    );
    if !windows_leg && !linux_leg {
        return Err(ApplyError::Unsupported);
    }
    // Same Omarchy refusal as [`apply_support`], enforced here so a direct POST
    // cannot run `pacman -Syu` into their guard or past the snapper snapshot.
    #[cfg(target_os = "linux")]
    if crate::osinfo::is_omarchy() {
        return Err(ApplyError::Unsupported);
    }
    #[cfg(target_os = "linux")]
    if linux_leg && kind != detect::InstallKind::SteamosSource {
        // SteamOS source rebuild is user-owned; every other Linux leg needs
        // the root helper.
        if !linux::helper_installed() {
            return Err(ApplyError::Unsupported);
        }
        // Helper enforces the full-sysupgrade opt-in too; refuse here so the
        // console never spawns a job that will fail.
        if kind == detect::InstallKind::Pacman && !linux::pacman_opted_in() {
            return Err(ApplyError::Unsupported);
        }
    }
    #[cfg(not(target_os = "linux"))]
    if linux_leg {
        return Err(ApplyError::Unsupported);
    }
    if session_active && !force {
        return Err(ApplyError::SessionActive);
    }

    let (target_version, serial, asset) = {
        let mut rt = runtime().lock().unwrap();
        if rt.job.is_some() {
            return Err(ApplyError::JobRunning);
        }
        // Fresh intent + old version is still an apply in flight. Reconcile
        // owns it; do not start a second job under it.
        if matches!(
            jobs::reconcile(
                jobs::read_intent(&jobs::intent_path()),
                env!("PUNKTFUNK_VERSION"),
                now_unix()
            ),
            jobs::Reconciled::StillApplying
        ) {
            return Err(ApplyError::JobRunning);
        }
        let Some(checked) = rt.checked.as_ref() else {
            return Err(ApplyError::NothingToApply);
        };
        let newer = detect::is_newer(
            &checked.manifest.version,
            checked.manifest.ci_run,
            env!("PUNKTFUNK_VERSION"),
            channel,
        );
        if !newer {
            return Err(ApplyError::NothingToApply);
        }
        // Linux legs resolve artifacts through the package manager.
        let asset = checked.manifest.windows_host.clone();
        if windows_leg && asset.is_none() {
            return Err(ApplyError::NothingToApply);
        }
        let version = checked.manifest.version.clone();
        let serial = checked.manifest.serial;
        rt.job = Some(jobs::JobSnapshot {
            target_version: version.clone(),
            stage: if windows_leg {
                "downloading"
            } else {
                "applying"
            },
            received_bytes: 0,
            total_bytes: None,
            started_unix: now_unix(),
        });
        (version, serial, asset)
    };

    tokio::task::spawn_blocking(move || {
        let stage = |s: &'static str| {
            let mut rt = runtime().lock().unwrap();
            if let Some(job) = rt.job.as_mut() {
                job.stage = s;
            }
        };
        let outcome: Result<PostApply, (&'static str, String)> = {
            #[cfg(target_os = "windows")]
            {
                let progress = |received: u64, total: Option<u64>| {
                    let mut rt = runtime().lock().unwrap();
                    if let Some(job) = rt.job.as_mut() {
                        job.received_bytes = received;
                        job.total_bytes = total;
                    }
                };
                let asset = asset.expect("windows leg reserved with an asset");
                windows::run_apply(&asset, &target_version, serial, &progress, &stage)
                    .map(|()| PostApply::AwaitRestart)
            }
            #[cfg(target_os = "linux")]
            {
                let _ = &asset; // unused: Linux legs use the package manager
                let run = if detect::detect().0 == detect::InstallKind::SteamosSource {
                    linux::run_apply_steamos(&target_version, serial, &stage)
                } else {
                    linux::run_apply(&target_version, serial, &stage)
                };
                run.map(|()| {
                    // Staged / nothing-to-do wrote a durable result; in-place
                    // wrote the intent and queued restart. Either way the
                    // in-process job is finished.
                    PostApply::Done
                })
            }
            #[cfg(not(any(target_os = "windows", target_os = "linux")))]
            {
                let _ = (&asset, &target_version, serial, &stage);
                Err(("applying", "no apply leg for this platform".to_string()))
            }
        };
        match outcome {
            Ok(PostApply::AwaitRestart) => {
                // Leave stage `restarting`. The installer is about to kill
                // this process; boot reconcile writes the durable outcome.
            }
            Ok(PostApply::Done) => {
                runtime().lock().unwrap().job = None;
            }
            Err((stage_name, error)) => {
                let record = jobs::ResultRecord {
                    ok: false,
                    from: env!("PUNKTFUNK_VERSION").into(),
                    to: target_version.clone(),
                    finished_unix: now_unix(),
                    stage: Some(stage_name.into()),
                    error: Some(error),
                    log_path: None,
                    staged: false,
                };
                let _ = jobs::write_json_atomic(&jobs::result_path(), &record);
                runtime().lock().unwrap().job = None;
            }
        }
    });
    Ok(())
}

/// What an apply leg leaves for the spawn wrapper. Each platform constructs
/// only its own variant; the other is matched but never built there.
#[allow(dead_code)]
enum PostApply {
    /// Process is about to die; reconcile owns the outcome.
    AwaitRestart,
    /// Finished in-process (staged / nothing-to-do); clear the job.
    Done,
}

/// Close an intent left by a previous apply. Call once from `mgmt::run`
/// before the API serves.
pub(crate) fn reconcile_at_boot() {
    let path = jobs::intent_path();
    let intent = jobs::read_intent(&path);
    match jobs::reconcile(intent, env!("PUNKTFUNK_VERSION"), now_unix()) {
        jobs::Reconciled::None | jobs::Reconciled::StillApplying => {}
        jobs::Reconciled::Success(record) => {
            tracing::info!(from = %record.from, to = %record.to, "host update applied");
            let _ = jobs::write_json_atomic(&jobs::result_path(), &record);
            let _ = std::fs::remove_file(&path);
            crate::events::emit(crate::events::EventKind::UpdateApplied {
                from: record.from,
                to: record.to,
            });
        }
        jobs::Reconciled::Failed(record) => {
            tracing::warn!(
                from = %record.from,
                to = %record.to,
                error = record.error.as_deref().unwrap_or(""),
                "host update did NOT stick"
            );
            let _ = jobs::write_json_atomic(&jobs::result_path(), &record);
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub(crate) struct Snapshot {
    pub checked: Option<Checked>,
    pub last_error: Option<String>,
    /// No release on this channel yet. Mutually exclusive with `last_error`.
    pub not_published: bool,
    /// Live in-process job. Mid-apply restart leaves this `None` while a
    /// fresh intent is still in flight — see [`Snapshot::applying_from_intent`].
    pub job: Option<jobs::JobSnapshot>,
    pub last_result: Option<jobs::ResultRecord>,
}

impl Snapshot {
    /// Apply in flight with no in-process job: a spawn that has not resolved
    /// (this process may be the old host in its last seconds, or a restart
    /// inside the grace window). The API surfaces it as a `restarting` job.
    pub(crate) fn applying_from_intent(&self) -> Option<jobs::IntentRecord> {
        if self.job.is_some() {
            return None;
        }
        let intent = jobs::read_intent(&jobs::intent_path())?;
        match jobs::reconcile(Some(intent.clone()), env!("PUNKTFUNK_VERSION"), now_unix()) {
            jobs::Reconciled::StillApplying => Some(intent),
            _ => None,
        }
    }

    /// Last check succeeded but the manifest's publish serial is older than
    /// [`STALE_AFTER`] (freeze detection).
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
        store_floor(&path, "stable", 50);
        assert_eq!(load_floor(&path, "stable"), 100);
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
        store_floor(&path, "stable", 5);
        assert_eq!(load_floor(&path, "stable"), 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `last_error` and `not_published` never arrive together. The benign
    /// 404 must not survive a channel that has already served a manifest.
    #[test]
    fn empty_channel_is_benign_only_until_a_manifest_has_been_seen() {
        let (err, not_published) = classify_failure(&FeedError::NotPublished, false);
        assert_eq!(err, None);
        assert!(not_published);

        let (err, not_published) = classify_failure(&FeedError::NotPublished, true);
        assert_eq!(
            err.as_deref(),
            Some("no release has been published on this channel yet")
        );
        assert!(!not_published);

        for had_manifest in [false, true] {
            let (err, not_published) = classify_failure(
                &FeedError::Failed("feed returned HTTP 500".into()),
                had_manifest,
            );
            assert_eq!(err.as_deref(), Some("feed returned HTTP 500"));
            assert!(!not_published);
        }
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
            not_published: false,
            job: None,
            last_result: None,
        };
        assert!(!mk(now_unix()).stale());
        assert!(mk(now_unix() - STALE_AFTER.as_secs() - 10).stale());
    }
}
