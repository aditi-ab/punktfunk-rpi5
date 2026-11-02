//! The stored session against a real socket. Its own process: the store is process-global.
//! Not under Miri, which has no Unix sockets.
#![cfg(all(unix, not(miri)))]

use pf_host_config::session::{self, SessionEnv};
use std::ffi::OsStr;
use std::os::unix::net::UnixListener;
use std::process::Command;

/// What `cmd` does to `name`: `None` inherits, `Some(None)` removes, `Some(Some(v))` sets.
fn var_of<'a>(cmd: &'a Command, name: &str) -> Option<Option<&'a OsStr>> {
    cmd.get_envs()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value)
}

fn wayland_of(cmd: &Command) -> Option<Option<&OsStr>> {
    var_of(cmd, "WAYLAND_DISPLAY")
}

#[test]
fn a_stored_session_decides_the_socket_and_the_childs_variable() {
    let dir = std::env::temp_dir().join(format!("pf-session-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(dir.join("wayland-test")).unwrap();
    let runtime = dir.to_str().unwrap().to_string();
    let bus = format!("unix:path={runtime}/bus");
    let stored = |name: Option<&str>, desktop: Option<&str>| SessionEnv {
        wayland_display: name.map(str::to_string),
        xdg_runtime_dir: runtime.clone(),
        dbus_session_bus_address: bus.clone(),
        xdg_current_desktop: desktop.map(str::to_string),
        ..Default::default()
    };

    // Nothing stored: the caller connects from the environment, a child inherits.
    assert!(session::wayland_stream().is_none());
    let mut child = Command::new("true");
    session::export_to(&mut child);
    assert_eq!(child.get_envs().count(), 0);

    session::set(stored(Some("wayland-test"), Some("KDE")), true);
    session::wayland_stream().unwrap().unwrap();
    listener.accept().unwrap();
    let mut child = Command::new("true");
    session::export_to(&mut child);
    assert_eq!(wayland_of(&child), Some(Some(OsStr::new("wayland-test"))));
    assert_eq!(
        var_of(&child, "XDG_CURRENT_DESKTOP"),
        Some(Some(OsStr::new("KDE")))
    );
    assert_eq!(
        var_of(&child, "XDG_RUNTIME_DIR"),
        Some(Some(OsStr::new(&runtime)))
    );
    assert_eq!(
        var_of(&child, "DBUS_SESSION_BUS_ADDRESS"),
        Some(Some(OsStr::new(&bus)))
    );
    // What a caller already decided for a child stays.
    let mut sandboxed = Command::new("true");
    sandboxed
        .env("XDG_RUNTIME_DIR", "/sandbox")
        .env_remove("WAYLAND_DISPLAY");
    session::export_to(&mut sandboxed);
    assert_eq!(
        var_of(&sandboxed, "XDG_RUNTIME_DIR"),
        Some(Some(OsStr::new("/sandbox")))
    );
    assert_eq!(wayland_of(&sandboxed), Some(None));
    assert_eq!(
        var_of(&sandboxed, "XDG_CURRENT_DESKTOP"),
        Some(Some(OsStr::new("KDE")))
    );
    assert_eq!(session::runtime_dir().as_deref(), Some(runtime.as_str()));
    assert_eq!(session::dbus_address().as_deref(), Some(bus.as_str()));
    assert_eq!(session::current_desktop().as_deref(), Some("KDE"));

    // A live session that names no socket keeps the last one.
    session::set(stored(None, Some("KDE")), true);
    assert_eq!(session::wayland_display().as_deref(), Some("wayland-test"));

    // No live session: no socket and no desktop, and a child's inherited ones are removed.
    session::set(stored(None, None), false);
    assert!(session::wayland_stream().unwrap().is_err());
    assert_eq!(session::current_desktop(), None);
    let mut child = Command::new("true");
    session::export_to(&mut child);
    assert_eq!(wayland_of(&child), Some(None));
    assert_eq!(var_of(&child, "XDG_CURRENT_DESKTOP"), Some(None));
    assert_eq!(
        var_of(&child, "XDG_RUNTIME_DIR"),
        Some(Some(OsStr::new(&runtime)))
    );

    std::fs::remove_dir_all(&dir).unwrap();
}
