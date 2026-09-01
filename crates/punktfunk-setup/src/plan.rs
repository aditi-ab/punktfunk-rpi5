//! Stage three: `(Facts, Choices) → Plan`. Pure — no I/O, no spawns, no env reads.
//!
//! A `Step` is data: what to do, which phase it belongs to and the one-line why. `--dry-run`
//! is therefore "render the Plan" by construction, and uninstall and channel-switch are not
//! modes with their own I/O but Plans from a different `Action`.
//!
//! Four actions carry intent the plan cannot resolve without running something first — the apt
//! madison pins, the pacman availability split, linger, and the unit enable. They stay
//! `StepAction` variants rather than command strings so `exec` owns the trap and the plan
//! stays pure; `design/installer-v2.md` §4 explains each.

use serde::{Deserialize, Serialize};

use crate::choices::{Action, Choices};
use crate::facts::{Channel, Facts, Family, Firewall, DOCS};
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Ok,
    Warn,
}

/// The phases the progress checklist shows, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Uninstall,
    Switch,
    Install,
    Omarchy,
    Conflicts,
    Groups,
    Options,
    Firewall,
    Linger,
    Start,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepAction {
    /// A shell snippet, echoed as `+ cmd` then run.
    Run(String),
    /// One `KEY=VALUE` line replaced or appended in `host.env`.
    SetEnv { key: String, value: String },
    /// Text only — an `ok` or `!!` line with no command behind it.
    Note(Level, String),
    /// apt will not walk back to a lower candidate on its own, so the exact version has to be
    /// looked up with `apt-cache madison` *after* the repo is rewritten.
    AptSwitch { pkgs: Vec<String> },
    /// A package the target channel does not carry can neither be named nor kept, so the split
    /// into `-Rdd` and `-S` needs `pacman -Si` against the repo the previous step just added.
    PacmanSwitch { pkgs: Vec<String> },
    /// Run `cmd` only when `program` is on PATH. Dry-run renders it regardless: the Omarchy
    /// hand-off is planned before the install that ships the binary, and `--dry-run` reports
    /// what a real box would do rather than what this one can do yet.
    RunIfPresent {
        program: String,
        cmd: String,
        warn_if_missing: Option<String>,
    },
    /// Skipped with a warning where systemd is not PID 1 — a container has no logind and
    /// nothing would honour it.
    Linger,
    /// Re-probes the user manager, which the linger step above may have just created.
    StartUnits { units: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub action: StepAction,
    /// On success nothing after this runs. Only the Omarchy hand-off sets it: `punktfunk-omarchy
    /// setup` does the groups, firewall and autostart work itself, better than the generic path.
    pub ends_run: bool,
}

impl Step {
    pub fn run(cmd: impl Into<String>) -> Step {
        Step {
            action: StepAction::Run(cmd.into()),
            ends_run: false,
        }
    }

    pub fn note(level: Level, text: impl Into<String>) -> Step {
        Step {
            action: StepAction::Note(level, text.into()),
            ends_run: false,
        }
    }

    pub fn set_env(key: &str, value: impl Into<String>) -> Step {
        Step {
            action: StepAction::SetEnv {
                key: key.to_string(),
                value: value.into(),
            },
            ends_run: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPhase {
    pub kind: Phase,
    /// The `==>` heading. Dynamic — it names the channel, the missing packages, the switch.
    pub title: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub phases: Vec<PlanPhase>,
}

impl Plan {
    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.phases.iter().flat_map(|p| p.steps.iter())
    }

    /// Every command this plan would run, in order — what the trap tests assert against.
    pub fn commands(&self) -> Vec<String> {
        self.steps()
            .filter_map(|s| match &s.action {
                StepAction::Run(c) | StepAction::RunIfPresent { cmd: c, .. } => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    fn push(&mut self, kind: Phase, title: impl Into<String>, steps: Vec<Step>) {
        self.phases.push(PlanPhase {
            kind,
            title: title.into(),
            steps,
        });
    }
}

/// The whole engine: probe results in, ordered work out.
pub fn build(facts: &Facts, choices: &Choices) -> Plan {
    let mut plan = Plan::default();
    let backend = platform::backend(facts.family);

    if choices.action == Action::Uninstall {
        let mut steps = backend.uninstall(facts);
        // The per-family sweep catches a native client; a flatpak one is invisible to it.
        if facts.has_flatpak_client && facts.family != Family::Flatpak {
            steps.extend(platform::backend(Family::Flatpak).uninstall(facts));
        }
        plan.push(
            Phase::Uninstall,
            format!("Uninstalling the host ({DOCS}/uninstall)"),
            steps,
        );
        return plan;
    }

    match choices.switch_from {
        Some(from) => plan.push(
            Phase::Switch,
            format!(
                "Channel switch: {} → {} ({DOCS}/channels)",
                from.as_str(),
                choices.channel.as_str()
            ),
            backend.switch(facts, choices),
        ),
        None => install_phase(&mut plan, facts, choices, backend),
    }

    if facts.omarchy {
        plan.push(Phase::Omarchy, "Omarchy", omarchy_steps(facts, choices));
    }

    // A client-only install wires nothing: no groups, no gamestream, no linger, no firewall —
    // the client listens on nothing fixed.
    if !choices.components.host {
        return plan;
    }

    plan.push(
        Phase::Conflicts,
        "Checking for Sunshine / Apollo / Vibeshine",
        conflict_steps(facts, choices),
    );
    plan.push(
        Phase::Groups,
        "Controller access",
        group_steps(facts, choices),
    );
    plan.push(
        Phase::Options,
        "Options (host.env — everything here is off by default and reversible)",
        option_steps(facts, choices),
    );
    plan.push(Phase::Firewall, "Firewall", firewall_steps(facts, choices));

    // Linger is configuration, not starting, so --no-start still honours it. It also creates
    // the user manager on a seatless box, so it MUST land before the unit enable below.
    if choices.linger {
        plan.push(
            Phase::Linger,
            "Starting at boot with nobody logged in",
            vec![Step {
                action: StepAction::Linger,
                ends_run: false,
            }],
        );
    }
    if choices.start {
        plan.push(
            Phase::Start,
            "Starting the host and the web console",
            start_steps(facts),
        );
    }
    plan
}

fn install_phase(
    plan: &mut Plan,
    facts: &Facts,
    choices: &Choices,
    backend: &dyn platform::PkgBackend,
) {
    // Ask per binary, not "is the host there": host, console and plugin runner are three
    // packages, and a weak-deps-off box would never grow a console otherwise.
    if facts.fully_installed() && choices.components.host {
        let version = facts.host_version.clone().unwrap_or_default();
        let channel = facts
            .current_channel
            .map(|c| format!(", {} channel", c.as_str()))
            .unwrap_or_default();
        let mut steps = vec![];
        if facts.current_channel.is_none() {
            steps.push(Step::note(
                Level::Warn,
                format!(
                    "--channel {} had nothing to act on: no punktfunk package repo is configured here, so this install did not come from one (built from source?). Channels: {DOCS}/channels",
                    choices.channel.as_str()
                ),
            ));
        }
        plan.push(
            Phase::Install,
            format!(
                "host, web console and plugin runner are already installed ({version}{channel}) — skipping the install, continuing with setup"
            ),
            steps,
        );
        return;
    }
    let mut what = if choices.components.host {
        facts.missing.join(" ")
    } else {
        String::new()
    };
    let mut steps = vec![];
    // The family backend covers the host and, where the repo carries it, the client in the
    // same transaction — so it also runs for a client-only install on apt, dnf and pacman.
    let native_client = choices.components.client && facts.family.has_native_client();
    if choices.components.host || native_client {
        steps.extend(backend.install(facts, choices));
    }
    // Where the family has no `punktfunk-client`, the client arrives as a user-scope flatpak
    // instead of not at all — the same line the docs give for any other distro.
    if choices.components.client {
        if !what.is_empty() {
            what.push(' ');
        }
        what.push_str("client");
        if !facts.family.has_native_client() {
            steps.extend(platform::backend(Family::Flatpak).install(facts, choices));
        }
    }
    plan.push(
        Phase::Install,
        format!("Installing: {what} ({} channel)", choices.channel.as_str()),
        steps,
    );
}

/// Everything from here to the start phase is generic Linux wiring — a group, a wide-open
/// firewall, a user unit. Omarchy has a better local answer for each and one command that does
/// them all and knows how to reverse itself, so offer that instead of a weaker second version.
///
/// Every optional part is passed explicitly. `punktfunk-omarchy setup` used to ask for four of
/// them itself, which is how an Omarchy install grew a second round of questions after this one
/// had finished — and how `--yes` skipped the integration entirely, since that script's prompt
/// defaulted to no on a pipe.
fn omarchy_steps(_facts: &Facts, choices: &Choices) -> Vec<Step> {
    if !choices.omarchy_setup {
        return vec![Step::note(
            Level::Ok,
            format!("Run it later with: punktfunk-omarchy setup   ({DOCS}/omarchy)"),
        )];
    }
    let cmd = format!(
        "punktfunk-omarchy setup --groups={} --cert={} --toasts={} --idle-guard={} --theme={}",
        bit(choices.punktfunk_group),
        bit(choices.omarchy_cert),
        bit(choices.omarchy_toasts),
        bit(choices.omarchy_idle),
        bit(choices.omarchy_theme),
    );
    vec![Step {
        action: StepAction::RunIfPresent {
            program: "punktfunk-omarchy".into(),
            cmd,
            warn_if_missing: Some(format!(
                "punktfunk-omarchy is not on PATH — the host package should ship it; see {DOCS}/omarchy"
            )),
        },
        ends_run: true,
    }]
}

/// The hand-off takes 1/0, never a bare flag: a missing one must be a parse error there, not a
/// silent no.
fn bit(on: bool) -> &'static str {
    if on {
        "1"
    } else {
        "0"
    }
}

fn conflict_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    if !facts.sunshine_active {
        return vec![Step::note(
            Level::Ok,
            "No conflicting game-streaming host detected.",
        )];
    }
    let mut steps = vec![Step::note(
        Level::Warn,
        "another streaming host is active on this box — both want TCP 47990 (its web UI, punktfunk's management API)",
    )];
    if choices.move_mgmt_port {
        steps.push(Step::set_env(
            "PUNKTFUNK_MGMT_BIND",
            format!("0.0.0.0:{}", choices.mgmt_port),
        ));
        steps.push(Step::note(
            Level::Ok,
            format!("Clients learn the port from discovery; the console and plugins read it from mgmt-endpoint. Details: {DOCS}/switching-from-sunshine"),
        ));
    } else {
        steps.push(Step::note(
            Level::Warn,
            format!("Stop it before you start punktfunk (e.g. sudo systemctl disable --now sunshine) — {DOCS}/switching-from-sunshine"),
        ));
    }
    steps
}

fn group_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    let mut steps = vec![];
    if facts.in_input_group {
        steps.push(Step::note(Level::Ok, "already in the input group"));
    } else if facts.has_ujust {
        // Bazzite: the input group is recipe-managed, so usermod is the wrong tool.
        steps.push(Step::run("ujust add-user-to-input-group"));
    } else if !facts.has_input_group {
        steps.push(Step::note(
            Level::Warn,
            format!(
                "no 'input' group on this system — virtual gamepads need /dev/uinput access; see {}",
                facts.docs_page
            ),
        ));
    } else {
        steps.push(Step::run(r#"sudo usermod -aG input "$USER""#));
    }
    if choices.punktfunk_group {
        if facts.in_punktfunk_group {
            steps.push(Step::note(Level::Ok, "already in the punktfunk group"));
        } else {
            steps.push(Step::run(r#"sudo usermod -aG punktfunk "$USER""#));
        }
    }
    steps
}

fn option_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    let mut steps = vec![];
    if choices.gamestream {
        if facts.sunshine_active {
            steps.push(Step::note(
                Level::Warn,
                "with another GameStream host running, only one can bind the Moonlight ports — stop the other first or skip this",
            ));
        }
        steps.push(Step::set_env("PUNKTFUNK_GAMESTREAM", "1"));
    }
    if choices.clipboard {
        steps.push(Step::set_env("PUNKTFUNK_CLIPBOARD", "on"));
    }
    steps
}

/// Packages never open ports; they install firewalld services and ufw profiles by these names.
fn firewall_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    let moved = choices.move_mgmt_port.then_some(choices.mgmt_port);
    match facts.firewall {
        Firewall::Firewalld => {
            let mut svcs =
                String::from("--add-service=punktfunk-native --add-service=punktfunk-web");
            if choices.gamestream {
                svcs.push_str(" --add-service=punktfunk-gamestream");
            }
            if let Some(port) = moved {
                svcs.push_str(&format!(" --add-port={port}/tcp"));
            }
            vec![
                Step::run("sudo firewall-cmd --reload"),
                Step::run(format!("sudo firewall-cmd --permanent {svcs}")),
                Step::run("sudo firewall-cmd --reload"),
            ]
        }
        Firewall::Ufw => {
            let mut steps = vec![
                Step::run("sudo ufw allow punktfunk-native"),
                Step::run("sudo ufw allow punktfunk-web"),
            ];
            if choices.gamestream {
                steps.push(Step::run("sudo ufw allow punktfunk-gamestream"));
            }
            if let Some(port) = moved {
                steps.push(Step::run(format!("sudo ufw allow {port}/tcp")));
            }
            steps
        }
        Firewall::None => vec![Step::note(
            Level::Ok,
            format!(
                "no active firewall found — nothing to open ({DOCS}/ports if you add one later)"
            ),
        )],
    }
}

fn start_steps(facts: &Facts) -> Vec<Step> {
    let mut steps = vec![];
    let mut units = vec!["punktfunk-host".to_string()];
    if facts.web_unit_present {
        units.push("punktfunk-web".to_string());
    } else {
        steps.push(Step::note(
            Level::Warn,
            format!(
                "no punktfunk-web.service on this box — the console is not installed, so nothing will answer on 47992 ({})",
                facts.docs_page
            ),
        ));
    }
    // The plugin runner fills the game library; apt/dnf/sysext start it themselves, Arch does not.
    if facts.scripting_unit_disabled {
        units.push("punktfunk-scripting".to_string());
    }
    steps.push(Step {
        action: StepAction::StartUnits { units },
        ends_run: false,
    });
    steps
}

/// The packages a switch must land on the new channel: the family's own three, plus anything
/// else punktfunk already installed, or `punktfunk-gamescope` and `punktfunk-client` are
/// stranded on the channel the box just left.
pub fn switch_pkgs(base: &[&str], installed: &[String]) -> Vec<String> {
    let mut out: Vec<String> = base.iter().map(|s| (*s).to_string()).collect();
    for pkg in installed {
        if !out.contains(pkg) {
            out.push(pkg.clone());
        }
    }
    out
}

/// Both channels write the repo in one place per family, so `Channel` never leaks into two.
pub fn repo_channel_suffix(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "",
        Channel::Canary => "-canary",
    }
}
