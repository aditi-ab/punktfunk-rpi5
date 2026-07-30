//! `pf-update` — the root helper behind web-console-triggered Linux host updates
//! (planning: `host-update-from-web-console.md` §7, plan U2.1).
//!
//! Invoked as `pf-update apply`, normally via the `punktfunk-update.service` oneshot that a
//! `punktfunk-update`-group member may start through polkit. **It takes zero
//! attacker-influenceable parameters**: no versions, no URLs, no package names from the
//! caller — the install kind comes from root-owned markers, the package list from the local
//! package database, and every payload from the distro package manager's own signed
//! repositories. Compromising the trigger yields "run the system's normal update for the
//! punktfunk packages", nothing more.
//!
//! After a successful package-manager run, the **run-the-binary gate** executes the newly
//! installed `/usr/bin/punktfunk-host --version` and requires it to exit cleanly — the
//! CI-green-on-the-wrong-program class (the 0.22.0 clobber) dies here for one binary run's
//! worth of cost. The outcome is written to `/var/lib/punktfunk/update-result.json`
//! (root-written, world-readable) for the unprivileged host to read; stdout/stderr land in
//! the unit's journal.

#[cfg(target_os = "linux")]
mod linux_main {
    use serde::Serialize;
    use std::path::Path;
    use std::process::Command;

    const MARKER: &str = "/usr/share/punktfunk/install-kind";
    const SYSEXT_MARKER: &str = "/usr/lib/extension-release.d/extension-release.punktfunk";
    const OSTREE_BOOTED: &str = "/run/ostree-booted";
    const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";
    const RESULT_PATH: &str = "/var/lib/punktfunk/update-result.json";
    const HOST_BIN: &str = "/usr/bin/punktfunk-host";

    /// What the host reads back. Field meanings mirror the mgmt API's `UpdateResultInfo`
    /// where they overlap; `changed=false` is the "your package source has nothing newer
    /// yet" case (not an error), `staged=true` means a reboot finishes the update
    /// (rpm-ostree).
    #[derive(Serialize)]
    struct HelperResult {
        ok: bool,
        kind: String,
        before_version: String,
        after_version: String,
        changed: bool,
        staged: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        finished_unix: u64,
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Root-owned facts → the apply strategy. Mirrors the host's ladder for the kinds a
    /// root helper serves (the helper decides for ITSELF — never trusts its caller).
    fn detect_kind() -> Result<&'static str, String> {
        if Path::new(SYSEXT_MARKER).exists() {
            return Ok("sysext");
        }
        let marker = std::fs::read_to_string(MARKER)
            .map_err(|e| format!("no install-kind marker at {MARKER}: {e}"))?;
        match marker.split_whitespace().next() {
            Some("apt") => Ok("apt"),
            Some("dnf") if Path::new(OSTREE_BOOTED).exists() => Ok("rpm-ostree"),
            Some("dnf") => Ok("dnf"),
            Some("pacman") => Ok("pacman"),
            other => Err(format!(
                "install-kind marker says {other:?} — no root apply leg for it"
            )),
        }
    }

    fn run(cmd: &mut Command, what: &str) -> Result<(), String> {
        println!("pf-update: running {cmd:?}");
        let status = cmd
            .status()
            .map_err(|e| format!("{what}: failed to launch: {e}"))?;
        if !status.success() {
            return Err(format!("{what}: exited {status}"));
        }
        Ok(())
    }

    fn run_capture(cmd: &mut Command, what: &str) -> Result<String, String> {
        let out = cmd
            .output()
            .map_err(|e| format!("{what}: failed to launch: {e}"))?;
        if !out.status.success() {
            return Err(format!("{what}: exited {}", out.status));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// The installed punktfunk packages, from the LOCAL package database — upgrade exactly
    /// what this box has (host-only installs don't grow a web console out of nowhere).
    fn installed_packages(query: &mut Command, what: &str) -> Result<Vec<String>, String> {
        let out = run_capture(query, what)?;
        let pkgs: Vec<String> = out
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("punktfunk"))
            .map(str::to_string)
            .collect();
        if pkgs.is_empty() {
            return Err(format!("{what}: no installed punktfunk packages found"));
        }
        Ok(pkgs)
    }

    fn host_version() -> Result<String, String> {
        run_capture(
            Command::new(HOST_BIN).arg("--version"),
            "punktfunk-host --version",
        )
    }

    /// The per-kind command tables (design §5). Returns `staged` (activation needs a reboot).
    fn apply_for_kind(kind: &str) -> Result<bool, String> {
        match kind {
            "apt" => {
                // Refresh only OUR index when the documented list file exists (S5);
                // otherwise a full refresh — normal admin behavior, just slower.
                let ours = "/etc/apt/sources.list.d/punktfunk.list";
                let mut update = Command::new("apt-get");
                update.env("DEBIAN_FRONTEND", "noninteractive");
                if Path::new(ours).exists() {
                    update.args([
                        "update",
                        "-o",
                        &format!("Dir::Etc::sourcelist={ours}"),
                        "-o",
                        "Dir::Etc::sourceparts=-",
                    ]);
                } else {
                    update.arg("update");
                }
                run(&mut update, "apt-get update")?;
                let pkgs = installed_packages(
                    Command::new("dpkg-query").args(["-W", "-f", "${Package}\n", "punktfunk*"]),
                    "dpkg-query",
                )?;
                let mut install = Command::new("apt-get");
                install
                    .env("DEBIAN_FRONTEND", "noninteractive")
                    .args(["install", "--only-upgrade", "-y"])
                    .args(&pkgs);
                run(&mut install, "apt-get install --only-upgrade")?;
                Ok(false)
            }
            "dnf" => {
                let pkgs = installed_packages(
                    Command::new("rpm").args(["-qa", "--qf", "%{NAME}\n", "punktfunk*"]),
                    "rpm -qa",
                )?;
                let mut upgrade = Command::new("dnf");
                upgrade.args(["-y", "upgrade"]).args(&pkgs);
                run(&mut upgrade, "dnf upgrade")?;
                Ok(false)
            }
            "rpm-ostree" => {
                // A layered package only re-resolves when forced — the single-transaction
                // uninstall+install dance (packaging/bazzite/update-punktfunk.sh). Staged;
                // a reboot activates it.
                let pkgs = installed_packages(
                    Command::new("rpm").args(["-qa", "--qf", "%{NAME}\n", "punktfunk*"]),
                    "rpm -qa",
                )?;
                run(
                    Command::new("rpm-ostree").args(["refresh-md", "--force"]),
                    "rpm-ostree refresh-md",
                )?;
                let mut update = Command::new("rpm-ostree");
                update.arg("update");
                for p in &pkgs {
                    update.args(["--uninstall", p, "--install", p]);
                }
                run(&mut update, "rpm-ostree update (re-resolve)")?;
                Ok(true)
            }
            "sysext" => {
                // The proven signed-feed updater; it refreshes the merged /usr in place.
                run(
                    Command::new("punktfunk-sysext").arg("update"),
                    "punktfunk-sysext update",
                )?;
                Ok(false)
            }
            "pacman" => {
                // Arch doctrine: partial upgrades break boxes, so the ONLY thing this
                // helper will run is a full -Syu — and only when the operator opted into
                // that explicitly (root-owned config, not the API).
                let optin = std::fs::read_to_string(PACMAN_OPTIN_CONF)
                    .ok()
                    .map(|c| c.lines().any(|l| l.trim() == "PACMAN_FULL_SYSUPGRADE=1"))
                    .unwrap_or(false);
                if !optin {
                    return Err(format!(
                        "pacman full-sysupgrade is not opted in — set PACMAN_FULL_SYSUPGRADE=1 \
                         in {PACMAN_OPTIN_CONF} (this runs `pacman -Syu` for the WHOLE system)"
                    ));
                }
                run(
                    Command::new("pacman").args(["-Syu", "--noconfirm"]),
                    "pacman -Syu",
                )?;
                Ok(false)
            }
            other => Err(format!("no apply leg for install kind {other}")),
        }
    }

    fn write_result(result: &HelperResult) {
        let path = Path::new(RESULT_PATH);
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if let Ok(bytes) = serde_json::to_vec_pretty(result) {
            if std::fs::write(&tmp, &bytes).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    pub fn main() {
        let arg = std::env::args().nth(1).unwrap_or_default();
        if arg != "apply" {
            eprintln!("usage: pf-update apply   (normally via punktfunk-update.service)");
            std::process::exit(2);
        }
        // Effective root is required for every leg; refuse early with a clear message
        // rather than half-running.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc_geteuid() } != 0 {
            eprintln!("pf-update: must run as root (start punktfunk-update.service)");
            std::process::exit(1);
        }

        let kind = match detect_kind() {
            Ok(k) => k,
            Err(e) => {
                eprintln!("pf-update: {e}");
                write_result(&HelperResult {
                    ok: false,
                    kind: "unknown".into(),
                    before_version: String::new(),
                    after_version: String::new(),
                    changed: false,
                    staged: false,
                    error: Some(e),
                    finished_unix: now_unix(),
                });
                std::process::exit(1);
            }
        };
        println!("pf-update: install kind {kind}");
        let before = host_version().unwrap_or_default();

        let outcome = apply_for_kind(kind).and_then(|staged| {
            // The run-the-binary gate: the freshly installed binary must actually run.
            // Skipped for staged (rpm-ostree) — the new binary isn't in /usr until reboot.
            let after = if staged {
                before.clone()
            } else {
                host_version()
                    .map_err(|e| format!("run-the-binary gate: {e} — the update did NOT stick"))?
            };
            Ok((staged, after))
        });

        let result = match outcome {
            Ok((staged, after)) => HelperResult {
                ok: true,
                kind: kind.into(),
                changed: staged || after != before,
                staged,
                before_version: before,
                after_version: after,
                error: None,
                finished_unix: now_unix(),
            },
            Err(e) => {
                eprintln!("pf-update: {e}");
                HelperResult {
                    ok: false,
                    kind: kind.into(),
                    before_version: before.clone(),
                    after_version: before,
                    changed: false,
                    staged: false,
                    error: Some(e),
                    finished_unix: now_unix(),
                }
            }
        };
        let ok = result.ok;
        write_result(&result);
        println!(
            "pf-update: {} ({} -> {}, changed: {}, staged: {})",
            if ok { "ok" } else { "FAILED" },
            result.before_version,
            result.after_version,
            result.changed,
            result.staged,
        );
        std::process::exit(if ok { 0 } else { 1 });
    }

    // One libc symbol, declared directly — not worth a libc dependency in a root helper.
    extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux_main::main();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pf-update is a Linux-only root helper");
    std::process::exit(2);
}
