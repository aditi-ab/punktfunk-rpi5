//! The Linux apply leg (design §7, plan U2.3): ask systemd to run the `pf-update` root
//! oneshot, read its result record, and — when the on-disk binary actually changed — hand
//! the outcome across our own restart via the same intent/reconcile machinery the Windows
//! leg uses.
//!
//! Privilege model: this process is unprivileged; `systemctl start punktfunk-update.service`
//! is authorized by polkit for members of the `punktfunk-update` group (an explicit,
//! auditable opt-in — the packaged group is empty). The request we send carries **nothing**:
//! the unit's ExecStart is fixed, and the helper derives everything from root-owned state.
//! polkit resolves group membership via NSS at request time, so a fresh
//! `usermod -aG punktfunk-update` counts without re-login (the *hint* probe in
//! [`opted_in`] uses the same NSS route for the same reason).

#![cfg(target_os = "linux")]

use super::jobs::{self, IntentRecord};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Where the root helper writes its outcome (mirrors `pf-update`'s `HelperResult`).
const HELPER_RESULT: &str = "/var/lib/punktfunk/update-result.json";

/// The unit the polkit rule scopes to. Its presence is the "helper is installed" probe.
const UNIT_PATH: &str = "/usr/lib/systemd/system/punktfunk-update.service";

/// The pacman escape hatch (root-owned; see the design's §5 pacman stance).
const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";

/// Wall-clock cap on one helper run (a full `pacman -Syu` on a stale box is slow; a stuck
/// package manager should still surface as an error eventually).
const HELPER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(serde::Deserialize)]
struct HelperResult {
    ok: bool,
    #[serde(default)]
    before_version: String,
    #[serde(default)]
    after_version: String,
    #[serde(default)]
    changed: bool,
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    finished_unix: u64,
}

/// The root helper (+ its unit) is installed on this box.
pub(super) fn helper_installed() -> bool {
    Path::new(UNIT_PATH).exists()
}

/// The pacman full-sysupgrade escape hatch is explicitly enabled (root-owned config).
pub(super) fn pacman_opted_in() -> bool {
    std::fs::read_to_string(PACMAN_OPTIN_CONF)
        .map(|c| c.lines().any(|l| l.trim() == "PACMAN_FULL_SYSUPGRADE=1"))
        .unwrap_or(false)
}

/// Is this process's user in the `punktfunk-update` group, **by NSS** (matching how polkit
/// will decide) — not by our (possibly stale) process credentials. Cached briefly; the
/// status endpoint polls this.
pub(super) fn opted_in() -> bool {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(Instant, bool)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    {
        let cached = cache.lock().unwrap();
        if let Some((at, val)) = *cached {
            if at.elapsed() < Duration::from_secs(60) {
                return val;
            }
        }
    }
    let val = probe_group_membership().unwrap_or(false);
    *cache.lock().unwrap() = Some((Instant::now(), val));
    val
}

fn probe_group_membership() -> Option<bool> {
    let user = capture(Command::new("id").arg("-un"))?;
    let groups = capture(Command::new("id").args(["-nG", user.trim()]))?;
    Some(groups.split_whitespace().any(|g| g == "punktfunk-update"))
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The console-facing opt-in instruction, shown instead of an Apply button.
pub(super) fn opt_in_hint() -> String {
    "sudo usermod -aG punktfunk-update $USER   # enables web-triggered updates for this host"
        .to_string()
}

/// Run the whole Linux apply: start the oneshot, wait, interpret its result record, and for
/// an in-place binary change, write the intent and restart ourselves (boot reconciliation
/// reports the outcome). Blocking — run on a blocking thread.
pub(super) fn run_apply(
    target_version: &str,
    serial: u64,
    stage: &dyn Fn(&'static str),
) -> Result<(), (&'static str, String)> {
    let started_unix = super::now_unix();

    let mut child = Command::new("systemctl")
        .args(["start", "punktfunk-update.service"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ("applying", format!("launch systemctl: {e}")))?;

    let deadline = Instant::now() + HELPER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return Err((
                    "applying",
                    format!(
                        "the update helper is still running after {} min — see \
                         `journalctl -u punktfunk-update.service`",
                        HELPER_TIMEOUT.as_secs() / 60
                    ),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(e) => return Err(("applying", format!("wait for systemctl: {e}"))),
        }
    };

    if !status.success() {
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read as _;
            let _ = stderr.read_to_string(&mut err);
        }
        let denial = err.contains("interactive authentication")
            || err.contains("Access denied")
            || err.contains("Permission denied");
        return Err((
            "applying",
            if denial {
                format!(
                    "not authorized to start the update helper — enable web-triggered \
                     updates first: {}",
                    opt_in_hint()
                )
            } else {
                format!(
                    "update helper failed ({status}) — see \
                     `journalctl -u punktfunk-update.service`. {err}"
                )
            },
        ));
    }

    // The unit succeeded; its record is the ground truth. Refuse a stale record (an old run's
    // leftovers with a fresh exit 0 would mean the helper never wrote — surface that).
    let result: HelperResult = std::fs::read(HELPER_RESULT)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or((
            "applying",
            format!("the update helper wrote no readable result at {HELPER_RESULT}"),
        ))?;
    if result.finished_unix + 5 < started_unix {
        return Err((
            "applying",
            format!(
                "the update helper's result record predates this run \
                 ({} < {started_unix}) — it never wrote one",
                result.finished_unix
            ),
        ));
    }
    if !result.ok {
        return Err((
            "applying",
            result
                .error
                .unwrap_or_else(|| "the update helper reported failure without detail".into()),
        ));
    }

    let current = env!("PUNKTFUNK_VERSION");
    if result.staged {
        // rpm-ostree: the new deployment activates on reboot — durable outcome now, no restart.
        let _ = jobs::write_json_atomic(
            &jobs::result_path(),
            &jobs::ResultRecord {
                ok: true,
                from: current.into(),
                to: target_version.into(),
                finished_unix: super::now_unix(),
                stage: None,
                error: None,
                log_path: None,
                staged: true,
            },
        );
        return Ok(());
    }
    if !result.changed {
        // The channel had nothing newer than what's installed (announce leads the mirrors) —
        // a real answer, not an error. from == to renders as "already up to date".
        let _ = jobs::write_json_atomic(
            &jobs::result_path(),
            &jobs::ResultRecord {
                ok: true,
                from: current.into(),
                to: current.into(),
                finished_unix: super::now_unix(),
                stage: None,
                error: None,
                log_path: None,
                staged: false,
            },
        );
        return Ok(());
    }

    // The on-disk binary changed (and the helper's run-the-binary gate already proved it
    // runs). Cross the restart on the intent record, exactly like the Windows leg.
    let to = result
        .after_version
        .split_whitespace()
        .last()
        .unwrap_or(target_version)
        .to_string();
    jobs::write_json_atomic(
        &jobs::intent_path(),
        &IntentRecord {
            from: current.into(),
            to,
            serial,
            started_unix: super::now_unix(),
            installer_sha256: String::new(),
            log_path: "journalctl -u punktfunk-update.service".into(),
        },
    )
    .map_err(|e| ("restarting", format!("write intent record: {e}")))?;

    stage("restarting");
    // Web console first (its own unit; a brief console blip the frontend rides out), then
    // ourselves — --no-block, because this very process is what's being restarted.
    let _ = Command::new("systemctl")
        .args(["--user", "--no-block", "restart", "punktfunk-web.service"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "--no-block", "restart", "punktfunk-host.service"])
        .status();
    Ok(())
}
