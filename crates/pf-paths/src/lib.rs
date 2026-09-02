//! Host config-dir and owner-private file helpers.
//!
//! Leaf crate so `pf-media`, `pf-vdisplay`, and the orchestrator share these
//! without depending on `gamestream`. Std + `tracing` only.
//!
//! [`config_dir`] is XDG / `%ProgramData%`, overridable with `PUNKTFUNK_CONFIG_DIR`.
//! [`create_private_dir`] / [`create_secret_dir`] / [`write_secret_file`] apply
//! 0700 / 0600 on Unix and a restrictive DACL on Windows. Secret dirs omit the
//! `BUILTIN\Users` read grant the config dir needs for the tray.
#![forbid(unsafe_code)]

use std::path::PathBuf;

/// `$XDG_RUNTIME_DIR/punktfunk-gamescope-ei` (per-user 0700), or `/tmp/…`
/// when the runtime dir is unset. `pf-vdisplay` writes it under the session
/// env lock; `pf-inject` reads it after that env is applied.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file() -> PathBuf {
    gamescope_ei_relay("punktfunk-gamescope-ei")
}

/// `$XDG_RUNTIME_DIR/punktfunk-gamescope-{id}-ei`. Isolated bare-spawn
/// sessions (`design/gamescope-multiuser.md`) must not overwrite each
/// other's relay; `id` is `pf-vdisplay`'s `SessionIsolation`.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file_for(id: &str) -> PathBuf {
    gamescope_ei_relay(&format!("punktfunk-gamescope-{id}-ei"))
}

#[cfg(target_os = "linux")]
fn gamescope_ei_relay(name: &str) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        Some(rt) => PathBuf::from(rt).join(name),
        None => PathBuf::from("/tmp").join(name),
    }
}

/// Host identity, pairing, mgmt token, library.
///
/// Windows uses `%ProgramData%` so the SYSTEM service and the interactive
/// user share one dir that survives logout. `PUNKTFUNK_CONFIG_DIR` overrides.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PUNKTFUNK_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
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

/// `punktfunk-host serve` writes `PUNKTFUNK_MGMT_URL=https://127.0.0.1:<port>`
/// on start. The tray cannot read `host.env` on Windows (SYSTEM/Administrators
/// DACL); this file is Users-readable so a `PUNKTFUNK_MGMT_BIND` move still
/// reaches loopback consumers. `None` if absent or unparsable.
pub fn published_mgmt_port() -> Option<u16> {
    published_mgmt_port_in(&config_dir())
}

/// Takes the directory so tests do not call `set_var` (`unsafe`; forbidden here).
pub fn published_mgmt_port_in(dir: &std::path::Path) -> Option<u16> {
    let raw = std::fs::read_to_string(dir.join("mgmt-endpoint")).ok()?;
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    value
        .trim_end_matches('/')
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
}

/// Tightens an already-existing dir. Windows refuses a reparse point
/// ([`reject_reparse_point`]): hardening a junction would harden the
/// attacker-chosen target while the link stays theirs. Default
/// `%ProgramData%` ACLs grant Users *create*.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let r = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
        // `recursive` does not re-chmod an existing dir.
        if dir.exists() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        r
    }
    #[cfg(not(unix))]
    {
        #[cfg(windows)]
        reject_reparse_point(dir)?;
        let r = std::fs::create_dir_all(dir);
        #[cfg(windows)]
        if r.is_ok() {
            let _hold = hold_plain_directory(dir)?;
            restrict_dir_to_system_admins(dir, first_hardening_of(dir), true);
        }
        r
    }
}

/// [`create_private_dir`] without the Windows `BUILTIN\Users` read grant.
///
/// The config dir's `Users:(OI)(CI)(RX)` is for the tray's `mgmt-endpoint`
/// read. `(OI)` would otherwise make every file under it (logs, uploaded
/// client bundles) Users-readable. Unix is already 0700.
pub fn create_secret_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        reject_reparse_point(dir)?;
        std::fs::create_dir_all(dir)?;
        let _hold = hold_plain_directory(dir)?;
        restrict_dir_to_system_admins(dir, first_hardening_of(dir), false);
        Ok(())
    }
    #[cfg(not(windows))]
    create_private_dir(dir)
}

#[cfg(windows)]
fn reject_reparse_point(path: &std::path::Path) -> std::io::Result<()> {
    // FILE_ATTRIBUTE_REPARSE_POINT. Hard-coded: this crate stays pure-std.
    const REPARSE: u32 = 0x400;
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            use std::os::windows::fs::MetadataExt;
            if md.file_attributes() & REPARSE != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "{} is a reparse point (junction/symlink) — refusing to use it as a \
                         security-sensitive path",
                        path.display()
                    ),
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn handle_is_reparse(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const REPARSE: u32 = 0x400;
    Ok(file.metadata()?.file_attributes() & REPARSE != 0)
}

/// Hold the directory open with no delete-share across the path-based ACL update.
#[cfg(windows)]
fn hold_plain_directory(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
        .open(path)?;
    if handle_is_reparse(&file)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a reparse point", path.display()),
        ));
    }
    Ok(file)
}

/// First hardening of `dir` in this process — the pass that re-owns recursively.
///
/// A planted tree exists before the host starts, so one deep pass is enough.
/// Library CRUD calls `create_private_dir` per write; repeating `/T` would
/// re-walk recordings and the art cache.
#[cfg(windows)]
fn first_hardening_of(dir: &std::path::Path) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut s| s.insert(dir.to_path_buf()))
        .unwrap_or(false)
}

/// Re-own to Administrators then re-ACL. A planted file's creator keeps
/// `WRITE_DAC`, so ACL-only would let them put access back.
#[cfg(windows)]
pub fn restrict_existing_secret_file(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let icacls = icacls_path();
    let _ = std::process::Command::new(&icacls)
        .arg(path.as_os_str())
        .args(["/setowner", "*S-1-5-32-544"]) // BUILTIN\Administrators
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if let Err(e) = restrict_to_system_admins(path) {
        tracing::warn!(path = %path.display(), error = %e, "icacls hardening did not succeed");
    }
}

/// No-op: POSIX modes are set at create, and a user-owned home dir is not `%ProgramData%`.
#[cfg(not(windows))]
pub fn restrict_existing_secret_file(_path: &std::path::Path) {}

/// `icacls` by absolute path — a privileged service must never resolve it through `PATH`.
#[cfg(windows)]
fn icacls_path() -> String {
    std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string())
}

/// Default `%ProgramData%` lets `BUILTIN\Users` create and become
/// `CREATOR OWNER`. Re-owns to Administrators, strips inheritance, grants
/// SYSTEM/Administrators `(OI)(CI)(F)`. `users_read` adds Users `(OI)(CI)(RX)`
/// so the tray can read non-secret config. Hard-coded SIDs; never fatal.
#[cfg(windows)]
fn restrict_dir_to_system_admins(dir: &std::path::Path, deep: bool, users_read: bool) {
    let icacls = icacls_path();
    // Re-own to Administrators first: an owner keeps WRITE_DAC.
    // `deep` (once per dir per process) also re-owns contents; directory-only
    // left planted files still writable by their creator.
    let mut own = std::process::Command::new(&icacls);
    own.arg(dir.as_os_str())
        .args(["/setowner", "*S-1-5-32-544"]); // BUILTIN\Administrators
    if deep {
        own.args(["/T", "/C", "/Q"]); // recurse, continue on error, quiet
    }
    let _ = own
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // Do not grant OWNER `(OI)(CI)(F)` — WRITE_DAC comes back on every child
    // the attacker already owned. SYSTEM and Administrators cover writers here.
    let mut acl = std::process::Command::new(&icacls);
    acl.arg(dir.as_os_str()).args([
        "/inheritance:r",
        "/grant:r",
        "*S-1-5-18:(OI)(CI)(F)", // NT AUTHORITY\SYSTEM
        "/grant:r",
        "*S-1-5-32-544:(OI)(CI)(F)", // BUILTIN\Administrators
    ]);
    if users_read {
        // Users read-only. `(OI)` hits every file born here — secret dirs omit this ACE.
        acl.args(["/grant:r", "*S-1-5-32-545:(OI)(CI)(RX)"]);
    }
    let status = acl
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

/// Unix: create and re-chmod 0600 so it is never group/world-readable.
/// Windows: `OpenOptions` cannot pass `SECURITY_ATTRIBUTES` and this crate
/// forbids `unsafe`, so the file is created empty, `icacls`'d, then written.
/// The DACL step is fatal; a failure unlinks the still-empty file. Do not
/// write first: the config dir grants `Users (OI)(CI)(RX)`, so a newborn
/// secret is Users-readable for the life of the `icacls` child.
pub fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Never write a secret through a link: the bytes would land on the attacker's target.
    #[cfg(windows)]
    reject_reparse_point(path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
        opts.share_mode(SHARE_READ_WRITE)
            .custom_flags(OPEN_REPARSE_POINT);
    }
    let mut f = opts.open(path)?;
    #[cfg(windows)]
    if handle_is_reparse(&f)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a reparse point", path.display()),
        ));
    }
    #[cfg(windows)]
    if let Err(e) = restrict_to_system_admins(path) {
        drop(f);
        // Callers treat a non-empty file as "secret present"; do not leave a 0-byte stub.
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    f.write_all(contents)?;
    f.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// OWNER RIGHTS is the creating account (SYSTEM service or a manual run).
/// Failure is returned: [`write_secret_file`] treats it as fatal (only
/// control over the bytes about to be written); [`restrict_existing_secret_file`]
/// only warns.
#[cfg(windows)]
fn restrict_to_system_admins(path: &std::path::Path) -> std::io::Result<()> {
    let icacls = icacls_path();
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
        .status()?;
    if status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "icacls could not restrict {} to SYSTEM/Administrators ({status}) — it would be readable \
         by other local users",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn per_session_ei_relay_shares_the_global_files_directory() {
        let global = gamescope_ei_socket_file();
        let per = gamescope_ei_socket_file_for("cafe0123");
        assert_eq!(per.parent(), global.parent());
        assert_eq!(
            per.file_name().unwrap().to_str().unwrap(),
            "punktfunk-gamescope-cafe0123-ei"
        );
    }

    #[test]
    fn published_mgmt_port_follows_the_endpoint_file_and_is_absent_without_it() {
        let dir = std::env::temp_dir().join(format!("pf-paths-endpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(
            published_mgmt_port_in(&dir),
            None,
            "no file → fall back to the default"
        );

        std::fs::write(
            dir.join("mgmt-endpoint"),
            "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47995\n",
        )
        .unwrap();
        assert_eq!(published_mgmt_port_in(&dir), Some(47995));

        std::fs::write(dir.join("mgmt-endpoint"), "\n").unwrap();
        assert_eq!(
            published_mgmt_port_in(&dir),
            None,
            "blank reads as unset, not port 0"
        );

        std::fs::write(dir.join("mgmt-endpoint"), "PUNKTFUNK_MGMT_URL=\n").unwrap();
        assert_eq!(published_mgmt_port_in(&dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn precreated_directory_junction_is_refused() {
        let root = std::env::temp_dir().join(format!("pf-paths-junction-{}", std::process::id()));
        let target = root.join("target");
        let link = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&target).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J must work for a standard user");
        assert!(create_private_dir(&link).is_err());
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn secrets_are_owner_only_on_create_and_on_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pf-paths-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        create_secret_dir(&dir).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "a secrets dir is owner-only");

        let key = dir.join("key.pem");
        write_secret_file(&key, b"-----BEGIN PRIVATE KEY-----\n").unwrap();
        assert_eq!(mode(&key), 0o600);
        write_secret_file(&key, b"rotated").unwrap();
        assert_eq!(mode(&key), 0o600, "the rewrite path keeps 0600");
        assert_eq!(std::fs::read(&key).unwrap(), b"rotated", "and truncates");

        let planted = dir.join("mgmt-token");
        std::fs::write(&planted, b"old").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret_file(&planted, b"new").unwrap();
        assert_eq!(mode(&planted), 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
