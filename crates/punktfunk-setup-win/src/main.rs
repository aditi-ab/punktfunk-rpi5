//! The Windows installer wizard's entry (WP2.1): parse the Inno dialect, pick the mode,
//! hand the box to the reactor shell.
//!
//! This build drives the `--demo` presets only — the real-box path needs a payload to
//! deploy, which WP3.1's pack step appends. The silent path (the D5 contract under
//! `/VERYSILENT`) lands with WP2.4; until then a silent flag is refused loudly, because a
//! silent no-op that exits 0 is exactly the fielded-updater bug D5 exists to prevent.

// No console window on double-click. Errors still reach redirected stderr (a GUI-subsystem
// child inherits pipes); the S2 lesson about PowerShell's `&` not waiting is a caller trap,
// noted in the handoff, not fixable here.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use punktfunk_setup_win::wizard;

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use punktfunk_setup::platform::windows::args::InnoArgs;
    use punktfunk_setup::platform::windows::demo::{win_preset, WIN_PRESETS};
    use punktfunk_setup::platform::windows::launched_as_uninstaller;
    use std::process::ExitCode;

    const USAGE: &str = "punktfunk setup wizard (demo presets only until the pack step lands)\n\n\
        usage: punktfunk-setup-win --demo <preset>\n\
        presets: win11-fresh win11-upgrade win11-sunshine win11-public win11-uninstall client-fresh client-win10";

    let args: Vec<String> = std::env::args().skip(1).collect();
    let inno = InnoArgs::parse(&args);
    if inno.silence.is_silent() {
        eprintln!(
            "the silent path is not wired yet (WP2.4) — this build drives --demo presets only"
        );
        return ExitCode::FAILURE;
    }

    let mut demo = None;
    let mut it = inno.rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => demo = it.next().cloned(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown option: {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(name) = demo else {
        eprintln!(
            "no payload in this build — walk a preset instead: --demo win11-fresh\n\n{USAGE}"
        );
        return ExitCode::from(2);
    };
    let Some(mut preset) = win_preset(&name) else {
        eprintln!(
            "unknown --demo preset '{name}'. Try: {}",
            WIN_PRESETS.join(", ")
        );
        return ExitCode::from(2);
    };
    // D6: the payload-less copy packed as unins000.exe is the uninstaller — the same wizard,
    // teardown path only. The name is the switch, so a demo copied to that name walks it.
    if std::env::current_exe().is_ok_and(|p| launched_as_uninstaller(&p)) {
        preset.uninstall = true;
    }

    match wizard::run(preset) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wizard failed: {e:?}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    // Windows-gated like clients/windows: a stub keeps `cargo build --workspace` green
    // elsewhere; the engine it drives tests cross-OS in punktfunk-setup itself.
    eprintln!("punktfunk-setup-win is Windows-only");
}
