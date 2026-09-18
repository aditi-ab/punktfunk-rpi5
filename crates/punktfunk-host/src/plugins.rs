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

pub mod manifest;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use self::windows as plat;
#[cfg(not(target_os = "windows"))]
mod posix;
#[cfg(not(target_os = "windows"))]
use self::posix as plat;

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("add") | Some("remove") | Some("rm") | Some("uninstall") | Some("list")
        | Some("ls") => {
            let listing = matches!(args.first().map(String::as_str), Some("list") | Some("ls"));
            if !listing {
                plat::require_elevation("installing or removing plugins")?;
            }
            forward_to_runner(args)?;
            if !listing {
                // The runner discovers units at startup; without this the change is dormant.
                match restart_runtime() {
                    Ok(true) => println!("Plugin runner restarted."),
                    Ok(false) => println!("The plugin runner is off — `plugins enable` starts it."),
                    Err(e) => println!("Couldn't restart the plugin runner: {e:#}"),
                }
            }
            Ok(())
        }
        Some("enable") => {
            plat::require_elevation("enabling the plugin runner")?;
            plat::enable()
        }
        Some("disable") => {
            plat::require_elevation("disabling the plugin runner")?;
            plat::disable()
        }
        Some("status") => status(),
        Some("grant") => grant(
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        Some("-h") | Some("--help") | Some("help") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => bail!("unknown plugins command '{other}' (try `plugins --help`)"),
    }
}

/// `plugins grant <plugin> <dir>` — the operator's answer to "this package may also reach here".
///
/// A package declares the standard locations it knows; where someone keeps their ROMs or installs
/// their games is not something it can know, and this is how that path becomes usable without the
/// package asking for the home directory.
fn grant(plugin: Option<&str>, dir: Option<&str>) -> Result<()> {
    let (Some(plugin), Some(dir)) = (
        plugin.map(str::trim).filter(|s| !s.is_empty()),
        dir.map(str::trim).filter(|s| !s.is_empty()),
    ) else {
        bail!("usage: punktfunk-host plugins grant <plugin> <dir>");
    };
    let path = std::path::Path::new(dir);
    // A typo must not report success: the grant would name a directory nothing ever reads.
    if !path.is_dir() {
        bail!("'{dir}' is not a directory (grant the folder, not a file inside it)");
    }
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let roots = manifest::grant_root(plugin, &path)
        .with_context(|| format!("record the grant for '{plugin}'"))?;
    plat::grant(Some(dir))?;
    println!("{plugin} may now reach:");
    for root in roots {
        println!("  {root}");
    }
    println!("Re-run the plugin's scan (or restart the runner) to pick it up.");
    Ok(())
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
    punktfunk-host plugins grant <plugin> <dir>
                                             let one plugin reach a directory of yours (a ROM
                                             library, a game install dir) — nothing else on
                                             this box changes

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
    plat::usage_note();
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
        .with_context(|| format!("run the plugin runner ({})", program.display()))?;
    if !status.success() {
        // The runner already printed the reason; do not add a second error line.
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// The runner's launch line. Also the store job executor ([`crate::store::jobs`]): console
/// installs use this same invocation so the box has one "install a plugin" path.
pub(crate) fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
    plat::runner_command()
}

// ---- service ops ------------------------------------------------------------------------------

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

pub(crate) fn runtime_status() -> RuntimeStatus {
    plat::runtime_status()
}

/// [`enable`]/[`disable`], also `POST /store/runtime`. Windows: the SYSTEM service
/// already clears the elevation bar the CLI checks.
pub(crate) fn set_runtime_enabled(enabled: bool) -> Result<()> {
    if enabled {
        plat::enable()
    } else {
        plat::disable()
    }
}

/// Restart so the runner rediscovers units. `false` when it is off — not an error;
/// the store reports "installed, but off".
///
/// Discovery runs once at runner startup ([`sdk/src/runner.ts`]); this restart is
/// how a newly installed plugin becomes active. An enabled runner that is not running
/// (installed after login, crashed out) is started, not skipped.
pub(crate) fn restart_runtime() -> Result<bool> {
    let st = runtime_status();
    if !st.installed || !st.enabled {
        return Ok(false);
    }
    plat::restart_runtime()?;
    Ok(true)
}
