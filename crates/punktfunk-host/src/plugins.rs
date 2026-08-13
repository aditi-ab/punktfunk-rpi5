//! `punktfunk-host plugins …` — the one-liner plugin CLI.
//!
//! Installing a plugin used to be a hand ritual: create the plugins dir, hand-write a `bunfig.toml`
//! registry scope map, `bun add` the package, then hand-enable a systemd unit (Linux) or a scheduled
//! task (Windows) — all with platform-divergent paths. This subcommand collapses that to
//! `punktfunk-host plugins add playnite` + `punktfunk-host plugins enable`.
//!
//! Split of duties (matching where the machinery already lives):
//! - **Package ops** (`add`/`remove`/`list`) are forwarded to the bun runner (`sdk/src/plugins.ts`),
//!   which owns the vendored bun, the `@punktfunk` registry scope, and the plugins dir. We locate the
//!   runner rather than reimplementing npm resolution in Rust.
//! - **Service ops** (`enable`/`disable`/`status`) run natively here — `systemctl --user` on Linux,
//!   the `PunktfunkScripting` scheduled task on Windows — so they work even without the runner
//!   package present.
//!
//! Windows needs elevation for both halves: the plugins dir lives under the ACL'd
//! `%ProgramData%\punktfunk` (see `pf_paths::create_private_dir`) and the task is admin-owned. We
//! check up front and print one actionable line instead of letting `bun add` fail with a bare
//! EACCES.
//!
//! The task itself runs as **`NT AUTHORITY\LocalService`**, not SYSTEM: plugins are
//! operator-installed code, and a plugin defect must cost a throwaway service account, not the
//! most privileged principal on the box. `enable` converges the principal (migrating tasks an
//! older installer registered as SYSTEM) and grants LocalService read on exactly the files the
//! runner's `connect()` needs — the scoped `plugin-token` and the TLS-pin cert
//! (`native-cert.pem` on identity-split hosts, `cert.pem` on legacy ones) — never the
//! full-admin `mgmt-token`.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// The systemd user unit / Windows scheduled task that supervises plugins.
#[cfg(target_os = "linux")]
const UNIT: &str = "punktfunk-scripting";
#[cfg(target_os = "windows")]
const TASK: &str = "PunktfunkScripting";

/// The runner executable's name. Every non-Windows package installs a wrapper under exactly this
/// name — the deb/rpm at `/usr/bin`, the SteamOS installer at `~/.local/bin`, Nix at
/// `$out/bin` — so one name covers every layout the resolver walks.
#[cfg(not(target_os = "windows"))]
const RUNNER_BIN: &str = "punktfunk-scripting";

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") | Some("remove") | Some("rm") | Some("uninstall") | Some("list")
        | Some("ls") => {
            // Package ops write into the (ACL'd, on Windows) plugins dir.
            #[cfg(target_os = "windows")]
            if !matches!(args.first().map(String::as_str), Some("list") | Some("ls")) {
                require_elevation("installing or removing plugins")?;
            }
            forward_to_runner(args)
        }
        Some("enable") => {
            #[cfg(target_os = "windows")]
            require_elevation("enabling the plugin runner")?;
            enable()
        }
        Some("disable") => {
            #[cfg(target_os = "windows")]
            require_elevation("disabling the plugin runner")?;
            disable()
        }
        Some("status") => status(),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => bail!("unknown plugins command '{other}' (try `plugins --help`)"),
    }
}

fn print_usage() {
    eprintln!(
        "punktfunk-host plugins — install and run host plugins

USAGE:
    punktfunk-host plugins add <name…>       install a plugin (playnite, rom-manager, …)
    punktfunk-host plugins remove <name…>    uninstall a plugin
    punktfunk-host plugins list              list installed plugins
    punktfunk-host plugins enable            enable + start the plugin runner (opt-in)
    punktfunk-host plugins disable           stop + disable the plugin runner
    punktfunk-host plugins status            is the runner enabled/running?

NAMES:
    A bare first-party name resolves into the @punktfunk scope: `playnite` installs
    @punktfunk/plugin-playnite, `rom-manager` installs @punktfunk/plugin-rom-manager —
    always from Punktfunk's own package registry. Any other name (`punktfunk-plugin-*`,
    a foreign @scope) installs from the PUBLIC npm registry and is refused unless you
    pass --allow-public-registry.

NOTES:
    Plugins run under the runner, which is OPT-IN — `plugins add` installs, `plugins enable`
    turns the runner on. Plugins are operator-installed code that runs with operator
    privileges; install only plugins you trust.
"
    );
    #[cfg(target_os = "windows")]
    eprintln!(
        "    On Windows, `add`/`remove`/`enable`/`disable` need an ELEVATED prompt (the plugins\n    \
         directory and the runner task are admin-owned)."
    );
}

// ---- package ops: forward to the bun runner ---------------------------------------------------

/// Locate the runner and hand it the argv verbatim, inheriting stdio so bun's progress output goes
/// straight to the user's terminal. Exits with the runner's own status code.
fn forward_to_runner(args: &[String]) -> Result<()> {
    // `bun add` installs into the nearest ancestor `package.json`, not into its working directory,
    // so the plugins dir has to own one before the runner runs or a stray `~/package.json` captures
    // the install — silently, exit 0 (see `store::ensure_plugin_root`). The runner seeds it too, but
    // the installed scripting package can predate this binary, so do it on this side as well.
    if args.first().map(String::as_str) == Some("add") {
        let dir = args
            .iter()
            .position(|a| a == "--plugins")
            .and_then(|i| args.get(i + 1))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(crate::store::plugins_dir);
        crate::store::ensure_plugin_root(&dir)
            .with_context(|| format!("prepare {}", dir.display()))?;
    }
    let (program, prefix) = runner_command()?;
    let status = Command::new(&program)
        .args(&prefix)
        .args(args)
        .status()
        .with_context(|| format!("failed to run the plugin runner ({})", program.display()))?;
    if !status.success() {
        // The runner already printed the reason; propagate its code without a second error line.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Resolve how to invoke the runner CLI: the program plus any leading args (the bundled bun needs
/// the runner script path passed to it).
///
/// Also the plugin store's executor seam ([`crate::store::jobs`]): a console-triggered install runs
/// the *same* package ops through the *same* runner as the CLI, so there is exactly one
/// implementation of "install a plugin" on the box (design D4).
pub(crate) fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
    #[cfg(target_os = "windows")]
    {
        // The installer lays the payload out as {app}\punktfunk-host.exe, {app}\bun\bun.exe and
        // {app}\scripting\runner-cli.js (packaging/windows/punktfunk-host.iss).
        let app = std::env::current_exe()
            .context("resolve current exe")?
            .parent()
            .context("resolve install dir")?
            .to_path_buf();
        let bun = app.join("bun").join("bun.exe");
        let runner = app.join("scripting").join("runner-cli.js");
        if !bun.exists() || !runner.exists() {
            bail!(
                "the plugin runner isn't installed (looked for {} and {}) — reinstall punktfunk \
                 with the scripting component",
                bun.display(),
                runner.display()
            );
        }
        // Tail expression, not `return`: after cfg-stripping this block is the whole fn body on
        // Windows, and a `return` here trips clippy's needless_return under CI's -D warnings.
        Ok((bun, vec![runner.to_string_lossy().into_owned()]))
    }
    #[cfg(not(target_os = "windows"))]
    {
        let exe = std::env::current_exe().ok();
        let path_var = std::env::var("PATH").ok();
        let home = std::env::var("HOME").ok();
        resolve_runner_in(
            std::env::var("PUNKTFUNK_SCRIPTING").ok().as_deref(),
            exe.as_deref().and_then(std::path::Path::parent),
            path_var.as_deref(),
            home.as_deref().map(std::path::Path::new),
            &|p| p.is_file(),
        )
        .ok_or_else(|| anyhow::anyhow!("{RUNNER_MISSING}"))
    }
}

/// What to say when no rung matched. Shared with [`runtime_status`], so the CLI and the console
/// tell an operator the same thing.
#[cfg(not(target_os = "windows"))]
pub(crate) const RUNNER_MISSING: &str =
    "the plugin runner isn't installed — install it first (Debian/Ubuntu: `sudo apt install \
     punktfunk-scripting`; SteamOS: re-run scripts/steamdeck/install.sh; NixOS: enable \
     `services.punktfunk.scripting`). If it is installed somewhere else, point PUNKTFUNK_SCRIPTING \
     at the punktfunk-scripting executable.";

/// The rungs, in order: `PUNKTFUNK_SCRIPTING` → beside the host binary → `PATH` → the packaged
/// `/usr` layout → the user-scoped SteamOS layout. Pure and fully injected so the table can be
/// tested without mutating process env, which races `getenv` in parallel tests.
///
/// `PATH` is load-bearing rather than a nicety: it is the ONLY rung a Nix install can land on.
/// `punktfunk-scripting` is a derivation of its own there (packaging/nix/packages.nix), so its
/// wrapper is neither beside the host binary nor anywhere under `/usr` — the layouts this
/// resolver used to check exclusively, which is why a fully working NixOS box reported the runner
/// as not installed.
#[cfg(not(target_os = "windows"))]
fn resolve_runner_in(
    env: Option<&str>,
    exe_dir: Option<&std::path::Path>,
    path_var: Option<&str>,
    home: Option<&std::path::Path>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Option<(std::path::PathBuf, Vec<String>)> {
    use std::path::{Path, PathBuf};

    // The two-file layout: a private bun plus the runner bundle, which the deb/rpm and the SteamOS
    // installer both lay down beside their wrapper. Only a rung when BOTH halves are present.
    let pair = |bun: PathBuf, runner: PathBuf| -> Option<(PathBuf, Vec<String>)> {
        (exists(&bun) && exists(&runner))
            .then(|| (bun, vec![runner.to_string_lossy().into_owned()]))
    };

    // The operator's own override, and deliberately NOT existence-checked: whoever names a path is
    // entitled to a failure that names it back, where falling through to a runner that happens to
    // be installed would hide the typo behind a working install.
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        return Some((PathBuf::from(v), Vec::new()));
    }
    // Beside the host binary — a source tree or any relocatable layout shipping both in one prefix.
    if let Some(p) = exe_dir.map(|d| d.join(RUNNER_BIN)).filter(|p| exists(p)) {
        return Some((p, Vec::new()));
    }
    if let Some(p) = path_var
        .into_iter()
        .flat_map(|v| v.split(':'))
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(RUNNER_BIN))
        .find(|p| exists(p))
    {
        return Some((p, Vec::new()));
    }
    // The packaged /usr layout (packaging/debian/build-scripting-deb.sh). Still checked explicitly
    // after `PATH` because a systemd unit can carry a PATH that does not include /usr/bin.
    let wrapper = Path::new("/usr/bin").join(RUNNER_BIN);
    if exists(&wrapper) {
        return Some((wrapper, Vec::new()));
    }
    if let Some(cmd) = pair(
        Path::new("/usr/lib").join(RUNNER_BIN).join("bun"),
        Path::new("/usr/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js"),
    ) {
        return Some(cmd);
    }
    // Immutable-/usr distros (SteamOS): scripts/steamdeck/install.sh lays the SAME payload out
    // user-scoped under ~/.local, because a system package can't exist there.
    let home = home?;
    let wrapper = home.join(".local/bin").join(RUNNER_BIN);
    if exists(&wrapper) {
        return Some((wrapper, Vec::new()));
    }
    pair(
        home.join(".local/lib").join(RUNNER_BIN).join("bun"),
        home.join(".local/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js"),
    )
}

// ---- service ops ------------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn enable() -> Result<()> {
    run_systemctl(&["enable", "--now", UNIT])?;
    println!("Plugin runner enabled and started ({UNIT}).");
    Ok(())
}

#[cfg(target_os = "linux")]
fn disable() -> Result<()> {
    run_systemctl(&["disable", "--now", UNIT])?;
    println!("Plugin runner stopped and disabled ({UNIT}).");
    Ok(())
}

fn status() -> Result<()> {
    let st = runtime_status();
    println!(
        "runner:  {}\nstate:   {}\nenabled: {}",
        st.unit,
        if !st.installed {
            "not installed"
        } else if st.running {
            "running"
        } else {
            "stopped"
        },
        st.enabled
    );
    if let Some(principal) = &st.principal {
        println!("runs as: {principal}");
    }
    if st.installed && !st.running {
        println!("\nStart it with: punktfunk-host plugins enable");
    } else if !st.installed {
        println!("\n{}", st.detail);
    }
    Ok(())
}

// ---- runtime state, shared by the CLI and the plugin store's mgmt API --------------------------

/// Whether the plugin runner is present, switched on, and up.
///
/// The store's console surface needs this as data (to offer "enable the runner" before the first
/// install, and to explain why a freshly installed plugin isn't running yet), so it lives here
/// rather than being formatted straight to stdout like the CLI once did.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeStatus {
    /// Is the runner payload / service unit on this box at all?
    pub installed: bool,
    /// Is it configured to start (systemd `enabled`, or a non-`Disabled` scheduled task)?
    pub enabled: bool,
    /// Is it up right now?
    pub running: bool,
    /// The unit / task name, so operator-facing copy can name the thing to look at.
    pub unit: &'static str,
    /// Windows: the account the task runs as (the SYSTEM→LocalService migration is visible here).
    pub principal: Option<String>,
    /// One line of human-readable context, mostly for the "not installed" case.
    pub detail: String,
}

#[cfg(target_os = "linux")]
pub(crate) fn runtime_status() -> RuntimeStatus {
    let enabled_raw = systemctl_output(&["is-enabled", UNIT]);
    let active = systemctl_output(&["is-active", UNIT]).unwrap_or_default();
    // `is-enabled` answers `not-found` when the unit file isn't installed at all; the runner
    // payload being present is the other half of "can we install plugins".
    let unit_known = enabled_raw.as_deref().is_some_and(|s| s != "not-found");
    let installed = unit_known || runner_command().is_ok();
    RuntimeStatus {
        installed,
        enabled: enabled_raw.as_deref() == Some("enabled"),
        running: active == "active",
        unit: UNIT,
        principal: None,
        detail: if installed {
            String::new()
        } else {
            RUNNER_MISSING.into()
        },
    }
}

#[cfg(target_os = "windows")]
pub(crate) fn runtime_status() -> RuntimeStatus {
    let out = powershell_output(&format!(
        "$t = Get-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         if ($null -eq $t) {{ 'missing' }} else {{ \"$($t.State)|$($t.Principal.UserId)\" }}"
    ));
    match out.as_deref().map(str::trim) {
        Some("missing") | None => RuntimeStatus {
            installed: false,
            enabled: false,
            running: false,
            unit: TASK,
            principal: None,
            detail: "reinstall punktfunk with the scripting component to get the plugin runner"
                .into(),
        },
        Some(raw) => {
            let (state, principal) = raw.split_once('|').unwrap_or((raw, ""));
            RuntimeStatus {
                installed: true,
                enabled: !state.eq_ignore_ascii_case("Disabled"),
                running: state.eq_ignore_ascii_case("Running"),
                unit: TASK,
                principal: (!principal.is_empty()).then(|| principal.to_string()),
                detail: String::new(),
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub(crate) fn runtime_status() -> RuntimeStatus {
    RuntimeStatus {
        installed: false,
        enabled: false,
        running: false,
        unit: "punktfunk-scripting",
        principal: None,
        detail: "the plugin runner is only available on Linux and Windows hosts".into(),
    }
}

/// Switch the runner on or off — the [`enable`]/[`disable`] the CLI runs, exposed for the store's
/// `POST /store/runtime`. On Windows this is reached from the SYSTEM service, which already clears
/// the elevation bar the CLI has to check for.
pub(crate) fn set_runtime_enabled(enabled: bool) -> Result<()> {
    if enabled {
        enable()
    } else {
        disable()
    }
}

/// Restart the runner so it rediscovers installed units. Returns `false` (not an error) when it
/// isn't running — there is nothing to restart, and the store reports that as "installed, but the
/// runner is off" rather than as a failure.
///
/// Unit discovery happens once at runner startup ([`sdk/src/runner.ts`]), so this restart *is* the
/// activation step for a newly installed plugin.
pub(crate) fn restart_runtime() -> Result<bool> {
    let st = runtime_status();
    if !st.installed || !st.running {
        return Ok(false);
    }
    #[cfg(target_os = "linux")]
    {
        run_systemctl(&["restart", UNIT])?;
        Ok(true)
    }
    #[cfg(target_os = "windows")]
    {
        // Stop then start: `Restart-ScheduledTask` does not exist, and a Start on an already-
        // running task is a no-op rather than a restart.
        powershell(&format!(
            "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
             Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
        ))?;
        Ok(true)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Ok(false)
    }
}

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("failed to run systemctl (is systemd available in this session?)")?;
    if !status.success() {
        bail!(
            "systemctl --user {} failed — is the punktfunk-scripting package installed?",
            args.join(" ")
        );
    }
    Ok(())
}

/// Trimmed stdout of a `systemctl --user` query, or `None` if it couldn't run. These queries exit
/// non-zero for a normal "inactive"/"disabled" answer, so the status text is what matters.
#[cfg(target_os = "linux")]
fn systemctl_output(args: &[&str]) -> Option<String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `NT AUTHORITY\LocalService` — the runner task's principal — in icacls SID form.
#[cfg(target_os = "windows")]
const LOCAL_SERVICE_SID: &str = "*S-1-5-19";

/// The secrets the runner needs to read to reach the mgmt API: the scoped plugin token and the
/// host identity cert it pins TLS against — `native-cert.pem` when the identity split minted
/// one (what mgmt then serves), `cert.pem` on legacy hosts. `mgmt-token` (full admin) is
/// deliberately NOT here. The grant loop tolerates absent files, so listing both is safe on
/// either kind of host.
#[cfg(target_os = "windows")]
const RUNNER_SECRET_FILES: [&str; 3] = ["plugin-token", "native-cert.pem", "cert.pem"];

/// The unit directories the runner imports code from. LocalService gets an inheritable
/// read+execute+write-attributes grant on these: bun's module loader opens unit files
/// requesting FILE_WRITE_ATTRIBUTES on top of read (plain `(RX)` makes every import die with
/// EPERM — found on-glass), and WA can only touch timestamps/readonly bits, never content —
/// the runner's own integrity check (`windowsSddlUnsafeReason`) treats it as harmless.
#[cfg(target_os = "windows")]
const RUNNER_UNIT_DIRS: [&str; 2] = ["plugins", "scripts"];

/// The runner's writable state root: `<config_dir>\plugin-state`. A plugin persists its config +
/// cache under `plugin-state\<name>` (`@punktfunk/host`'s `pluginStateDir`), so LocalService needs
/// real **Modify** here — unlike the code dirs (RX,WA) and the secrets (R). This keeps the
/// three-way split crisp: code is read-only (a plugin can't rewrite itself), secrets are
/// read-only, only this one dir is writable. Inheritable so per-plugin subdirs the runner creates
/// carry the grant. Users stay read-only (config-dir default), so another non-admin still can't
/// tamper with a plugin's launch templates.
#[cfg(target_os = "windows")]
const RUNNER_STATE_DIRS: [&str; 1] = ["plugin-state"];

/// The plugin **ingest** inbox: `<config_dir>\ingest`. The INVERSE grant of `plugin-state` —
/// `BUILTIN\Users` gets **Modify**, so an app running as the interactive user (e.g. the Playnite
/// exporter, a Playnite extension) can drop data (`ingest\<plugin>\…`) that the de-privileged
/// LocalService runner then READS (LocalService is a member of Users, so it inherits read here).
/// This is the one place a plugin can receive data produced by *another* account — the runner can
/// no longer traverse the interactive user's profile the way the old SYSTEM runner could. Scoped
/// to this one inbox: the rest of the config tree stays Users-read-only, so the widening is a
/// well-defined drop box, not a general write hole. (Accepted tradeoff: any local user can drop a
/// file here — trusted-single-user model, and the runner it feeds is only LocalService.)
#[cfg(target_os = "windows")]
const RUNNER_INGEST_DIRS: [&str; 1] = ["ingest"];

/// `BUILTIN\Users` (S-1-5-32-545) in icacls SID form — the ingest inbox's writer.
#[cfg(target_os = "windows")]
const USERS_SID: &str = "*S-1-5-32-545";

#[cfg(target_os = "windows")]
fn enable() -> Result<()> {
    // Converge the task principal BEFORE starting it: the installer registers it as LocalService,
    // but a task from an older install (or a hand-registered dev box) still runs as SYSTEM, and
    // enabling that unmigrated would hand operator plugins the highest privilege on the box.
    // Idempotent; -LogonType ServiceAccount needs no stored password.
    powershell(&format!(
        "$p = New-ScheduledTaskPrincipal -UserId 'LocalService' -LogonType ServiceAccount; \
         Set-ScheduledTask -TaskName {TASK} -Principal $p -ErrorAction Stop | Out-Null"
    ))?;
    grant_runner_secret_reads();
    powershell(&format!(
        "Enable-ScheduledTask -TaskName {TASK} -ErrorAction Stop | Out-Null; \
         Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
    ))?;
    println!("Plugin runner enabled and started ({TASK}, runs as LocalService).");
    Ok(())
}

#[cfg(target_os = "windows")]
fn disable() -> Result<()> {
    powershell(&format!(
        "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         Disable-ScheduledTask -TaskName {TASK} -ErrorAction Stop | Out-Null"
    ))?;
    revoke_runner_secret_reads();
    println!("Plugin runner stopped and disabled ({TASK}).");
    Ok(())
}

/// Grant LocalService **read** on the runner's two secret files. Both are written by the host's
/// `serve` with a SYSTEM/Administrators-only DACL (`pf_paths::write_secret_file`), which the
/// de-privileged runner cannot read — this is the one, narrow widening it needs. `/grant:r`
/// replaces only LocalService's ACE, leaving the lockdown otherwise intact. Files the host hasn't
/// minted yet get an actionable note instead of a failed icacls: the grant re-runs on the next
/// `plugins enable`. NOTE: the host re-locks a secret's DACL whenever it rewrites the file (e.g.
/// a regenerated identity cert) — re-running `plugins enable` restores the grant.
#[cfg(target_os = "windows")]
fn grant_runner_secret_reads() {
    let cfg = pf_paths::config_dir();
    for name in RUNNER_SECRET_FILES {
        let path = cfg.join(name);
        if !path.exists() {
            println!(
                "note: {} does not exist yet (the host writes it on first serve). Start the \
                 host once, then run `punktfunk-host plugins enable` again so the runner can \
                 authenticate.",
                path.display()
            );
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&path)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(R)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService read on {} - the plugin runner may fail \
                 to authenticate to the management API",
                path.display()
            );
        }
    }
    // The unit dirs: inheritable (RX,WA) so the runner can import what lives there (see
    // RUNNER_UNIT_DIRS). Created here if absent — an elevated create inherits the config dir's
    // protected DACL, and granting now means files the operator adds later are covered by
    // inheritance rather than needing another `plugins enable`.
    for name in RUNNER_UNIT_DIRS {
        let dir = cfg.join(name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("warning: could not create {}: {e}", dir.display());
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX,WA)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService read on {} - the runner may fail to \
                 import plugins/scripts from it",
                dir.display()
            );
        }
    }
    // The state root: inheritable Modify so plugins can persist config/cache under
    // `plugin-state\<name>` (see RUNNER_STATE_DIRS). This is the ONLY writable grant.
    for name in RUNNER_STATE_DIRS {
        let dir = cfg.join(name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("warning: could not create {}: {e}", dir.display());
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(M)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService write on {} - state-writing plugins \
                 (config/cache) may fail to persist",
                dir.display()
            );
        }
    }
    // The ingest inbox: inheritable Modify for BUILTIN\Users, so an interactive-user app (the
    // Playnite exporter) can drop `ingest\<plugin>\…` for the LocalService runner to read (see
    // RUNNER_INGEST_DIRS). The one Users-writable carve-out in the otherwise Users-read-only tree.
    for name in RUNNER_INGEST_DIRS {
        let dir = cfg.join(name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("warning: could not create {}: {e}", dir.display());
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{USERS_SID}:(OI)(CI)(M)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not open the ingest inbox {} for writes - a plugin fed by an \
                 interactive-user app (e.g. playnite) may see no data",
                dir.display()
            );
        }
    }
    // The runner's OWN bundle, in the install dir rather than under the config dir. Same reason as
    // RUNNER_UNIT_DIRS: bun opens the file it is asked to run requesting FILE_WRITE_ATTRIBUTES on
    // top of read, and {app}\scripting only carries Users:(RX), which LocalService reaches through
    // Authenticated Users. So `bun runner-cli.js` died with
    //   error: EPERM reading "C:\Program Files\punktfunk\scripting\runner-cli.js"
    // the task exited 1 within a second of every start, and the console showed the runner as
    // enabled-but-not-running with nothing to explain it. The unit dirs were given (RX,WA) when
    // that behaviour was first found on-glass; the entry script itself was missed, so the runner
    // could never start at all. Verified on glass: without this the task is Ready/lastResult=1,
    // with it the task is Running and `GET /store/runtime` reports running:true.
    // WA touches timestamps and the read-only bit, never content, so "code is read-only — a plugin
    // cannot rewrite itself" still holds for the runner's own bundle.
    if let Some(dir) = runner_bundle_dir() {
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX,WA)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService read on {} - the plugin runner will not \
                 start (bun exits EPERM on its own entry script)",
                dir.display()
            );
        }
    }
}

/// `{app}\scripting` — where the installer lays down `runner-cli.js` + `scripting-run.cmd`
/// (packaging/windows/punktfunk-host.iss), resolved from the running exe like
/// [`runner_command`] does. `None` when the exe path cannot be resolved; callers treat that as
/// "nothing to grant" rather than failing the whole enable.
#[cfg(target_os = "windows")]
fn runner_bundle_dir() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("scripting"))
}

/// Best-effort removal of the LocalService read grants when the runner is switched off — the
/// mirror of [`grant_runner_secret_reads`]; `enable` re-grants.
#[cfg(target_os = "windows")]
fn revoke_runner_secret_reads() {
    let cfg = pf_paths::config_dir();
    for name in RUNNER_SECRET_FILES
        .iter()
        .chain(RUNNER_UNIT_DIRS.iter())
        .chain(RUNNER_STATE_DIRS.iter())
    {
        let path = cfg.join(name);
        if !path.exists() {
            continue;
        }
        let _ = Command::new(icacls_path())
            .arg(&path)
            .args(["/remove:g", LOCAL_SERVICE_SID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // The ingest inbox was opened to Users, not LocalService — remove that explicit grant (the
    // inherited Users:RX from the config dir remains, so it reverts to read-only, not orphaned).
    for name in RUNNER_INGEST_DIRS {
        let path = cfg.join(name);
        if !path.exists() {
            continue;
        }
        let _ = Command::new(icacls_path())
            .arg(&path)
            .args(["/remove:g", USERS_SID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // …and the bundle dir in the install tree, which is not under `cfg` so the loop above misses
    // it. Removing the explicit ACE leaves the inherited Users:(RX) from Program Files, so it
    // reverts to plain read-only rather than losing access altogether.
    if let Some(dir) = runner_bundle_dir().filter(|d| d.exists()) {
        let _ = Command::new(icacls_path())
            .arg(&dir)
            .args(["/remove:g", LOCAL_SERVICE_SID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Resolve icacls by full System32 path rather than PATH — same planted-binary reasoning as
/// [`powershell_path`]; matches `pf_paths`.
#[cfg(target_os = "windows")]
fn icacls_path() -> String {
    std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string())
}

/// Resolve powershell by full System32 path rather than PATH — CreateProcess searches the launching
/// EXE's own directory first, so a planted `powershell.exe` beside the host binary would otherwise
/// run with our privileges (security-review 2026-07-17; matches service.rs / pf_vdisplay).
#[cfg(target_os = "windows")]
fn powershell_path() -> String {
    std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\WindowsPowerShell\v1.0\powershell.exe"))
        .unwrap_or_else(|_| "powershell.exe".to_string())
}

#[cfg(target_os = "windows")]
fn powershell(command: &str) -> Result<()> {
    let status = Command::new(powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .status()
        .context("failed to run powershell")?;
    if !status.success() {
        bail!(
            "the {TASK} scheduled task couldn't be changed — is punktfunk installed with the \
             scripting component, and is this prompt elevated?"
        );
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn powershell_output(command: &str) -> Option<String> {
    let out = Command::new(powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---- elevation --------------------------------------------------------------------------------

/// Refuse early, with an actionable message, when an admin-only operation is run unelevated. We do
/// NOT self-elevate via UAC: that spawns a separate console window which closes on exit, hiding
/// bun's install output and any error the user needs to read.
#[cfg(target_os = "windows")]
fn require_elevation(what: &str) -> Result<()> {
    if is_elevated() {
        return Ok(());
    }
    // ASCII only: the Windows console's default codepage drops non-ASCII (an em-dash or arrow
    // renders as a blank), which mangles the one message the user most needs to read.
    bail!(
        "{what} needs administrator rights (the plugins directory under %ProgramData%\\punktfunk \
         and the runner task are admin-owned).\n\nOpen an elevated prompt: Start -> type \
         \"PowerShell\" -> right-click -> Run as administrator, then run this command again."
    )
}

/// Does this process have local-Administrator rights *in effect*?
///
/// Deliberately `CheckTokenMembership` against the built-in Administrators group, NOT
/// `GetTokenInformation(TokenElevation)`. `TokenElevation` answers "was this token elevated via
/// UAC", which is not the same question: a restricted/SAFER token derived from an elevated one
/// (`runas /trustlevel:0x20000`) still reports `TokenIsElevated = 1` while the Administrators SID
/// is deny-only, so the guard waved through a process that then failed on the ACL'd plugins dir.
/// Verified on-glass 2026-07-19: under such a token this returns false where `TokenElevation`
/// returned true. `CheckTokenMembership(None, …)` uses the effective token and honors deny-only
/// SIDs — the same test PowerShell's `IsInRole([…]::Administrator)` performs.
#[cfg(target_os = "windows")]
fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SID_IDENTIFIER_AUTHORITY,
    };

    // The well-known BUILTIN\Administrators SID, S-1-5-32-544: NT authority (5) + the
    // SECURITY_BUILTIN_DOMAIN_RID (32) and DOMAIN_ALIAS_RID_ADMINS (544) sub-authorities. Spelled
    // out rather than imported so this doesn't depend on which module the crate exposes the RID
    // constants from.
    const NT_AUTHORITY: SID_IDENTIFIER_AUTHORITY = SID_IDENTIFIER_AUTHORITY {
        Value: [0, 0, 0, 0, 0, 5],
    };
    const BUILTIN_DOMAIN_RID: u32 = 32;
    const ALIAS_RID_ADMINS: u32 = 544;

    let mut admins = PSID::default();
    // SAFETY: AllocateAndInitializeSid is given a valid authority and exactly the 2 sub-authorities
    // its count argument declares (the remaining 6 are the API's required zero padding). On success
    // it yields a valid PSID that we pass to CheckTokenMembership and free on every path below;
    // `None` for the token means "the calling thread's effective token".
    unsafe {
        if AllocateAndInitializeSid(
            &NT_AUTHORITY,
            2,
            BUILTIN_DOMAIN_RID,
            ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admins,
        )
        .is_err()
        {
            return false;
        }
        let mut is_member = windows::core::BOOL::default();
        let ok = CheckTokenMembership(Some(HANDLE::default()), admins, &mut is_member).is_ok();
        FreeSid(admins);
        ok && is_member.as_bool()
    }
}

// Non-Linux, non-Windows (macOS dev builds): the runner and its service manager don't exist there.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn enable() -> Result<()> {
    bail!("the plugin runner is only available on Linux and Windows hosts")
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn disable() -> Result<()> {
    bail!("the plugin runner is only available on Linux and Windows hosts")
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Build an `exists` probe over a fixed set of paths.
    fn present(ps: Vec<PathBuf>) -> impl Fn(&Path) -> bool {
        move |p: &Path| ps.iter().any(|q| q == p)
    }

    /// Every layout the resolver has to serve, in one table — the regression guard for the NixOS
    /// report (a runner on `PATH` and nowhere else read as "not installed").
    #[test]
    fn runner_resolution_table() {
        let beside = Path::new("/opt/punktfunk/bin");
        let nix = Path::new("/run/current-system/sw/bin");
        let home = Path::new("/home/deck");

        // The rung a Nix install lands on: NOT beside the host binary, NOT under /usr — `PATH`
        // only. This is the whole bug.
        let exists = present(vec![nix.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(beside),
                Some(nix.to_str().unwrap()),
                None,
                &exists
            ),
            Some((nix.join(RUNNER_BIN), Vec::new()))
        );

        // An explicit override wins over every discovery…
        let exists = present(vec![beside.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                Some("/nix/store/abc/bin/punktfunk-scripting"),
                Some(beside),
                Some("/usr/bin"),
                Some(home),
                &exists
            ),
            Some(("/nix/store/abc/bin/punktfunk-scripting".into(), Vec::new()))
        );
        // …and is not existence-checked, so a typo surfaces as a spawn failure naming the path
        // rather than silently running some other runner.
        assert_eq!(
            resolve_runner_in(Some("/nope/pf"), Some(beside), None, None, &exists),
            Some(("/nope/pf".into(), Vec::new()))
        );
        // Empty/whitespace reads as unset, not as a path.
        assert_eq!(
            resolve_runner_in(Some("  "), Some(beside), None, None, &exists),
            Some((beside.join(RUNNER_BIN), Vec::new()))
        );
        // Beside the host binary beats PATH.
        let exists = present(vec![beside.join(RUNNER_BIN), nix.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(beside),
                Some(nix.to_str().unwrap()),
                None,
                &exists
            ),
            Some((beside.join(RUNNER_BIN), Vec::new()))
        );
        // PATH is walked entry by entry, skipping empties.
        let exists = present(vec![nix.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(Path::new("/nowhere")),
                Some(":/nope:/run/current-system/sw/bin"),
                None,
                &exists
            ),
            Some((nix.join(RUNNER_BIN), Vec::new()))
        );

        // The deb/rpm wrapper, found even when the unit's PATH omits /usr/bin.
        let exists = present(vec![PathBuf::from("/usr/bin").join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(Path::new("/nowhere")),
                Some("/nope"),
                None,
                &exists
            ),
            Some((PathBuf::from("/usr/bin").join(RUNNER_BIN), Vec::new()))
        );
        // …and its private two-file layout when the wrapper is absent.
        let bun = PathBuf::from("/usr/lib").join(RUNNER_BIN).join("bun");
        let cli = PathBuf::from("/usr/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js");
        let exists = present(vec![bun.clone(), cli.clone()]);
        assert_eq!(
            resolve_runner_in(None, None, None, None, &exists),
            Some((bun, vec![cli.to_string_lossy().into_owned()]))
        );
        // Half of that layout is not a rung — a partial install must fall through, not spawn a
        // bun with no script.
        let exists = present(vec![PathBuf::from("/usr/lib").join(RUNNER_BIN).join("bun")]);
        assert_eq!(resolve_runner_in(None, None, None, None, &exists), None);

        // SteamOS: the same payload, user-scoped. Reached only via HOME.
        let exists = present(vec![home.join(".local/bin").join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(None, None, None, Some(home), &exists),
            Some((home.join(".local/bin").join(RUNNER_BIN), Vec::new()))
        );
        assert_eq!(resolve_runner_in(None, None, None, None, &exists), None);
        let bun = home.join(".local/lib").join(RUNNER_BIN).join("bun");
        let cli = home
            .join(".local/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js");
        let exists = present(vec![bun.clone(), cli.clone()]);
        assert_eq!(
            resolve_runner_in(None, None, None, Some(home), &exists),
            Some((bun, vec![cli.to_string_lossy().into_owned()]))
        );

        // Nothing anywhere: the "not installed" rung the error text speaks for.
        let exists = present(vec![]);
        assert_eq!(
            resolve_runner_in(None, Some(beside), Some("/nope"), Some(home), &exists),
            None
        );
    }

    /// The operator-facing miss must name NixOS — the report's second half was that the error
    /// pointed a NixOS operator at `apt`.
    #[test]
    fn the_missing_runner_error_names_every_platform_it_can_be_installed_on() {
        for hint in [
            "apt install",
            "steamdeck/install.sh",
            "NixOS",
            "PUNKTFUNK_SCRIPTING",
        ] {
            assert!(RUNNER_MISSING.contains(hint), "missing hint: {hint}");
        }
    }
}
