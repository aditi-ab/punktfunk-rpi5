//! punktfunk-tray — a small per-user system-tray companion for the punktfunk host service.
//!
//! Shows at a glance whether the host is running / stopped / degraded / failed (no more digging
//! through logs after a reboot or an update), and offers the common one-click actions: open the
//! web console, start/stop/restart the service (UAC-elevated per action on Windows,
//! `systemctl --user` on Linux), review a pending pairing request, exit.
//!
//! Status comes from two sources, service manager FIRST (a fake listener on the mgmt port can
//! never make a stopped service look running): the SCM / systemd user unit for the process state,
//! then the host's loopback-only unauthenticated `GET /api/v1/local/summary` for the streaming
//! details. Windows-subsystem binary — a console exe in the HKLM Run key would flash a terminal
//! window at every sign-in.
// Unsafe-proof program: every `unsafe {}` in the tray carries a `// SAFETY:` proof.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(windows, target_os = "linux"))]
mod status;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod win_theme;

/// CLI configuration (hand-rolled parse, house style). The mgmt address/port default to the
/// host's defaults; they are flags because the tray cannot read `host.env` on Windows (it is
/// DACL-locked to SYSTEM/Administrators), so an operator who moved `--mgmt-bind` adjusts the
/// autostart command line instead.
pub struct Args {
    /// Ask an already-running tray instance to exit (Windows; used by the uninstaller).
    pub quit: bool,
    /// Launched from the desktop autostart entry: exit silently when this box doesn't run a host
    /// (Linux; the package installs the autostart file for every desktop user).
    pub autostart: bool,
    /// Management API address to poll (loopback only; the summary route rejects anything else).
    pub mgmt_addr: String,
    pub mgmt_port: u16,
    /// Web console port for the "Open web console" action.
    pub web_port: u16,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            quit: false,
            autostart: false,
            mgmt_addr: "127.0.0.1".into(),
            mgmt_port: 47990,
            web_port: 47992,
        }
    }
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut value = |flag: &str| {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
        };
        match a.as_str() {
            "--quit" => args.quit = true,
            "--autostart" => args.autostart = true,
            "--mgmt-addr" => args.mgmt_addr = value("--mgmt-addr")?,
            "--mgmt-port" => args.mgmt_port = value("--mgmt-port")?.parse()?,
            "--web-port" => args.web_port = value("--web-port")?.parse()?,
            "--version" | "-V" => {
                println!("punktfunk-tray {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => anyhow::bail!(
                "unknown argument '{other}'\n\nUSAGE:\n    punktfunk-tray [--autostart] [--quit] \
                 [--mgmt-addr <IP>] [--mgmt-port <N>] [--web-port <N>]"
            ),
        }
    }
    Ok(args)
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    run(args)
}

#[cfg(windows)]
fn run(args: Args) -> anyhow::Result<()> {
    win::run(args)
}

#[cfg(target_os = "linux")]
fn run(args: Args) -> anyhow::Result<()> {
    linux::run(args)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn run(_args: Args) -> anyhow::Result<()> {
    // Workspace-stub build (macOS CI etc.) — the tray ships on Windows and Linux only.
    anyhow::bail!("punktfunk-tray supports Windows and Linux hosts only")
}
