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
//! `%ProgramData%\punktfunk` (see `pf_paths::create_private_dir`) and the task runs as SYSTEM. We
//! check up front and print one actionable line instead of letting `bun add` fail with a bare
//! EACCES.

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
    @punktfunk/plugin-playnite, `rom-manager` installs @punktfunk/plugin-rom-manager.
    A scoped (@scope/pkg) or `punktfunk-plugin-*` name is used verbatim.

NOTES:
    Plugins run under the runner, which is OPT-IN — `plugins add` installs, `plugins enable`
    turns the runner on. Plugins are operator-installed code that runs as the host user;
    install only plugins you trust.
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

#[cfg(target_os = "windows")]
fn enable() -> Result<()> {
    powershell(&format!(
        "Enable-ScheduledTask -TaskName {TASK} -ErrorAction Stop | Out-Null; \
         Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
    ))?;
    println!("Plugin runner enabled and started ({TASK}).");
    Ok(())
}

#[cfg(target_os = "windows")]
fn disable() -> Result<()> {
    powershell(&format!(
        "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         Disable-ScheduledTask -TaskName {TASK} -ErrorAction Stop | Out-Null"
    ))?;
    println!("Plugin runner stopped and disabled ({TASK}).");
    Ok(())
}

#[cfg(target_os = "windows")]
fn status() -> Result<()> {
    let out = powershell_output(&format!(
        "$t = Get-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         if ($null -eq $t) {{ 'missing' }} else {{ \
           $i = Get-ScheduledTaskInfo -TaskName {TASK} -ErrorAction SilentlyContinue; \
           \"$($t.State)\" }}"
    ));
    match out.as_deref().map(str::trim) {
        Some("missing") | None => {
            println!(
                "runner:  {TASK}\nstate:   not installed\n\nReinstall punktfunk with the \
                 scripting component to get the plugin runner."
            );
        }
        Some(state) => {
            println!("runner:  {TASK}\nstate:   {state}");
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
