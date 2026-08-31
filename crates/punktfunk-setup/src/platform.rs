//! One package family per implementation, behind `PkgBackend`.
//!
//! This trait is the Windows-port deliverable (`design/installer-v2.md` D3): a winget backend
//! produces the same `Step`s and every UI renders them unchanged. Nothing above this module
//! knows what apt or pacman are.
//!
//! The install commands are **generated from the embedded `data/platforms.json`**, not copied
//! beside it, so the D6 promise ("the install lines are verbatim") holds by construction and
//! not by a gate. What can still drift is the *shape* — which entry is the repo block and
//! which is the install — so `shape` tests pin every family's split.

use std::sync::OnceLock;

use crate::choices::Choices;
use crate::facts::{Channel, Facts, Family};
use crate::plan::{switch_pkgs, Step, StepAction};
use crate::seam::{BasePaths, CommandRunner};

/// The single source for every install line (`design/installer-v2.md` D6).
const PLATFORMS_JSON: &str = include_str!("../../../data/platforms.json");

/// Drops whichever punktfunk section pacman.conf holds — the stable one, the canary one, or both.
const PACMAN_RM_REPO: &str =
    r"sudo sed -i '/^\[punktfunk\(-canary\)\{0,1\}\]$/,/^Server = /d' /etc/pacman.conf";

fn platforms() -> &'static serde_json::Value {
    static PARSED: OnceLock<serde_json::Value> = OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str(PLATFORMS_JSON).expect("data/platforms.json is not valid JSON")
    })
}

/// The `install` array platforms.json states for a platform id.
pub fn install_lines(id: &str) -> Vec<String> {
    platforms()["platforms"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"] == id)
        .and_then(|p| p["install"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_string))
        .collect()
}

/// Split a platform's lines at the first one starting with `marker`: everything before is the
/// repo block, the rest is the install.
fn split_at(id: &str, marker: &str) -> (Vec<String>, Vec<String>) {
    let lines = install_lines(id);
    let at = lines
        .iter()
        .position(|l| l.starts_with(marker))
        .unwrap_or_else(|| panic!("platforms.json '{id}' has no line starting with '{marker}'"));
    (lines[..at].to_vec(), lines[at..].to_vec())
}

pub trait PkgBackend {
    /// The packages this family installs for a host. Note dnf's is `punktfunk`, not
    /// `punktfunk-host`.
    fn base_pkgs(&self) -> Vec<&'static str>;
    /// The verb a client-only install hangs `punktfunk-client` off.
    fn install_verb(&self) -> &'static str;
    fn write_repo(&self, facts: &Facts, choices: &Choices) -> Vec<Step>;
    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step>;
    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step>;
    fn uninstall(&self, facts: &Facts) -> Vec<Step>;
    fn current_channel(&self, paths: &BasePaths, run: &dyn CommandRunner) -> Option<Channel>;
    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String>;
}

pub fn backend(family: Family) -> &'static dyn PkgBackend {
    match family {
        Family::Apt => &Apt,
        Family::Dnf => &Dnf,
        Family::Pacman => &Pacman,
        Family::Sysext => &Sysext,
    }
}

/// The install line for the chosen components: the platforms.json one verbatim for a host,
/// plus or replaced by the client package when `--client` asked for it.
fn compose_install(base_line: &str, verb: &str, choices: &Choices) -> String {
    match (choices.components.host, choices.components.client) {
        (true, false) => base_line.to_string(),
        (true, true) => format!("{base_line} punktfunk-client"),
        _ => format!("{verb} punktfunk-client"),
    }
}

// ------------------------------------------------------------------------------------- apt

struct Apt;

impl PkgBackend for Apt {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec!["punktfunk-host", "punktfunk-web", "punktfunk-scripting"]
    }

    fn install_verb(&self) -> &'static str {
        "sudo apt install"
    }

    fn write_repo(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (repo, _) = split_at("debian", "sudo apt install");
        repo.into_iter()
            .map(|line| {
                Step::run(if choices.channel == Channel::Canary {
                    line.replace(" stable main", " canary main")
                } else {
                    line
                })
            })
            .collect()
    }

    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at("debian", "sudo apt install");
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step::run(compose_install(
            &install[0],
            self.install_verb(),
            choices,
        )));
        steps
    }

    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step {
            action: StepAction::AptSwitch {
                pkgs: switch_pkgs(&self.base_pkgs(), &facts.installed_pf),
            },
            ends_run: false,
        });
        steps
    }

    fn uninstall(&self, facts: &Facts) -> Vec<Step> {
        let mut steps = vec![Step::run(UNIT_TEARDOWN)];
        if !facts.installed_pf.is_empty() {
            steps.push(Step::run(format!(
                "sudo apt purge {}",
                facts.installed_pf.join(" ")
            )));
        }
        steps.push(Step::run(
            "sudo rm -f /etc/apt/sources.list.d/punktfunk.list /etc/apt/keyrings/punktfunk.asc",
        ));
        steps.push(Step::run("sudo apt update"));
        steps
    }

    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let text = paths.read(&paths.etc("apt/sources.list.d/punktfunk.list"))?;
        Some(if text.contains(" canary main") {
            Channel::Canary
        } else {
            Channel::Stable
        })
    }

    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String> {
        run.probe(
            "dpkg-query",
            &["-W", "-f=${Package} ${db:Status-Status}\n", "punktfunk*"],
        )
        .map(|o| {
            o.stdout
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let name = it.next()?;
                    (it.next() == Some("installed")).then(|| name.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }
}

// ------------------------------------------------------------------------------------- dnf

struct Dnf;

impl PkgBackend for Dnf {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec!["punktfunk", "punktfunk-web", "punktfunk-scripting"]
    }

    fn install_verb(&self) -> &'static str {
        "sudo dnf install"
    }

    /// The repo block is one heredoc, so its lines rejoin into a single command; the group is
    /// then edited in, exactly as the sh installer does it.
    fn write_repo(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (repo, _) = split_at("fedora", "sudo dnf install");
        let mut steps = vec![Step::run(repo.join("\n"))];
        let group = match (facts.rpm_group.as_deref(), choices.channel) {
            (Some(g), Channel::Canary) => format!("{g}-canary"),
            (Some(g), Channel::Stable) => g.to_string(),
            (None, _) => return steps,
        };
        if group != "fedora-44" {
            steps.push(Step::run(format!(
                "sudo sed -i 's|/rpm/fedora-44|/rpm/{group}|' /etc/yum.repos.d/punktfunk.repo"
            )));
        }
        steps
    }

    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at("fedora", "sudo dnf install");
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step::run(compose_install(
            &install[0],
            self.install_verb(),
            choices,
        )));
        steps
    }

    /// `install` covers stable→canary and anything missing; `distro-sync` is what pulls the set
    /// back DOWN onto a lower stable version on the way home.
    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at("fedora", "sudo dnf install");
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step::run(install[0].clone()));
        steps.push(Step::run(format!(
            "sudo dnf distro-sync {}",
            switch_pkgs(&self.base_pkgs(), &facts.installed_pf).join(" ")
        )));
        steps
    }

    fn uninstall(&self, facts: &Facts) -> Vec<Step> {
        let mut steps = vec![Step::run(UNIT_TEARDOWN)];
        if !facts.installed_pf.is_empty() {
            steps.push(Step::run(format!(
                "sudo dnf remove {}",
                facts.installed_pf.join(" ")
            )));
        }
        steps.push(Step::run("sudo rm -f /etc/yum.repos.d/punktfunk.repo"));
        steps
    }

    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let text = paths.read(&paths.etc("yum.repos.d/punktfunk.repo"))?;
        let canary = text
            .lines()
            .any(|l| l.starts_with("baseurl=") && l.contains("-canary"));
        Some(if canary {
            Channel::Canary
        } else {
            Channel::Stable
        })
    }

    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String> {
        run.probe("rpm", &["-qa", "--qf", "%{NAME} ", "punktfunk*"])
            .map(|o| o.stdout.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------- pacman

struct Pacman;

impl Pacman {
    /// Omarchy is the same repo and the same packages — only the transaction shape differs, so
    /// it reads its own platforms.json entry rather than being a special case in the commands.
    fn entry(facts: &Facts) -> &'static str {
        if facts.omarchy {
            "omarchy"
        } else {
            "arch"
        }
    }
}

impl PkgBackend for Pacman {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec!["punktfunk-host", "punktfunk-web", "punktfunk-scripting"]
    }

    fn install_verb(&self) -> &'static str {
        "sudo pacman -S"
    }

    fn write_repo(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (repo, _) = split_at(Self::entry(facts), "sudo pacman -S");
        repo.into_iter()
            .map(|line| {
                Step::run(if choices.channel == Channel::Canary {
                    // Both the grep guard (escaped brackets) and the printf body name the repo.
                    line.replace(r"punktfunk\]", r"punktfunk-canary\]")
                        .replace("[punktfunk]", "[punktfunk-canary]")
                } else {
                    line
                })
            })
            .collect()
    }

    /// On Omarchy `-Sy` then `-S`: its libalpm hook aborts any transaction carrying both `-S`
    /// and `-u`, so Arch's `-Syu` installs nothing there. The trailing hand-off line is the
    /// Omarchy phase's, not this one's.
    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at(Self::entry(facts), "sudo pacman -S");
        let mut steps = self.write_repo(facts, choices);
        let pkg_lines: Vec<&String> = install
            .iter()
            .filter(|l| l.starts_with("sudo pacman"))
            .collect();
        let (last, rest) = pkg_lines
            .split_last()
            .expect("pacman entry has an install line");
        steps.extend(rest.iter().map(|l| Step::run(l.to_string())));
        steps.push(Step::run(compose_install(
            last,
            self.install_verb(),
            choices,
        )));
        steps
    }

    /// Drop the old section first or both repos end up enabled, then `-Sy` and `-S` — never
    /// `-Syu`, which sees a lower version on the way home and does nothing at all.
    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let mut steps = vec![Step::run(PACMAN_RM_REPO)];
        steps.extend(self.write_repo(facts, choices));
        steps.push(Step::run("sudo pacman -Sy"));
        steps.push(Step {
            action: StepAction::PacmanSwitch {
                pkgs: switch_pkgs(&self.base_pkgs(), &facts.installed_pf),
            },
            ends_run: false,
        });
        steps
    }

    /// `punktfunk-omarchy setup` put wiring OUTSIDE the packages, and its `remove` ships IN the
    /// host package — so it has to run before pacman takes the binary away.
    fn uninstall(&self, facts: &Facts) -> Vec<Step> {
        let mut steps = vec![Step::run(UNIT_TEARDOWN)];
        if facts.omarchy {
            // Idempotent, and silent when setup never ran — so it is skipped only when the
            // binary is genuinely gone, not planned away.
            steps.push(Step {
                action: StepAction::RunIfPresent {
                    program: "punktfunk-omarchy".into(),
                    cmd: "punktfunk-omarchy remove".into(),
                    warn_if_missing: None,
                },
                ends_run: false,
            });
        }
        if !facts.installed_pf.is_empty() {
            steps.push(Step::run(format!(
                "sudo pacman -Rns {}",
                facts.installed_pf.join(" ")
            )));
        }
        steps.push(Step::run(PACMAN_RM_REPO));
        steps
    }

    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let text = paths.read(&paths.etc("pacman.conf"))?;
        if text.lines().any(|l| l.trim_end() == "[punktfunk-canary]") {
            Some(Channel::Canary)
        } else if text.lines().any(|l| l.trim_end() == "[punktfunk]") {
            Some(Channel::Stable)
        } else {
            None
        }
    }

    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String> {
        run.probe("pacman", &["-Qq"])
            .map(|o| {
                o.stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with("punktfunk"))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------------- sysext

struct Sysext;

impl PkgBackend for Sysext {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec![]
    }

    fn install_verb(&self) -> &'static str {
        "sudo bash punktfunk-sysext.sh install"
    }

    /// No repo file — punktfunk-sysext records the channel itself, in its own conf.
    fn write_repo(&self, _facts: &Facts, _choices: &Choices) -> Vec<Step> {
        vec![]
    }

    fn install(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let lines = install_lines("bazzite");
        let (install, fetch) = lines
            .split_last()
            .expect("bazzite entry has an install line");
        let mut steps: Vec<Step> = fetch.iter().map(Step::run).collect();
        steps.push(Step::run(if choices.channel == Channel::Canary {
            format!("{install} --channel canary")
        } else {
            install.clone()
        }));
        steps
    }

    /// The sysext script keeps its own per-feed rollback floor, so it moves both ways.
    fn switch(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let lines = install_lines("bazzite");
        let (install, fetch) = lines
            .split_last()
            .expect("bazzite entry has an install line");
        let mut steps: Vec<Step> = fetch.iter().map(Step::run).collect();
        steps.push(Step::run(format!(
            "{install} --channel {}",
            choices.channel.as_str()
        )));
        steps
    }

    fn uninstall(&self, _facts: &Facts) -> Vec<Step> {
        vec![
            Step::run(UNIT_TEARDOWN),
            Step::run("sudo punktfunk-sysext remove"),
        ]
    }

    /// `punktfunk-sysext` writes its conf only when `--channel` was passed, so "absent" cannot
    /// tell an untouched box from a stable one. The installed binary breaks the tie.
    fn current_channel(&self, paths: &BasePaths, run: &dyn CommandRunner) -> Option<Channel> {
        if !run.which("punktfunk-host") {
            return None;
        }
        let text = paths
            .read(&paths.etc("punktfunk-sysext.conf"))
            .unwrap_or_default();
        let found = text.lines().find_map(|l| l.strip_prefix("CHANNEL="));
        Some(match found.map(str::trim) {
            Some("canary") => Channel::Canary,
            _ => Channel::Stable,
        })
    }

    fn installed_pf(&self, _run: &dyn CommandRunner) -> Vec<String> {
        vec![]
    }
}

/// User units go off first: package removal cannot see the enable symlinks in `$HOME`.
const UNIT_TEARDOWN: &str =
    "systemctl --user disable --now punktfunk-host punktfunk-web punktfunk-scripting 2>/dev/null || true";

#[cfg(test)]
mod tests {
    use super::*;

    // A bad platforms.json edit fails the build here rather than at install time on a box.
    #[test]
    fn every_host_platform_parses_and_carries_install_lines() {
        for id in ["debian", "arch", "omarchy", "fedora", "bazzite"] {
            assert!(!install_lines(id).is_empty(), "{id} has no install lines");
        }
    }

    // The split is the one assumption generating commands from platforms.json makes. If a line
    // is inserted or reordered upstream, these fail instead of the box getting a wrong command.
    #[test]
    fn shape_of_every_family_split() {
        let (repo, install) = split_at("debian", "sudo apt install");
        assert_eq!(repo.len(), 4, "keyring dir, key, sources line, update");
        assert_eq!(
            install,
            ["sudo apt install punktfunk-host punktfunk-web punktfunk-scripting"]
        );

        let (repo, install) = split_at("arch", "sudo pacman -S");
        assert_eq!(repo.len(), 3, "key add, lsign, pacman.conf section");
        assert_eq!(
            install,
            ["sudo pacman -Syu punktfunk-host punktfunk-web punktfunk-scripting"]
        );

        let (repo, install) = split_at("omarchy", "sudo pacman -S");
        assert_eq!(repo.len(), 3);
        assert_eq!(install[0], "sudo pacman -Sy");
        assert!(install[1].starts_with("sudo pacman -S punktfunk-host"));
        assert_eq!(
            install[2], "punktfunk-omarchy setup",
            "the hand-off is its own phase"
        );

        let (repo, install) = split_at("fedora", "sudo dnf install");
        assert_eq!(
            repo.len(),
            11,
            "the whole heredoc, rejoined into one command"
        );
        assert_eq!(
            repo[0],
            "sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'"
        );
        assert_eq!(repo[10], "REPO");
        assert_eq!(
            install,
            ["sudo dnf install punktfunk punktfunk-web punktfunk-scripting"]
        );

        assert_eq!(
            install_lines("bazzite").len(),
            2,
            "fetch the script, run it"
        );
    }
}
