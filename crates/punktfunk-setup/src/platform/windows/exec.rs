//! Stage four, Windows: run a `WinPlan` (WP1.4). One walk — echo always, mutate only when
//! not dry — so what dry-run prints is what a real run executes, the Linux executor's rule.
//!
//! Two kinds of legs. Everything expressible as a spawn goes through `CommandRunner` and is
//! FakeRunner-testable on any OS: `reg.exe` for the surgical PATH and the ARP entry, netstat
//! for the port sweep (PID column + port match — the localized STATE word is never parsed),
//! `schtasks` for tasks and the de-elevated tray launch. Only what no process can do — SCM
//! stop/wait, `.lnk` writing, the env-change broadcast, the Appx presence check — lives in
//! `sys.rs` behind `cfg(windows)` with honest error stubs elsewhere.
//!
//! Placeholders (`<staging>`, `<temp>`, `<version>`, and the client's `%LocalAppData%`,
//! `<start menu>`, `<desktop>`) render verbatim in a dry run and are substituted from
//! `Subst` on a real one — except in the PATH edit, where `%LocalAppData%` stays literal on
//! purpose (`REG_EXPAND_SZ` expands it per user, the way the `.iss` wrote it).

use std::path::Path;

use super::plan::{join_argv, WinAction, WinPlan};
use super::sys;
use super::NetProbe;
use crate::exec::Failed;
use crate::plan::Level;
use crate::seam::{BasePaths, CommandRunner};
use crate::ui::Reporter;

/// What a real run substitutes for the plan's placeholders.
#[derive(Debug, Clone, Default)]
pub struct Subst {
    pub version: String,
    /// The admin-only extraction dir the driver payloads were staged into.
    pub staging: String,
    /// A scratch dir with the same ACLs (the password file, generated task XML).
    pub temp: String,
    /// The per-user roots the client plan names symbolically (M4).
    pub local_app_data: String,
    pub start_menu: String,
    pub desktop: String,
}

/// Where the artifact's files come from. The real overlay reader lands with WP3.1; tests and
/// `--demo` inject one that deploys nothing.
pub trait PayloadSource {
    fn deploy(&self, dest: &Path) -> Result<(), String>;
}

/// Records the destinations and touches nothing.
#[derive(Debug, Default)]
pub struct FakePayload {
    pub deployed: std::cell::RefCell<Vec<String>>,
}

impl PayloadSource for FakePayload {
    fn deploy(&self, dest: &Path) -> Result<(), String> {
        self.deployed.borrow_mut().push(dest.display().to_string());
        Ok(())
    }
}

pub struct WinExecutor<'a> {
    pub run: &'a dyn CommandRunner,
    pub net: &'a dyn NetProbe,
    pub payload: &'a dyn PayloadSource,
    pub paths: &'a BasePaths,
    pub ui: &'a dyn Reporter,
    pub dry: bool,
    /// Inno-silent run: never a window, and the tray launch is skipped (the `.iss` rule —
    /// the new host's supervision puts one back).
    pub silent: bool,
    /// The web password when the wizard edited it; `None` = generate at the step.
    pub web_password: Option<String>,
    pub subst: Subst,
}

/// The goldens' entry: render as `--dry-run` with seams that cannot touch anything.
pub fn render(plan: &WinPlan, ui: &dyn Reporter) {
    let run = crate::seam::FakeRunner::new();
    let net = super::FakeNet::default();
    let payload = FakePayload::default();
    let paths = BasePaths::rooted(Path::new("/nowhere"));
    WinExecutor {
        run: &run,
        net: &net,
        payload: &payload,
        paths: &paths,
        ui,
        dry: true,
        silent: false,
        web_password: None,
        subst: Subst::default(),
    }
    .execute(plan)
    .expect("a dry run cannot fail");
}

impl WinExecutor<'_> {
    pub fn execute(&self, plan: &WinPlan) -> Result<(), Failed> {
        for phase in &plan.phases {
            self.ui.say(&phase.title);
            for step in &phase.steps {
                self.step(step)?;
            }
        }
        Ok(())
    }

    fn sub(&self, s: &str) -> String {
        if self.dry {
            return s.to_string();
        }
        s.replace("<staging>", &self.subst.staging)
            .replace("<temp>", &self.subst.temp)
            .replace("<version>", &self.subst.version)
            .replace("<start menu>", &self.subst.start_menu)
            .replace("<desktop>", &self.subst.desktop)
            .replace("%LocalAppData%", &self.subst.local_app_data)
    }

    /// Echo the (substituted) argv, then spawn it. `lenient` tolerates a non-zero exit and a
    /// binary that is not even there — absence is the goal state for those steps (quitting a
    /// tray that was already removed must not fail the uninstall).
    fn spawn(&self, argv: &[String], lenient: bool) -> Result<(), Failed> {
        let argv: Vec<String> = argv.iter().map(|a| self.sub(a)).collect();
        self.ui.plus(&join_argv(&argv));
        if self.dry {
            return Ok(());
        }
        let args: Vec<&str> = argv[1..].iter().map(String::as_str).collect();
        match self.run.probe(&argv[0], &args) {
            Some(out) if out.ok() || lenient => Ok(()),
            Some(out) => Err(Failed(format!(
                "'{}' exited {} — {}",
                argv[0],
                out.code,
                out.stderr.lines().last().unwrap_or("(no error output)")
            ))),
            None if lenient => {
                self.ui
                    .detail(&format!("{} — not there, nothing to do", argv[0]));
                Ok(())
            }
            None => Err(Failed(format!("'{}' did not start", argv[0]))),
        }
    }

    /// A spawn whose echo the step's own line already covered — one logical step, several
    /// helper spawns, and echoing all of them would drown the transcript.
    fn spawn_quiet(&self, argv: &[&str], lenient: bool) -> Result<(), Failed> {
        let owned: Vec<String> = argv.iter().map(|a| self.sub(a)).collect();
        if self.dry {
            return Ok(());
        }
        let args: Vec<&str> = owned[1..].iter().map(String::as_str).collect();
        match self.run.probe(&owned[0], &args) {
            Some(out) if out.ok() || lenient => Ok(()),
            Some(out) => Err(Failed(format!("'{}' exited {}", owned[0], out.code))),
            None if lenient => Ok(()),
            None => Err(Failed(format!("'{}' did not start", owned[0]))),
        }
    }

    fn step(&self, action: &WinAction) -> Result<(), Failed> {
        match action {
            WinAction::Run(argv) => self.spawn(argv, false),
            WinAction::RunLenient(argv) => self.spawn(argv, true),
            WinAction::Note(Level::Ok, text) => {
                self.ui.ok(text);
                Ok(())
            }
            WinAction::Note(Level::Warn, text) => {
                self.ui.warn(text);
                Ok(())
            }
            WinAction::SetEnv { key, value } => self.set_env(key, value),
            WinAction::DeployFiles { dest } => {
                if self.dry {
                    self.ui.ok(&format!("would unpack the payload into {dest}"));
                    return Ok(());
                }
                self.payload
                    .deploy(Path::new(&self.sub(dest)))
                    .map_err(Failed)?;
                self.ui.ok(&format!("payload unpacked into {dest}"));
                Ok(())
            }
            WinAction::DeleteFiles { paths } => {
                if self.dry {
                    self.ui.ok(&format!("would delete {}", paths.join(", ")));
                    return Ok(());
                }
                for path in paths {
                    match std::fs::remove_file(path) {
                        Ok(()) => self.ui.ok(&format!("deleted {path}")),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            self.ui.detail(&format!("{path} — already gone"));
                        }
                        Err(e) => self.ui.warn(&format!("could not delete {path}: {e}")),
                    }
                }
                Ok(())
            }
            WinAction::RemoveFiles { dir } => {
                if self.dry {
                    self.ui.ok(&format!("would remove {dir}"));
                    return Ok(());
                }
                // Best-effort: the uninstaller itself lives here until WP3.x's copy-to-temp
                // dance, and a locked file must not fail the teardown that already ran.
                match std::fs::remove_dir_all(dir) {
                    Ok(()) => self.ui.ok(&format!("removed {dir}")),
                    Err(e) => self.ui.warn(&format!("could not fully remove {dir}: {e}")),
                }
                Ok(())
            }
            WinAction::PathAdd { machine, dir } => self.path_edit(*machine, dir, true),
            WinAction::PathRemove { machine, dir } => self.path_edit(*machine, dir, false),
            WinAction::ArpRegister {
                key,
                display_name,
                version,
                location,
            } => self.arp_register(key, display_name, version, location),
            WinAction::ArpRemove { key } => {
                if self.dry {
                    self.ui.ok(&format!(
                        "would remove the Add/Remove Programs entry ({key})"
                    ));
                    return Ok(());
                }
                self.spawn(&["reg", "delete", key, "/f"].map(str::to_string), true)
            }
            WinAction::Shortcut { link, target } => {
                if self.dry {
                    self.ui.ok(&format!("would create {link} → {target}"));
                    return Ok(());
                }
                match sys::create_shortcut(&self.sub(link), &self.sub(target)) {
                    Ok(()) => self.ui.ok(&format!("created {link}")),
                    Err(e) => self.ui.warn(&format!("could not create {link}: {e}")),
                }
                Ok(())
            }
            WinAction::MakeNetworkPrivate { network } => {
                if self.dry {
                    self.ui
                        .ok(&format!("would set network '{network}' to Private"));
                    return Ok(());
                }
                if self.net.make_private(network) {
                    self.ui.ok(&format!("network '{network}' is now Private"));
                } else {
                    self.ui.warn(&format!(
                        "could not change '{network}' — set it to Private in Windows Settings, or re-run and open the public firewall"
                    ));
                }
                Ok(())
            }
            WinAction::StopHostRuntime => self.stop_host_runtime(),
            WinAction::RestoreTasks {
                web_enabled,
                scripting_enabled,
            } => {
                if self.dry {
                    self.ui
                        .ok("would re-enable only the tasks that were enabled before the stop");
                    return Ok(());
                }
                for (task, enabled) in [
                    ("PunktfunkWeb", web_enabled),
                    ("PunktfunkScripting", scripting_enabled),
                ] {
                    if *enabled == Some(true) {
                        self.spawn_quiet(&["schtasks", "/Change", "/TN", task, "/ENABLE"], true)?;
                    }
                }
                self.ui
                    .ok("re-enabled the tasks that were enabled before the stop");
                Ok(())
            }
            WinAction::WebSetup {
                app_dir,
                fresh_password,
            } => self.web_setup(app_dir, *fresh_password),
            WinAction::RegisterScriptingTask { app_dir, start_now } => {
                self.register_scripting(app_dir, *start_now)
            }
            WinAction::LaunchTray { exe } => self.launch_tray(exe),
            WinAction::EnsureAppRuntime { arch } => self.ensure_app_runtime(arch),
            WinAction::KillPortListeners { ports } => self.kill_port_listeners(ports),
        }
    }

    fn set_env(&self, key: &str, value: &str) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                r"would set {key}={value} in %ProgramData%\punktfunk\host.env"
            ));
            return Ok(());
        }
        let path = self.paths.host_env();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let body = upsert_env(&existing, key, value);
        std::fs::write(&path, body)
            .map_err(|e| Failed(format!("could not write host.env: {e}")))?;
        self.ui.ok(&format!(
            r"{key}={value} → %ProgramData%\punktfunk\host.env"
        ));
        Ok(())
    }

    /// The surgical PATH edit, entirely through `reg.exe`: read, rebuild, write EXPAND_SZ.
    fn path_edit(&self, machine: bool, dir: &str, add: bool) -> Result<(), Failed> {
        let scope = if machine { "machine" } else { "user" };
        if self.dry {
            if add {
                self.ui.ok(&format!("would add {dir} to the {scope} PATH"));
            } else {
                self.ui.ok(&format!(
                    "would remove {dir} from the {scope} PATH (entry-by-entry, never a substring delete)"
                ));
            }
            return Ok(());
        }
        let key = if machine {
            r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment"
        } else {
            r"HKCU\Environment"
        };
        let current = self
            .run
            .probe("reg", &["query", key, "/v", "Path"])
            .filter(|o| o.ok())
            .and_then(|o| super::parse_reg_value(&o.stdout, "Path"))
            .unwrap_or_default();
        let Some(new) = (if add {
            path_with(&current, dir)
        } else {
            path_without(&current, dir)
        }) else {
            self.ui.ok(&format!("{scope} PATH already right"));
            return Ok(());
        };
        self.spawn_quiet(
            &[
                "reg",
                "add",
                key,
                "/v",
                "Path",
                "/t",
                "REG_EXPAND_SZ",
                "/d",
                &new,
                "/f",
            ],
            false,
        )?;
        self.ui.ok(&format!("{scope} PATH updated"));
        if let Err(e) = sys::broadcast_env_change() {
            self.ui
                .detail(&format!("env-change broadcast skipped: {e}"));
        }
        Ok(())
    }

    fn arp_register(
        &self,
        key: &str,
        display_name: &str,
        version: &str,
        location: &str,
    ) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                "would register '{display_name}' in Add/Remove Programs ({key})"
            ));
            return Ok(());
        }
        let location = self.sub(location);
        let uninstall = format!("\"{location}\\unins000.exe\"");
        let values: [(&str, &str, String); 8] = [
            ("DisplayName", "REG_SZ", display_name.into()),
            ("DisplayVersion", "REG_SZ", self.sub(version)),
            ("Publisher", "REG_SZ", "unom".into()),
            ("InstallLocation", "REG_SZ", location.clone()),
            (
                "DisplayIcon",
                "REG_SZ",
                format!("{location}\\punktfunk.ico"),
            ),
            ("UninstallString", "REG_SZ", uninstall.clone()),
            (
                "QuietUninstallString",
                "REG_SZ",
                format!("{uninstall} /VERYSILENT /SUPPRESSMSGBOXES"),
            ),
            ("NoModify", "REG_DWORD", "1".into()),
        ];
        for (name, ty, data) in &values {
            self.spawn_quiet(
                &["reg", "add", key, "/v", name, "/t", ty, "/d", data, "/f"],
                false,
            )?;
        }
        self.ui.ok(&format!(
            "registered '{display_name}' in Add/Remove Programs"
        ));
        Ok(())
    }

    fn stop_host_runtime(&self) -> Result<(), Failed> {
        if self.dry {
            self.ui
                .ok("would stop the service, every tray, and the console/plugin tasks");
            return Ok(());
        }
        if let Err(e) = sys::stop_service_wait("PunktfunkHost") {
            self.ui.warn(&format!("service stop: {e}"));
        }
        self.spawn_quiet(&["taskkill", "/F", "/IM", "punktfunk-tray.exe"], true)?;
        for task in ["PunktfunkWeb", "PunktfunkScripting"] {
            self.spawn_quiet(&["schtasks", "/Change", "/TN", task, "/DISABLE"], true)?;
            self.spawn_quiet(&["schtasks", "/End", "/TN", task], true)?;
        }
        self.ui
            .ok("stopped the service, trays and console/plugin tasks");
        self.kill_port_listeners(&[47992, 47993, 3000])
    }

    fn web_setup(&self, app_dir: &str, fresh_password: bool) -> Result<(), Failed> {
        let host_exe = format!("{app_dir}\\punktfunk-host.exe");
        if self.dry {
            let password = if fresh_password {
                r#" --password-file "<temp>\webpw.txt""#
            } else {
                ""
            };
            self.ui.plus(&format!(
                r#""{host_exe}" web setup --app-dir "{app_dir}"{password}"#
            ));
            return Ok(());
        }
        let mut argv = vec![
            host_exe,
            "web".into(),
            "setup".into(),
            "--app-dir".into(),
            app_dir.to_string(),
        ];
        let mut pw_file = None;
        if fresh_password {
            let password = match &self.web_password {
                Some(p) => p.clone(),
                None => sys::random_hex(12).map_err(Failed)?,
            };
            let file = format!("{}\\webpw.txt", self.subst.temp);
            std::fs::write(&file, format!("{password}\n"))
                .map_err(|e| Failed(format!("could not stage the password file: {e}")))?;
            argv.push("--password-file".into());
            argv.push(file.clone());
            pw_file = Some(file);
        }
        let outcome = self.spawn(&argv, false);
        if let Some(file) = pw_file {
            let _ = std::fs::remove_file(file);
        }
        outcome
    }

    fn register_scripting(&self, app_dir: &str, start_now: bool) -> Result<(), Failed> {
        self.ui
            .plus("schtasks /Create /TN PunktfunkScripting /XML <generated> /F");
        if !self.dry {
            let xml = scripting_task_xml(app_dir);
            let file = format!("{}\\pf-scripting-task.xml", self.subst.temp);
            std::fs::write(&file, to_utf16le_bom(&xml))
                .map_err(|e| Failed(format!("could not stage the task XML: {e}")))?;
            let outcome = self.spawn_quiet(
                &[
                    "schtasks",
                    "/Create",
                    "/TN",
                    "PunktfunkScripting",
                    "/XML",
                    &file,
                    "/F",
                ],
                false,
            );
            let _ = std::fs::remove_file(&file);
            outcome?;
        }
        if start_now {
            self.spawn(
                &["schtasks", "/Run", "/TN", "PunktfunkScripting"].map(str::to_string),
                true,
            )?;
        }
        Ok(())
    }

    /// De-elevated launch via a `/IT` scheduled task without `/RL` — a limited, interactive
    /// token out of an elevated process, with no COM and no PowerShell (the S1 trick).
    fn launch_tray(&self, exe: &str) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                "would start the tray ({exe}) — skipped in silent installs"
            ));
            return Ok(());
        }
        if self.silent {
            self.ui
                .ok("tray launch skipped (silent install) — the host's supervision starts one");
            return Ok(());
        }
        let name = "pf-tray-launch";
        for argv in [
            vec![
                "schtasks", "/Create", "/TN", name, "/TR", exe, "/SC", "ONCE", "/ST", "00:00",
                "/IT", "/F",
            ],
            vec!["schtasks", "/Run", "/TN", name],
            vec!["schtasks", "/Delete", "/TN", name, "/F"],
        ] {
            self.spawn_quiet(&argv, true)?;
        }
        self.ui.ok(&format!("started the tray ({exe})"));
        Ok(())
    }

    fn ensure_app_runtime(&self, arch: &str) -> Result<(), Failed> {
        if self.dry {
            self.ui.ok(&format!(
                "would ensure the Windows App Runtime ({arch}; downloaded when missing — a failure warns and never aborts)"
            ));
            return Ok(());
        }
        if sys::app_runtime_present() {
            self.ui.ok("Windows App Runtime already installed");
            return Ok(());
        }
        // Best-effort, exactly as shipped: every failure warns and points at the download.
        let url =
            format!("https://aka.ms/windowsappsdk/2.2/latest/windowsappruntimeinstall-{arch}.exe");
        let file = format!("{}\\windowsappruntimeinstall.exe", self.subst.temp);
        let steps: [Vec<&str>; 2] = [
            vec!["curl", "-fsSL", "-o", &file, &url],
            vec![&file, "--quiet"],
        ];
        for argv in steps {
            if self.spawn_quiet(&argv, false).is_err() {
                self.ui.warn(&format!(
                    "could not install the Windows App Runtime — install it manually: {url}"
                ));
                return Ok(());
            }
        }
        self.ui.ok("Windows App Runtime installed");
        Ok(())
    }

    fn kill_port_listeners(&self, ports: &[u16]) -> Result<(), Failed> {
        if self.dry {
            let list = ports
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            self.ui
                .ok(&format!("would stop anything still listening on {list}"));
            return Ok(());
        }
        let Some(out) = self
            .run
            .probe("netstat", &["-ano", "-p", "TCP"])
            .filter(|o| o.ok())
        else {
            return Ok(());
        };
        for pid in pids_listening_on(&out.stdout, ports) {
            self.spawn_quiet(&["taskkill", "/F", "/PID", &pid], true)?;
        }
        Ok(())
    }
}

/// Replace or append one `KEY=VALUE` line — the same shape the Linux executor applies.
fn upsert_env(existing: &str, key: &str, value: &str) -> String {
    let prefix = format!("{key}=");
    let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
    match lines.iter_mut().find(|l| l.starts_with(&prefix)) {
        Some(line) => *line = format!("{key}={value}"),
        None => lines.push(format!("{key}={value}")),
    }
    let mut body = lines.join("\n");
    body.push('\n');
    body
}

/// `None` = the dir is already an entry (case-insensitive, slash-insensitive) — nothing to do.
fn path_with(current: &str, dir: &str) -> Option<String> {
    let want = dir.trim_end_matches('\\');
    if current
        .split(';')
        .any(|e| e.trim_end_matches('\\').eq_ignore_ascii_case(want))
    {
        return None;
    }
    if current.is_empty() {
        Some(dir.to_string())
    } else {
        Some(format!("{current};{dir}"))
    }
}

/// `None` = the dir was not an entry. Rebuilds entry-by-entry, never a substring delete.
fn path_without(current: &str, dir: &str) -> Option<String> {
    let want = dir.trim_end_matches('\\');
    let kept: Vec<&str> = current
        .split(';')
        .filter(|e| !e.trim_end_matches('\\').eq_ignore_ascii_case(want))
        .collect();
    if kept.len() == current.split(';').count() {
        return None;
    }
    Some(kept.join(";"))
}

/// The PID column of `netstat -ano` rows whose local address ends in one of `ports`. The
/// state column is localized (ABHÖREN on a German box) and deliberately never read.
fn pids_listening_on(netstat: &str, ports: &[u16]) -> Vec<String> {
    let suffixes: Vec<String> = ports.iter().map(|p| format!(":{p}")).collect();
    let mut pids: Vec<String> = vec![];
    for line in netstat.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 || cols[0] != "TCP" {
            continue;
        }
        if suffixes.iter().any(|s| cols[1].ends_with(s.as_str()))
            && let Some(pid) = cols.last()
            && pid.chars().all(|c| c.is_ascii_digit())
            && !pids.iter().any(|p| p == pid)
        {
            pids.push((*pid).to_string());
        }
    }
    pids
}

/// The `.iss` registration as XML: boot trigger, LocalService, restart 999×/1 min, battery
/// tolerant — `schtasks` flags cannot express the restart backoff.
fn scripting_task_xml(app_dir: &str) -> String {
    let cmd = format!("{app_dir}\\scripting\\scripting-run.cmd");
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><BootTrigger><Enabled>true</Enabled></BootTrigger></Triggers>
  <Principals><Principal id="LocalService"><UserId>S-1-5-19</UserId><LogonType>ServiceAccount</LogonType></Principal></Principals>
  <Settings>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <RestartOnFailure><Interval>PT1M</Interval><Count>999</Count></RestartOnFailure>
  </Settings>
  <Actions Context="LocalService"><Exec><Command>{cmd}</Command></Exec></Actions>
</Task>
"#
    )
}

fn to_utf16le_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::super::choices::WinChoices;
    use super::super::plan::{self, Artifact};
    use super::super::{FakeNet, TaskState, WinFacts, WinInstall};
    use super::*;
    use crate::seam::FakeRunner;
    use crate::ui::Plain;

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
            web_task: TaskState::Absent,
            scripting_task: TaskState::Absent,
            inno_uninstaller: false,
            client_installed: None,
        }
    }

    fn executor<'a>(
        run: &'a FakeRunner,
        net: &'a FakeNet,
        payload: &'a FakePayload,
        paths: &'a BasePaths,
        ui: &'a dyn Reporter,
    ) -> WinExecutor<'a> {
        WinExecutor {
            run,
            net,
            payload,
            paths,
            ui,
            dry: false,
            silent: true,
            web_password: Some("test-password".into()),
            subst: Subst {
                version: "9.9.9".into(),
                staging: r"C:\stage".into(),
                temp: std::env::temp_dir().display().to_string(),
                local_app_data: r"C:\Users\me\AppData\Local".into(),
                start_menu: r"C:\Users\me\Start Menu\Programs".into(),
                desktop: r"C:\Users\me\Desktop".into(),
            },
        }
    }

    // M4: the client plan names per-user roots symbolically (so its goldens render on every
    // OS); a real run lands the payload where the user actually lives.
    #[test]
    fn a_real_client_run_expands_the_per_user_roots() {
        let (ui, _buf) = Plain::capture();
        let run = FakeRunner::new();
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        exec.step(&WinAction::DeployFiles {
            dest: r"%LocalAppData%\Programs\Punktfunk".into(),
        })
        .unwrap();
        assert_eq!(
            payload.deployed.borrow().as_slice(),
            [r"C:\Users\me\AppData\Local\Programs\Punktfunk"]
        );
        assert_eq!(
            exec.sub(r"<start menu>\Punktfunk.lnk"),
            r"C:\Users\me\Start Menu\Programs\Punktfunk.lnk"
        );
    }

    #[test]
    fn upsert_env_replaces_in_place_and_appends() {
        assert_eq!(upsert_env("", "K", "1"), "K=1\n");
        assert_eq!(upsert_env("A=2\nK=0\n", "K", "1"), "A=2\nK=1\n");
        assert_eq!(upsert_env("A=2\n", "K", "1"), "A=2\nK=1\n");
    }

    #[test]
    fn path_edit_is_containment_checked_and_entry_exact() {
        assert_eq!(path_with(r"C:\a;C:\b", r"C:\c").unwrap(), r"C:\a;C:\b;C:\c");
        // Case-insensitive, trailing-slash-insensitive: never a duplicate.
        assert!(path_with(r"C:\a;c:\PF\", r"C:\pf").is_none());
        // Entry-by-entry removal — a substring of another entry survives.
        assert_eq!(
            path_without(r"C:\pf;C:\pf-tools;C:\b", r"C:\pf").unwrap(),
            r"C:\pf-tools;C:\b"
        );
        assert!(path_without(r"C:\a", r"C:\nope").is_none());
    }

    // The .iss identified the console's survivors by port + PID, never the localized state
    // word — a German box says ABHÖREN where an English one says LISTENING.
    #[test]
    fn netstat_parse_matches_ports_and_ignores_the_state_word() {
        let out = "\r\nAktive Verbindungen\r\n\r\n  Proto  Lokale Adresse  Remoteadresse  Status  PID\r\n  TCP    0.0.0.0:47992   0.0.0.0:0      ABH\u{00d6}REN  4711\r\n  TCP    127.0.0.1:9000  0.0.0.0:0      ABH\u{00d6}REN  1234\r\n  TCP    [::]:47992      [::]:0         ABH\u{00d6}REN  4711\r\n";
        assert_eq!(pids_listening_on(out, &[47992, 3000]), ["4711"]);
    }

    #[test]
    fn a_real_run_substitutes_placeholders() {
        let (ui, buf) = Plain::capture();
        let run = FakeRunner::new().answer(
            r"C:\app\punktfunk-host.exe driver install --dir C:\stage\pfvdisplay",
            0,
            "",
        );
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        exec.spawn(
            &[
                r"C:\app\punktfunk-host.exe",
                "driver",
                "install",
                "--dir",
                r"<staging>\pfvdisplay",
            ]
            .map(str::to_string),
            false,
        )
        .unwrap();
        assert!(buf.borrow().contains(r"C:\stage\pfvdisplay"));
    }

    #[test]
    fn a_lenient_run_tolerates_exit_codes_but_not_a_missing_binary() {
        let (ui, _buf) = Plain::capture();
        let run = FakeRunner::new().with_path("taskkill");
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        // An unscripted probe of an on-PATH program exits 1: fine leniently, fatal otherwise.
        let argv = ["taskkill", "/F", "/IM", "x.exe"].map(str::to_string);
        assert!(exec.spawn(&argv, true).is_ok());
        assert!(exec.spawn(&argv, false).is_err());
        // A missing binary is fine leniently (absence is the goal state), fatal otherwise.
        let missing = ["no-such-tool"].map(str::to_string);
        assert!(exec.spawn(&missing, true).is_ok());
        assert!(exec.spawn(&missing, false).is_err());
    }

    #[test]
    fn arp_register_writes_the_frozen_uninstall_contract() {
        let (ui, _buf) = Plain::capture();
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let mut ok = FakeRunner::new();
        for (name, ty, data) in [
            ("DisplayName", "REG_SZ", "Punktfunk Host".to_string()),
            ("DisplayVersion", "REG_SZ", "9.9.9".to_string()),
            ("Publisher", "REG_SZ", "unom".to_string()),
            ("InstallLocation", "REG_SZ", r"C:\app".to_string()),
            ("DisplayIcon", "REG_SZ", r"C:\app\punktfunk.ico".to_string()),
            (
                "UninstallString",
                "REG_SZ",
                r#""C:\app\unins000.exe""#.to_string(),
            ),
            (
                "QuietUninstallString",
                "REG_SZ",
                r#""C:\app\unins000.exe" /VERYSILENT /SUPPRESSMSGBOXES"#.to_string(),
            ),
            ("NoModify", "REG_DWORD", "1".to_string()),
        ] {
            ok = ok.answer(
                &format!(
                    "reg add {} /v {name} /t {ty} /d {data} /f",
                    super::super::HOST_ARP_KEY
                ),
                0,
                "",
            );
        }
        let exec = executor(&ok, &net, &payload, &paths, &ui);
        exec.arp_register(
            super::super::HOST_ARP_KEY,
            "Punktfunk Host",
            "<version>",
            r"C:\app",
        )
        .unwrap();
        // And without a scripted reg, the first write fails — proof the writes go through
        // the runner seam, not around it.
        let bare = FakeRunner::new();
        let exec = executor(&bare, &net, &payload, &paths, &ui);
        assert!(exec
            .arp_register(super::super::HOST_ARP_KEY, "x", "1", r"C:\app")
            .is_err());
    }

    #[test]
    fn the_full_uninstall_plan_executes_through_the_seams() {
        let facts = WinFacts {
            installed: Some(WinInstall {
                version: Some("0.34.0".into()),
                location: Some(r"C:\Program Files\punktfunk\".into()),
            }),
            ..fresh_facts()
        };
        let choices = WinChoices::derive(&facts);
        let plan = plan::build(&facts, &choices, Artifact::Host, true);
        let (ui, _buf) = Plain::capture();
        let mut run = FakeRunner::new();
        for tool in ["taskkill", "schtasks", "netsh", "reg", "netstat"] {
            run = run.with_path(tool);
        }
        for cmd in [
            "service uninstall",
            "driver uninstall",
            "driver uninstall --gamepad",
            "driver uninstall --audio",
        ] {
            run = run.answer(
                &format!(r"C:\Program Files\punktfunk\punktfunk-host.exe {cmd}"),
                0,
                "",
            );
        }
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        // Lenient legs tolerate absent targets; the PATH read sees an empty value; the file
        // removal warns on a nonexistent dir — the honest degradations, none a failure.
        exec.execute(&plan).unwrap();
        assert!(
            run.ran.borrow().is_empty(),
            "nothing may go through run_shell"
        );
    }

    #[test]
    fn make_network_private_goes_through_the_net_seam() {
        let (ui, _buf) = Plain::capture();
        let run = FakeRunner::new();
        let (net, payload) = (FakeNet::default(), FakePayload::default());
        let paths = BasePaths::rooted(Path::new("/box"));
        let exec = executor(&run, &net, &payload, &paths, &ui);
        exec.step(&WinAction::MakeNetworkPrivate {
            network: "Netzwerk 2".into(),
        })
        .unwrap();
        assert_eq!(net.made_private.borrow().as_slice(), ["Netzwerk 2"]);
    }

    #[test]
    fn scripting_task_xml_carries_the_iss_semantics() {
        let xml = scripting_task_xml(r"C:\app");
        assert!(xml.contains(r"C:\app\scripting\scripting-run.cmd"));
        assert!(xml.contains("<UserId>S-1-5-19</UserId>"));
        assert!(xml.contains("<Count>999</Count>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false"));
        assert_eq!(to_utf16le_bom("ab"), [0xFF, 0xFE, b'a', 0, b'b', 0]);
    }
}
