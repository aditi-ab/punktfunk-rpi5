//! The shell↔session handoff: streams run in the spawned `punktfunk-session` binary,
//! spawned and supervised by `pf_client_core::orchestrate` like the GTK shell's. This module
//! adds what only this shell needs: CREATE_NO_WINDOW (the session keeps the console subsystem
//! for its stdout contract, and a GUI parent would otherwise pop a console window),
//! `--window-pos`, the log-file tee for the child's stderr, and [`SpawnEvent`]s for the app's
//! navigation closure.

use pf_client_core::orchestrate::{self, CancelHandle, SessionEvent};
use std::os::windows::process::CommandExt as _;
use std::path::PathBuf;
use std::process::Command;

/// One event from the session child.
pub(crate) enum SpawnEvent {
    /// The child presented its first frame (its window is up and streaming).
    Ready,
    /// One stats window for the session status page.
    Stats(Box<punktfunk_core::hud::StatsSnapshot>),
    /// The child exited (stdout EOF + reap; a kill lands here too). `error`/`ended` carry the
    /// contract lines seen on the way out. `code` (-1 = no code) covers a child that died
    /// before it could speak the contract, which would otherwise read as a clean quit.
    Exited {
        error: Option<(String, bool)>,
        ended: Option<String>,
        code: i32,
    },
}

/// The banner for a child that exited having said NOTHING on stdout — no `ready`, no
/// `error`, no `ended`. `None` keeps the silent return the UI has always given a clean
/// quit: code 0 is the user closing the stream window, and -1 is our own Disconnect/Cancel
/// kill (no exit code). Anything else is the session dying before it could speak its
/// contract — a missing runtime DLL, a crash, or the wrong binary sitting next to the
/// shell — and reporting the code is the difference between a diagnosable failure and a
/// connect that silently drops back to the host list.
pub(crate) fn silent_exit_banner(code: i32) -> Option<String> {
    (code != 0 && code != -1).then(|| {
        // Name the log's actual location — "check the client log" without a path is a
        // scavenger hunt (Settings ▸ About's "Open log folder" reaches it too).
        let log = crate::logfile::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "the client log".into());
        // NTSTATUS codes arrive as negative i32s; the hex form matches the crash filter's
        // log line and the Event Log record, so the two can be tied together.
        let how = match code as u32 {
            0xC000_0005 => "crashed with an access violation (0xC0000005)".to_string(),
            c if code < 0 => format!("died with exception 0x{c:08X}"),
            _ => format!("exited with code {code}"),
        };
        format!("The session didn't start (punktfunk-session {how}). Check {log}.")
    })
}

/// Spawn the session binary for a connect with `fp_hex` pinned and feed its lifecycle to
/// `on_event` from a reader thread. `slot` is the handle Disconnect/Cancel kill. `launch`
/// carries a library title id for the host to launch during the handshake; `preset` is a
/// ONE-OFF settings-preset pick. `Err` = the spawn itself failed (binary missing?) —
/// surfaced as a connect error by the caller.
///
/// The argv and the `--resolved-spec` come from [`orchestrate::session_command`], so this
/// shell's sessions run from the same effective (preset-aware) settings as every other one.
#[allow(clippy::too_many_arguments)] // one cohesive spawn spec (session_params precedent)
pub(crate) fn spawn_session(
    addr: &str,
    port: u16,
    fp_hex: &str,
    connect_timeout_secs: u64,
    launch: Option<&str>,
    preset: Option<&str>,
    slot: CancelHandle,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    use pf_client_core::orchestrate::{ConnectPlan, HostTarget};
    let mut plan = ConnectPlan::for_target(
        HostTarget {
            name: String::new(), // display-only; this shell's screens carry their own copy
            addr: addr.to_string(),
            port,
            fp_hex: Some(fp_hex.to_string()),
            mac: Vec::new(), // wake ran before this spawn (initiate_waking) — not the plan's job
            id: None,
            mgmt_port: None, // the library fetch runs in the shell (`Target`), never off a spawn plan
        },
        launch.map(str::to_string),
        preset.map(str::to_string),
    );
    plan.connect_timeout_secs = Some(connect_timeout_secs);
    let (cmd, spec_path) = orchestrate::session_command(&plan);
    spawn(cmd, spec_path, &format!("{addr}:{port}"), slot, on_event)
}

/// Spawn the session binary in `--browse` mode: the console home, in the session window —
/// launches run as streams in that same window. The same stdout contract as a connect
/// (`--json-status`): `ready` when the console window presents, `error` on a failed start,
/// EOF on quit.
pub(crate) fn spawn_browse(
    fullscreen: bool,
    slot: CancelHandle,
    on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    let mut cmd = Command::new(orchestrate::session_binary());
    cmd.arg("--browse");
    cmd.arg("--json-status");
    if fullscreen {
        cmd.arg("--fullscreen");
    }
    spawn(cmd, None, "console", slot, on_event)
}

/// Hand the shell window's position to the child (`--window-pos`) so the session window
/// opens on the same monitor, where the shell is — the hide/restore handoff then reads as
/// one window changing content instead of a window jumping displays.
fn add_window_pos(cmd: &mut Command) {
    if let Some((x, y)) = crate::shell_window::position() {
        cmd.arg("--window-pos").arg(format!("{x},{y}"));
    }
}

/// [`orchestrate::spawn_child`] with this shell's window flags and log tee, folding the
/// contract's `error`/`ended` lines into [`SpawnEvent::Exited`].
fn spawn(
    mut cmd: Command,
    spec_path: Option<PathBuf>,
    label: &str,
    slot: CancelHandle,
    mut on_event: impl FnMut(SpawnEvent) + Send + 'static,
) -> Result<(), String> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    add_window_pos(&mut cmd);
    cmd.creation_flags(CREATE_NO_WINDOW);
    let (mut error, mut ended) = (None::<(String, bool)>, None::<String>);
    orchestrate::spawn_child(cmd, spec_path, Some(slot), crate::logfile::Tee, move |ev| {
        match ev {
            SessionEvent::Ready => on_event(SpawnEvent::Ready),
            SessionEvent::Stats(s) => on_event(SpawnEvent::Stats(s)),
            SessionEvent::Error {
                msg,
                trust_rejected,
            } => error = Some((msg, trust_rejected)),
            SessionEvent::Ended(msg) => ended = Some(msg),
            // orchestrate persists the window size on the way past.
            SessionEvent::Window { .. } => {}
            SessionEvent::Exited(code) => on_event(SpawnEvent::Exited {
                error: error.take(),
                ended: ended.take(),
                code,
            }),
        }
    })
    .map_err(|e| {
        tracing::error!(error = %e, "spawning the session binary");
        "The session didn't start. Check the client log.".to_string()
    })?;
    tracing::info!(host = %label, "session binary spawned");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_failing_exit_is_never_blank() {
        // Clean quit (stream window closed) and our own kill stay silent.
        assert!(silent_exit_banner(0).is_none());
        assert!(silent_exit_banner(-1).is_none());
        // A child that died without speaking the contract names its code — the 0.22.0
        // regression (a stub session binary exiting 2) showed as a blank bounce to the
        // host list precisely because nothing filled this in.
        let banner = silent_exit_banner(2).expect("failing exit must say something");
        assert!(banner.contains('2'), "{banner}");
        assert!(silent_exit_banner(101).is_some());
        // An NTSTATUS names the crash in hex, the form the Event Log and the crash filter use.
        let av = silent_exit_banner(-1073741819).unwrap();
        assert!(av.contains("access violation (0xC0000005)"), "{av}");
        let other = silent_exit_banner(0xC000_0409u32 as i32).unwrap();
        assert!(other.contains("0xC0000409"), "{other}");
    }
}
