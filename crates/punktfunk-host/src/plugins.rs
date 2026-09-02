//! `punktfunk-host plugins …` — install plugins and opt in the runner.
//!
//! Package ops (`add`/`remove`/`list`) go to the bun runner (`sdk/src/plugins.ts`):
//! this binary locates it; it owns the vendored bun, `@punktfunk` scope, and plugins dir.
//! Service ops (`enable`/`disable`/`status`) run here — `systemctl --user` or the
//! `PunktfunkScripting` scheduled task — so they work without the runner package.
//!
//! Windows: both halves need elevation (`%ProgramData%\punktfunk` is ACL'd;
//! the task is admin-owned). Refuse unelevated rather than a bare EACCES from `bun add`.
//!
//! The task runs as `NT AUTHORITY\LocalService`, not SYSTEM. `enable` converges the
//! principal and grants LocalService read on `plugin-token` plus the TLS-pin cert
//! (`native-cert.pem` or legacy `cert.pem`) — never `mgmt-token`.
//!
//! Runner discovery is pinned in this module's tests.

use anyhow::{bail, Context, Result};
use std::process::Command;

#[cfg(target_os = "linux")]
const UNIT: &str = "punktfunk-scripting";
#[cfg(target_os = "windows")]
const TASK: &str = "PunktfunkScripting";

/// Wrapper name every non-Windows package installs (`/usr/bin`, `~/.local/bin`, `$out/bin`).
#[cfg(not(target_os = "windows"))]
const RUNNER_BIN: &str = "punktfunk-scripting";

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") | Some("remove") | Some("rm") | Some("uninstall") | Some("list")
        | Some("ls") => {
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

fn forward_to_runner(args: &[String]) -> Result<()> {
    // `bun add` walks up to the nearest `package.json`, so seed the plugins dir first or a
    // stray `~/package.json` captures the install (exit 0). The installed runner may predate
    // this binary (`store::ensure_plugin_root`).
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
        // The runner already printed the reason; do not add a second error line.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Bundled bun needs the runner script path as a leading arg.
///
/// Also the store job executor ([`crate::store::jobs`]): console installs use this same
/// invocation so the box has one "install a plugin" path.
pub(crate) fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
    #[cfg(target_os = "windows")]
    {
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
        // Tail expression, not `return`: after cfg-stripping this is the whole fn body,
        // and `return` trips clippy needless_return under -D warnings.
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

/// Shared with [`runtime_status`] so CLI and console say the same thing.
#[cfg(not(target_os = "windows"))]
pub(crate) const RUNNER_MISSING: &str =
    "the plugin runner isn't installed — install it first (Debian/Ubuntu: `sudo apt install \
     punktfunk-scripting`; SteamOS: re-run scripts/steamdeck/install.sh; NixOS: enable \
     `services.punktfunk.scripting`). If it is installed somewhere else, point PUNKTFUNK_SCRIPTING \
     at the punktfunk-scripting executable.";

/// Rungs: `PUNKTFUNK_SCRIPTING` → beside the host → `PATH` → `/usr` → `~/.local`.
/// Injected so tests do not mutate process env (races `getenv` in parallel).
///
/// `PATH` is the only rung a Nix install can land on: `punktfunk-scripting` is its
/// own derivation, neither beside the host nor under `/usr`.
#[cfg(not(target_os = "windows"))]
fn resolve_runner_in(
    env: Option<&str>,
    exe_dir: Option<&std::path::Path>,
    path_var: Option<&str>,
    home: Option<&std::path::Path>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Option<(std::path::PathBuf, Vec<String>)> {
    use std::path::{Path, PathBuf};

    // Two-file layout (private bun + runner bundle). A rung only when both exist.
    let pair = |bun: PathBuf, runner: PathBuf| -> Option<(PathBuf, Vec<String>)> {
        (exists(&bun) && exists(&runner))
            .then(|| (bun, vec![runner.to_string_lossy().into_owned()]))
    };

    // Operator override: not existence-checked, so a typo fails naming that path
    // instead of silently using some other installed runner.
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        return Some((PathBuf::from(v), Vec::new()));
    }
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
    // Packaged `/usr` after `PATH`: a systemd unit PATH may omit `/usr/bin`.
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
    // Immutable `/usr` (SteamOS): the same payload, user-scoped under `~/.local`.
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

/// Data for the store console (offer enable before first install; explain why a
/// just-installed plugin is not running). Not formatted for stdout.
#[derive(Debug, Clone)]
pub(crate) struct RuntimeStatus {
    pub installed: bool,
    /// systemd `enabled`, or a non-`Disabled` scheduled task.
    pub enabled: bool,
    pub running: bool,
    pub unit: &'static str,
    pub principal: Option<String>,
    pub detail: String,
}

#[cfg(target_os = "linux")]
pub(crate) fn runtime_status() -> RuntimeStatus {
    let enabled_raw = systemctl_output(&["is-enabled", UNIT]);
    let active = systemctl_output(&["is-active", UNIT]).unwrap_or_default();
    // `is-enabled` is `not-found` when the unit file is missing; the runner payload
    // is the other half of "can we install plugins".
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

/// [`enable`]/[`disable`], also `POST /store/runtime`. Windows: the SYSTEM service
/// already clears the elevation bar the CLI checks.
pub(crate) fn set_runtime_enabled(enabled: bool) -> Result<()> {
    if enabled {
        enable()
    } else {
        disable()
    }
}

/// Restart so the runner rediscovers units. `false` when it is not running — not an
/// error; the store reports "installed, but off".
///
/// Discovery runs once at runner startup ([`sdk/src/runner.ts`]); this restart is
/// how a newly installed plugin becomes active.
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
        // Stop then start: there is no `Restart-ScheduledTask`, and Start on a
        // running task is a no-op.
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

/// Trimmed `systemctl --user` stdout, or `None` if it could not run. Queries exit
/// non-zero for a normal "inactive"/"disabled", so the text is the answer.
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

/// `NT AUTHORITY\LocalService` in icacls SID form.
#[cfg(target_os = "windows")]
const LOCAL_SERVICE_SID: &str = "*S-1-5-19";

/// Secrets the runner may read: scoped `plugin-token` and the TLS-pin cert
/// (`native-cert.pem` after the identity split, else `cert.pem`). Never `mgmt-token`.
/// Absent files are skipped, so listing both certs is safe on either host.
#[cfg(target_os = "windows")]
const RUNNER_SECRET_FILES: [&str; 3] = ["plugin-token", "native-cert.pem", "cert.pem"];

/// Unit dirs the runner imports. Inheritable `(RX,WA)`: bun's loader opens unit
/// files with FILE_WRITE_ATTRIBUTES; plain `(RX)` is EPERM on every import. WA
/// can only touch timestamps/readonly bits — `windowsSddlUnsafeReason` treats it
/// as harmless.
#[cfg(target_os = "windows")]
const RUNNER_UNIT_DIRS: [&str; 2] = ["plugins", "scripts"];

/// Writable state: `<config_dir>\plugin-state`. Plugins persist under
/// `plugin-state\<name>`, so LocalService needs Modify here — code dirs are
/// (RX,WA), secrets are (R). Inheritable onto per-plugin subdirs. Users stay
/// read-only (config-dir default).
#[cfg(target_os = "windows")]
const RUNNER_STATE_DIRS: [&str; 1] = ["plugin-state"];

/// Ingest inbox: `<config_dir>\ingest`. Inverse of `plugin-state`: `BUILTIN\Users`
/// gets Modify so an interactive-user app can drop `ingest\<plugin>\…` for the
/// LocalService runner to read. The rest of the config tree stays Users-read-only.
/// Any local user can drop a file here (trusted-single-user; the reader is LocalService).
#[cfg(target_os = "windows")]
const RUNNER_INGEST_DIRS: [&str; 1] = ["ingest"];

/// `BUILTIN\Users` (S-1-5-32-545) in icacls SID form — the ingest inbox's writer.
#[cfg(target_os = "windows")]
const USERS_SID: &str = "*S-1-5-32-545";

#[cfg(target_os = "windows")]
fn enable() -> Result<()> {
    // Converge the principal before start: an older task may still be SYSTEM.
    // Idempotent; `-LogonType ServiceAccount` needs no stored password.
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

/// Grant LocalService read on the runner secrets. `serve` writes them with a
/// SYSTEM/Administrators-only DACL (`pf_paths::write_secret_file`); `/grant:r`
/// replaces only LocalService's ACE. A later rewrite of the file drops the ACE —
/// re-run `plugins enable`. Missing files get a note; the grant retries next enable.
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
    // Unit dirs: inheritable (RX,WA). Create now so later files inherit rather
    // than needing another `plugins enable`.
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
    // `{app}\scripting` is not under the config dir. Same (RX,WA) as the unit
    // dirs: bun opens the entry script with FILE_WRITE_ATTRIBUTES, and the
    // install tree only carries Users:(RX). WA cannot change content.
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

/// `None` if the exe path cannot be resolved; callers skip the grant rather than fail enable.
#[cfg(target_os = "windows")]
fn runner_bundle_dir() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("scripting"))
}

/// Drop the LocalService grants when the runner is switched off. `enable` re-grants.
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
    // Ingest was granted to Users, not LocalService. Removing that ACE leaves
    // the inherited Users:RX, so the dir reverts to read-only.
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
    // Bundle dir is not under `cfg`. Removing the ACE leaves inherited
    // Users:(RX) from Program Files — read-only, not inaccessible.
    if let Some(dir) = runner_bundle_dir().filter(|d| d.exists()) {
        let _ = Command::new(icacls_path())
            .arg(&dir)
            .args(["/remove:g", LOCAL_SERVICE_SID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// System32 `icacls`, not PATH — same planted-binary rule as [`powershell_path`].
#[cfg(target_os = "windows")]
fn icacls_path() -> String {
    std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string())
}

/// System32 powershell, not PATH. CreateProcess searches the launching EXE's
/// directory first, so a planted `powershell.exe` beside the host would run
/// with these privileges.
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

/// Refuse unelevated admin-only ops. Do not self-elevate via UAC: that opens a
/// new console that closes on exit, hiding bun's output.
#[cfg(target_os = "windows")]
fn require_elevation(what: &str) -> Result<()> {
    if is_elevated() {
        return Ok(());
    }
    // ASCII only: the default Windows console codepage drops em-dashes and arrows.
    bail!(
        "{what} needs administrator rights (the plugins directory under %ProgramData%\\punktfunk \
         and the runner task are admin-owned).\n\nOpen an elevated prompt: Start -> type \
         \"PowerShell\" -> right-click -> Run as administrator, then run this command again."
    )
}

/// Effective local-Administrator membership via `CheckTokenMembership`.
///
/// Not `TokenElevation`: a restricted/SAFER token from an elevated one
/// (`runas /trustlevel:0x20000`) still reports `TokenIsElevated = 1` while
/// Administrators is deny-only.
#[cfg(target_os = "windows")]
fn is_elevated() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SID_IDENTIFIER_AUTHORITY,
    };

    // BUILTIN\Administrators, S-1-5-32-544. Spelled out so this does not depend
    // on which windows crate module exports the RID constants.
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

    fn present(ps: Vec<PathBuf>) -> impl Fn(&Path) -> bool {
        move |p: &Path| ps.iter().any(|q| q == p)
    }

    /// Layouts the resolver must serve. Nix lands on `PATH` and nowhere else.
    #[test]
    fn runner_resolution_table() {
        let beside = Path::new("/opt/punktfunk/bin");
        let nix = Path::new("/run/current-system/sw/bin");
        let home = Path::new("/home/deck");

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
        // Override is not existence-checked: a typo fails naming that path.
        assert_eq!(
            resolve_runner_in(Some("/nope/pf"), Some(beside), None, None, &exists),
            Some(("/nope/pf".into(), Vec::new()))
        );
        // Empty/whitespace is unset, not a path.
        assert_eq!(
            resolve_runner_in(Some("  "), Some(beside), None, None, &exists),
            Some((beside.join(RUNNER_BIN), Vec::new()))
        );
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
        // PATH is walked entry by entry; empty entries skipped.
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

        // Packaged wrapper even when the unit PATH omits `/usr/bin`.
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
        // Private two-file layout when the wrapper is absent.
        let bun = PathBuf::from("/usr/lib").join(RUNNER_BIN).join("bun");
        let cli = PathBuf::from("/usr/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js");
        let exists = present(vec![bun.clone(), cli.clone()]);
        assert_eq!(
            resolve_runner_in(None, None, None, None, &exists),
            Some((bun, vec![cli.to_string_lossy().into_owned()]))
        );
        // Half of that layout is not a rung — do not spawn bun with no script.
        let exists = present(vec![PathBuf::from("/usr/lib").join(RUNNER_BIN).join("bun")]);
        assert_eq!(resolve_runner_in(None, None, None, None, &exists), None);

        // SteamOS payload is user-scoped; reached only via HOME.
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

        let exists = present(vec![]);
        assert_eq!(
            resolve_runner_in(None, Some(beside), Some("/nope"), Some(home), &exists),
            None
        );
    }

    /// Miss text must name every install path, including NixOS (not only `apt`).
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
