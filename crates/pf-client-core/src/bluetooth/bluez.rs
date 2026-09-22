//! BlueZ ObjectManager and a connection-local pairing agent.
//!
//! The agent accepts requests only for the device the user is pairing. It is not
//! the system default agent and never grants unattended pairing or service access.

use super::*;
use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use zbus::blocking::{connection::Builder, Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const AGENT: &str = "/org/punktfunk/BluetoothAgent";
type Properties = HashMap<String, OwnedValue>;
type Objects = HashMap<OwnedObjectPath, HashMap<String, Properties>>;
type PendingReply = (u64, async_channel::Sender<Option<String>>);
static PENDING: Mutex<Option<PendingReply>> = Mutex::new(None);
static PAIRING: Mutex<Option<String>> = Mutex::new(None);
static NEXT_PROMPT: AtomicU64 = AtomicU64::new(1);
static CANCEL_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    Rejected(String),
    Canceled(String),
}

fn clear_prompt(id: u64) {
    let mut state = STATE.lock().unwrap();
    if state.prompt.as_ref().is_some_and(|p| p.id == id) {
        state.prompt = None;
    }
}

pub(super) fn reply(id: u64, value: Option<String>) {
    let pending = PENDING.lock().unwrap();
    if let Some((current, sender)) = pending.as_ref().filter(|(current, _)| *current == id) {
        let _ = sender.try_send(value);
        clear_prompt(*current);
    }
}

fn cancel_prompt() {
    if let Some((id, sender)) = PENDING.lock().unwrap().take() {
        let _ = sender.try_send(None);
        clear_prompt(id);
    }
    STATE.lock().unwrap().prompt = None;
}

fn require_pairing(device: &OwnedObjectPath) -> std::result::Result<(), AgentError> {
    if PAIRING.lock().unwrap().as_deref() != Some(device.as_str()) {
        return Err(AgentError::Rejected(
            "No pairing was requested for this device".into(),
        ));
    }
    Ok(())
}

fn publish_prompt(device: &OwnedObjectPath, kind: PromptKind) -> u64 {
    let id = NEXT_PROMPT.fetch_add(1, Ordering::Relaxed);
    let mut state = STATE.lock().unwrap();
    let name = state
        .devices
        .iter()
        .find(|d| d.path == device.as_str())
        .map_or_else(
            || device.to_string(),
            |d| format!("{} ({})", d.name, d.address),
        );
    state.prompt = Some(Prompt {
        id,
        device: name,
        kind,
    });
    id
}

async fn ask(device: OwnedObjectPath, kind: PromptKind) -> std::result::Result<String, AgentError> {
    require_pairing(&device)?;
    let (sender, receiver) = async_channel::bounded(1);
    let id;
    {
        let mut pending = PENDING.lock().unwrap();
        require_pairing(&device)?;
        if pending.is_some() {
            return Err(AgentError::Rejected(
                "Another pairing prompt is pending".into(),
            ));
        }
        id = publish_prompt(&device, kind);
        *pending = Some((id, sender));
    }
    let result = tokio::time::timeout(Duration::from_secs(60), receiver.recv()).await;
    {
        let mut pending = PENDING.lock().unwrap();
        if pending.as_ref().is_some_and(|(current, _)| *current == id) {
            *pending = None;
        }
    }
    clear_prompt(id);
    match result {
        Ok(Ok(Some(value))) => Ok(value),
        _ => Err(AgentError::Canceled(
            "Pairing was canceled or timed out".into(),
        )),
    }
}

struct Agent;

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {
        cancel_prompt();
    }
    fn cancel(&self) {
        cancel_prompt();
    }

    async fn request_pin_code(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<String, AgentError> {
        let pin = ask(device, PromptKind::Pin).await?;
        if pin.is_empty() || pin.len() > 16 || !pin.is_ascii() {
            return Err(AgentError::Rejected(
                "Enter a PIN of 1 to 16 ASCII characters".into(),
            ));
        }
        Ok(pin)
    }

    async fn request_passkey(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<u32, AgentError> {
        let value = ask(device, PromptKind::Passkey).await?;
        parse_passkey(&value).ok_or_else(|| AgentError::Rejected("Enter up to six digits".into()))
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> std::result::Result<(), AgentError> {
        if ask(device, PromptKind::Confirm { passkey }).await? == "yes" {
            Ok(())
        } else {
            Err(AgentError::Rejected("Pairing was declined".into()))
        }
    }

    async fn request_authorization(
        &self,
        device: OwnedObjectPath,
    ) -> std::result::Result<(), AgentError> {
        if ask(device, PromptKind::Authorize).await? == "yes" {
            Ok(())
        } else {
            Err(AgentError::Rejected("Pairing was declined".into()))
        }
    }

    fn display_pin_code(
        &self,
        device: OwnedObjectPath,
        pincode: String,
    ) -> std::result::Result<(), AgentError> {
        require_pairing(&device)?;
        publish_prompt(&device, PromptKind::Display { code: pincode });
        Ok(())
    }

    fn display_passkey(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        entered: u16,
    ) -> std::result::Result<(), AgentError> {
        require_pairing(&device)?;
        publish_prompt(
            &device,
            PromptKind::Display {
                code: format!("{passkey:06} ({entered}/6 entered)"),
            },
        );
        Ok(())
    }

    fn authorize_service(
        &self,
        device: OwnedObjectPath,
        _uuid: String,
    ) -> std::result::Result<(), AgentError> {
        require_pairing(&device)
    }
}

fn parse_passkey(value: &str) -> Option<u32> {
    (!value.is_empty() && value.len() <= 6 && value.bytes().all(|c| c.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn proxy<'a>(conn: &'a Connection, path: &'a str, interface: &'a str) -> zbus::Result<Proxy<'a>> {
    Proxy::new(conn, "org.bluez", path, interface)
}

fn boolean(props: &Properties, name: &str) -> bool {
    props
        .get(name)
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

fn string(props: &Properties, name: &str) -> String {
    props
        .get(name)
        .and_then(|v| <&str>::try_from(v).ok())
        .unwrap_or("")
        .to_string()
}

fn read_devices(conn: &Connection) -> Result<Option<String>> {
    let objects: Objects =
        proxy(conn, "/", "org.freedesktop.DBus.ObjectManager")?.call("GetManagedObjects", &())?;
    let mut adapters: Vec<_> = objects
        .iter()
        .filter(|(_, interfaces)| interfaces.contains_key("org.bluez.Adapter1"))
        .collect();
    adapters.sort_by_key(|(path, _)| path.as_str());
    let adapter = adapters
        .iter()
        .find(|(_, interfaces)| boolean(&interfaces["org.bluez.Adapter1"], "Powered"))
        .or(adapters.first());
    let Some((path, interfaces)) = adapter else {
        let mut state = STATE.lock().unwrap();
        state.available = false;
        state.devices.clear();
        return Ok(None);
    };
    let path = path.to_string();
    let props = &interfaces["org.bluez.Adapter1"];
    let mut devices = Vec::new();
    for (device_path, interfaces) in &objects {
        let Some(device) = interfaces.get("org.bluez.Device1") else {
            continue;
        };
        if !device_path.as_str().starts_with(&format!("{path}/")) {
            continue;
        }
        let address = string(device, "Address");
        let name = string(device, "Alias");
        devices.push(Device {
            path: device_path.to_string(),
            name: if name.is_empty() {
                address.clone()
            } else {
                name
            },
            address,
            paired: boolean(device, "Paired"),
            connected: boolean(device, "Connected"),
            trusted: boolean(device, "Trusted"),
            battery: interfaces
                .get("org.bluez.Battery1")
                .and_then(|p| p.get("Percentage"))
                .and_then(|v| u8::try_from(v).ok()),
        });
    }
    devices.sort_by(|a, b| {
        (!a.connected, !a.paired, &a.name, &a.path).cmp(&(
            !b.connected,
            !b.paired,
            &b.name,
            &b.path,
        ))
    });
    let mut state = STATE.lock().unwrap();
    state.available = true;
    state.powered = boolean(props, "Powered");
    state.devices = devices;
    Ok(Some(path))
}

fn device_action(conn: Connection, command: Command, generation: u64) {
    let result = (|| -> Result<()> {
        let path = match &command {
            Command::Pair(p)
            | Command::Connect(p)
            | Command::Disconnect(p)
            | Command::Forget(p) => p,
            _ => bail!("invalid Bluetooth operation"),
        };
        let device = snapshot()
            .devices
            .into_iter()
            .find(|d| &d.path == path)
            .context("device is no longer available")?;
        let dev = proxy(&conn, path, "org.bluez.Device1")?;
        match command {
            Command::Pair(_) => {
                ensure_not_canceled(generation)?;
                if !device.paired {
                    dev.call::<_, _, ()>("Pair", &())?;
                }
                ensure_not_canceled(generation)?;
                dev.set_property("Trusted", true)?;
                ensure_not_canceled(generation)?;
                dev.call::<_, _, ()>("Connect", &())
                    .context("paired, but connection did not complete; select Connect to retry")?;
            }
            Command::Connect(_) => dev.call::<_, _, ()>("Connect", &())?,
            Command::Disconnect(_) => dev.call::<_, _, ()>("Disconnect", &())?,
            Command::Forget(_) => {
                let adapter = path.rsplit_once('/').context("device adapter path")?.0;
                proxy(&conn, adapter, "org.bluez.Adapter1")?.call::<_, _, ()>(
                    "RemoveDevice",
                    &(OwnedObjectPath::try_from(path.as_str())?,),
                )?;
            }
            _ => unreachable!(),
        }
        Ok(())
    })();
    *PAIRING.lock().unwrap() = None;
    cancel_prompt();
    let mut state = STATE.lock().unwrap();
    state.busy = false;
    if let Err(error) = result {
        tracing::warn!(%error, "Bluetooth operation did not complete");
        state.error = Some(user_error(&error));
    }
}

fn ensure_not_canceled(generation: u64) -> Result<()> {
    if CANCEL_GENERATION.load(Ordering::SeqCst) != generation {
        bail!("pairing was canceled");
    }
    Ok(())
}

fn user_error(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    if detail.contains("paired, but") {
        "The device is paired but didn't connect. Select Connect to try again."
    } else if detail.contains("AccessDenied") || detail.contains("NotAuthorized") {
        "Bluetooth access was denied. Check this user's Bluetooth permissions."
    } else if detail.contains("Authentication") || detail.contains("Rejected") {
        "Pairing didn't complete. Check the code and put the device in pairing mode again."
    } else if detail.contains("canceled") || detail.contains("Canceled") {
        "Pairing was canceled. You can try again."
    } else if detail.contains("NotReady") {
        "Bluetooth isn't ready. Turn it on and try again."
    } else {
        "The device didn't respond. Keep it nearby and try again."
    }
    .into()
}

pub(super) fn run(receiver: mpsc::Receiver<Command>) {
    loop {
        if let Err(error) = serve(&receiver) {
            tracing::debug!(%error, "Bluetooth service unavailable");
            CANCEL_GENERATION.fetch_add(1, Ordering::SeqCst);
            *PAIRING.lock().unwrap() = None;
            cancel_prompt();
            let mut state = STATE.lock().unwrap();
            state.available = false;
            state.discovering = false;
            state.devices.clear();
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn serve(receiver: &mpsc::Receiver<Command>) -> Result<()> {
    let conn = Builder::system()?
        .method_timeout(Duration::from_secs(75))
        .serve_at(AGENT, Agent)?
        .build()?;
    let manager = proxy(&conn, "/org/bluez", "org.bluez.AgentManager1")?;
    manager.call::<_, _, ()>(
        "RegisterAgent",
        &(OwnedObjectPath::try_from(AGENT)?, "KeyboardDisplay"),
    )?;
    tracing::info!("Bluetooth management agent registered");
    let mut scan: Option<(String, Instant)> = None;
    loop {
        let adapter = read_devices(&conn)?;
        if scan
            .as_ref()
            .is_some_and(|(_, start)| start.elapsed() >= Duration::from_secs(60))
        {
            if let Some((path, _)) = scan.take() {
                let _ = proxy(&conn, &path, "org.bluez.Adapter1")?
                    .call::<_, _, ()>("StopDiscovery", &());
            }
        }
        STATE.lock().unwrap().discovering = scan.is_some();
        let command = match receiver.recv_timeout(Duration::from_millis(750)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => return Ok(()),
        };
        if matches!(command, Command::ClearError) {
            STATE.lock().unwrap().error = None;
            continue;
        }
        if matches!(command, Command::CancelPairing) {
            CANCEL_GENERATION.fetch_add(1, Ordering::SeqCst);
            let pairing = PAIRING.lock().unwrap().take();
            cancel_prompt();
            if let Some(path) = pairing {
                let conn = conn.clone();
                std::thread::spawn(move || {
                    if let Ok(dev) = proxy(&conn, &path, "org.bluez.Device1") {
                        let _ = dev.call::<_, _, ()>("CancelPairing", &());
                    }
                });
            }
            continue;
        }
        if matches!(command, Command::Scan(false)) {
            if let Some((path, _)) = scan.take() {
                let _ = proxy(&conn, &path, "org.bluez.Adapter1")?
                    .call::<_, _, ()>("StopDiscovery", &());
            }
            continue;
        }
        if snapshot().busy {
            continue;
        }
        STATE.lock().unwrap().error = None;
        let result = (|| -> Result<()> {
            let adapter = adapter.context("no Bluetooth adapter is available")?;
            match command {
                Command::Power(on) => {
                    if !on {
                        if let Some((path, _)) = scan.take() {
                            let _ = proxy(&conn, &path, "org.bluez.Adapter1")?
                                .call::<_, _, ()>("StopDiscovery", &());
                        }
                    }
                    proxy(&conn, &adapter, "org.bluez.Adapter1")?.set_property("Powered", on)?
                }
                Command::Scan(true) => {
                    if scan.is_none() {
                        proxy(&conn, &adapter, "org.bluez.Adapter1")?
                            .call::<_, _, ()>("StartDiscovery", &())?;
                        scan = Some((adapter, Instant::now()));
                    }
                }
                Command::Pair(_)
                | Command::Connect(_)
                | Command::Disconnect(_)
                | Command::Forget(_) => {
                    STATE.lock().unwrap().busy = true;
                    if let Command::Pair(path) = &command {
                        *PAIRING.lock().unwrap() = Some(path.clone());
                    }
                    let generation = CANCEL_GENERATION.load(Ordering::SeqCst);
                    let conn = conn.clone();
                    std::thread::spawn(move || device_action(conn, command, generation));
                }
                _ => (),
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "Bluetooth adapter operation did not complete");
            STATE.lock().unwrap().error = Some(user_error(&error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passkeys_are_six_decimal_digits_or_less() {
        assert_eq!(parse_passkey("000042"), Some(42));
        for bad in ["", "-1", "+1", "1.5", "１２", "1000000", " 12"] {
            assert_eq!(parse_passkey(bad), None);
        }
    }
    #[test]
    fn a_stale_reply_does_not_answer_the_current_prompt() {
        let (sender, receiver) = async_channel::bounded(1);
        *PENDING.lock().unwrap() = Some((42, sender));
        reply(41, Some("yes".into()));
        assert!(receiver.try_recv().is_err());
        reply(42, None);
        assert_eq!(receiver.try_recv().unwrap(), None);
        *PENDING.lock().unwrap() = None;
    }

    #[test]
    fn canceled_pairing_revokes_agent_consent_and_followup_work() {
        let device = OwnedObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_55").unwrap();
        let other = OwnedObjectPath::try_from("/org/bluez/hci0/dev_00_11_22_33_44_66").unwrap();
        *PAIRING.lock().unwrap() = Some(device.to_string());
        assert!(require_pairing(&device).is_ok());
        assert!(require_pairing(&other).is_err());
        let generation = CANCEL_GENERATION.fetch_add(1, Ordering::SeqCst);
        *PAIRING.lock().unwrap() = None;
        assert!(ensure_not_canceled(generation).is_err());
        assert!(require_pairing(&device).is_err());
    }
}
