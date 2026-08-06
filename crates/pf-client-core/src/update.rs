//! **Is a newer client available, and can this box install it?** (Linux)
//!
//! The counterpart to `punktfunk-host::update`, sharing its trust machinery through
//! [`pf_update_check`]: the same per-channel Ed25519-signed manifest, the same validation
//! rules, the same "is this newer?" comparison. The host and the client ship from one repo at
//! one version, so one manifest answers for both — only the install kind and what can be done
//! about it differ, and that is what this module works out.
//!
//! **Why this lives in the client and not in the Decky plugin.** The plugin's backend is
//! unprivileged Python with no crypto dependency it could verify a signature with, and the
//! plugin is not the only surface that wants this (the GTK About page and a bare
//! `punktfunk-client --check-update` want the same answer). So the client is the engine and
//! every UI is a caller — exactly how `--pair`, `--library` and `--list-hosts` already work.
//!
//! **Privilege.** Nothing here is privileged. A flatpak updates through flatpak; a
//! system-packaged client updates through the root helper (`pf-update`, started as a fixed,
//! parameterless systemd oneshot that polkit authorises for members of the `punktfunk-update`
//! group); everything else reports the command to run and stops. The request we can make
//! carries no version, URL or package name — the helper derives all of it from root-owned
//! state, so being able to trigger it grants only "run this box's normal update".

#![cfg(target_os = "linux")]

use pf_update_check::detect::{self, InstallKind, Product};
use pf_update_check::version::{is_newer, Channel};
use pf_update_check::PublicKey;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// The unit the client-side polkit rule scopes to. Its presence is the "root helper is
/// installed" probe — the client packages ship unit + helper + rule together.
const HELPER_UNIT: &str = "punktfunk-client-update.service";
const HELPER_UNIT_PATH: &str = "/usr/lib/systemd/system/punktfunk-client-update.service";

/// Where the helper's `apply-client` verb writes its outcome. Separate from the host's record
/// so a box running both never reads the other product's run as its own.
const HELPER_RESULT: &str = "/var/lib/punktfunk/client-update-result.json";

/// The pacman escape hatch (root-owned; see the design's §5 pacman stance). Shared with the
/// host — one box, one answer to "may something run a full sysupgrade unattended".
const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";

/// The opt-in group. Mirrors `packaging/linux/49-punktfunk-client-update.rules`.
const OPT_IN_GROUP: &str = "punktfunk-update";

/// Wall-clock cap on one helper run (a stale box's package manager is slow; a stuck one must
/// still surface as an error eventually).
const HELPER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the box can do about a pending update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Apply {
    /// One action installs it now.
    Full,
    /// One action stages it; a reboot finishes (rpm-ostree).
    Staged,
    /// Nothing here can install it — show [`Status::command`].
    Notify,
}

/// Who performs the apply. The client can drive the root helper itself, but it can never
/// update its own flatpak (inside the sandbox there is no host `flatpak` to run) — that one
/// belongs to whoever launched it, which on a Deck is the Decky plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Applier {
    /// The caller runs `flatpak update --user io.unom.Punktfunk`.
    Flatpak,
    /// `punktfunk-client --apply-update` drives the packaged root helper.
    Helper,
    /// Manual only.
    None,
}

/// The whole answer, as the CLI serialises it for the Decky plugin and the GTK About page.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// Install kind (`flatpak`, `apt`, `dnf`, `sysext`, …).
    pub kind: String,
    /// `stable` | `canary`.
    pub channel: String,
    /// The running client's version.
    pub current: String,
    /// The channel's newest version, or `current` when the check couldn't run.
    pub latest: String,
    pub update_available: bool,
    pub apply: Apply,
    pub applier: Applier,
    /// One copy-pastable line that updates this install, always populated.
    pub command: String,
    /// Set when one-click COULD work but the operator hasn't joined the group yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opt_in_hint: Option<String>,
    /// Release-notes link from the manifest (validated to our forge), empty when unknown.
    pub notes_url: String,
    /// Why the check couldn't complete. `update_available` is always false when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The feed answered, but this channel has **no release published yet** — an expected
    /// state rather than a malfunction, so a caller can say so plainly instead of showing a
    /// raw "HTTP 404".
    ///
    /// Deliberately NOT symmetric with the host's `UpdateStatus`, which clears `last_error`
    /// for this case: there the consumer is a human reading a console, and a red "last check
    /// failed" on an empty feed is the bug being fixed. Here the consumer is a shell script
    /// reading an exit code, so `error` stays set and `--check-update` keeps returning 1.
    /// An empty channel is not evidence that this build is current, and a mistyped
    /// `PUNKTFUNK_UPDATE_FEED` is indistinguishable from one out here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_published: bool,
}

/// The Ed25519 keys trusted for update manifests — pinned once in [`pf_update_check`] so the
/// host and the client can never disagree about who may announce a release.
fn pinned_keys() -> Vec<PublicKey> {
    pf_update_check::OFFICIAL_UPDATE_KEYS
        .iter()
        .filter(|k| !k.is_empty())
        .filter_map(|k| PublicKey::parse(k).ok())
        .collect()
}

/// Update checks disabled by operator config — same switch name the host honours.
pub fn check_disabled() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_UPDATE_CHECK").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// One-click apply disabled by operator config (the kill switch): the box still reports what
/// is available and how to install it by hand.
pub fn apply_disabled() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_UPDATE_APPLY").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// This box's install kind + channel for the CLIENT (not the host — separate marker files;
/// see [`Product`]).
///
/// `current` is the calling binary's own version string. It is a parameter rather than an
/// `env!` here because only the BINARY is version-stamped (`clients/linux/build.rs` reads
/// `PUNKTFUNK_BUILD_VERSION`, which is what carries the canary `~ciN` suffix the channel
/// comparison needs); a library crate baking in its own build-time constant would quietly
/// report the workspace version instead.
pub fn detect_install(current: &str) -> (InstallKind, Channel) {
    detect::classify(&detect::gather(Product::Client, current), Product::Client)
}

/// The root helper (+ its unit) is installed on this box.
fn helper_installed() -> bool {
    Path::new(HELPER_UNIT_PATH).exists()
}

/// The pacman full-sysupgrade escape hatch is explicitly enabled (root-owned config).
fn pacman_opted_in() -> bool {
    std::fs::read_to_string(PACMAN_OPTIN_CONF)
        .map(|c| c.lines().any(|l| l.trim() == "PACMAN_FULL_SYSUPGRADE=1"))
        .unwrap_or(false)
}

/// Is this user in the opt-in group, **by NSS** (matching how polkit will decide) rather than
/// by our possibly-stale process credentials — so a fresh `usermod -aG` counts without a
/// re-login. Mirrors `punktfunk-host::update::linux::opted_in`.
fn opted_in() -> bool {
    let Some(user) = capture(Command::new("id").arg("-un")) else {
        return false;
    };
    let Some(groups) = capture(Command::new("id").args(["-nG", user.trim()])) else {
        return false;
    };
    groups.split_whitespace().any(|g| g == OPT_IN_GROUP)
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The opt-in instruction shown instead of an apply button.
pub fn opt_in_hint() -> String {
    format!("sudo usermod -aG {OPT_IN_GROUP} $USER   # enables one-tap client updates on this box")
}

/// Everything about the BOX that decides what an apply may do — gathered once, so the routing
/// below is a pure function of (kind, caps). Env vars and root-owned files are read here and
/// nowhere else, which is what lets the routing rules be tested exhaustively without a box.
#[derive(Debug, Clone, Copy)]
struct Caps {
    /// Operator kill switch (`PUNKTFUNK_UPDATE_APPLY=0`).
    apply_disabled: bool,
    /// The packaged root helper + its unit are installed.
    helper: bool,
    /// This user is in the opt-in group.
    opted_in: bool,
    /// The root-owned pacman full-sysupgrade escape hatch is set.
    pacman_optin: bool,
}

impl Caps {
    fn probe() -> Self {
        let helper = helper_installed();
        Self {
            apply_disabled: apply_disabled(),
            helper,
            // Both of these shell out or read root-owned config; skip them entirely when no
            // helper is installed, since nothing they could say would change the answer.
            opted_in: helper && opted_in(),
            pacman_optin: helper && pacman_opted_in(),
        }
    }
}

/// What this install can do about an update, and who would do it.
fn apply_route(kind: InstallKind, c: Caps) -> (Apply, Applier) {
    if c.apply_disabled {
        return (Apply::Notify, Applier::None);
    }
    let helper_ready = c.helper && c.opted_in;
    match kind {
        // Always available and needs no opt-in: it is a per-user install the user already owns.
        InstallKind::Flatpak => (Apply::Full, Applier::Flatpak),
        InstallKind::Apt | InstallKind::Dnf if helper_ready => (Apply::Full, Applier::Helper),
        InstallKind::RpmOstree if helper_ready => (Apply::Staged, Applier::Helper),
        InstallKind::Pacman if helper_ready && c.pacman_optin => (Apply::Full, Applier::Helper),
        // The signed sysext feed carries the HOST image only — there is nothing for a client
        // sysext to update FROM, so this is honestly notify-only rather than a button that
        // would fail. Same for nix, source builds and the Deck's own tree.
        _ => (Apply::Notify, Applier::None),
    }
}

/// Would this box one-click apply if the operator joined the group? (Drives the hint.)
fn opt_in_would_help(kind: InstallKind, c: Caps) -> bool {
    !c.apply_disabled
        && c.helper
        && !c.opted_in
        && matches!(
            kind,
            InstallKind::Apt | InstallKind::Dnf | InstallKind::RpmOstree | InstallKind::Pacman
        )
}

// ---------------------------------------------------------------- serial floor

fn state_path() -> Option<PathBuf> {
    crate::trust::config_dir()
        .ok()
        .map(|d| d.join("client-update-state.json"))
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FloorFile {
    #[serde(default)]
    serial_floor: std::collections::BTreeMap<String, u64>,
}

fn load_floor(path: &Path, channel: &str) -> u64 {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<FloorFile>(&b).ok())
        .and_then(|f| f.serial_floor.get(channel).copied())
        .unwrap_or(0)
}

/// Raise (never lower) the floor, through the crate's one config writer — this used to
/// hand-roll its own tmp+rename, which meant it neither cleaned up its temp on a failed
/// rename nor picked up [`crate::trust::write_atomic`]'s in-place fallback, so on an install
/// where the rename cannot work the floor silently never rose and a declined update came
/// back forever.
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
    let _ = crate::trust::write_atomic(path, &bytes);
}

// ---------------------------------------------------------------- check

/// Detect the install, fetch + verify the channel manifest, and report. Blocking (one HTTPS
/// round trip); never panics — a failed check reports `error` and `update_available: false`,
/// because "we could not tell" must never render as "you are up to date".
pub fn check(current: &str) -> Status {
    let (kind, channel) = detect_install(current);
    let caps = Caps::probe();
    let current = current.to_string();
    let mut status = Status {
        kind: kind.as_str().to_string(),
        channel: channel.as_str().to_string(),
        current: current.clone(),
        latest: current.clone(),
        update_available: false,
        apply: Apply::Notify,
        applier: Applier::None,
        command: detect::update_command(kind, Product::Client),
        opt_in_hint: opt_in_would_help(kind, caps).then(opt_in_hint),
        notes_url: String::new(),
        error: None,
        not_published: false,
    };
    let (apply, applier) = apply_route(kind, caps);
    status.apply = apply;
    status.applier = applier;

    if check_disabled() {
        status.error = Some("update checks are disabled (PUNKTFUNK_UPDATE_CHECK=0)".into());
        return status;
    }

    let manifest = match pf_update_check::feed::fetch_manifest_blocking(
        &pf_update_check::feed::feed_base(),
        channel.as_str(),
        &pinned_keys(),
        &format!("punktfunk-client/{current} (update-check)"),
    ) {
        Ok(m) => m,
        Err(e) => {
            status.not_published = e.is_not_published();
            status.error = Some(e.to_string());
            return status;
        }
    };

    // Anti-rollback: a validly-signed but OLDER manifest replayed at us is an error, not a
    // silent downgrade of what we believe the channel holds.
    if let Some(path) = state_path() {
        let floor = load_floor(&path, channel.as_str());
        if manifest.serial < floor {
            status.error = Some(format!(
                "manifest serial {} is older than the last accepted {} — refusing rollback",
                manifest.serial, floor
            ));
            return status;
        }
        store_floor(&path, channel.as_str(), manifest.serial);
    }

    status.latest = manifest.version.clone();
    status.notes_url = manifest.notes_url.clone();
    status.update_available = is_newer(&manifest.version, manifest.ci_run, &current, channel);
    status
}

// ---------------------------------------------------------------- apply

/// What an apply attempt did.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    pub ok: bool,
    /// The package set actually changed on disk.
    pub changed: bool,
    /// Installed, but a reboot activates it (rpm-ostree).
    pub staged: bool,
    pub before: String,
    pub after: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ApplyOutcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            changed: false,
            staged: false,
            before: String::new(),
            after: String::new(),
            error: Some(error.into()),
        }
    }
}

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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the packaged root helper for this install, if that is the applier for it. Blocking for
/// as long as the package manager takes.
///
/// Deliberately refuses every kind it does not own rather than trying something clever: a
/// flatpak can only be updated from outside its sandbox, and there is no feed behind a client
/// sysext or a source build. Callers read [`Status::applier`] first.
pub fn apply(current: &str) -> ApplyOutcome {
    let (kind, _) = detect_install(current);
    let caps = Caps::probe();
    let (_, applier) = apply_route(kind, caps);
    match applier {
        Applier::Helper => {}
        Applier::Flatpak => {
            return ApplyOutcome::failed(
                "a flatpak client updates from outside its sandbox — run `flatpak update --user \
                 io.unom.Punktfunk` (the Decky plugin does this for you)",
            )
        }
        Applier::None => {
            let hint = opt_in_would_help(kind, caps)
                .then(opt_in_hint)
                .unwrap_or_else(|| detect::update_command(kind, Product::Client));
            return ApplyOutcome::failed(format!(
                "no one-tap update for a `{}` install — {hint}",
                kind.as_str()
            ));
        }
    }

    let started = now_unix();
    let output = Command::new("systemctl")
        .args(["start", HELPER_UNIT])
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => return ApplyOutcome::failed(format!("launch systemctl: {e}")),
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let denied = err.contains("interactive authentication")
            || err.contains("Access denied")
            || err.contains("Permission denied");
        return ApplyOutcome::failed(if denied {
            format!(
                "not authorized to start the update helper — enable one-tap updates first: {}",
                opt_in_hint()
            )
        } else {
            format!(
                "update helper failed ({}) — see `journalctl -u {HELPER_UNIT}`. {}",
                output.status,
                err.trim()
            )
        });
    }

    // The unit succeeded; its record is the ground truth. A record older than this run means
    // the helper never wrote one — surface that instead of reporting a previous run's success.
    let result: HelperResult = match std::fs::read(HELPER_RESULT)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(r) => r,
        None => {
            return ApplyOutcome::failed(format!(
                "the update helper wrote no readable result at {HELPER_RESULT}"
            ))
        }
    };
    if result.finished_unix + 5 < started {
        return ApplyOutcome::failed(format!(
            "the update helper's result record predates this run ({} < {started}) — it never \
             wrote one",
            result.finished_unix
        ));
    }
    if !result.ok {
        return ApplyOutcome::failed(
            result
                .error
                .unwrap_or_else(|| "the update helper reported failure without detail".into()),
        );
    }
    ApplyOutcome {
        ok: true,
        changed: result.changed,
        staged: result.staged,
        before: result.before_version,
        after: result.after_version,
        error: None,
    }
}

/// The helper's wall-clock cap, exposed so a caller can size its own timeout above ours.
pub const fn helper_timeout() -> Duration {
    HELPER_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fully-enabled box: helper installed, user opted in, pacman hatch open.
    fn ready() -> Caps {
        Caps {
            apply_disabled: false,
            helper: true,
            opted_in: true,
            pacman_optin: true,
        }
    }

    /// The kill switch must reach BOTH halves of the answer: no applier, and the notify tier.
    /// It is the operator's "stop offering this" and has to beat every kind, including the
    /// flatpak one that needs no other permission.
    #[test]
    fn kill_switch_forces_notify() {
        let off = Caps {
            apply_disabled: true,
            ..ready()
        };
        for kind in [
            InstallKind::Flatpak,
            InstallKind::Apt,
            InstallKind::Dnf,
            InstallKind::RpmOstree,
            InstallKind::Pacman,
        ] {
            assert_eq!(
                apply_route(kind, off),
                (Apply::Notify, Applier::None),
                "{}",
                kind.as_str()
            );
        }
    }

    /// Kinds with no feed behind them must never claim an apply route, however permissive the
    /// box is — a button that cannot work is worse than a command the user can run.
    #[test]
    fn feedless_kinds_are_notify_only() {
        for kind in [
            InstallKind::Sysext,
            InstallKind::Nix,
            InstallKind::Source,
            InstallKind::SteamosSource,
            InstallKind::WindowsInstaller,
        ] {
            assert_eq!(
                apply_route(kind, ready()),
                (Apply::Notify, Applier::None),
                "{}",
                kind.as_str()
            );
        }
    }

    /// The package-manager kinds are gated on BOTH the helper being installed and the group
    /// opt-in; neither alone may produce a Helper applier.
    #[test]
    fn helper_kinds_need_helper_and_opt_in() {
        for kind in [InstallKind::Apt, InstallKind::Dnf, InstallKind::RpmOstree] {
            for caps in [
                Caps {
                    helper: false,
                    ..ready()
                },
                Caps {
                    opted_in: false,
                    ..ready()
                },
            ] {
                assert_eq!(
                    apply_route(kind, caps),
                    (Apply::Notify, Applier::None),
                    "{}",
                    kind.as_str()
                );
            }
            let (tier, applier) = apply_route(kind, ready());
            assert_eq!(applier, Applier::Helper, "{}", kind.as_str());
            // rpm-ostree activates on reboot; the others land immediately.
            let want = if kind == InstallKind::RpmOstree {
                Apply::Staged
            } else {
                Apply::Full
            };
            assert_eq!(tier, want, "{}", kind.as_str());
        }
    }

    /// pacman carries the extra root-owned hatch (a full `-Syu` is a whole-system action).
    #[test]
    fn pacman_needs_its_own_opt_in() {
        assert_eq!(
            apply_route(
                InstallKind::Pacman,
                Caps {
                    pacman_optin: false,
                    ..ready()
                }
            ),
            (Apply::Notify, Applier::None)
        );
        assert_eq!(
            apply_route(InstallKind::Pacman, ready()),
            (Apply::Full, Applier::Helper)
        );
    }

    /// A flatpak is a per-user install the user already owns — no group, no helper, and the
    /// caller (not the sandboxed client) performs it.
    #[test]
    fn flatpak_needs_no_opt_in_and_is_applied_by_the_caller() {
        let bare = Caps {
            apply_disabled: false,
            helper: false,
            opted_in: false,
            pacman_optin: false,
        };
        assert_eq!(
            apply_route(InstallKind::Flatpak, bare),
            (Apply::Full, Applier::Flatpak)
        );
    }

    /// The opt-in hint is only worth showing when joining the group would actually change the
    /// answer: a helper must already be installed, and the kind must be one it serves.
    #[test]
    fn opt_in_hint_only_where_it_would_help() {
        let not_opted = Caps {
            opted_in: false,
            ..ready()
        };
        assert!(opt_in_would_help(InstallKind::Apt, not_opted));
        assert!(!opt_in_would_help(InstallKind::Sysext, not_opted));
        assert!(!opt_in_would_help(InstallKind::Flatpak, not_opted));
        assert!(!opt_in_would_help(
            InstallKind::Apt,
            Caps {
                helper: false,
                ..not_opted
            }
        ));
        // Already in the group ⇒ nothing to hint at.
        assert!(!opt_in_would_help(InstallKind::Apt, ready()));
    }

    #[test]
    fn serial_floor_never_lowers() {
        let dir = std::env::temp_dir().join(format!("pf-update-floor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        store_floor(&path, "stable", 100);
        assert_eq!(load_floor(&path, "stable"), 100);
        store_floor(&path, "stable", 50);
        assert_eq!(
            load_floor(&path, "stable"),
            100,
            "a replay must not lower it"
        );
        store_floor(&path, "stable", 101);
        assert_eq!(load_floor(&path, "stable"), 101);
        // Channels are independent floors.
        assert_eq!(load_floor(&path, "canary"), 0);
        std::fs::remove_dir_all(&dir).ok();
    }
}
