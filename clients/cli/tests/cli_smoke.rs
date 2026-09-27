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

/// `default-host` is the only door to the start-screen pointer on a headless box, and the
/// verbs that read it must refuse rather than guess. Runs against a scratch directory.
/// `PUNKTFUNK_CONFIG_DIR` is set there too, so a developer export cannot point the
/// verb at the real identity.
#[test]
fn default_host_is_set_read_and_cleared() {
    let home = std::env::temp_dir().join(format!("pf-cli-default-host-{}", std::process::id()));
    let store = home.join(if cfg!(windows) {
        "punktfunk"
    } else {
        ".config/punktfunk"
    });
    std::fs::create_dir_all(&store).expect("scratch config dir");
    std::fs::write(
        store.join("client-known-hosts.json"),
        r#"{"hosts":[{"name":"Desk","addr":"10.0.0.5","port":9777,
           "fp_hex":"aa","paired":true,"id":"rec-1"}]}"#,
    )
    .expect("seed the store");

    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_punktfunk"))
            .args(args)
            .env(if cfg!(windows) { "APPDATA" } else { "HOME" }, &home)
            .env("PUNKTFUNK_CONFIG_DIR", &store)
            .output()
            .expect("run punktfunk")
    };

    // One paired host derives, with nothing written.
    let out = run(&["default-host"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Desk") && stdout.contains("derived"),
        "{stdout}"
    );

    let out = run(&["default-host", "Desk"]);
    assert!(
        out.status.success(),
        "naming a paired host must be accepted"
    );
    assert!(String::from_utf8_lossy(&run(&["default-host"]).stdout).contains("explicit"));

    let out = run(&["default-host", "--clear"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&run(&["default-host"]).stdout).contains("derived"));

    // No records at all: nothing to derive, and the verbs that read it say so.
    std::fs::write(store.join("client-known-hosts.json"), r#"{"hosts":[]}"#).expect("empty store");
    assert!(String::from_utf8_lossy(&run(&["default-host"]).stdout).contains("none"));
    let out = run(&["library"]);
    assert_eq!(out.status.code(), Some(5), "no default host is not-found");
    assert!(String::from_utf8_lossy(&out.stderr).contains("no default host"));

    std::fs::remove_dir_all(&home).ok();
}

/// `hosts forget` takes what is keyed on the record with it: a default pointing at it and its
/// cached game list, which a re-pair of another box would otherwise inherit.
#[test]
fn forgetting_a_host_clears_its_default_and_cached_catalog() {
    let home = std::env::temp_dir().join(format!("pf-cli-forget-{}", std::process::id()));
    let store = home.join("config");
    let fp = "ab".repeat(32);
    let catalog = if cfg!(windows) {
        home.join("punktfunk/cache/library")
    } else {
        home.join("punktfunk/library")
    };
    std::fs::create_dir_all(&store).expect("scratch config dir");
    std::fs::create_dir_all(&catalog).expect("scratch cache dir");
    std::fs::write(
        store.join("client-known-hosts.json"),
        format!(r#"{{"hosts":[{{"name":"Desk","addr":"10.0.0.5","port":9777,"fp_hex":"{fp}","paired":true,"id":"rec-1"}}]}}"#),
    )
    .expect("seed the store");
    let settings_file = store.join(if cfg!(windows) {
        "client-windows-settings.json"
    } else {
        "client-gtk-settings.json"
    });
    std::fs::write(&settings_file, r#"{"default_host":"rec-1"}"#).expect("seed the default");
    let cached = catalog.join(format!("{fp}.json"));
    std::fs::write(&cached, r#"{"games":[],"fetched_at":0}"#).expect("seed the catalog");

    let out = Command::new(env!("CARGO_BIN_EXE_punktfunk"))
        .args(["hosts", "forget", "Desk"])
        .env("PUNKTFUNK_CONFIG_DIR", &store)
        .env("XDG_CACHE_HOME", &home)
        .env("LOCALAPPDATA", &home)
        .output()
        .expect("run punktfunk");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let settings = std::fs::read_to_string(&settings_file).expect("settings written back");
    assert!(!settings.contains("rec-1"), "{settings}");
    assert!(!cached.exists(), "the cached catalog outlived the host");

    std::fs::remove_dir_all(&home).ok();
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
