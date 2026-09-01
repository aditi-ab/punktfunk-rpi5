//! The Windows installer wizard's entry: parse the Inno dialect, pick the mode, hand the
//! box to the reactor shell — or, under `/VERYSILENT`, to the windowless silent path.
//!
//! The wizard drives the `--demo` presets only until WP3.1's pack step appends a payload;
//! the silent path additionally runs `--dry-run` against the real box (the runner smoke's
//! surface). A real silent run without a payload refuses with exit 1 — a silent no-op that
//! exits 0 is exactly the fielded-updater bug D5 exists to prevent.

// No console window on double-click. Errors still reach redirected stderr (a GUI-subsystem
// child inherits pipes); the S2 lesson about PowerShell's `&` not waiting is a caller trap,
// noted in the handoff, not fixable here.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use punktfunk_setup_win::{silent, wizard};

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use punktfunk_setup::platform::windows::args::InnoArgs;
    use punktfunk_setup::platform::windows::demo::{win_preset, WIN_PRESETS};
    use punktfunk_setup::platform::windows::launched_as_uninstaller;
    use std::process::ExitCode;

    const USAGE: &str = "punktfunk setup wizard (demo presets only until the pack step lands)\n\n\
        usage: punktfunk-setup-win --demo <preset>\n\
               punktfunk-setup-win /VERYSILENT [/LOG=<file>] [/MERGETASKS=...] (--dry-run | --demo <preset>)\n\
        presets: win11-fresh win11-upgrade win11-sunshine win11-public win11-uninstall client-fresh client-win10";

    let args: Vec<String> = std::env::args().skip(1).collect();
    let inno = InnoArgs::parse(&args);

    let mut demo = None;
    let mut dry = false;
    let mut it = inno.rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => demo = it.next().cloned(),
            "--dry-run" => dry = true,
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
    let preset = match demo {
        Some(name) => match win_preset(&name) {
            Some(preset) => Some(preset),
            None => {
                eprintln!(
                    "unknown --demo preset '{name}'. Try: {}",
                    WIN_PRESETS.join(", ")
                );
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    // D6: the payload-less copy packed as unins000.exe is the uninstaller — the same wizard,
    // teardown path only. The name is the switch, so a demo copied to that name walks it.
    let uninstaller = std::env::current_exe().is_ok_and(|p| launched_as_uninstaller(&p));

    if inno.silence.is_silent() {
        return silent::main(&inno, preset, uninstaller, dry);
    }
    if dry {
        eprintln!("--dry-run is the silent path's flag — add /VERYSILENT\n\n{USAGE}");
        return ExitCode::from(2);
    }
    let Some(mut preset) = preset else {
        eprintln!(
            "no payload in this build — walk a preset instead: --demo win11-fresh\n\n{USAGE}"
        );
        return ExitCode::from(2);
    };
    preset.uninstall |= uninstaller;

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
