//! Stage four: run a `Plan`. Echo, sudo shim, `--yes` rewrites, TTY stdin, per-step results.
//!
//! `--dry-run` walks the same code and returns before the spawn, so what dry-run prints is
//! what a real run executes — there is no second rendering path to drift.
//!
//! Four `StepAction`s resolve here rather than in `plan`, because each needs something the
//! previous step created: apt's madison pins want the rewritten repo, pacman's availability
//! split wants the `-Sy`, and the unit enable wants the user manager linger may have just made.
//! `design/installer-v2.md` §4 explains why each is a trap and not an optimisation.

use crate::choices::Choices;
use crate::facts::{Channel, Facts, DOCS};
use crate::plan::{Level, Plan, StepAction};
use crate::seam::{BasePaths, CommandRunner, Stdin};
use crate::ui::Reporter;

/// Under `--yes` a package manager must not stop for its own confirmation. Ported verbatim
/// from the sh installer's rewrite table; `-Syu` is tested before `-S` so it wins.
const NONINTERACTIVE: [(&str, &str); 11] = [
    ("flatpak install --user ", "flatpak install --user -y "),
    ("flatpak uninstall --user ", "flatpak uninstall --user -y "),
    ("sudo apt install ", "sudo apt install -y "),
    ("sudo dnf install ", "sudo dnf install -y "),
    ("sudo pacman -Syu ", "sudo pacman -Syu --noconfirm "),
    ("sudo pacman -S ", "sudo pacman -S --noconfirm "),
    ("sudo dnf distro-sync ", "sudo dnf distro-sync -y "),
    ("sudo apt purge ", "sudo apt purge -y "),
    ("sudo dnf remove ", "sudo dnf remove -y "),
    ("sudo pacman -Rns ", "sudo pacman -Rns --noconfirm "),
    ("sudo pacman -Rdd ", "sudo pacman -Rdd --noconfirm "),
];

pub fn noninteractive(cmd: &str) -> String {
    for (from, to) in NONINTERACTIVE {
        if let Some(rest) = cmd.strip_prefix(from) {
            return format!("{to}{rest}");
        }
    }
    cmd.to_string()
}

#[derive(Debug, Clone, Copy)]
pub struct Opts {
    pub dry: bool,
    pub yes: bool,
    /// A terminal the package manager's own prompt can reach.
    pub tty: bool,
}

/// What the run left behind, for the report to footnote.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub relogin: bool,
    pub started: bool,
    /// The Omarchy hand-off succeeded, so the generic wiring deliberately did not run.
    pub ended_early: bool,
}

/// A step failed. The message is the sh installer's, pointing at the per-distro page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed(pub String);

pub struct Executor<'a> {
    pub paths: &'a BasePaths,
    pub run: &'a dyn CommandRunner,
    pub ui: &'a dyn Reporter,
    pub opts: Opts,
}

impl Executor<'_> {
    pub fn execute(
        &self,
        plan: &Plan,
        facts: &Facts,
        choices: &Choices,
    ) -> Result<Outcome, Failed> {
        // A client-only run enables no services, so the verify pass must not go looking for a
        // host that was never installed here.
        let mut outcome = Outcome {
            started: choices.start && choices.components.host,
            ..Outcome::default()
        };
        for phase in &plan.phases {
            self.ui.say(&phase.title);
            for step in &phase.steps {
                let ran = self.step(&step.action, facts, choices, &mut outcome)?;
                // A hand-off that was skipped for a missing binary has done nothing, so the
                // generic wiring it would have replaced still has to run.
                if step.ends_run && ran && !self.opts.dry {
                    outcome.ended_early = true;
                    return Ok(outcome);
                }
            }
        }
        Ok(outcome)
    }

    /// `Ok(true)` when the step actually did something — a skipped conditional step is `false`.
    fn step(
        &self,
        action: &StepAction,
        facts: &Facts,
        choices: &Choices,
        outcome: &mut Outcome,
    ) -> Result<bool, Failed> {
        match action {
            StepAction::Run(cmd) => {
                // A group change only applies after a re-login, and the outro has to say so.
                if cmd.contains("usermod -aG") || cmd.contains("add-user-to-input-group") {
                    outcome.relogin = true;
                }
                self.shell(cmd, facts).map(|()| true)
            }
            StepAction::RunIfPresent {
                program,
                cmd,
                warn_if_missing,
            } => {
                if !self.opts.dry && !self.run.which(program) {
                    if let Some(text) = warn_if_missing {
                        self.ui.warn(text);
                    }
                    return Ok(false);
                }
                self.shell(cmd, facts).map(|()| true)
            }
            StepAction::Note(Level::Ok, text) => {
                self.ui.ok(text);
                Ok(false)
            }
            StepAction::Note(Level::Warn, text) => {
                self.ui.warn(text);
                Ok(false)
            }
            StepAction::SetEnv { key, value } => {
                self.set_env(key, value);
                Ok(true)
            }
            StepAction::AptSwitch { pkgs } => self.apt_switch(pkgs, facts, choices).map(|()| true),
            StepAction::PacmanSwitch { pkgs } => self.pacman_switch(pkgs, facts).map(|()| true),
            StepAction::Linger => self.linger(facts),
            StepAction::StartUnits { units } => self.start_units(units, outcome, facts),
        }
    }

    /// Echo the command, then run it — under `--yes` the non-interactive form is what both the
    /// echo and the spawn get, so the transcript is what actually ran.
    fn shell(&self, cmd: &str, facts: &Facts) -> Result<(), Failed> {
        let cmd = if self.opts.yes {
            noninteractive(cmd)
        } else {
            cmd.to_string()
        };
        self.ui.plus(&cmd);
        if self.opts.dry {
            return Ok(());
        }
        let stdin = if self.opts.tty {
            Stdin::Tty
        } else {
            Stdin::Null
        };
        self.run.run_shell(&cmd, stdin).map_err(|_| {
            Failed(format!(
                "that step failed — fix it and re-run (the script is safe to repeat), or follow the page by hand: {}",
                facts.docs_page
            ))
        })
    }

    /// Replace or append one `KEY=VALUE` line in host.env, creating it on first use.
    fn set_env(&self, key: &str, value: &str) {
        let path = self.paths.host_env();
        // These are Linux box paths, and the goldens must be byte-identical on every OS the
        // suite runs on — a Windows test host's PathBuf::join writes `\` into the transcript.
        let shown = path.display().to_string().replace('\\', "/");
        if self.opts.dry {
            self.ui.ok(&format!("would set {key}={value} in {shown}"));
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let prefix = format!("{key}=");
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        match lines.iter_mut().find(|l| l.starts_with(&prefix)) {
            Some(line) => *line = format!("{key}={value}"),
            None => lines.push(format!("{key}={value}")),
        }
        let mut body = lines.join("\n");
        body.push('\n');
        if std::fs::write(&path, body).is_ok() {
            self.ui.ok(&format!("{key}={value} → {shown}"));
        } else {
            self.ui.warn(&format!("could not write {shown}"));
        }
    }

    /// apt will not walk back to a lower candidate on its own, so name the exact version. After
    /// the repo rewrite the target channel is the only punktfunk source, so madison's first row
    /// is that channel's newest.
    fn apt_switch(&self, pkgs: &[String], facts: &Facts, choices: &Choices) -> Result<(), Failed> {
        let mut pins = String::new();
        for pkg in pkgs {
            if self.opts.dry {
                pins.push_str(&format!(" {pkg}=<version>"));
                continue;
            }
            // A package the target channel does not carry keeps what it has; naming it with no
            // version would drag it to the highest version from ANY source.
            if let Some(version) = self
                .run
                .first_line("apt-cache", &["madison", pkg])
                .and_then(|l| l.split_whitespace().nth(2).map(str::to_string))
            {
                pins.push_str(&format!(" {pkg}={version}"));
            }
        }
        if pins.is_empty() {
            return Err(Failed(format!(
                "the {} apt channel offers no punktfunk packages — check /etc/apt/sources.list.d/punktfunk.list ({DOCS}/channels)",
                choices.channel.as_str()
            )));
        }
        self.shell(&format!("sudo apt install --allow-downgrades{pins}"), facts)
    }

    /// A package the target channel does not carry can neither be named (the transaction aborts)
    /// nor kept (its files collide with the channel's own), so a switch lands the box on exactly
    /// what the target offers and removes what it does not, saying so.
    fn pacman_switch(&self, pkgs: &[String], facts: &Facts) -> Result<(), Failed> {
        let mut want = String::new();
        let mut drop = String::new();
        for pkg in pkgs {
            let available = self.opts.dry
                || self
                    .run
                    .probe("pacman", &["-Si", pkg])
                    .is_some_and(|o| o.ok());
            if available {
                want.push_str(&format!(" {pkg}"));
            } else {
                let channel = facts.current_channel.map_or("target", Channel::as_str);
                self.ui.warn(&format!(
                    "{pkg} is not on the {channel} channel — removing it (the {channel} packages carry its files)"
                ));
                drop.push_str(&format!(" {pkg}"));
            }
        }
        // -Rdd, not -R: the very next -S replaces whatever depended on the leaving package, and
        // a dependency check against the outgoing set would refuse the removal that makes room.
        if !drop.is_empty() {
            self.shell(&format!("sudo pacman -Rdd{drop}"), facts)?;
        }
        self.shell(&format!("sudo pacman -S{want}"), facts)
    }

    /// A container has no logind: enable-linger fails there and would mean nothing anyway.
    /// `--dry-run` still prints the command, because it reports what a real box would do.
    fn linger(&self, facts: &Facts) -> Result<bool, Failed> {
        if self.opts.dry || facts.systemd_pid1 {
            return self
                .shell(r#"sudo loginctl enable-linger "$USER""#, facts)
                .map(|()| true);
        }
        self.ui.warn(
            "no systemd as PID 1 here (a container?) — skipping linger, nothing would honour it",
        );
        Ok(false)
    }

    /// Probed here, not in `plan`: on a seatless box the linger step above is what created the
    /// user manager, so asking earlier prints the enable command and stops for nothing.
    fn start_units(
        &self,
        units: &[String],
        outcome: &mut Outcome,
        facts: &Facts,
    ) -> Result<bool, Failed> {
        let live = self
            .run
            .probe("systemctl", &["--user", "show-environment"])
            .is_some_and(|o| o.ok());
        if !live {
            self.ui.warn(
                "no user systemd session here (ssh without a login session?) — run this from a terminal in your desktop session:",
            );
            self.ui
                .detail("systemctl --user enable --now punktfunk-host punktfunk-web");
            outcome.started = false;
            return Ok(false);
        }
        if !self.opts.dry {
            let _ = self.run.probe("systemctl", &["--user", "daemon-reload"]);
        }
        self.shell(
            &format!("systemctl --user enable --now {}", units.join(" ")),
            facts,
        )
        .map(|()| true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_yes_rewrite_table_matches_the_sh_installer() {
        assert_eq!(
            noninteractive("sudo apt install punktfunk-host"),
            "sudo apt install -y punktfunk-host"
        );
        assert_eq!(
            noninteractive("sudo pacman -Syu punktfunk-host"),
            "sudo pacman -Syu --noconfirm punktfunk-host"
        );
        assert_eq!(
            noninteractive("sudo pacman -S punktfunk-host"),
            "sudo pacman -S --noconfirm punktfunk-host"
        );
        assert_eq!(
            noninteractive("sudo pacman -Rdd punktfunk-icons"),
            "sudo pacman -Rdd --noconfirm punktfunk-icons"
        );
    }

    // -Syu is a prefix of neither the -S rule's pattern nor the reverse, but only because the
    // -Syu rule is tested first. Swapping them would emit `-Syu --noconfirm --noconfirm`.
    #[test]
    fn syu_is_rewritten_once_not_twice() {
        let once = noninteractive("sudo pacman -Syu punktfunk-host");
        assert_eq!(once.matches("--noconfirm").count(), 1);
    }

    #[test]
    fn a_command_with_no_rule_is_left_alone() {
        assert_eq!(
            noninteractive("sudo ufw allow punktfunk-web"),
            "sudo ufw allow punktfunk-web"
        );
        assert_eq!(
            noninteractive("systemctl --user enable --now punktfunk-host"),
            "systemctl --user enable --now punktfunk-host"
        );
    }
}
