//! Apply-job bookkeeping: the in-memory job snapshot the console polls, and the two on-disk
//! records that let an update **report its own outcome across its own restart** (design §4.2,
//! plan U1.1):
//!
//! - the **intent record** (`update-intent.json`) — written just before anything irreversible
//!   (spawning the installer). It is the only witness once the installer kills this process.
//! - the **result record** (`update-result.json`) — the durable outcome of the *last* apply,
//!   written either by the failing stage itself or by boot-time reconciliation.
//!
//! Reconciliation ([`reconcile`]) runs once per boot: intent + running-the-target-version ⇒
//! success; intent + still the old version after the grace window ⇒ failure with the installer
//! log path attached; a fresh intent ⇒ the apply is still in flight (the installer may not have
//! stopped us yet). Pure over its inputs so every branch is table-testable.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How long after intent-write we still call the apply "in flight" when the running version is
/// unchanged. Beyond it, the host restarting *without* the new version is a failed apply
/// (installer aborted, rolled back, or never ran) — surfaced, never silent.
pub(crate) const APPLY_GRACE_SECS: u64 = 10 * 60;

/// Written immediately before the point of no return.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct IntentRecord {
    /// The version that started the apply (us, at write time).
    pub from: String,
    /// The version being installed.
    pub to: String,
    /// The manifest serial that announced it.
    pub serial: u64,
    /// Unix seconds at write.
    pub started_unix: u64,
    /// SHA-256 of the verified installer (diagnostics — ties a result to exact bytes).
    pub installer_sha256: String,
    /// Where the installer was told to log (`/LOG=`).
    pub log_path: String,
}

/// The durable outcome of the most recent apply attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResultRecord {
    pub ok: bool,
    pub from: String,
    pub to: String,
    pub finished_unix: u64,
    /// The stage that failed (`downloading` | `verifying` | `applying` | `restarting`);
    /// absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The installer log, when one was in play by the time it failed (or succeeded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// The update is applied but activates on the next reboot (rpm-ostree).
    #[serde(default)]
    pub staged: bool,
}

/// The live job the console polls, mirrored into `GET /update/status`.
#[derive(Debug, Clone)]
pub(crate) struct JobSnapshot {
    pub target_version: String,
    /// `downloading` | `verifying` | `applying` | `restarting`.
    pub stage: &'static str,
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
    pub started_unix: u64,
}

pub(crate) fn intent_path() -> PathBuf {
    pf_paths::config_dir().join("update-intent.json")
}

pub(crate) fn result_path() -> PathBuf {
    pf_paths::config_dir().join("update-result.json")
}

pub(crate) fn read_intent(path: &Path) -> Option<IntentRecord> {
    let bytes = std::fs::read(path).ok()?;
    // An unparseable intent (half-written before atomic-rename existed, disk fault) must not
    // crash reconcile — it reads as "no intent" and the stale file is swept.
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn read_result(path: &Path) -> Option<ResultRecord> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Atomic tmp+rename JSON write (the intent/result records must never be half-written — R12).
pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)
}

/// What boot-time reconciliation decided.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reconciled {
    /// No intent on disk — nothing to close out.
    None,
    /// Intent is fresh and we still run the old version: the installer likely hasn't stopped
    /// us yet (or is mid-copy). Leave the intent in place; status shows the apply in flight.
    StillApplying,
    /// We came back at the target version.
    Success(ResultRecord),
    /// We came back at the wrong version after the grace window.
    Failed(ResultRecord),
}

/// Pure reconcile: current version + wall clock vs. the intent record.
pub(crate) fn reconcile(
    intent: Option<IntentRecord>,
    current_version: &str,
    now_unix: u64,
) -> Reconciled {
    let Some(intent) = intent else {
        return Reconciled::None;
    };
    if current_version == intent.to {
        return Reconciled::Success(ResultRecord {
            ok: true,
            from: intent.from,
            to: intent.to,
            finished_unix: now_unix,
            stage: None,
            error: None,
            log_path: Some(intent.log_path),
            staged: false,
        });
    }
    if now_unix.saturating_sub(intent.started_unix) < APPLY_GRACE_SECS {
        return Reconciled::StillApplying;
    }
    Reconciled::Failed(ResultRecord {
        ok: false,
        from: intent.from.clone(),
        to: intent.to.clone(),
        finished_unix: now_unix,
        stage: Some("restarting".into()),
        error: Some(format!(
            "the host restarted still running {} (expected {}) — the installer aborted or \
             rolled back; see its log",
            intent.from, intent.to
        )),
        log_path: Some(intent.log_path),
        staged: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(started: u64) -> IntentRecord {
        IntentRecord {
            from: "0.23.100".into(),
            to: "0.23.200".into(),
            serial: 42,
            started_unix: started,
            installer_sha256: "ab".repeat(32),
            log_path: "/logs/update-0.23.200.log".into(),
        }
    }

    #[test]
    fn no_intent_is_none() {
        assert_eq!(reconcile(None, "0.23.100", 1000), Reconciled::None);
    }

    #[test]
    fn target_version_is_success_regardless_of_age() {
        for now in [1, 10_000_000] {
            match reconcile(Some(intent(0)), "0.23.200", now) {
                Reconciled::Success(r) => {
                    assert!(r.ok);
                    assert_eq!(r.to, "0.23.200");
                    assert_eq!(r.log_path.as_deref(), Some("/logs/update-0.23.200.log"));
                }
                other => panic!("expected success, got {other:?}"),
            }
        }
    }

    #[test]
    fn fresh_intent_old_version_is_still_applying() {
        assert_eq!(
            reconcile(Some(intent(1000)), "0.23.100", 1000 + APPLY_GRACE_SECS - 1),
            Reconciled::StillApplying
        );
    }

    #[test]
    fn stale_intent_old_version_is_failure_with_log() {
        match reconcile(Some(intent(1000)), "0.23.100", 1000 + APPLY_GRACE_SECS) {
            Reconciled::Failed(r) => {
                assert!(!r.ok);
                assert_eq!(r.stage.as_deref(), Some("restarting"));
                assert!(r.error.as_deref().unwrap().contains("0.23.200"));
                assert!(r.log_path.is_some());
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    #[test]
    fn intent_and_result_roundtrip_atomically() {
        let dir = std::env::temp_dir().join(format!("pf-update-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ip = dir.join("update-intent.json");
        write_json_atomic(&ip, &intent(7)).unwrap();
        assert_eq!(read_intent(&ip).unwrap().started_unix, 7);
        // Corrupt bytes read as "no intent", never a crash.
        std::fs::write(&ip, b"{half").unwrap();
        assert!(read_intent(&ip).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
