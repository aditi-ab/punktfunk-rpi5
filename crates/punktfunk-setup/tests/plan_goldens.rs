//! The core suite: every Facts preset renders its Plan to dry-run text, compared against
//! `tests/golden/*.txt`. Regenerate with `UPDATE_GOLDEN=1 cargo test -p punktfunk-setup`.
//!
//! The goldens deliberately embed `data/platforms.json`'s install lines, so a platforms.json
//! edit is *supposed* to show up here as a diff to read in review — that is the drift alarm,
//! not a failure to paper over.
//!
//! Below the goldens, one named test per §4 trap of `design/installer-v2.md`. Those assert on
//! the command list rather than the rendering, so they keep meaning when the text moves.

use std::path::Path;

use punktfunk_setup::choices::{Action, Choices, Pins};
use punktfunk_setup::exec::{Executor, Opts};
use punktfunk_setup::facts::{Channel, Facts, Family, Firewall, Nvidia, OsRelease};
use punktfunk_setup::plan::{self, Plan, StepAction};
use punktfunk_setup::report;
use punktfunk_setup::seam::{BasePaths, FakeRunner};
use punktfunk_setup::ui::Plain;

// ------------------------------------------------------------------------------- presets

/// A box with nothing punktfunk on it. Every preset below is this with fields moved, so a
/// golden diff shows the one thing that changed.
fn fresh(id: &str, family: Family) -> Facts {
    let docs = match family {
        Family::Apt if id == "ubuntu" => "ubuntu",
        Family::Apt => "debian",
        Family::Dnf => "fedora",
        Family::Sysext => "bazzite",
        Family::Pacman if id == "omarchy" => "omarchy",
        Family::Pacman => "arch",
        Family::Flatpak => "install",
    };
    Facts {
        os: OsRelease {
            id: id.to_string(),
            id_like: String::new(),
            version_id: String::new(),
            pretty: id.to_string(),
        },
        family,
        omarchy: id == "omarchy",
        docs_page: format!("https://docs.punktfunk.unom.io/docs/{docs}"),
        host_punt: None,
        has_flatpak_client: false,
        rpm_group: (family == Family::Dnf).then(|| "fedora-44".to_string()),
        floor: None,
        couch_box: id == "bazzite" || id == "nobara",
        graphical_seat: true,
        sunshine_active: false,
        current_channel: None,
        installed_pf: vec![],
        missing: vec!["host".into(), "web-console".into(), "plugin-runner".into()],
        host_version: None,
        has_web_server: false,
        has_omarchy_bin: id == "omarchy",
        has_ujust: false,
        in_input_group: false,
        in_punktfunk_group: false,
        has_input_group: true,
        nvidia: Nvidia::Absent,
        has_rpmfusion_ffmpeg: false,
        firewall: Firewall::None,
        systemd_pid1: true,
        user_manager: true,
        web_unit_present: true,
        scripting_unit_disabled: false,
        ip: Some("192.168.1.10".into()),
        user: "pf".into(),
    }
}

/// A box that already has all three binaries, on `channel`.
fn installed(id: &str, family: Family, channel: Channel) -> Facts {
    let pkgs = match family {
        Family::Dnf => vec!["punktfunk", "punktfunk-web", "punktfunk-scripting"],
        Family::Sysext => vec![],
        _ => vec!["punktfunk-host", "punktfunk-web", "punktfunk-scripting"],
    };
    Facts {
        missing: vec![],
        installed_pf: pkgs.into_iter().map(str::to_string).collect(),
        current_channel: Some(channel),
        host_version: Some("punktfunk-host 0.34.0".into()),
        has_web_server: true,
        in_input_group: true,
        ..fresh(id, family)
    }
}

fn pins() -> Pins {
    Pins::default()
}

// -------------------------------------------------------------------------- the mechanism

/// The full dry-run transcript: what `--dry-run` prints, minus the constant banner.
fn render(facts: &Facts, choices: &Choices) -> String {
    let (ui, buf) = Plain::capture();
    let paths = BasePaths::rooted(Path::new("/box"));
    let run =
        FakeRunner::new()
            .with_path("systemctl")
            .answer("systemctl --user show-environment", 0, "");
    // yes:false on purpose — the goldens must carry platforms.json's lines unrewritten, which
    // is what makes them the drift alarm. The --yes rewrite has its own test in exec.
    let opts = Opts {
        dry: true,
        yes: false,
        tty: false,
    };

    report::detected(&ui, facts);
    report::choices_summary(&ui, choices);
    let plan = plan::build(facts, choices);
    let exec = Executor {
        paths: &paths,
        run: &run,
        ui: &ui,
        opts,
    };
    let outcome = exec
        .execute(&plan, facts, choices)
        .expect("a dry run cannot fail");
    if choices.action == Action::Uninstall {
        report::uninstall_outro(&ui);
    } else {
        report::verify(&ui, &run, facts, choices, &outcome, opts);
    }
    buf.borrow().clone()
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("no golden for {name} — run UPDATE_GOLDEN=1 cargo test -p punktfunk-setup")
    });
    assert_eq!(
        actual, expected,
        "golden {name} changed (UPDATE_GOLDEN=1 to accept)"
    );
}

fn check(name: &str, facts: &Facts, pins: &Pins) {
    let choices = Choices::derive(facts, pins);
    golden(name, &render(facts, &choices));
}

fn plan_for(facts: &Facts, pins: &Pins) -> Plan {
    plan::build(facts, &Choices::derive(facts, pins))
}

// ----------------------------------------------------------------------------- the goldens

#[test]
fn fresh_installs() {
    check("arch-fresh", &fresh("arch", Family::Pacman), &pins());
    check("debian-fresh", &fresh("debian", Family::Apt), &pins());
    check("fedora-fresh", &fresh("fedora", Family::Dnf), &pins());
    check("bazzite-couch", &fresh("bazzite", Family::Sysext), &pins());
    check("omarchy-fresh", &fresh("omarchy", Family::Pacman), &pins());
}

#[test]
fn fresh_installs_on_canary() {
    let canary = Pins {
        channel: Some(Channel::Canary),
        ..pins()
    };
    check("arch-fresh-canary", &fresh("arch", Family::Pacman), &canary);
    check(
        "debian-fresh-canary",
        &fresh("debian", Family::Apt),
        &canary,
    );
    check(
        "fedora-fresh-canary",
        &fresh("fedora", Family::Dnf),
        &canary,
    );
    check(
        "bazzite-fresh-canary",
        &fresh("bazzite", Family::Sysext),
        &canary,
    );
}

/// Fedora 43 resolves to the `bazzite` RPM group, which is a sed over the written repo file.
#[test]
fn fedora_43_uses_the_bazzite_rpm_group() {
    let mut facts = fresh("fedora", Family::Dnf);
    facts.rpm_group = Some("bazzite".into());
    check("fedora-43-group", &facts, &pins());
}

#[test]
fn channel_switches_in_both_directions() {
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let to_canary = Pins {
        channel: Some(Channel::Canary),
        ..pins()
    };
    check(
        "arch-switch-to-stable",
        &installed("arch", Family::Pacman, Channel::Canary),
        &to_stable,
    );
    check(
        "debian-switch-to-canary",
        &installed("debian", Family::Apt, Channel::Stable),
        &to_canary,
    );
    check(
        "fedora-switch-to-stable",
        &installed("fedora", Family::Dnf, Channel::Canary),
        &to_stable,
    );
    check(
        "bazzite-switch-to-canary",
        &installed("bazzite", Family::Sysext, Channel::Stable),
        &to_canary,
    );
}

#[test]
fn uninstalls() {
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    check(
        "debian-uninstall",
        &installed("debian", Family::Apt, Channel::Stable),
        &un,
    );
    check(
        "fedora-uninstall",
        &installed("fedora", Family::Dnf, Channel::Stable),
        &un,
    );
    check(
        "arch-uninstall",
        &installed("arch", Family::Pacman, Channel::Stable),
        &un,
    );
    check(
        "omarchy-uninstall",
        &installed("omarchy", Family::Pacman, Channel::Stable),
        &un,
    );
    check(
        "bazzite-uninstall",
        &installed("bazzite", Family::Sysext, Channel::Stable),
        &un,
    );
}

/// An already-complete box: the install is skipped and the setup continues.
#[test]
fn a_re_run_on_a_complete_box_is_a_no_op_install() {
    check(
        "arch-installed-rerun",
        &installed("arch", Family::Pacman, Channel::Canary),
        &pins(),
    );
}

/// The console is missing, so the next-steps text must NOT hand out a URL for it.
#[test]
fn a_box_without_the_console_is_told_so_instead_of_given_a_url() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.has_web_server = false;
    facts.web_unit_present = false;
    facts.missing = vec!["web-console".into()];
    facts.installed_pf = vec!["punktfunk-host".into(), "punktfunk-scripting".into()];
    check("debian-noweb", &facts, &pins());
}

#[test]
fn a_sunshine_box_moves_the_management_port_and_opens_gamestream() {
    let mut facts = fresh("fedora", Family::Dnf);
    facts.sunshine_active = true;
    facts.firewall = Firewall::Firewalld;
    check("fedora-sunshine", &facts, &pins());
}

#[test]
fn a_ufw_box_opens_the_named_profiles() {
    let mut facts = fresh("debian", Family::Apt);
    facts.firewall = Firewall::Ufw;
    let with_gs = Pins {
        gamestream: Some(true),
        clipboard: Some(true),
        ..pins()
    };
    check("debian-ufw-gamestream", &facts, &with_gs);
}

#[test]
fn an_nvidia_box_without_a_driver_is_warned_after_a_successful_install() {
    let mut facts = installed("fedora", Family::Dnf, Channel::Stable);
    facts.nvidia = Nvidia::NoDriver;
    check("fedora-nvidia-nodriver", &facts, &pins());
}

// ------------------------------------------------------------------------- M3, the client

/// The client comes from the same family repo, so host+client is one transaction.
///
/// The `install` assertions are not decoration: a client-only plan once rendered its heading
/// and then no commands at all, because the host guard also gated the family backend.
#[test]
fn client_installs_per_family() {
    let client = Pins {
        client: true,
        ..pins()
    };
    let both = Pins {
        host: true,
        client: true,
        ..pins()
    };
    // The expected line keeps the family's own flags. `pacman -Syu`, not `-S`: a client-only
    // run adds the repo and installs in one go, and `-S` against a database that has never
    // been fetched dies with "target not found" (measured in a container).
    for (name, facts, line) in [
        (
            "debian-client-only",
            fresh("debian", Family::Apt),
            "sudo apt install punktfunk-client",
        ),
        (
            "arch-client-only",
            fresh("arch", Family::Pacman),
            "sudo pacman -Syu punktfunk-client",
        ),
        (
            "fedora-client-only",
            fresh("fedora", Family::Dnf),
            "sudo dnf install punktfunk-client",
        ),
    ] {
        check(name, &facts, &client);
        let cmds = plan_for(&facts, &client).commands();
        assert!(
            cmds.iter().any(|c| c == line),
            "{name} should run `{line}`: {cmds:?}"
        );
    }

    check(
        "debian-host-and-client",
        &fresh("debian", Family::Apt),
        &both,
    );
    let cmds = plan_for(&fresh("debian", Family::Apt), &both).commands();
    assert!(
        cmds.iter()
            .any(|c| c.ends_with("punktfunk-scripting punktfunk-client")),
        "host and client should be one transaction: {cmds:?}"
    );
}

/// A couch box has no `punktfunk-client` package, so the client arrives as a flatpak beside
/// the sysext host rather than not at all.
#[test]
fn a_couch_box_gets_the_client_as_a_flatpak() {
    let both = Pins {
        host: true,
        client: true,
        ..pins()
    };
    check(
        "bazzite-host-and-client",
        &fresh("bazzite", Family::Sysext),
        &both,
    );
    let cmds = plan_for(&fresh("bazzite", Family::Sysext), &both).commands();
    assert!(
        cmds.iter()
            .any(|c| c.contains("punktfunk-sysext.sh install")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("flatpak install --user")),
        "{cmds:?}"
    );
}

/// §5: a client install on a distro with no punktfunk repo takes the flatpak line instead of
/// dying. The host punt stays — `main` enforces that, this proves the plan exists at all.
#[test]
fn an_unsupported_distro_still_installs_a_client() {
    let mut facts = fresh("voidlinux", Family::Flatpak);
    facts.host_punt = Some("no package repo for 'Void Linux' yet".into());
    let client = Pins {
        client: true,
        ..pins()
    };
    check("unsupported-client-only", &facts, &client);
    let cmds = plan_for(&facts, &client).commands();
    assert_eq!(
        cmds.len(),
        1,
        "a client-only install wires nothing else: {cmds:?}"
    );
    assert!(cmds[0].starts_with("flatpak install --user"), "{cmds:?}");
}

/// The per-family sweep cannot see a flatpak, so uninstall needs its own leg for it.
#[test]
fn uninstall_sweeps_a_flatpak_client_too() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.has_flatpak_client = true;
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    assert!(
        cmds.iter()
            .any(|c| c == "flatpak uninstall --user io.unom.Punktfunk"),
        "the flatpak client was left behind: {cmds:?}"
    );
    // A box without one must not be told to remove what is not there.
    let mut bare = installed("debian", Family::Apt, Channel::Stable);
    bare.has_flatpak_client = false;
    assert!(!plan_for(&bare, &un)
        .commands()
        .iter()
        .any(|c| c.contains("flatpak")));
}

/// Client-only asks almost nothing: no groups, gamestream, linger or firewall.
#[test]
fn a_client_only_plan_wires_none_of_the_host_setup() {
    let client = Pins {
        client: true,
        ..pins()
    };
    let cmds = plan_for(&fresh("debian", Family::Apt), &client).commands();
    assert!(!cmds.iter().any(|c| c.contains("usermod")), "{cmds:?}");
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("ufw") || c.contains("firewall-cmd")),
        "{cmds:?}"
    );
    assert!(!cmds.iter().any(|c| c.contains("loginctl")), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.contains("systemctl --user enable")),
        "{cmds:?}"
    );
}

/// The flatpak line is platforms.json's, verbatim — the same one the docs give.
#[test]
fn the_flatpak_line_is_carried_verbatim() {
    let client = Pins {
        client: true,
        ..pins()
    };
    let facts = fresh("bazzite", Family::Sysext);
    let text = render(&facts, &Choices::derive(&facts, &client));
    for line in punktfunk_setup::platform::install_lines("linux-client") {
        assert!(
            text.contains(&line),
            "the flatpak line drifted:\n  {line}\n\n{text}"
        );
    }
}

// -------------------------------------------------------- design/installer-v2.md §4 traps

/// A bare re-run on a canary machine must never drag it to stable.
#[test]
fn trap_channel_follows_the_box_without_an_explicit_flag() {
    let facts = installed("arch", Family::Pacman, Channel::Canary);
    let choices = Choices::derive(&facts, &pins());
    assert_eq!(choices.channel, Channel::Canary);
    assert_eq!(choices.switch_from, None);
    let cmds = plan_for(&facts, &pins()).commands();
    assert!(
        !cmds.iter().any(|c| c.contains("-Sy")),
        "a no-op re-run touched the repo: {cmds:?}"
    );
}

/// apt cannot walk back to a lower candidate, so the switch names exact versions and allows
/// the downgrade. In dry-run the version is a placeholder, which is what sh prints too.
#[test]
fn trap_apt_switch_pins_versions_and_allows_downgrades() {
    let facts = installed("debian", Family::Apt, Channel::Canary);
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let text = render(&facts, &Choices::derive(&facts, &to_stable));
    assert!(
        text.contains("sudo apt install --allow-downgrades"),
        "{text}"
    );
    assert!(text.contains("punktfunk-host=<version>"), "{text}");
    // The repo is rewritten to the TARGET channel first, so madison's first row is its newest.
    let repo = text
        .find(" stable main")
        .expect("the sources line names the target channel");
    let install = text.find("--allow-downgrades").expect("the pinned install");
    assert!(
        repo < install,
        "the pins were resolved against the old channel:\n{text}"
    );
}

/// `-Sy` then `-S`, never `-Syu`: a sysupgrade sees a lower stable version and does nothing.
#[test]
fn trap_pacman_switch_uses_sy_then_s_and_never_syu() {
    let facts = installed("arch", Family::Pacman, Channel::Canary);
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let cmds = plan_for(&facts, &to_stable).commands();
    assert!(cmds.contains(&"sudo pacman -Sy".to_string()), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.starts_with("sudo pacman -Syu")),
        "{cmds:?}"
    );
    // The old section goes first, or both repos end up enabled.
    assert!(
        cmds[0].contains("sed -i"),
        "the old repo section must be dropped first: {cmds:?}"
    );
}

/// Omarchy's libalpm hook aborts any transaction carrying both -S and -u.
#[test]
fn trap_omarchy_installs_with_sy_then_s_not_syu() {
    let cmds = plan_for(&fresh("omarchy", Family::Pacman), &pins()).commands();
    assert!(cmds.contains(&"sudo pacman -Sy".to_string()), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.starts_with("sudo pacman -Syu")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.contains("punktfunk-client")),
        "omarchy gets the client too"
    );
}

/// `punktfunk-omarchy remove` ships IN the host package, so it must run before pacman takes it.
#[test]
fn trap_omarchy_uninstall_runs_remove_before_pacman_rns() {
    let facts = installed("omarchy", Family::Pacman, Channel::Stable);
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    let remove = cmds
        .iter()
        .position(|c| c == "punktfunk-omarchy remove")
        .expect("remove step");
    let rns = cmds
        .iter()
        .position(|c| c.starts_with("sudo pacman -Rns"))
        .expect("pacman -Rns");
    assert!(
        remove < rns,
        "the wiring must come off before the binary that removes it: {cmds:?}"
    );
}

/// The hand-off does the groups, firewall and autostart itself, so nothing generic runs after.
#[test]
fn trap_the_omarchy_hand_off_ends_the_run() {
    let plan = plan_for(&fresh("omarchy", Family::Pacman), &pins());
    let handoff = plan
        .steps()
        .find(|s| matches!(&s.action, StepAction::RunIfPresent { cmd, .. } if cmd == "punktfunk-omarchy setup"))
        .expect("the hand-off step");
    assert!(handoff.ends_run);
}

/// The smoke has to be able to decline the hand-off; the sh installer's question had no twin.
#[test]
fn trap_the_hand_off_can_be_declined() {
    let declined = Pins {
        omarchy_setup: Some(false),
        ..pins()
    };
    let cmds = plan_for(&fresh("omarchy", Family::Pacman), &declined).commands();
    assert!(
        !cmds.contains(&"punktfunk-omarchy setup".to_string()),
        "{cmds:?}"
    );
    // Declining continues generically; nothing is lost.
    assert!(
        cmds.iter().any(|c| c.contains("usermod -aG punktfunk")),
        "{cmds:?}"
    );
}

/// A switch must MOVE every installed punktfunk package, or the ones this installer does not
/// itself install are stranded on the channel the box just left.
#[test]
fn trap_switch_pkgs_carries_packages_the_installer_never_installed() {
    let mut facts = installed("arch", Family::Pacman, Channel::Canary);
    facts.installed_pf.push("punktfunk-gamescope".into());
    facts.installed_pf.push("punktfunk-client".into());
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let text = render(&facts, &Choices::derive(&facts, &to_stable));
    assert!(
        text.contains("punktfunk-gamescope"),
        "gamescope was stranded:\n{text}"
    );
    assert!(
        text.contains("punktfunk-client"),
        "the client was stranded:\n{text}"
    );
}

/// dnf goes down with distro-sync; install alone only ever moves up.
#[test]
fn trap_dnf_switch_installs_then_distro_syncs() {
    let facts = installed("fedora", Family::Dnf, Channel::Canary);
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let cmds = plan_for(&facts, &to_stable).commands();
    let install = cmds
        .iter()
        .position(|c| c.starts_with("sudo dnf install"))
        .expect("install");
    let sync = cmds
        .iter()
        .position(|c| c.starts_with("sudo dnf distro-sync"))
        .expect("sync");
    assert!(install < sync, "{cmds:?}");
}

/// Linger creates the user manager on a seatless box, so it has to land before the enable.
#[test]
fn trap_linger_is_planned_before_the_unit_enable() {
    use punktfunk_setup::plan::Phase;
    let mut facts = fresh("debian", Family::Apt);
    facts.graphical_seat = false;
    let plan = plan_for(&facts, &pins());
    let linger = plan
        .phases
        .iter()
        .position(|p| p.kind == Phase::Linger)
        .expect("linger phase");
    let start = plan
        .phases
        .iter()
        .position(|p| p.kind == Phase::Start)
        .expect("start phase");
    assert!(linger < start, "the user manager would not exist yet");
}

/// Three packages, so "is the host there?" is the wrong question to skip the install on.
#[test]
fn trap_a_box_with_the_host_but_no_console_still_installs() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.missing = vec!["web-console".into()];
    let cmds = plan_for(&facts, &pins()).commands();
    assert!(
        cmds.iter()
            .any(|c| c == "sudo apt install punktfunk-host punktfunk-web punktfunk-scripting"),
        "a weak-deps-off box would never grow a console: {cmds:?}"
    );
}

/// Only Bazzite and Nobara. Silverblue and Bluefin ship rpm-ostree and ujust and are desktops.
#[test]
fn trap_couch_defaults_do_not_reach_a_workstation_image() {
    let couch = Choices::derive(&fresh("bazzite", Family::Sysext), &pins());
    assert!(couch.linger);
    let mut workstation = fresh("bluefin", Family::Sysext);
    workstation.couch_box = false;
    assert!(!Choices::derive(&workstation, &pins()).linger);
}

/// User units come off first: package removal cannot see the enable symlinks in $HOME.
#[test]
fn trap_uninstall_disables_user_units_before_removing_packages() {
    for (id, family, verb) in [
        ("debian", Family::Apt, "sudo apt purge"),
        ("fedora", Family::Dnf, "sudo dnf remove"),
        ("arch", Family::Pacman, "sudo pacman -Rns"),
    ] {
        let facts = installed(id, family, Channel::Stable);
        let un = Pins {
            action: Action::Uninstall,
            ..pins()
        };
        let cmds = plan_for(&facts, &un).commands();
        assert!(
            cmds[0].starts_with("systemctl --user disable --now"),
            "{id}: {cmds:?}"
        );
        assert!(cmds.iter().any(|c| c.starts_with(verb)), "{id}: {cmds:?}");
    }
}

/// An uninstall names only the packages actually installed — never a fixed list.
#[test]
fn trap_uninstall_removes_only_what_is_installed() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.installed_pf = vec!["punktfunk-host".into()];
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    assert!(
        cmds.contains(&"sudo apt purge punktfunk-host".to_string()),
        "{cmds:?}"
    );
}

/// Nothing punktfunk installed: there is no purge line at all to fail on.
#[test]
fn trap_uninstall_on_a_bare_box_skips_the_package_removal() {
    let facts = fresh("debian", Family::Apt);
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    assert!(
        !cmds.iter().any(|c| c.starts_with("sudo apt purge")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("sudo rm -f")),
        "the repo still goes: {cmds:?}"
    );
}

/// Bazzite's input group is recipe-managed; usermod is the wrong tool there.
#[test]
fn trap_a_ujust_box_uses_the_recipe_not_usermod() {
    let mut facts = fresh("bazzite", Family::Sysext);
    facts.has_ujust = true;
    let cmds = plan_for(&facts, &pins()).commands();
    assert!(
        cmds.contains(&"ujust add-user-to-input-group".to_string()),
        "{cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("usermod -aG input")),
        "{cmds:?}"
    );
}

/// Every host platform's platforms.json lines must appear in the dry-run text verbatim. This
/// is D6's drift gate, moved into the crate that embeds the file.
#[test]
fn every_platforms_json_install_line_is_carried_verbatim() {
    let cases = [
        ("debian", fresh("debian", Family::Apt)),
        ("arch", fresh("arch", Family::Pacman)),
        ("omarchy", fresh("omarchy", Family::Pacman)),
        ("fedora", fresh("fedora", Family::Dnf)),
        ("bazzite", fresh("bazzite", Family::Sysext)),
    ];
    for (id, facts) in cases {
        let text = render(&facts, &Choices::derive(&facts, &pins()));
        for line in punktfunk_setup::platform::install_lines(id) {
            assert!(
                text.contains(&line),
                "the {id} dry-run no longer carries platforms.json's line verbatim:\n  {line}\n\n{text}"
            );
        }
    }
}
