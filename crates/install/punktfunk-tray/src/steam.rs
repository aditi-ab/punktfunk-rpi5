//! `punktfunk-tray --steam <steam.exe> [args…]`: what the PC's shared Steam shortcuts start while
//! seats are on. On a seat's account it starts that seat's own copy under the seat's IPC name,
//! from `HKCU\Software\Punktfunk\SeatSteam` (`Exe`, `Ipc`), which the seat supervisor writes and
//! the seat can only read; on any other account it starts `<steam.exe>` as the shortcut did.
//! Steam runs once per IPC name per PC, so the PC's own Steam started on a seat would close the
//! Steam already running.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use winreg::enums::HKEY_CURRENT_USER;
use winreg::RegKey;

/// The seat's copy and its IPC name, when this account is a seat's.
fn seat_steam() -> Option<(PathBuf, String)> {
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Punktfunk\SeatSteam")
        .ok()?;
    let exe = PathBuf::from(key.get_value::<String, _>("Exe").ok()?);
    let ipc: String = key.get_value("Ipc").ok()?;
    (exe.is_file() && !ipc.is_empty()).then_some((exe, ipc))
}

/// Start Steam for this account with `args`: the box's `steam.exe`, then what it is passed.
pub fn launch(mut args: impl Iterator<Item = OsString>) -> anyhow::Result<()> {
    let box_exe = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("usage: punktfunk-tray --steam <steam.exe> [args]"))?,
    );
    let (exe, ipc) = match seat_steam() {
        Some((exe, ipc)) => (exe, Some(ipc)),
        None => (box_exe, None),
    };
    let mut command = Command::new(&exe);
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    if let Some(ipc) = ipc {
        command.arg("-master_ipc_name_override").arg(ipc);
    }
    command.args(args);
    command
        .spawn()
        .map(drop)
        .map_err(|e| anyhow::anyhow!("start steam: {e}"))
}
