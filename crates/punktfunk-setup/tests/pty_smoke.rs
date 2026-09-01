//! The TUI in a real pseudo-terminal, end to end.
//!
//! The frame goldens in `tui_frames` prove what the renderer *builds*; this proves the binary
//! survives an actual terminal — raw mode, cursor control, key decoding, the intro repaint —
//! which is the "TUI panics on a real terminal" class no in-process test can reach.
//!
//! It drives `--demo`, so nothing it runs can touch the machine: demo mode hands the plan a
//! runner that cannot spawn. Runs on the Linux and macOS lanes; unix-only, because the TUI
//! through a real pty is a unix contract — ConPTY re-encodes the stream and the reads hang.
//! The Windows face is the WinUI wizard; its silent path gets its own smoke (WP2.4).
#![cfg(unix)]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Long enough for the intro animation plus ~12 simulated steps on a loaded CI box.
const DEADLINE: Duration = Duration::from_secs(45);

/// Run the binary under a pty, send `keys`, and return everything it drew.
fn run(args: &[&str], keys: &[u8], until: &str) -> String {
    run_with_home(args, keys, until, None)
}

fn run_with_home(args: &[&str], keys: &[u8], until: &str, home: Option<&str>) -> String {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_punktfunk-setup"));
    for arg in args {
        cmd.arg(arg);
    }
    // A real terminal that claims truecolor, so the mark and the rail are exercised rather
    // than degraded away.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("NO_COLOR");
    if let Some(home) = home {
        cmd.env("HOME", home);
        cmd.env_remove("XDG_CONFIG_HOME");
    }
    let mut child = pty.slave.spawn_command(cmd).expect("spawn");
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("reader");
    let mut writer = pty.master.take_writer().expect("writer");
    let collected = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = collected.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });

    // The settings screen has to be on the glass before a keystroke means anything.
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if collected.lock().unwrap().contains("Install now") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = writer.write_all(keys);
    let _ = writer.flush();

    while start.elapsed() < DEADLINE {
        if collected.lock().unwrap().contains(until) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.wait();
    let text = collected.lock().unwrap().clone();
    drop(writer);
    text
}

/// Strip the escape sequences so an assertion reads the text, not the paint.
fn plain(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI and the handful of two-character sequences the renderer emits.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[test]
fn the_demo_walks_from_the_settings_screen_to_the_outro() {
    let text = plain(&run(&["--demo", "debian-fresh", "-v"], b"\r", "Done. Next"));
    assert!(
        text.contains("Install now with these settings"),
        "no settings screen:\n{text}"
    );
    assert!(
        text.contains("Full controller"),
        "the rows did not render:\n{text}"
    );
    // Under `-v` the commands still echo. That transparency is the trust feature, and it is
    // what the flag exists to preserve now that a run collapses to a progress line by default.
    assert!(
        text.contains("+ sudo apt install -y punktfunk-host punktfunk-web punktfunk-scripting"),
        "the command echo is missing:\n{text}"
    );
    assert!(
        text.contains("Done. Next"),
        "never reached the outro:\n{text}"
    );
}

/// The failure state has to render as a failure, not as a silent stop.
#[test]
fn a_failed_step_renders_the_failure_and_points_at_the_docs() {
    let text = plain(&run(
        &["--demo", "debian-fresh", "--fail", "installing"],
        b"\r",
        "that step failed",
    ));
    assert!(
        text.contains("that step failed"),
        "no failure text:\n{text}"
    );
    assert!(
        text.contains("docs.punktfunk.unom.io/docs/debian"),
        "no docs pointer:\n{text}"
    );
    assert!(
        !text.contains("Done. Next"),
        "a failed run printed the success outro:\n{text}"
    );
}

/// D9's promise, as a regression test: `--demo` once wrote a real `~/.config/punktfunk/host.env`
/// because `SetEnv` edits the file directly instead of spawning, so the fake runner never saw
/// it. The preset below is the one that sets two of them.
#[test]
fn the_demo_writes_nothing_into_the_users_home() {
    let home = tempfile::tempdir().expect("tempdir");
    let text = plain(&run_with_home(
        &["--demo", "fedora-sunshine", "-v"],
        b"\r",
        "Done. Next",
        home.path().to_str(),
    ));
    assert!(
        text.contains("PUNKTFUNK_MGMT_BIND"),
        "the preset did not reach a SetEnv step"
    );
    let touched = home.path().join(".config/punktfunk");
    assert!(
        !touched.exists(),
        "demo mode created {} — it must reach the filesystem only through BasePaths",
        touched.display()
    );
}

/// q leaves without running anything, and says so.
#[test]
fn quitting_the_settings_screen_changes_nothing() {
    let text = plain(&run(&["--demo", "arch-fresh"], b"q", "Nothing was changed"));
    assert!(
        text.contains("Nothing was changed"),
        "no cancel outro:\n{text}"
    );
    assert!(
        !text.contains("+ sudo pacman"),
        "a cancelled run echoed a command:\n{text}"
    );
}

/// The default run shows progress, not a transcript: luxus counted the lines on Omarchy and
/// the wall of them is what hid the one warning that mattered.
#[test]
fn the_default_run_collapses_to_a_progress_line() {
    let text = plain(&run(&["--demo", "debian-fresh"], b"\r", "Done. Next"));
    assert!(
        !text.contains("+ sudo apt install"),
        "the command echo should be behind -v:\n{text}"
    );
    assert!(
        text.contains("Done. Next"),
        "never reached the outro:\n{text}"
    );
}
