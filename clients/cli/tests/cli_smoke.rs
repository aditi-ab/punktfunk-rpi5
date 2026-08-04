//! Runs the REAL `punktfunk` binary and pins its self-documentation contract: help goes
//! to stdout and exits 0, an unknown verb refuses on stderr with the not-found code.
//!
//! This exists so CI *executes* the shipped binary at least once per platform. A binary
//! that compiles but is the wrong program passes every build/clippy/fmt gate — that is
//! exactly how 0.22.0 shipped a stub as `punktfunk-session` — and only a gate that runs
//! the thing catches the class. The help paths are the right probe: they touch no config
//! stores and no network, so they are safe on any runner.
#![cfg(any(target_os = "linux", windows))]

use std::process::Command;

fn punktfunk(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_punktfunk"))
        .args(args)
        .output()
        .expect("run punktfunk")
}

#[test]
fn bare_and_help_print_the_overview_on_stdout() {
    for args in [&[][..], &["help"][..], &["--help"][..], &["-h"][..]] {
        let out = punktfunk(args);
        assert!(out.status.success(), "{args:?} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("punktfunk pair"),
            "{args:?} overview lists the verbs"
        );
        assert!(
            stdout.contains("help <command>"),
            "{args:?} overview points at per-verb help"
        );
    }
}

#[test]
fn per_verb_help_answers_both_spellings() {
    for args in [
        &["help", "launch"][..],
        &["launch", "--help"][..],
        &["launch", "-h"][..],
    ] {
        let out = punktfunk(args);
        assert!(out.status.success(), "{args:?} must exit 0");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("--exec"),
            "{args:?} documents launch's flags"
        );
    }
}

#[test]
fn version_prints_the_crate_version() {
    let out = punktfunk(&["--version"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn unknown_verbs_refuse_with_the_not_found_code() {
    let out = punktfunk(&["frobnicate"]);
    assert_eq!(out.status.code(), Some(5), "unknown verb exits 5");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown command"));

    let out = punktfunk(&["help", "frobnicate"]);
    assert_eq!(out.status.code(), Some(5), "unknown help topic exits 5");
}

/// `discover` and `launch --request-access` document themselves. Help only — the verbs
/// themselves browse the LAN and dial a host, which no runner may be asked to do.
///
/// The Decky panel detects a too-old client by exactly the signature the test above pins
/// (exit 5 + `unknown command`), so this is the other half of that contract: on a client new
/// enough, `discover` is a verb with help rather than an unknown word.
#[test]
fn the_request_access_surfaces_document_themselves() {
    let out = punktfunk(&["help", "discover"]);
    assert!(out.status.success(), "discover has its own help topic");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--timeout"), "discover documents --timeout");
    assert!(stdout.contains("--json"), "discover documents --json");

    let out = punktfunk(&["launch", "--help"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("--request-access"));
}
