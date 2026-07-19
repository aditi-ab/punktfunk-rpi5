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
//! older installer registered as SYSTEM) and grants LocalService read on exactly the two files
//! the runner's `connect()` needs — the scoped `plugin-token` and the TLS pin `cert.pem` — never
//! the full-admin `mgmt-token`.

use anyhow::{bail, Context, Result};
use std::process::Command;

/// The systemd user unit / Windows scheduled task that supervises plugins.
#[cfg(target_os = "linux")]
const UNIT: &str = "punktfunk-scripting";
#[cfg(target_os = "windows")]
const TASK: &str = "PunktfunkScripting";

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
fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
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
        // The scripting package ships /usr/bin/punktfunk-scripting, a wrapper that runs the bundled
        // bun on the runner bundle and forwards "$@" (packaging/debian/build-scripting-deb.sh).
        let wrapper = std::path::PathBuf::from("/usr/bin/punktfunk-scripting");
        if wrapper.exists() {
            return Ok((wrapper, Vec::new()));
        }
        // Fall back to the package's private layout in case the wrapper is absent.
        let bun = std::path::PathBuf::from("/usr/lib/punktfunk-scripting/bun");
        let runner = std::path::PathBuf::from("/usr/share/punktfunk-scripting/runner-cli.js");
        if bun.exists() && runner.exists() {
            return Ok((bun, vec![runner.to_string_lossy().into_owned()]));
        }
        bail!(
            "the plugin runner isn't installed — install it first (Debian/Ubuntu: \
             `sudo apt install punktfunk-scripting`)"
        )
    }
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

#[cfg(target_os = "linux")]
fn status() -> Result<()> {
    let active = systemctl_output(&["is-active", UNIT]).unwrap_or_else(|| "unknown".into());
    let enabled = systemctl_output(&["is-enabled", UNIT]).unwrap_or_else(|| "unknown".into());
    println!("runner:  {UNIT}\nenabled: {enabled}\nactive:  {active}");
    if active != "active" {
        println!("\nStart it with: punktfunk-host plugins enable");
    }
    Ok(())
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

/// The two (and only two) secrets the runner needs to read to reach the mgmt API: the scoped
/// plugin token and the host identity cert it pins TLS against. `mgmt-token` (full admin) is
/// deliberately NOT here.
#[cfg(target_os = "windows")]
const RUNNER_SECRET_FILES: [&str; 2] = ["plugin-token", "cert.pem"];

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
}

/// Resolve icacls by full System32 path rather than PATH — same planted-binary reasoning as
/// [`powershell_path`]; matches `pf_paths`.
#[cfg(target_os = "windows")]
fn icacls_path() -> String {
    std::env::var("SystemRoot")
        .map(|r| format!(r"{r}\System32\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string())
}

#[cfg(target_os = "windows")]
fn status() -> Result<()> {
    let out = powershell_output(&format!(
        "$t = Get-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         if ($null -eq $t) {{ 'missing' }} else {{ \
           \"$($t.State)|$($t.Principal.UserId)\" }}"
    ));
    match out.as_deref().map(str::trim) {
        Some("missing") | None => {
            println!(
                "runner:  {TASK}\nstate:   not installed\n\nReinstall punktfunk with the \
                 scripting component to get the plugin runner."
            );
        }
        Some(state) => {
            // "State|Principal" — the principal line makes the SYSTEM→LocalService migration
            // verifiable at a glance (`plugins enable` converges a legacy SYSTEM task).
            let (state, principal) = state.split_once('|').unwrap_or((state, "?"));
            println!("runner:  {TASK}\nstate:   {state}\nruns as: {principal}");
            if state.eq_ignore_ascii_case("Disabled") {
                println!("\nEnable it with: punktfunk-host plugins enable");
            }
        }
    }
    Ok(())
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

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn status() -> Result<()> {
    bail!("the plugin runner is only available on Linux and Windows hosts")
}
