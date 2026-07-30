//! `pf-update` — the root helper behind triggered Linux package updates
//! (planning: `host-update-from-web-console.md` §7, plan U2.1).
//!
//! Two verbs, one per product:
//!
//! * `pf-update apply` — the HOST, via `punktfunk-update.service` (triggered from the web
//!   console).
//! * `pf-update apply-client` — the CLIENT, via `punktfunk-client-update.service` (triggered
//!   by `punktfunk-client --apply-update`, which is what the Decky plugin's one-tap runs).
//!
//! Both are started by an unprivileged process through polkit, authorised for members of the
//! `punktfunk-update` group. **The helper takes zero attacker-influenceable parameters**: no
//! versions, no URLs, no package names from the caller — the verb comes from a root-owned
//! unit's fixed `ExecStart`, the install kind from root-owned markers, the package list from
//! the local package database, and every payload from the distro package manager's own signed
//! repositories. Compromising a trigger yields "run the system's normal update for the
//! punktfunk packages", nothing more.
//!
//! Both verbs upgrade every installed `punktfunk*` package — a box with both gets both,
//! whichever unit ran. What the verb changes is which marker is read (the two packages cannot
//! own one marker path: that is a hard conflict in deb, rpm and pacman alike) and which binary
//! the **run-the-binary gate** executes afterwards, requiring a clean exit — the
//! CI-green-on-the-wrong-program class (the 0.22.0 clobber) dies there for one binary run's
//! worth of cost. The outcome is written to `/var/lib/punktfunk/{,client-}update-result.json`
//! (root-written, world-readable) for the unprivileged caller to read; stdout/stderr land in
//! the unit's journal.

#[cfg(target_os = "linux")]
mod linux_main {
    use serde::Serialize;
    use std::path::Path;
    use std::process::Command;

    const OSTREE_BOOTED: &str = "/run/ostree-booted";
    const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";

    /// Which product this run was started for. It comes from the VERB in a root-owned unit's
    /// fixed `ExecStart` — never from an unprivileged caller — so it stays inside the
    /// zero-attacker-influenceable-parameters rule: the two units differ only in which
    /// product's marker they read and which binary the run-the-binary gate executes.
    ///
    /// Two units exist rather than one because the host and the client are separate packages
    /// and every packaging format we ship treats two packages owning one path as a hard
    /// conflict — a client-only box (a Steam Deck, a handheld) must be able to install the
    /// helper without the host package.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Host,
        Client,
    }

    impl Mode {
        fn marker(self) -> &'static str {
            match self {
                Mode::Host => "/usr/share/punktfunk/install-kind",
                Mode::Client => "/usr/share/punktfunk-client/install-kind",
            }
        }

        fn sysext_marker(self) -> &'static str {
            match self {
                Mode::Host => "/usr/lib/extension-release.d/extension-release.punktfunk",
                Mode::Client => "/usr/lib/extension-release.d/extension-release.punktfunk-client",
            }
        }

        /// The binary the run-the-binary gate executes after a package-manager run.
        fn gate_binary(self) -> &'static str {
            match self {
                Mode::Host => "/usr/bin/punktfunk-host",
                Mode::Client => "/usr/bin/punktfunk-client",
            }
        }

        fn result_path(self) -> &'static str {
            match self {
                Mode::Host => "/var/lib/punktfunk/update-result.json",
                Mode::Client => "/var/lib/punktfunk/client-update-result.json",
            }
        }

        fn as_str(self) -> &'static str {
            match self {
                Mode::Host => "host",
                Mode::Client => "client",
            }
        }
    }

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
    fn detect_kind(mode: Mode) -> Result<&'static str, String> {
        if Path::new(mode.sysext_marker()).exists() {
            return match mode {
                Mode::Host => Ok("sysext"),
                // `punktfunk-sysext update` pulls the HOST image from the signed feed; there
                // is no client feed to pull from (a client sysext is the local
                // packaging/arch/build-sysext.sh wrapper). Refusing here is the honest answer
                // — the alternative would install the host over a client-only box.
                Mode::Client => Err(
                    "this client is a sysext, and the sysext feed carries the host image only \
                     — rebuild and re-install the client image instead"
                        .to_string(),
                ),
            };
        }
        let marker_path = mode.marker();
        let marker = std::fs::read_to_string(marker_path)
            .map_err(|e| format!("no install-kind marker at {marker_path}: {e}"))?;
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

    /// The run-the-binary gate's reading: execute what we just installed and take its
    /// `--version`. A binary that cannot run is an update that did NOT stick, whatever the
    /// package manager reported (the 0.22.0 clobber lesson).
    fn gate_version(mode: Mode) -> Result<String, String> {
        let bin = mode.gate_binary();
        run_capture(
            Command::new(bin).arg("--version"),
            &format!("{bin} --version"),
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

    fn write_result(mode: Mode, result: &HelperResult) {
        let path = Path::new(mode.result_path());
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
        let mode = match arg.as_str() {
            "apply" => Mode::Host,
            "apply-client" => Mode::Client,
            _ => {
                eprintln!(
                    "usage: pf-update apply | apply-client   (normally via \
                     punktfunk-update.service / punktfunk-client-update.service)"
                );
                std::process::exit(2);
            }
        };
        // Effective root is required for every leg; refuse early with a clear message
        // rather than half-running.
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc_geteuid() } != 0 {
            eprintln!("pf-update: must run as root (start punktfunk-update.service)");
            std::process::exit(1);
        }

        let kind = match detect_kind(mode) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("pf-update: {e}");
                write_result(
                    mode,
                    &HelperResult {
                        ok: false,
                        kind: "unknown".into(),
                        before_version: String::new(),
                        after_version: String::new(),
                        changed: false,
                        staged: false,
                        error: Some(e),
                        finished_unix: now_unix(),
                    },
                );
                std::process::exit(1);
            }
        };
        println!("pf-update: {} install kind {kind}", mode.as_str());
        let before = gate_version(mode).unwrap_or_default();

        let outcome = apply_for_kind(kind).and_then(|staged| {
            // The run-the-binary gate: the freshly installed binary must actually run.
            // Skipped for staged (rpm-ostree) — the new binary isn't in /usr until reboot.
            let after = if staged {
                before.clone()
            } else {
                gate_version(mode)
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
        write_result(mode, &result);
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
