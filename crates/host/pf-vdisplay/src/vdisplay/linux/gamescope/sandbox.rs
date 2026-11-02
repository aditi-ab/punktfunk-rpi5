//! The `bwrap` shim over the flatpak binary for everything a nested Steam starts.
//!
//! Steam runs a shortcut's `/usr/bin/flatpak` by absolute path with Steam's own environment,
//! which every game shares, so neither `PATH` nor `WAYLAND_DISPLAY` reaches the shortcut alone.
//! [`flatpak_shim`] binds a script over that path inside a mount namespace of the nested
//! command's own; the script gives `flatpak run` what [`super::shape_flatpak_command`] gives a
//! direct launch.

use std::path::{Path, PathBuf};

/// `bwrap` as an absolute path. The nested shell runs under gamescope's `PATH`, which is not
/// ours, so the argv names the binary we resolved.
fn bwrap_bin() -> Option<PathBuf> {
    on_path("bwrap")
}

fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// Off-switch for [`flatpak_shim`].
const FLATPAK_SHIM_ENV: &str = "PUNKTFUNK_GAMESCOPE_FLATPAK_SHIM";

/// A local bwrap that has not answered by now never will.
const NEST_PROBE: std::time::Duration = std::time::Duration::from_secs(5);

/// The `bwrap` argv that runs the nested command under the shim, or empty when flatpak is not
/// installed or bwrap cannot nest here, as pressure-vessel and flatpak must inside it.
pub(crate) fn flatpak_shim() -> Vec<String> {
    if pf_host_config::env_on(FLATPAK_SHIM_ENV) == Some(false) {
        return Vec::new();
    }
    let Some(flatpak) = on_path("flatpak").and_then(|p| std::fs::canonicalize(p).ok()) else {
        return Vec::new();
    };
    let Some(bwrap) = bwrap_bin().filter(|b| nests(b)) else {
        tracing::warn!(
            "gamescope: bwrap cannot run here, so a Steam shortcut to a flatpak may open on the \
             desktop instead of the stream — install bubblewrap and allow user namespaces"
        );
        return Vec::new();
    };
    let dir = Path::new(&crate::session::runtime_dir()).join("punktfunk-flatpak");
    let binds = match write_shim(&dir, &flatpak) {
        Ok(binds) => binds,
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "gamescope: flatpak shim not written");
            return Vec::new();
        }
    };
    let mut argv = vec![bwrap.to_string_lossy().into_owned()];
    argv.extend(["--dev-bind", "/", "/", "--die-with-parent"].map(String::from));
    argv.extend(binds);
    tracing::info!(
        flatpak = %flatpak.display(),
        "gamescope: the flatpaks this Steam starts get gamescope's Wayland socket and X11"
    );
    argv
}

/// Writes the shim under `dir` and returns the binds that put it over `real`. The real binary
/// stays reachable at `flatpak.real`, an empty file outside the namespace.
fn write_shim(dir: &Path, real: &Path) -> std::io::Result<Vec<String>> {
    pf_paths::create_private_dir(dir)?;
    let (shim, mount) = (dir.join("flatpak"), dir.join("flatpak.real"));
    std::fs::File::options()
        .create(true)
        .append(true)
        .open(&mount)?;
    // A rename, never a rewrite: a running session's namespace still has the old file bound. The
    // temp name is unique per call: overlapping spawns share the pid.
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("flatpak.{}.{seq}", std::process::id()));
    std::fs::write(&tmp, shim_script(&mount))?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&tmp, &shim)?;
    let s = |p: &Path| p.to_string_lossy().into_owned();
    Ok(vec![
        "--ro-bind".into(),
        s(real),
        s(&mount),
        "--ro-bind".into(),
        s(&shim),
        s(real),
    ])
}

/// Adds `--socket=x11` after `run` and points `WAYLAND_DISPLAY` at gamescope; any other flatpak
/// command passes through.
fn shim_script(real: &Path) -> String {
    // `for a` walks the original arguments while `set --` rebuilds them; the first word that is
    // not an option is the flatpak command.
    let lines = [
        "#!/bin/sh".to_string(),
        "cmd=".into(),
        "for a do".into(),
        "shift".into(),
        "set -- \"$@\" \"$a\"".into(),
        "if [ -z \"$cmd\" ] && [ \"${a#-}\" = \"$a\" ]; then".into(),
        "cmd=$a".into(),
        "if [ \"$a\" = run ]; then set -- \"$@\" --socket=x11; fi".into(),
        "fi".into(),
        "done".into(),
        format!("if [ \"$cmd\" = run ]; then {}fi", super::FLATPAK_WAYLAND),
        format!("exec {} \"$@\"", super::shell_word(&real.to_string_lossy())),
    ];
    lines.join("\n") + "\n"
}

/// Whether bwrap runs nested here, as pressure-vessel and flatpak will inside the shim's
/// namespace. Once per host: user namespaces do not come and go. A probe that could not
/// run is asked again ([`super::discovery::remember_once`]).
fn nests(bwrap: &Path) -> bool {
    static NESTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    super::discovery::remember_once(
        &NESTS,
        || {
            let mut cmd = std::process::Command::new(bwrap);
            cmd.args(["--dev-bind", "/", "/", "--"])
                .arg(bwrap)
                .args(["--dev-bind", "/", "/", "--", "true"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            crate::proc::status_within(&mut cmd, NEST_PROBE)
                .ok()
                .map(|s| s.success())
        },
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim run against a stand-in flatpak that prints what it got. `sh` reads the shim
    /// instead of executing it: a child a sibling test forks while this process writes a file
    /// holds that file open for writing until it execs, and `execve` on it then fails with
    /// `ETXTBSY`. The stand-in is written before the shim, so its window has passed when the
    /// shim execs it.
    fn run_shim(args: &[&str]) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pf-flatpak-shim-{}-{}",
            std::process::id(),
            args.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real");
        std::fs::write(
            &real,
            "#!/bin/sh\nprintf '%s|' \"$@\"\nprintf 'WL=%s' \"${WAYLAND_DISPLAY-unset}\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let shim = dir.join("shim");
        std::fs::write(&shim, shim_script(&real)).unwrap();
        let out = std::process::Command::new("sh")
            .arg(&shim)
            .args(args)
            .env("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-3")
            .env("XDG_RUNTIME_DIR", &dir)
            .env_remove("WAYLAND_DISPLAY")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn the_flatpak_shim_shapes_run_and_nothing_else() {
        assert_eq!(
            run_shim(&[
                "--user",
                "run",
                "--branch=stable",
                "org.x.Y",
                "--fullscreen"
            ]),
            "--user|run|--socket=x11|--branch=stable|org.x.Y|--fullscreen|WL=wayland-gamescope-3"
        );
        // An app argument named `run` is the app's.
        assert_eq!(
            run_shim(&["run", "org.x.Y", "run"]),
            "run|--socket=x11|org.x.Y|run|WL=wayland-gamescope-3"
        );
        assert_eq!(run_shim(&["info", "org.x.Y"]), "info|org.x.Y|WL=unset");
    }

    /// The whole mount namespace, as a normal user with `bwrap` and a stand-in `flatpak` first
    /// on `PATH`: `cargo test -p pf-vdisplay --lib -- --ignored live_flatpak_shim`. A nested
    /// `bwrap` must still run, as pressure-vessel and the real flatpak start one.
    #[test]
    #[ignore = "needs bwrap, user namespaces and a stand-in flatpak on PATH"]
    fn live_flatpak_shim_covers_an_absolute_flatpak() {
        let flatpak = std::fs::canonicalize(on_path("flatpak").unwrap()).unwrap();
        let mut argv = flatpak_shim();
        assert!(!argv.is_empty(), "the shim did not apply");
        let inner = format!(
            "{} --user run org.x.Y; {} --dev-bind / / true && echo nested-ok",
            flatpak.display(),
            bwrap_bin().unwrap().display()
        );
        argv.extend(["sh".into(), "-c".into(), inner]);
        let out = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .env("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-5")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(
            text.contains("--user|run|--socket=x11|org.x.Y|WL=wayland-gamescope-5"),
            "{text} / {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("nested-ok"), "{text}");
    }

    /// Two spawns overlap on one runtime directory. Each writes its own temp file, so every
    /// rename finds its file and none of them leaves one behind.
    #[test]
    fn overlapping_shim_writes_do_not_share_a_temp_file() {
        const THREADS: usize = 8;
        let dir = std::env::temp_dir().join(format!("pf-flatpak-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let barrier = std::sync::Barrier::new(THREADS);
        let failures: usize = std::thread::scope(|s| {
            let writers: Vec<_> = (0..THREADS)
                .map(|_| {
                    s.spawn(|| {
                        (0..50)
                            .filter(|_| {
                                barrier.wait();
                                write_shim(&dir, Path::new("/usr/bin/flatpak")).is_err()
                            })
                            .count()
                    })
                })
                .collect();
            writers.into_iter().map(|w| w.join().unwrap()).sum()
        });
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(failures, 0, "a write lost its temp file to another");
        assert_eq!(left, ["flatpak", "flatpak.real"]);
    }

    /// The real binary is parked before the shim covers its path, and both are read-only.
    #[test]
    fn the_shim_binds_park_the_real_flatpak() {
        let dir = std::env::temp_dir().join(format!("pf-flatpak-binds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let binds = write_shim(&dir, Path::new("/usr/bin/flatpak")).unwrap();
        let spell = |n: &str| dir.join(n).to_string_lossy().into_owned();
        assert_eq!(
            binds,
            [
                "--ro-bind".to_string(),
                "/usr/bin/flatpak".into(),
                spell("flatpak.real"),
                "--ro-bind".into(),
                spell("flatpak"),
                "/usr/bin/flatpak".into(),
            ]
        );
        assert!(dir.join("flatpak.real").is_file(), "the mount point exists");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
