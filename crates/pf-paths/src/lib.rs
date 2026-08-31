//! Host config-dir + owner-private file helpers — a leaf crate so the subsystem crates
//! (`pf-media`, `pf-vdisplay`) and the orchestrator can all reach them WITHOUT depending on the
//! `gamestream` module they used to live in (plan §2.4 / §W6: the secret helpers were shared
//! vocabulary parked above their consumers in the junk drawer). Pure std + `tracing`; no I/O stack.
//!
//! - [`config_dir`] resolves the per-host config directory (XDG / `%ProgramData%`, `PUNKTFUNK_CONFIG_DIR` override).
//! - [`create_private_dir`] makes it owner-private (0700 / restrictive DACL).
//! - [`create_secret_dir`] the same, minus the Windows `BUILTIN\Users` read grant.
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

/// The mgmt port the host actually bound, from `<config_dir>/mgmt-endpoint` — the one
/// `PUNKTFUNK_MGMT_URL=https://127.0.0.1:<port>` line `punktfunk-host serve` publishes on every
/// start (`mgmt::publish_endpoint`). This is how a `PUNKTFUNK_MGMT_BIND` move reaches a loopback
/// consumer that inherits nothing from `host.env` — the tray, which on Windows cannot even read
/// `host.env` (DACL-locked to SYSTEM/Administrators) while this file is deliberately Users-readable.
/// `None` when the file is absent (an older host, or no host on this box) or unparsable; callers
/// fall back to 47990, which is strictly what they did before.
pub fn published_mgmt_port() -> Option<u16> {
    published_mgmt_port_in(&config_dir())
}

/// The IO half of [`published_mgmt_port`], taking the directory so it is testable without touching
/// `PUNKTFUNK_CONFIG_DIR` (this crate forbids the `unsafe` that `set_var` now needs).
pub fn published_mgmt_port_in(dir: &std::path::Path) -> Option<u16> {
    let raw = std::fs::read_to_string(dir.join("mgmt-endpoint")).ok()?;
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    // `https://127.0.0.1:47995` → the last `:`-separated field, tolerating a trailing `/`.
    value
        .trim_end_matches('/')
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
}

/// Create `dir` (and parents) owner-private — **0700** on Unix (so the host's secrets aren't readable
/// by other local users via a traversable config path). On Windows, applies a restrictive DACL
/// ([`restrict_dir_to_system_admins`]) so a local unprivileged user can't pre-create / plant files in
/// the config tree (the default `%ProgramData%` ACL grants Users *create*; security-review
/// 2026-06-28 #3/#11). Tightens (and re-owns) an already-existing dir too, and **refuses a reparse
/// point** ([`reject_reparse_point`]): hardening a junction hardens the attacker-chosen target while
/// the link object stays theirs (security-review 2026-08-31 W-1).
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
        #[cfg(windows)]
        reject_reparse_point(dir)?;
        let r = std::fs::create_dir_all(dir);
        #[cfg(windows)]
        restrict_dir_to_system_admins(dir, first_hardening_of(dir), true);
        r
    }
}

/// [`create_private_dir`] without the Windows `BUILTIN\Users` read grant — for a subdirectory whose
/// *contents* are secrets rather than merely tamper-sensitive config: the host/service logs and the
/// client log bundles paired devices upload.
///
/// The config dir's `Users:(OI)(CI)(RX)` is deliberate (the tray reads `mgmt-endpoint` out of it),
/// but `(OI)` means every file born anywhere under it inherits that read — which left the logs
/// (webhook URLs, command lines) and the uploaded bundles readable by any local user, the latter
/// flatly contradicting the "reading them stays on the loopback-only bearer lane" split
/// `mgmt::client_logs` documents (security-review 2026-08-25). Unix behaviour is identical to
/// [`create_private_dir`] (0700 — the mode already excludes everyone else).
pub fn create_secret_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        reject_reparse_point(dir)?;
        let r = std::fs::create_dir_all(dir);
        restrict_dir_to_system_admins(dir, first_hardening_of(dir), false);
        r
    }
    #[cfg(not(windows))]
    create_private_dir(dir)
}

/// Refuse a path that exists as a reparse point — junction, symlink, or any other tag.
///
/// A standard user can ordinarily create a directory junction, `icacls` without `/L` follows a
/// link to its target, and `std::fs::OpenOptions` follows reparse points on open — so hardening
/// or writing through a pre-created link secures/feeds the ATTACKER-chosen target while the link
/// object stays theirs to retarget (security-review 2026-08-31 W-1). `symlink_metadata` never
/// follows, so this examines the object at `path` itself. A check-then-create race remains for a
/// window after this returns; the fully handle-relative open needs raw Win32 and this crate is
/// `forbid(unsafe_code)` — this closes the planted-before-install shape, which is the practical
/// one against a config root created at first service start.
#[cfg(windows)]
fn reject_reparse_point(path: &std::path::Path) -> std::io::Result<()> {
    // FILE_ATTRIBUTE_REPARSE_POINT — hard-coded (pure-std crate, no winapi dependency).
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

/// Whether this is the first hardening pass of `dir` in this process — the pass that also does the
/// expensive recursive re-own.
///
/// A planted config dir is planted once, before the host ever starts, so one deep pass at startup
/// closes it; repeating it on every `create_private_dir` call (the library CRUD calls it per write)
/// would re-walk the whole config tree — recordings, art cache — for nothing.
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

/// Re-apply the secret-file DACL to a file that **already exists** — including re-owning it to
/// Administrators.
///
/// [`write_secret_file`] hardens what it writes, but a file that was planted before the host first
/// ran was never written by us: it is owned by whoever created it, and an owner always retains
/// `WRITE_DAC`, so re-ACLing without re-owning leaves them able to put their access straight back.
/// Used on startup for `host.env`, whose contents become the SYSTEM service's environment and
/// command line (2026-08-05 review H-4). Best-effort and never fatal.
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

/// No-op off Windows: POSIX modes are set at creation by [`write_secret_file`] and a config dir a
/// non-root user pre-created is not a privilege boundary the way `%ProgramData%` is.
#[cfg(not(windows))]
pub fn restrict_existing_secret_file(_path: &std::path::Path) {}

/// `icacls` by absolute path — a privileged service must never resolve it through `PATH`.
#[cfg(windows)]
fn icacls_path() -> String {
    std::env::var("SystemRoot")
        .map(|r| format!("{r}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string())
}

/// Best-effort Windows DACL lockdown of the config *directory* (the companion to
/// [`restrict_to_system_admins`] for files). The default `%ProgramData%` ACL lets `BUILTIN\Users`
/// create subfolders/files (and become `CREATOR OWNER`), so a non-admin could pre-create the
/// `punktfunk` dir or plant a `host.env`/`apps.json` that the privileged SYSTEM service then trusts
/// (LPE; security-review 2026-06-28 #3). This re-owns the dir to Administrators (defeating a
/// pre-creation), strips inheritance, and sets an explicit DACL: SYSTEM/Administrators/OWNER full
/// (object+container inherit so child files/dirs inherit it), and — when `users_read` — Users
/// **read-only** (so existing reads of non-secret config keep working but a local user can no longer
/// write/plant). [`create_secret_dir`] passes `false` for a dir whose contents are all secrets, and
/// secret files are additionally locked to SYSTEM/Admins by [`write_secret_file`]. Hard-coded SIDs
/// (locale-independent) via the absolute `%SystemRoot%` path; never fatal.
#[cfg(windows)]
fn restrict_dir_to_system_admins(dir: &std::path::Path, deep: bool, users_read: bool) {
    let icacls = icacls_path();
    // Reset ownership to Administrators first, so a dir a non-admin may have pre-created can't keep
    // OWNER control (an owner always retains WRITE_DAC and can put its access straight back).
    //
    // `deep` (once per directory per process — see `first_hardening_of`) also re-owns the CONTENTS.
    // Re-owning only the directory left every file the attacker had already created still owned by
    // them, and therefore still theirs to rewrite, which is half of why the 2026-08-05 review's H-4
    // was exploitable end to end. A planted tree is planted once, before the host first runs, so one
    // deep pass at startup closes it without re-walking recordings and art cache on every write.
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
    // NO inheritable OWNER RIGHTS (`*S-1-3-4`) in the grant below, deliberately. It used to be
    // granted `(OI)(CI)(F)`, which handed full control of every child object to whoever owned it —
    // so a file a local user created before the hardening ran stayed writable by them even after
    // the directory was re-owned (2026-08-05 review H-4, second half). SYSTEM and Administrators
    // cover every account that legitimately writes here; a non-elevated manual run gets read-only
    // config, which is the intended boundary rather than a regression — this directory drives
    // command execution as SYSTEM.
    let mut acl = std::process::Command::new(&icacls);
    acl.arg(dir.as_os_str()).args([
        "/inheritance:r",
        "/grant:r",
        "*S-1-5-18:(OI)(CI)(F)", // NT AUTHORITY\SYSTEM
        "/grant:r",
        "*S-1-5-32-544:(OI)(CI)(F)", // BUILTIN\Administrators
    ]);
    if users_read {
        // BUILTIN\Users — read-only (no create/write → no plant). `(OI)` reaches every FILE born
        // under here as well, which is why [`create_secret_dir`] leaves this ACE off entirely.
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

/// Write `contents` to `path` as an **owner-only secret**: created and re-chmod'd **0600** on Unix
/// (never even briefly group/world-readable), and DACL-restricted to SYSTEM/Administrators/owner on
/// Windows (the default `%ProgramData%` ACL is Users-readable). Mirrors the mgmt-token hardening; used
/// for the host private key and the persisted trust stores so a local unprivileged user can neither
/// read the key (impersonation) nor tamper with the paired allow-list (unauthorized pairing).
///
/// **Windows ordering** (2026-08-05 review L-17, corrected by security-review 2026-08-25): the file
/// cannot be BORN with the right DACL — `std::fs::OpenOptions` cannot pass a `SECURITY_ATTRIBUTES`
/// and this crate is `#![forbid(unsafe_code)]`, so it cannot call `CreateFileW` itself. So it is
/// created EMPTY, `icacls`'d, and only then written — `install::set_web_password`'s ordering, and
/// the DACL step is **fatal**.
///
/// This used to write first and harden after, on the argument that the INHERITED ACL was already
/// SYSTEM/Administrators-only. It never was: [`restrict_dir_to_system_admins`] deliberately grants
/// `BUILTIN\Users` `(OI)(CI)(RX)` so non-secret config stays readable, and `(OI)` means every file
/// born in the config dir inherits that read. Every secret was therefore world-readable for the
/// life of the `icacls` child — long enough for a `ReadDirectoryChangesW` watcher (which the same
/// directory ACL permits by design) to take `native-key.pem`, `key.pem` and `mgmt-token`. The
/// `icacls` call is the ONLY control here, not defence in depth, which is also why a failure now
/// returns an error and unlinks the still-empty file instead of filling it with a secret anyone can
/// read. The open handle carries our own write access across the DACL change.
pub fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Never write a secret THROUGH a link: a pre-created junction/symlink at this path would
    // deliver the bytes to an attacker-chosen file (security-review 2026-08-31 W-1).
    #[cfg(windows)]
    reject_reparse_point(path)?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    #[cfg(windows)]
    if let Err(e) = restrict_to_system_admins(path) {
        drop(f);
        // Never leave a 0-byte secret behind: callers gate on "does it exist / is it non-empty".
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

/// Windows DACL lockdown of a secret file: strip inherited ACEs and grant Full only to
/// SYSTEM, Administrators, and OWNER RIGHTS (the creating account — the SYSTEM service or a manually
/// running user keeps access). Without this the host key under the default Users-readable
/// `%ProgramData%` ACL is readable by ANY local user. Uses `icacls` with hard-coded SIDs
/// (locale-independent) via the absolute `%SystemRoot%` path (a privileged service must not trust
/// `PATH`). Reports failure to the caller: [`write_secret_file`] treats it as fatal (it is the only
/// control over the bytes it is about to write), [`restrict_existing_secret_file`] only warns.
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

        // exactly what `mgmt::endpoint_line` writes
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

    /// The unix half of the create-empty → harden → write ordering: a secret is never even briefly
    /// group/world-readable, on the first write OR the truncate-and-rewrite one. (The Windows half —
    /// the `icacls` step being fatal — can only be exercised on Windows.)
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

        // A pre-existing world-readable file is tightened, not adopted.
        let planted = dir.join("mgmt-token");
        std::fs::write(&planted, b"old").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret_file(&planted, b"new").unwrap();
        assert_eq!(mode(&planted), 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
