//! Host config-dir + owner-private file helpers — a leaf crate so the subsystem crates
//! (`pf-media`, `pf-vdisplay`) and the orchestrator can all reach them WITHOUT depending on the
//! `gamestream` module they used to live in (plan §2.4 / §W6: the secret helpers were shared
//! vocabulary parked above their consumers in the junk drawer). Pure std + `tracing`; no I/O stack.
//!
//! - [`config_dir`] resolves the per-host config directory (XDG / `%ProgramData%`, `PUNKTFUNK_CONFIG_DIR` override).
//! - [`create_private_dir`] makes it owner-private (0700 / restrictive DACL).
//! - [`write_secret_file`] writes an owner-only secret (0600 / SYSTEM+Admins DACL).
#![forbid(unsafe_code)]

use std::path::PathBuf;

/// The shared path of the file where the gamescope backend relays the nested session's
/// `LIBEI_SOCKET` (gamescope's EIS server) for the input injector: `$XDG_RUNTIME_DIR/
/// punktfunk-gamescope-ei` (per-user 0700), or `/tmp/…` when the runtime dir is unset. It is a
/// **contract shared** by the gamescope producer (`pf-vdisplay`, which writes it under the session
/// env lock) and the libei consumer (`pf-inject`, which reads it after the session env is applied) —
/// a leaf so neither subsystem crate has to reach into the other (plan §W6). Linux-only.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        Some(rt) => PathBuf::from(rt).join("punktfunk-gamescope-ei"),
        None => PathBuf::from("/tmp/punktfunk-gamescope-ei"),
    }
}

/// The host config dir (host identity, pairing state, mgmt token, library) — created on demand.
/// Linux: `$XDG_CONFIG_HOME/punktfunk` or `~/.config/punktfunk`. Windows: `%ProgramData%\punktfunk`
/// (machine-wide — the SYSTEM service and the interactive user share ONE dir that survives logout).
/// `PUNKTFUNK_CONFIG_DIR` overrides on both platforms (used by the Windows service config / tests).
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PUNKTFUNK_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    // Windows: %ProgramData% (e.g. C:\ProgramData\punktfunk) — machine-wide, SYSTEM-readable,
    // persists across user logout, correct for a SYSTEM service. Falls back to %APPDATA% then CWD.
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("ProgramData")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("punktfunk")
}

/// Create `dir` (and parents) owner-private — **0700** on Unix (so the host's secrets aren't readable
/// by other local users via a traversable config path). On Windows, applies a restrictive DACL
/// ([`restrict_dir_to_system_admins`]) so a local unprivileged user can't pre-create / plant files in
/// the config tree (the default `%ProgramData%` ACL grants Users *create*; security-review
/// 2026-06-28 #3/#11). Tightens (and re-owns) an already-existing dir too.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let r = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
        // `recursive` doesn't re-chmod an existing dir — tighten it so an old 0755 dir gets locked.
        if dir.exists() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        r
    }
    #[cfg(not(unix))]
    {
        let r = std::fs::create_dir_all(dir);
        #[cfg(windows)]
        restrict_dir_to_system_admins(dir);
        r
    }
}

/// Best-effort Windows DACL lockdown of the config *directory* (the companion to
/// [`restrict_to_system_admins`] for files). The default `%ProgramData%` ACL lets `BUILTIN\Users`
/// create subfolders/files (and become `CREATOR OWNER`), so a non-admin could pre-create the
/// `punktfunk` dir or plant a `host.env`/`apps.json` that the privileged SYSTEM service then trusts
/// (LPE; security-review 2026-06-28 #3). This re-owns the dir to Administrators (defeating a
/// pre-creation), strips inheritance, and sets an explicit DACL: SYSTEM/Administrators/OWNER full
/// (object+container inherit so child files/dirs inherit it), and Users **read-only** (so existing
/// reads of non-secret config keep working but a local user can no longer write/plant). Secret files
/// are additionally locked to SYSTEM/Admins by [`write_secret_file`]. Hard-coded SIDs
/// (locale-independent) via the absolute `%SystemRoot%` path; never fatal.
#[cfg(windows)]
fn restrict_dir_to_system_admins(dir: &std::path::Path) {
    let icacls = std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string());
    // Reset ownership of the directory object to Administrators first, so a dir a non-admin may have
    // pre-created can't keep OWNER control (an owner can always rewrite the DACL). No `/T` — re-owning
    // the dir itself is what defeats the pre-creation; recursing a large captures tree each call is
    // needless churn (secret files are individually owner-locked by `write_secret_file`).
    let _ = std::process::Command::new(&icacls)
        .arg(dir.as_os_str())
        .args(["/setowner", "*S-1-5-32-544"]) // BUILTIN\Administrators
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let status = std::process::Command::new(&icacls)
        .arg(dir.as_os_str())
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)(F)", // NT AUTHORITY\SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)(F)", // BUILTIN\Administrators
            "/grant:r",
            "*S-1-3-4:(OI)(CI)(F)", // OWNER RIGHTS
            "/grant:r",
            "*S-1-5-32-545:(OI)(CI)(RX)", // BUILTIN\Users — read-only (no create/write → no plant)
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => tracing::warn!(
            dir = %dir.display(),
            "config-dir DACL hardening did not fully succeed — a local user may be able to plant config files"
        ),
    }
}

/// Write `contents` to `path` as an **owner-only secret**: created and re-chmod'd **0600** on Unix
/// (never even briefly group/world-readable), and DACL-restricted to SYSTEM/Administrators/owner on
/// Windows (the default `%ProgramData%` ACL is Users-readable). Mirrors the mgmt-token hardening; used
/// for the host private key and the persisted trust stores so a local unprivileged user can neither
/// read the key (impersonation) nor tamper with the paired allow-list (unauthorized pairing).
pub fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(contents)?;
    f.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(windows)]
    restrict_to_system_admins(path);
    Ok(())
}

/// Best-effort Windows DACL lockdown of a secret file: strip inherited ACEs and grant Full only to
/// SYSTEM, Administrators, and OWNER RIGHTS (the creating account — the SYSTEM service or a manually
/// running user keeps access). Without this the host key under the default Users-readable
/// `%ProgramData%` ACL is readable by ANY local user. Uses `icacls` with hard-coded SIDs
/// (locale-independent) via the absolute `%SystemRoot%` path (a privileged service must not trust
/// `PATH`). Never fatal — on failure the file is simply left at the inherited ACL (today's behaviour).
#[cfg(windows)]
fn restrict_to_system_admins(path: &std::path::Path) {
    let icacls = std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string());
    let status = std::process::Command::new(icacls)
        .arg(path.as_os_str())
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(F)", // NT AUTHORITY\SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(F)", // BUILTIN\Administrators
            "/grant:r",
            "*S-1-3-4:(F)", // OWNER RIGHTS
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => tracing::warn!(
            path = %path.display(),
            "icacls hardening did not succeed — this secret may be readable by other local users"
        ),
    }
}
