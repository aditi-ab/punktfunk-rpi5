//! The Windows option set: today's Inno task checkboxes as engine choices (WP1.2).
//!
//! Defaults derive from `WinFacts` (`design/installer-v2-windows.md` §3): fresh installs get
//! the `.iss` defaults; upgrades pre-fill from what is observably on the box instead of
//! Inno's remembered-checkbox registry. The two settings the service persists itself
//! (GameStream compat, the public-firewall opt-in) are `Option<bool>`: `None` means the plan
//! passes nothing and the box keeps its state — the `.iss` passed those flags on fresh
//! installs only, and a default must never rewrite an upgrade. An explicit task flag or the
//! D12 network answer sets `Some`, which deliberately reaches the plan even on upgrades:
//! that is what makes Reconfigure able to fix a fielded box.

use std::path::PathBuf;

use crate::seam::Env;

use super::args::{InnoArgs, TaskFlag};
use super::{NetCategory, WinFacts};

/// The D12 answer. `Skip` is the silent default — a profile change needs a consent surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkAnswer {
    /// Set the named network's NLA category to Private (the recommended fix).
    MakePrivate(String),
    /// Keep it Public and add the public-profile firewall rules.
    OpenPublicRules,
    Skip,
}

/// Environment twins for the Windows rows, same `1`/`0` reading as the Linux set.
const ENV_TWINS: [(&str, Task); 8] = [
    ("PUNKTFUNK_INSTALL_DRIVER", Task::Driver),
    ("PUNKTFUNK_INSTALL_GAMEPAD", Task::Gamepad),
    ("PUNKTFUNK_INSTALL_HDR_LAYER", Task::HdrLayer),
    ("PUNKTFUNK_INSTALL_GAMESTREAM", Task::Gamestream),
    ("PUNKTFUNK_INSTALL_PUBLIC_FIREWALL", Task::AllowPublicFw),
    ("PUNKTFUNK_INSTALL_START_SERVICE", Task::StartService),
    ("PUNKTFUNK_INSTALL_TRAY", Task::TrayIcon),
    ("PUNKTFUNK_INSTALL_DESKTOP_ICON", Task::DesktopIcon),
];

/// The task vocabulary — the `/MERGETASKS` names are a published contract (winget manifest,
/// troubleshooting docs), so the strings here can never change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Driver,
    Gamepad,
    HdrLayer,
    Gamestream,
    AllowPublicFw,
    StartService,
    TrayIcon,
    DesktopIcon,
}

impl Task {
    fn from_name(name: &str) -> Option<Task> {
        Some(match name {
            "installdriver" => Task::Driver,
            "installgamepad" => Task::Gamepad,
            "installhdrlayer" => Task::HdrLayer,
            "gamestream" => Task::Gamestream,
            "allowpublicfw" => Task::AllowPublicFw,
            "startservice" => Task::StartService,
            "trayicon" => Task::TrayIcon,
            "desktopicon" => Task::DesktopIcon,
            _ => return None,
        })
    }
}

/// Everything the Windows plan lets the user decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinChoices {
    pub install_driver: bool,
    pub install_gamepad: bool,
    pub install_hdr_layer: bool,
    /// `None` = pass nothing, the box keeps its persisted state.
    pub gamestream: Option<bool>,
    /// `None` = pass nothing; the opt-in marker file persists the previous choice.
    pub allow_public_fw: Option<bool>,
    pub start_service: bool,
    pub tray_autostart: bool,
    /// Client artifact only; the host creates no shortcuts.
    pub desktop_icon: bool,
    /// Fresh installs only. `None` = the executor generates one (24 hex chars, real RNG);
    /// the wizard's password row edits it. Never rendered into a transcript or argv.
    pub web_password: Option<String>,
    /// Where the files go. Upgrades pre-fill the ARP location and ignore `/DIR`, as Inno's
    /// `UsePreviousAppDir` did.
    pub dir: Option<PathBuf>,
    pub network: NetworkAnswer,
}

impl WinChoices {
    pub fn derive(facts: &WinFacts) -> WinChoices {
        let upgrade = facts.installed.is_some();
        WinChoices {
            install_driver: true,
            install_gamepad: true,
            install_hdr_layer: if upgrade {
                facts.vulkan_layer_registered
            } else {
                true
            },
            gamestream: if upgrade { None } else { Some(false) },
            allow_public_fw: if upgrade { None } else { Some(false) },
            start_service: true,
            tray_autostart: if upgrade { facts.tray_autostart } else { true },
            desktop_icon: false,
            web_password: None,
            dir: facts
                .installed
                .as_ref()
                .and_then(|i| i.location.clone())
                .map(PathBuf::from),
            network: NetworkAnswer::Skip,
        }
    }

    /// D12's trigger: a Public network, the host leg, and no public rules already chosen.
    pub fn needs_network_step(&self, facts: &WinFacts) -> bool {
        self.allow_public_fw != Some(true)
            && self.network == NetworkAnswer::Skip
            && facts
                .networks
                .iter()
                .any(|n| n.category == NetCategory::Public)
    }

    /// Apply the Inno dialect over the derived defaults. Returns the warning lines the
    /// caller renders — unknown task names, an ignored `/DIR` — never an error (D5).
    pub fn apply(&mut self, args: &InnoArgs, env: &Env) -> Vec<String> {
        let mut warnings = Vec::new();
        for (key, task) in ENV_TWINS {
            if let Some(v) = env.get(key) {
                self.set(task, v == "1");
            }
        }
        // /TASKS replaces the defaults (Inno's semantics): everything off, then the list.
        if let Some(tasks) = &args.tasks {
            for task in [
                Task::Driver,
                Task::Gamepad,
                Task::HdrLayer,
                Task::Gamestream,
                Task::AllowPublicFw,
                Task::StartService,
                Task::TrayIcon,
                Task::DesktopIcon,
            ] {
                self.set(task, false);
            }
            self.apply_flags(tasks, &mut warnings);
        }
        self.apply_flags(&args.merge_tasks, &mut warnings);
        if let Some(dir) = &args.dir {
            match &self.dir {
                Some(existing) => warnings.push(format!(
                    "/DIR ignored — an existing install stays in {}",
                    existing.display()
                )),
                None => self.dir = Some(dir.clone()),
            }
        }
        warnings
    }

    fn apply_flags(&mut self, flags: &[TaskFlag], warnings: &mut Vec<String>) {
        for flag in flags {
            match Task::from_name(&flag.name) {
                Some(task) => self.set(task, flag.selected),
                None => warnings.push(format!("unknown task '{}' ignored", flag.name)),
            }
        }
    }

    fn set(&mut self, task: Task, selected: bool) {
        match task {
            Task::Driver => self.install_driver = selected,
            Task::Gamepad => self.install_gamepad = selected,
            Task::HdrLayer => self.install_hdr_layer = selected,
            Task::Gamestream => self.gamestream = Some(selected),
            Task::AllowPublicFw => self.allow_public_fw = Some(selected),
            Task::StartService => self.start_service = selected,
            Task::TrayIcon => self.tray_autostart = selected,
            Task::DesktopIcon => self.desktop_icon = selected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{NetProfile, WinInstall};
    use super::*;

    fn fresh_facts() -> WinFacts {
        WinFacts {
            os_build: 26200,
            arch: "x64".into(),
            installed: None,
            host_env_present: false,
            web_password_present: false,
            mgmt_bind_set: false,
            competing_hosts: vec![],
            mgmt_port_in_use: false,
            networks: vec![],
            steam_audio_drivers: true,
            tray_autostart: false,
            vulkan_layer_registered: false,
            web_task: super::super::TaskState::Absent,
            scripting_task: super::super::TaskState::Absent,
            inno_uninstaller: false,
        }
    }

    fn upgrade_facts() -> WinFacts {
        WinFacts {
            installed: Some(WinInstall {
                version: Some("0.34.0".into()),
                location: Some(r"C:\Program Files\punktfunk\".into()),
            }),
            host_env_present: true,
            tray_autostart: false,
            vulkan_layer_registered: true,
            ..fresh_facts()
        }
    }

    // The .iss defaults, literally: all install tasks on, both opt-ins off.
    #[test]
    fn fresh_defaults_match_the_iss_task_table() {
        let c = WinChoices::derive(&fresh_facts());
        assert!(c.install_driver && c.install_gamepad && c.install_hdr_layer);
        assert_eq!(c.gamestream, Some(false));
        assert_eq!(c.allow_public_fw, Some(false));
        assert!(c.start_service && c.tray_autostart);
        assert!(!c.desktop_icon);
        assert!(c.dir.is_none());
    }

    // Defaults must never rewrite an upgrade: the persisted settings pass nothing, and the
    // observable ones pre-fill from the box (the user who removed the tray keeps no tray).
    #[test]
    fn upgrade_defaults_leave_box_state_alone() {
        let c = WinChoices::derive(&upgrade_facts());
        assert_eq!(c.gamestream, None);
        assert_eq!(c.allow_public_fw, None);
        assert!(!c.tray_autostart);
        assert!(c.install_hdr_layer);
        assert_eq!(
            c.dir.as_ref().unwrap().to_str().unwrap(),
            r"C:\Program Files\punktfunk\"
        );
    }

    // The D12 Reconfigure story: an explicit ask reaches the plan even on an upgrade.
    #[test]
    fn an_explicit_task_overrides_even_on_upgrade() {
        let mut c = WinChoices::derive(&upgrade_facts());
        let args = InnoArgs::parse(&[r#"/MERGETASKS="allowpublicfw""#.to_string()]);
        let warnings = c.apply(&args, &Env::default());
        assert!(warnings.is_empty());
        assert_eq!(c.allow_public_fw, Some(true));
        assert_eq!(c.gamestream, None);
    }

    #[test]
    fn tasks_replaces_and_mergetasks_merges_over_defaults() {
        let mut c = WinChoices::derive(&fresh_facts());
        let replace = InnoArgs::parse(&["/TASKS=installdriver".to_string()]);
        c.apply(&replace, &Env::default());
        assert!(c.install_driver);
        assert!(!c.install_gamepad && !c.start_service && !c.tray_autostart);

        let mut c = WinChoices::derive(&fresh_facts());
        let merge = InnoArgs::parse(&[r#"/MERGETASKS="!trayicon""#.to_string()]);
        c.apply(&merge, &Env::default());
        assert!(!c.tray_autostart);
        assert!(c.install_driver && c.start_service);
    }

    #[test]
    fn an_unknown_task_warns_and_changes_nothing() {
        let mut c = WinChoices::derive(&fresh_facts());
        let args = InnoArgs::parse(&["/MERGETASKS=frobnicate".to_string()]);
        let warnings = c.apply(&args, &Env::default());
        assert_eq!(warnings, ["unknown task 'frobnicate' ignored"]);
        assert_eq!(c, WinChoices::derive(&fresh_facts()));
    }

    // Inno's UsePreviousAppDir, ported: an upgrade stays where it is, and says so.
    #[test]
    fn dir_is_honoured_fresh_and_ignored_with_a_warning_on_upgrade() {
        let mut c = WinChoices::derive(&fresh_facts());
        let args = InnoArgs::parse(&[r"/DIR=D:\pf".to_string()]);
        assert!(c.apply(&args, &Env::default()).is_empty());
        assert_eq!(c.dir.as_ref().unwrap().to_str().unwrap(), r"D:\pf");

        let mut c = WinChoices::derive(&upgrade_facts());
        let warnings = c.apply(&args, &Env::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("/DIR ignored"));
        assert_eq!(
            c.dir.as_ref().unwrap().to_str().unwrap(),
            r"C:\Program Files\punktfunk\"
        );
    }

    #[test]
    fn env_twins_read_one_and_zero_and_flags_overwrite_them() {
        let mut c = WinChoices::derive(&fresh_facts());
        let env = Env::of(&[("PUNKTFUNK_INSTALL_TRAY", "0")]);
        c.apply(&InnoArgs::parse(&[]), &env);
        assert!(!c.tray_autostart);

        // A task flag wins over the twin, matching the sh installer's env-then-args order.
        let mut c = WinChoices::derive(&fresh_facts());
        let args = InnoArgs::parse(&[r#"/MERGETASKS="trayicon""#.to_string()]);
        c.apply(&args, &env);
        assert!(c.tray_autostart);
    }

    #[test]
    fn the_network_step_triggers_on_public_without_opted_rules() {
        let mut facts = fresh_facts();
        facts.networks = vec![NetProfile {
            name: "Cafe".into(),
            category: NetCategory::Public,
        }];
        let mut c = WinChoices::derive(&facts);
        assert!(c.needs_network_step(&facts));
        c.allow_public_fw = Some(true);
        assert!(!c.needs_network_step(&facts));
        c.allow_public_fw = Some(false);
        c.network = NetworkAnswer::MakePrivate("Cafe".into());
        assert!(!c.needs_network_step(&facts));
    }
}
