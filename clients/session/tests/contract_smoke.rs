//! Runs the REAL `punktfunk-session` binary and asserts it speaks the stdout contract.
//!
//! 0.22.0 shipped a stub as `punktfunk-session` — a stray copy of the GTK shell replaced
//! this crate's `main.rs`, and every build gate stayed green because the wrong program
//! compiled perfectly. Nothing in CI ever *ran* the binary; the failure only existed at
//! runtime, as a connect that silently bounced back to the host list. This test is the
//! gate that would have caught it: whatever the binary does, it must SAY so on stdout.
#![cfg(any(target_os = "linux", windows))]

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A failing connect must still produce a contract line. Port 1 on loopback refuses
/// instantly and the all-zero pin keeps trust logic out of the way; whatever fails first
/// on this machine — presenter init on a headless runner, the dial on a box with a
/// display — the contract requires one `{"error"…}` JSON line on stdout saying so. (On a
/// desktop this may flash a window for under a second; the connect refuses immediately.)
#[test]
fn a_failing_connect_still_speaks_the_contract() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_punktfunk-session"))
        .args([
            "--connect",
            "127.0.0.1:1",
            "--fp",
            &"0".repeat(64),
            "--connect-timeout",
            "5",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn punktfunk-session");

    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });

    // Generous bound: presenter init on a cold CI runner can be slow, but a healthy
    // binary answers in seconds. Only a hang (or a stub that inherited our stdout and
    // blocked) gets anywhere near it.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait().expect("wait on punktfunk-session") {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("punktfunk-session neither exited nor spoke within 120 s");
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }

    let out = reader.join().expect("stdout reader");
    let spoke = out.lines().map(str::trim).any(|l| {
        l.starts_with('{')
            && (l.contains("\"error\"") || l.contains("\"ready\"") || l.contains("\"ended\""))
    });
    assert!(
        spoke,
        "no stdout-contract line — the wrong program may be wearing this binary's name. \
         stdout was:\n{out}"
    );
}
