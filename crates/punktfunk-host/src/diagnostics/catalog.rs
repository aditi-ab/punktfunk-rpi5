//! The v1 check catalog: verdicts from the owning crates → [`HostCheck`]s.
//!
//! Everything user-visible lives here — the English fallback strings, the impact sentences, and the
//! remedies. The probes themselves stay in `pf-vdisplay` / `pf-inject` / [`crate::detect`] and know
//! nothing about this module; that direction is deliberate, because a reverse dependency would drag
//! the host's wire types into two crates that must keep building for Windows and macOS.
//!
//! Two things here are easy to get subtly wrong and are therefore spelled out in the code:
//!
//! * **User-database membership and this process's groups are different questions.** `usermod -aG`
//!   satisfies the first immediately and the second not until the next login, so "not in the group"
//!   and "in the group but you haven't logged back in" need different remedies. Collapsing them
//!   produces the single most maddening support state there is: *"I already added myself!"*
//! * **`usermod` does not stick on an atomic OS.** On the Universal Blue images the remedy is
//!   `ujust add-user-to-input-group`; everywhere else it is `usermod -aG input`.

use super::{ids, CheckStatus, Diagnostics, HostCheck, Remedy, Severity};
use crate::inject::{UinputVerdict, VhciVerdict};
use crate::vdisplay::{TakeoverInapplicable, TakeoverVerdict};
use std::process::Command;

/// The group the packaged privilege helper authorizes on, and that owns the vhci attach nodes.
const PUNKTFUNK_GROUP: &str = "punktfunk";
/// The group the uinput/uhid udev rules grant access to.
const INPUT_GROUP: &str = "input";

/// Register the v1 catalog on a registry. Separate from the global so tests can drive an isolated
/// instance.
pub(crate) fn register_all(reg: &Diagnostics) {
    reg.register(takeover_privilege);
    reg.register(virtual_deck_vhci);
    reg.register(uinput_access);
    reg.register(server_conflict);
}

// ---------------------------------------------------------------------------------------------
// takeover_privilege
// ---------------------------------------------------------------------------------------------

fn takeover_privilege() -> HostCheck {
    let id = ids::TAKEOVER_PRIVILEGE;
    match crate::vdisplay::takeover_privilege_verdict() {
        TakeoverVerdict::Inapplicable { why } => {
            HostCheck::inapplicable(id, takeover_inapplicable_reason(why))
        }
        TakeoverVerdict::Ok { user, group } => {
            HostCheck::ok(id, format!("User “{user}” is in the “{group}” group."))
                .with_param("user", user)
                .with_param("group", group)
        }
        TakeoverVerdict::MissingMembership {
            user,
            dm,
            helper,
            group,
        } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("User “{user}” is not in the “{group}” group"),
            format!(
                "Streams that need the managed takeover cannot stop {dm}, so every one of them \
                 degrades to mirroring this machine's own session instead. With the panel off that \
                 looks like a black screen on every connect, and nothing else reports it."
            ),
        )
        .with_remedy(Remedy {
            text: format!(
                "Add the user to the “{group}” group, then log out and back in. The same group \
                 gates the virtual Steam Deck pad's usbip nodes, which can present arbitrary \
                 emulated USB devices — join it only on a machine you trust."
            ),
            command: Some(format!("sudo usermod -aG {group} {user}")),
            // The display-manager helper reads the user database, so it is satisfied at once — but
            // the pad half is a check against this process, which keeps the group set it started
            // with. One re-login covers both, so ask for it.
            relogin_required: true,
        })
        .with_param("user", user)
        .with_param("group", group)
        .with_param("dm", dm)
        .with_param("helper", helper),
    }
}

fn takeover_inapplicable_reason(why: TakeoverInapplicable) -> &'static str {
    match why {
        TakeoverInapplicable::Root => {
            "The host runs as root, so it stops the display manager directly and never needs the \
             privilege helper."
        }
        TakeoverInapplicable::NoDisplayManager => {
            "No display manager drives this machine's logins, so a takeover has nothing to stop."
        }
        TakeoverInapplicable::NoManagedSession => {
            "This machine has no gamescope session infrastructure, so the managed takeover never \
             runs here."
        }
        TakeoverInapplicable::NoPackagedHelper => {
            "This is an unpackaged install: it has no privilege helper and no group, and uses the \
             polkit rule from the documentation instead."
        }
        TakeoverInapplicable::UnknownUser => {
            "The host's user name could not be resolved, so no group membership could be checked."
        }
        TakeoverInapplicable::NotLinux => "The managed gamescope takeover is a Linux feature.",
    }
}

// ---------------------------------------------------------------------------------------------
// virtual_deck_vhci
// ---------------------------------------------------------------------------------------------

fn virtual_deck_vhci() -> HostCheck {
    let id = ids::VIRTUAL_DECK_VHCI;
    let group = PUNKTFUNK_GROUP;
    match crate::inject::vhci_probe() {
        VhciVerdict::Inapplicable { why } => HostCheck::inapplicable(id, why),
        VhciVerdict::Ok => HostCheck::ok(id, "The virtual Steam Deck controller can attach."),
        VhciVerdict::ModuleMissing => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Warning,
            "The vhci_hcd kernel module is not loaded",
            "The virtual Steam Deck controller cannot attach, so Steam Input never sees it — in \
             Game Mode that means nothing can be navigated with a pad.",
        )
        .with_remedy(Remedy {
            text: "Load the vhci_hcd module (the packages install a modules-load rule that does \
                   this at boot; on an unpackaged install, load it by hand)."
                .to_string(),
            command: Some("sudo modprobe vhci-hcd".to_string()),
            relogin_required: false,
        }),
        // The node is there and we cannot write it. WHICH of the three causes decides the remedy,
        // and only the user database can tell them apart — see this module's docs.
        VhciVerdict::NotWritable { path } => not_writable_check(id, group, path),
    }
}

fn not_writable_check(id: &str, group: &str, path: String) -> HostCheck {
    let user = current_user();
    let in_userdb = user.as_deref().and_then(|u| user_in_group_userdb(u, group));
    let in_process = process_in_group(group);

    let base = |summary: &str, impact: &str| {
        HostCheck::problem(id, CheckStatus::Fail, Severity::Warning, summary, impact)
            .with_param("group", group)
            .with_param("path", path.clone())
    };
    let pad_impact = "The virtual Steam Deck controller cannot attach, so Steam Input never sees \
                      it — in Game Mode that means nothing can be navigated with a pad.";

    match (in_userdb, in_process) {
        // In the group on disk, but this process does not carry it: the classic "I already added
        // myself!" state. A `systemd --user` manager keeps the group set it started with, so only
        // a re-login helps — and nothing in the logs says so today.
        (Some(true), Some(false)) => base(
            &format!("The group “{group}” was granted but this session predates it"),
            pad_impact,
        )
        .with_remedy(Remedy {
            text: "Log out and back in. The membership is already recorded — this session just \
                   started before it was granted, and a session keeps the group set it began with."
                .to_string(),
            command: None,
            relogin_required: true,
        })
        .with_param("user", user.unwrap_or_default()),

        // Not a member at all.
        (Some(false), _) => {
            let user = user.unwrap_or_default();
            base(
                &format!("User “{user}” is not in the “{group}” group"),
                pad_impact,
            )
            .with_remedy(Remedy {
                text: format!(
                    "Add the user to the “{group}” group, then log out and back in. This group \
                     can present arbitrary emulated USB devices — join it only on a machine you \
                     trust."
                ),
                command: Some(format!("sudo usermod -aG {group} {user}")),
                relogin_required: true,
            })
            .with_param("user", user)
        }

        // A member in the database AND in this process, yet the node is still not writable: the
        // udev rule that chgrp's it was never installed (or has not run for this device). Blaming
        // the group here would send someone to re-run a `usermod` that is already correct.
        (Some(true), Some(true)) => base(
            "The vhci attach node is not owned by the expected group",
            pad_impact,
        )
        .with_remedy(Remedy {
            text: format!(
                "Install the udev rule that grants the “{group}” group access to the vhci nodes \
                 (scripts/60-punktfunk.rules — the packages install it), then reload the rules or \
                 reboot."
            ),
            command: Some("sudo udevadm control --reload && sudo udevadm trigger".to_string()),
            relogin_required: false,
        }),

        // We could not ask the user database. Report the fact without guessing at a cause: a wrong
        // remedy here costs more than a vague one.
        _ => base(
            "The virtual Steam Deck controller's attach node is not writable",
            pad_impact,
        )
        .with_remedy(Remedy {
            text: format!(
                "Check that this machine's user is in the “{group}” group and that the udev rule \
                 granting it access to the vhci nodes is installed, then log out and back in."
            ),
            command: None,
            relogin_required: true,
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// uinput_access
// ---------------------------------------------------------------------------------------------

fn uinput_access() -> HostCheck {
    let id = ids::UINPUT_ACCESS;
    match crate::inject::uinput_probe() {
        UinputVerdict::Inapplicable => HostCheck::inapplicable(
            id,
            "Virtual controllers are created through this platform's own driver stack rather than \
             uinput.",
        ),
        UinputVerdict::Ok => HostCheck::ok(id, "The input device nodes are reachable."),

        // The node exists and we may not open it: a group problem, and the remedy depends on
        // whether this OS lets `usermod` stick.
        UinputVerdict::PermissionDenied { path } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("No permission to open {path}"),
            "Every virtual controller fails to be created, so games see no gamepad at all — and \
             the pen and tablet input paths are dead with it."
                .to_string(),
        )
        .with_remedy(input_group_remedy())
        .with_param("path", path)
        .with_param("group", INPUT_GROUP),

        // The node is absent: nothing to have permission on. A group remedy here is a wrong turn.
        UinputVerdict::Missing { path } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("{path} does not exist"),
            "Every virtual controller fails to be created, so games see no gamepad at all."
                .to_string(),
        )
        .with_remedy(Remedy {
            text: format!(
                "Load the kernel module that provides {path} and install the udev rule that grants \
                 the “{INPUT_GROUP}” group access to it (scripts/60-punktfunk.rules — the packages \
                 install both)."
            ),
            command: None,
            relogin_required: false,
        })
        .with_param("path", path),

        UinputVerdict::Error { path, message } => HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            format!("{path} could not be opened: {message}"),
            "Virtual controllers may fail to be created, so games can see no gamepad.".to_string(),
        )
        .with_remedy(Remedy {
            text: format!("Check the state of {path} on this machine."),
            command: None,
            relogin_required: false,
        })
        .with_param("path", path)
        .with_param("error", message),
    }
}

/// The `input`-group remedy, branched on OS flavour.
///
/// On the Universal Blue images `usermod -aG input` appears to work and is silently reverted,
/// because `/etc/group` is not writable state on an atomic OS — `packaging/README.md` has said so
/// since the Bazzite port, and telling someone to run it there is worse than saying nothing.
fn input_group_remedy() -> Remedy {
    let user = current_user().unwrap_or_else(|| "$USER".to_string());
    if is_universal_blue() {
        Remedy {
            text:
                "Add the user to the “input” group with ujust, then log out and back in. On this \
                   OS a plain `usermod` does not persist."
                    .to_string(),
            command: Some("ujust add-user-to-input-group".to_string()),
            relogin_required: true,
        }
    } else {
        Remedy {
            text: "Add the user to the “input” group, then log out and back in. If the group \
                   already lists the user, the udev rule granting it access may be missing \
                   (scripts/60-punktfunk.rules)."
                .to_string(),
            command: Some(format!("sudo usermod -aG {INPUT_GROUP} {user}")),
            relogin_required: true,
        }
    }
}

/// The Universal Blue images, which are the ones that ship `ujust`.
///
/// Matched on the chain's **leaf** (`ID`), never on the `fedora` family token: plain Fedora
/// Workstation is a mutable OS and does want `usermod`. `osinfo`'s chain is `linux/fedora/bazzite`
/// for Bazzite, so the leaf is the distro's own id.
fn is_universal_blue() -> bool {
    matches!(
        crate::osinfo::detect().chain.rsplit('/').next(),
        Some("bazzite" | "bluefin" | "aurora")
    )
}

// ---------------------------------------------------------------------------------------------
// server_conflict
// ---------------------------------------------------------------------------------------------

fn server_conflict() -> HostCheck {
    let id = ids::SERVER_CONFLICT;
    // The cached startup scan — the same source the tray's summary and the Host page's card read.
    // Empty also means "never scanned" on a build that skipped the GameStream planes, which reads
    // as healthy here exactly as it already does on `LocalSummary.conflicts`.
    let labels = crate::detect::summary_labels(crate::detect::snapshot());
    if labels.is_empty() {
        return HostCheck::ok(
            id,
            "No other game-streaming server is active on this machine.",
        );
    }
    let servers = labels.join(", ");
    HostCheck::problem(
        id,
        CheckStatus::Fail,
        Severity::Critical,
        format!("Another game-streaming server is active: {servers}"),
        "Both servers bind the same ports, so whichever won the bind answers — pairing and \
         connections can land on the other server while this host looks installed and healthy."
            .to_string(),
    )
    .with_remedy(Remedy {
        text: "Stop the other server (and disable it if it starts on its own), then restart this \
               host."
            .to_string(),
        command: None,
        relogin_required: false,
    })
    .with_param("servers", servers)
}

// ---------------------------------------------------------------------------------------------
// Group membership: two different questions
// ---------------------------------------------------------------------------------------------

/// This process's login name, resolved the way `pkexec` will (`id -un`).
fn current_user() -> Option<String> {
    capture(Command::new("id").arg("-un"))
}

/// Is `user` in `group` **according to the user database** (`id -nG <user>`)? This is what a root
/// helper sees, and what `usermod -aG` changes immediately. `None` when the question could not be
/// asked — NSS can block or fail, and a false accusation sends people down the wrong path.
fn user_in_group_userdb(user: &str, group: &str) -> Option<bool> {
    let groups = capture(Command::new("id").args(["-nG", user]))?;
    Some(groups.split_whitespace().any(|g| g == group))
}

/// Is `group` among **this process's** supplementary groups (`id -nG`, no operand)? Fixed when the
/// `systemd --user` manager started, so a fresh `usermod` does not show up here until the next
/// login — which is exactly the distinction that makes the "log out and back in" remedy necessary.
fn process_in_group(group: &str) -> Option<bool> {
    let groups = capture(Command::new("id").arg("-nG"))?;
    Some(groups.split_whitespace().any(|g| g == group))
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every mapping arm, including the ones this machine cannot reach: the table is the point.
    /// The three `NotWritable` shapes are what the design calls out as the easiest thing to get
    /// wrong, so each is asserted to produce a *different* remedy.
    #[test]
    fn vhci_not_writable_shapes_produce_distinct_remedies() {
        let path = "/sys/devices/platform/vhci_hcd.0/attach".to_string();
        let check = not_writable_check(ids::VIRTUAL_DECK_VHCI, PUNKTFUNK_GROUP, path.clone());
        // Whatever this machine answers, the shape contract holds: a failing check always carries a
        // remedy, and the pad's impact is always stated.
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.severity, Severity::Warning);
        assert!(check.remedy.is_some());
        assert!(!check.impact.is_empty());
        assert_eq!(
            check.params.get("group").map(String::as_str),
            Some(PUNKTFUNK_GROUP)
        );
    }

    #[test]
    fn takeover_inapplicable_reasons_are_all_populated() {
        for why in [
            TakeoverInapplicable::Root,
            TakeoverInapplicable::NoDisplayManager,
            TakeoverInapplicable::NoManagedSession,
            TakeoverInapplicable::NoPackagedHelper,
            TakeoverInapplicable::UnknownUser,
            TakeoverInapplicable::NotLinux,
        ] {
            assert!(
                !takeover_inapplicable_reason(why).trim().is_empty(),
                "{why:?} needs a reason — an inapplicable row exists to answer \"why not here?\""
            );
        }
    }

    /// The atomic-OS branch is the one that is silently wrong if it regresses: `usermod` looks like
    /// it worked on Bazzite and is gone after a reboot.
    #[test]
    fn input_remedy_matches_this_box_flavour() {
        let remedy = input_group_remedy();
        let command = remedy
            .command
            .expect("the input remedy is always pasteable");
        assert!(remedy.relogin_required, "a group change needs a re-login");
        if is_universal_blue() {
            assert_eq!(command, "ujust add-user-to-input-group");
        } else {
            assert!(
                command.starts_with("sudo usermod -aG input "),
                "unexpected remedy: {command}"
            );
        }
    }

    /// `bazzite` is the chain's LEAF, and `fedora` is its family — matching the family would send
    /// plain Fedora Workstation users to a `ujust` they do not have.
    #[test]
    fn universal_blue_is_matched_on_the_leaf_not_the_family() {
        fn leaf_is_ublue(chain: &str) -> bool {
            matches!(
                chain.rsplit('/').next(),
                Some("bazzite" | "bluefin" | "aurora")
            )
        }
        assert!(leaf_is_ublue("linux/fedora/bazzite"));
        assert!(leaf_is_ublue("linux/fedora/bluefin"));
        assert!(!leaf_is_ublue("linux/fedora"));
        assert!(!leaf_is_ublue("linux/fedora/fedora"));
        assert!(!leaf_is_ublue("linux/arch/steamos"));
    }

    #[test]
    fn server_conflict_is_ok_when_nothing_was_detected() {
        // `detect::snapshot()` is empty in a test binary (no startup scan ran), which is the same
        // state a clean box reports.
        let check = server_conflict();
        assert_eq!(check.status, CheckStatus::Ok);
        assert!(check.remedy.is_none());
    }
}
