//! One `EventStream` reader per host process: window and workspace events move
//! [`desktop::window_gen`], so an idle session asks niri nothing.
//!
//! A config reload needs no handler: output changes made over IPC (mode, scale, position,
//! off) survive it.

use super::{desktop, ipc};
use anyhow::{Context, Result};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Once;
use std::thread;
use std::time::Duration;

/// Start the reader once. It outlives sessions and reconnects when niri restarts.
pub(crate) fn ensure_running() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let spawned = thread::Builder::new()
            .name("punktfunk-niri-events".into())
            .spawn(|| loop {
                if let Err(e) = read_events() {
                    tracing::debug!(error = %format!("{e:#}"), "niri: event stream ended");
                }
                desktop::set_window_reader_live(false);
                thread::sleep(Duration::from_secs(2));
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "niri: event reader not started — window lists re-read each time");
        }
    });
}

fn read_events() -> Result<()> {
    let path = crate::session::niri_socket().context("find the niri IPC socket")?;
    let mut stream = UnixStream::connect(&path).with_context(|| format!("connect {path}"))?;
    stream
        .write_all(b"\"EventStream\"\n")
        .context("request the niri event stream")?;
    let mut lines = BufReader::new(stream).lines();
    let first = lines.next().context("niri closed the event stream")??;
    ipc::parse_reply(&first)?;
    desktop::set_window_reader_live(true);
    // Anything may have changed while no reader ran.
    desktop::bump_window_gen();
    for line in lines {
        if moves_windows(&line?) {
            desktop::bump_window_gen();
        }
    }
    Ok(())
}

/// Whether an event (`{"<Kind>": …}`) can change a window list or a window's workspace.
fn moves_windows(line: &str) -> bool {
    let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line) else {
        return false;
    };
    event.keys().any(|kind| {
        kind.starts_with("Window") && kind != "WindowFocusTimestampChanged"
            || kind.starts_with("Workspace")
    })
}

#[cfg(test)]
mod tests {
    use super::moves_windows;

    #[test]
    fn window_and_workspace_events_move_the_token() {
        assert!(moves_windows(
            r#"{"WindowOpenedOrChanged":{"window":{"id":3}}}"#
        ));
        assert!(moves_windows(r#"{"WindowClosed":{"id":3}}"#));
        assert!(moves_windows(r#"{"WorkspacesChanged":{"workspaces":[]}}"#));
        assert!(!moves_windows(
            r#"{"WindowFocusTimestampChanged":{"id":3}}"#
        ));
        assert!(!moves_windows(r#"{"KeyboardLayoutSwitched":{"idx":0}}"#));
        assert!(!moves_windows(r#"{"ConfigLoaded":{"failed":false}}"#));
        assert!(!moves_windows("not json"));
    }
}
