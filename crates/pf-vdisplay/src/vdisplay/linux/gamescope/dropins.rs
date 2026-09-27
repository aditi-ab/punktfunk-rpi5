//! Files the host writes for the box's systemd user units: the SteamOS headless shim and
//! drop-in, the `gamescope-session-plus@` bind drop-in, and the idle drop-in that parks
//! Game Mode during a takeover.

use super::*;

fn headless_shim_dir() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gsbin")
}

/// PATH shim: rewrite SteamOS's hardcoded panel args to headless at `PF_W`/`PF_H`/`PF_HZ`.
/// `PF_HZ` is [`game_hz`] on `-r` only — it must not change the negotiated resolution.
pub(super) fn write_headless_shim() -> Result<std::path::PathBuf> {
    // `$PF_HDR_ARGS` is unquoted for the same reason as in the GAMESCOPE_BIN wrapper: it is our
    // own flag list ([`hdr_args`]) and must word-split into separate argv entries.
    let shim_body = format!(
        r#"#!/bin/bash
W="${{PF_W:-1920}}"; H="${{PF_H:-1080}}"; HZ="${{PF_HZ:-60}}"
keep=()
while [ $# -gt 0 ]; do
  case "$1" in
    --generate-drm-mode|-w|-h|-W|-H|-O|--prefer-output) shift 2;;
    *) keep+=("$1"); shift;;
  esac
done
exec {bin} --backend headless -W "$W" -H "$H" -w "$W" -h "$H" -r "$HZ" ${{PF_HDR_ARGS}} "${{keep[@]}}"
"#,
        bin = gamescope_bin()
    );
    let dir = headless_shim_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let shim = dir.join("gamescope");
    std::fs::write(&shim, &shim_body).with_context(|| format!("write shim {}", shim.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod shim {}", shim.display()))?;
    Ok(dir)
}

/// `zz-` sorts last, overriding any distro drop-in.
fn steamos_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session.service.d/zz-punktfunk-headless.conf")
}

pub(super) fn write_steamos_dropin(
    shim_dir: &std::path::Path,
    mode: Mode,
    hdr: bool,
) -> Result<()> {
    let path = steamos_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    // Stale desktop DISPLAY/WAYLAND_DISPLAY in the manager env would make gamescope attach instead
    // of becoming the display server.
    let body = format!(
        "[Service]\n\
         Environment=PATH={shim}:/usr/bin:/bin:/usr/local/bin\n\
         Environment=PF_W={w}\n\
         Environment=PF_H={h}\n\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {xkb}\
         UnsetEnvironment=DISPLAY WAYLAND_DISPLAY\n",
        shim = shim_dir.display(),
        xkb = xkb_unit_lines(),
        w = mode.width,
        h = mode.height,
        hz = game_hz(mode.refresh_hz),
        // Quoted: systemd `Environment=` with spaces otherwise keeps only the first flag.
        // SteamOS never reads `CUSTOM_REFRESH_RATES`; the shim only forwards `PF_HDR_ARGS`.
        hdr_args = our_flags(hdr, game_hz(mode.refresh_hz))
            .into_iter()
            // Advertised set vs `-r` = `PF_HZ` (frame-limited) — same split as `launch_session`.
            .chain(refresh_rate_args(mode.refresh_hz.max(1)))
            .collect::<Vec<_>>()
            .join(" "),
    );
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))
}

pub(super) fn remove_steamos_dropin() {
    let _ = std::fs::remove_file(steamos_dropin_path());
}

/// Autologin-unit bind drop-in. Must live in `$XDG_RUNTIME_DIR`, not `$HOME`: it applies to the
/// whole `gamescope-session-plus@` template, and both paths it names are tmpfs. A `$HOME` copy
/// survives a reboot that deletes its sources, and a missing bind source fails Game Mode outright.
fn session_plus_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// `$HOME` copy of the bind drop-in: outlives the tmpfs paths it names. Swept on sight.
fn legacy_session_plus_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// Idle drop-in replaces Game Mode `ExecStart`. Runtime-dir so a dead host cannot leave Game Mode
/// as a sleep; [`restore_takeover_on_startup`] still sweeps it.
pub(super) fn idle_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-idle.conf")
}

/// Idle `ExecStart` must actually execute: a unit that dies on start is the relogin storm.
fn sleep_binary() -> &'static str {
    ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("/usr/bin/sleep")
}

/// Idle autologin for the stream: replace `ExecStart` with sleep on the template. Steam is freed,
/// the autologin still succeeds (so the DM does not storm), and a user session-switch can still
/// be serviced — a stopped DM cannot.
pub(super) fn install_idle_dropin() -> Result<()> {
    let path = idle_dropin_path();
    let dir = path
        .parent()
        .context("the idle drop-in path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, idle_dropin_body(sleep_binary()))
        .with_context(|| format!("write {}", path.display()))?;
    systemctl_user(&["daemon-reload"]);
    *IDLE_DROPIN_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = true;
    Ok(())
}

/// Empty `ExecStart=` first: the directive is list-valued, so an add-only drop-in would run the
/// real session *and* the sleep.
fn idle_dropin_body(sleep_bin: &str) -> String {
    format!("[Service]\nExecStart=\nExecStart={sleep_bin} infinity\n")
}

/// Not gated on [`IDLE_DROPIN_ARMED`]: a drop-in that outlived a dead host still has to be swept.
pub(super) fn remove_idle_dropin() -> bool {
    let removed = std::fs::remove_file(idle_dropin_path()).is_ok();
    *IDLE_DROPIN_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
    removed
}

/// Box-session drop-in: bind + WSI opt-out. No bind to arm → remove any drop-in (`Ok(false)`);
/// keeping a bind the host decided against is the crash-loop the backstop exists to prevent.
pub(super) fn write_session_plus_dropin(
    wrapper: &std::path::Path,
    mode: Mode,
    hdr: bool,
    wsi: WsiPlan,
) -> Result<bool> {
    let Some(bind) = arm_session_bind(wrapper) else {
        remove_session_plus_dropin();
        return Ok(false);
    };
    let path = session_plus_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = format!(
        "[Service]\n\
         {binds}\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {xkb}\
         {wsi}",
        binds = bind.unit_lines(),
        xkb = xkb_unit_lines(),
        hz = game_hz(mode.refresh_hz),
        hdr_args = our_flags(hdr, game_hz(mode.refresh_hz)).join(" "),
        wsi = wsi.unit_lines(hdr),
    );
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))?;
    Ok(true)
}

/// Both homes: runtime and [`legacy_session_plus_dropin_path`]. Caller owes `daemon-reload` if
/// anything was removed — a removal that isn't reloaded still applies at next boot.
pub(super) fn remove_session_plus_dropin() -> bool {
    // Both paths every time; short-circuit would leave the `$HOME` copy that outlives a reboot.
    let mut removed = false;
    for path in [
        session_plus_dropin_path(),
        legacy_session_plus_dropin_path(),
    ] {
        removed |= std::fs::remove_file(&path).is_ok();
    }
    removed
}

/// Remove + clear [`SESSION_DROPIN_ARMED`] + `daemon-reload`, so the flag and the template cannot disagree.
pub(super) fn disarm_session_plus_dropin() {
    let removed = remove_session_plus_dropin();
    *SESSION_DROPIN_ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_dropin_replaces_exec_start_rather_than_appending() {
        let body = idle_dropin_body("/usr/bin/sleep");
        assert_eq!(
            body, "[Service]\nExecStart=\nExecStart=/usr/bin/sleep infinity\n",
            "{body}"
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[1], "ExecStart=", "the reset must come first: {body}");
        // The path is resolved per box ([`sleep_binary`]) and must reach the unit verbatim — a
        // bare `sleep` would depend on the unit's PATH, and an ExecStart that fails to execute is
        // the failing unit the display manager relogin-loops against.
        assert!(idle_dropin_body("/bin/sleep").contains("ExecStart=/bin/sleep infinity"));
    }
}
