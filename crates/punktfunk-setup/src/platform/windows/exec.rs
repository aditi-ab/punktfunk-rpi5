//! Stage four, Windows. WP1.3 ships the dry-run rendering — the transcript the goldens pin —
//! and WP1.4 lands the real legs behind the same walk, so what dry-run prints stays what a
//! real run executes, exactly like the Linux executor's single-path rule.
//!
//! Rendering vocabulary matches `Plain`: `==>` phase titles via `say`, `+ argv` echoes via
//! `plus`, `ok`/`!!` notes. Placeholders (`<staging>`, `<temp>`, `<version>`) render verbatim
//! in a dry run; the executor substitutes them at run time.

use super::plan::{join_argv, WinAction, WinPlan};
use crate::plan::Level;
use crate::ui::Reporter;

/// Render the plan as `--dry-run` output. Nothing here touches the machine.
pub fn render(plan: &WinPlan, ui: &dyn Reporter) {
    for phase in &plan.phases {
        ui.say(&phase.title);
        for step in &phase.steps {
            render_step(step, ui);
        }
    }
}

fn render_step(action: &WinAction, ui: &dyn Reporter) {
    match action {
        WinAction::Run(argv) => ui.plus(&join_argv(argv)),
        WinAction::Note(Level::Ok, text) => ui.ok(text),
        WinAction::Note(Level::Warn, text) => ui.warn(text),
        WinAction::SetEnv { key, value } => ui.ok(&format!(
            r"would set {key}={value} in %ProgramData%\punktfunk\host.env"
        )),
        WinAction::DeployFiles { dest } => {
            ui.ok(&format!("would unpack the payload into {dest}"));
        }
        WinAction::RemoveFiles { dir } => ui.ok(&format!("would remove {dir}")),
        WinAction::PathAdd { machine, dir } => {
            ui.ok(&format!("would add {dir} to the {} PATH", scope(*machine)));
        }
        WinAction::PathRemove { machine, dir } => ui.ok(&format!(
            "would remove {dir} from the {} PATH (entry-by-entry, never a substring delete)",
            scope(*machine)
        )),
        WinAction::ArpRegister {
            display_name, key, ..
        } => ui.ok(&format!(
            "would register '{display_name}' in Add/Remove Programs ({key})"
        )),
        WinAction::ArpRemove { key } => {
            ui.ok(&format!("would remove the Add/Remove Programs entry ({key})"));
        }
        WinAction::Shortcut { link, target } => {
            ui.ok(&format!("would create {link} → {target}"));
        }
        WinAction::MakeNetworkPrivate { network } => {
            ui.ok(&format!("would set network '{network}' to Private"));
        }
        WinAction::StopHostRuntime => {
            ui.ok("would stop the service, every tray, and the console/plugin tasks");
        }
        WinAction::RestoreTasks { .. } => {
            ui.ok("would re-enable only the tasks that were enabled before the stop");
        }
        WinAction::WebSetup {
            app_dir,
            fresh_password,
        } => {
            let password = if *fresh_password {
                r#" --password-file "<temp>\webpw.txt""#
            } else {
                ""
            };
            ui.plus(&format!(
                r#""{app_dir}\punktfunk-host.exe" web setup --app-dir "{app_dir}"{password}"#
            ));
        }
        WinAction::RegisterScriptingTask { start_now } => {
            ui.plus("schtasks /Create /TN PunktfunkScripting /XML <generated> /F");
            if *start_now {
                ui.plus("schtasks /Run /TN PunktfunkScripting");
            }
        }
        WinAction::LaunchTray { exe } => {
            ui.ok(&format!("would start the tray ({exe}) — skipped in silent installs"));
        }
        WinAction::EnsureAppRuntime { arch } => ui.ok(&format!(
            "would ensure the Windows App Runtime ({arch}; downloaded when missing — a failure warns and never aborts)"
        )),
        WinAction::KillPortListeners { ports } => {
            let list = ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            ui.ok(&format!("would stop anything still listening on {list}"));
        }
    }
}

fn scope(machine: bool) -> &'static str {
    if machine {
        "machine"
    } else {
        "user"
    }
}
