//! Local appliance controls run outside the rendering thread.
use serde::Deserialize;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const HELPER: &str = "/usr/libexec/punktfunk/kiosk-control";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Enable,
    Disable,
    ResetPassword,
    InstallVirtualHere,
    EnableVirtualHere,
    DisableVirtualHere,
}
impl Action {
    fn argument(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Enable => "ssh-enable",
            Self::Disable => "ssh-disable",
            Self::ResetPassword => "ssh-reset-password",
            Self::InstallVirtualHere => "virtualhere-install",
            Self::EnableVirtualHere => "virtualhere-enable",
            Self::DisableVirtualHere => "virtualhere-disable",
        }
    }
}

#[derive(Default, Clone, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub available: bool,
    pub hostname: String,
    pub version: String,
    pub ip_addresses: Vec<String>,
    pub connection: String,
    pub temperature_c: Option<f64>,
    pub uptime_seconds: f64,
    pub ssh_enabled: bool,
    pub ssh_user: String,
    pub ssh_port: u16,
    pub password: Option<String>,
    pub error: Option<String>,
    pub virtualhere_supported: bool,
    pub virtualhere_installed: bool,
    pub virtualhere_running: bool,
    pub virtualhere_license_saved: bool,
}

pub fn available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| cfg!(target_os = "linux") && std::path::Path::new(HELPER).is_file())
}

/// A screen owns the response channel, including any generated password.
#[derive(Default)]
pub struct Request {
    pending: Option<Receiver<Result<Snapshot, String>>>,
    action: Option<Action>,
}
impl Request {
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }
    pub fn action(&self) -> Option<Action> {
        self.action
    }
    pub fn start(&mut self, action: Action) -> bool {
        if self.busy() {
            return false;
        }
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        self.action = Some(action);
        std::thread::spawn(move || {
            let _ = tx.send(run(action));
        });
        true
    }
    pub fn poll(&mut self) -> Option<Result<Snapshot, String>> {
        let rx = self.pending.as_ref()?;
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return None,
            Err(_) => Err("Couldn't read the device status. Try again.".into()),
        };
        self.pending = None;
        self.action = None;
        Some(result)
    }
}

fn parse(bytes: &[u8], success: bool) -> Result<Snapshot, String> {
    let state: Snapshot = serde_json::from_slice(bytes)
        .map_err(|_| "Couldn't read the device status. Try again.".to_string())?;
    if let Some(error) = &state.error {
        return Err(error.clone());
    }
    if !success {
        return Err("Couldn't change device settings. Try again.".into());
    }
    Ok(state)
}

fn run(action: Action) -> Result<Snapshot, String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("/usr/bin/sudo")
        .args(["-n", HELPER, action.argument()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Device controls are unavailable.".to_string())?;
    let timeout = if action == Action::InstallVirtualHere {
        180
    } else {
        30
    };
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("The device didn't respond. Try again.".into());
            }
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|_| "Couldn't read the device status.".to_string())?;
    parse(&output.stdout, output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_actions_are_fixed_arguments() {
        assert_eq!(Action::Status.argument(), "status");
        assert_eq!(Action::Enable.argument(), "ssh-enable");
        assert_eq!(Action::Disable.argument(), "ssh-disable");
        assert_eq!(Action::ResetPassword.argument(), "ssh-reset-password");
    }
    #[test]
    fn status_does_not_need_a_password() {
        let state = parse(
            br#"{"available":true,"ip_addresses":["192.168.1.5"],"ssh_enabled":false}"#,
            true,
        )
        .unwrap();
        assert!(state.available);
        assert!(state.password.is_none());
        assert_eq!(state.ip_addresses, ["192.168.1.5"]);
    }
    #[test]
    fn helper_error_is_not_success() {
        assert!(parse(br#"{"error":"Couldn't start SSH."}"#, true).is_err());
        assert!(parse(b"not json", true).is_err());
        assert!(parse(b"{}", false).is_err());
    }
}
