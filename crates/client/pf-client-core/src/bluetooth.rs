//! Bluetooth management snapshots for native client settings.
//!
//! BlueZ I/O stays on worker threads. UI code reads snapshots and submits commands;
//! pairing replies bypass the operation queue so a pending Pair never blocks consent.
//! Platforms without the Linux desktop backend publish an unavailable snapshot.

use std::sync::{mpsc, LazyLock, Mutex};

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub available: bool,
    pub powered: bool,
    pub discovering: bool,
    pub busy: bool,
    pub devices: Vec<Device>,
    pub prompt: Option<Prompt>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Device {
    pub path: String,
    pub name: String,
    pub address: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub battery: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub id: u64,
    pub device: String,
    pub kind: PromptKind,
}

#[derive(Clone, Debug)]
pub enum PromptKind {
    Confirm { passkey: u32 },
    Pin,
    Passkey,
    Display { code: String },
    Authorize,
}

#[derive(Clone, Debug)]
pub enum Command {
    Power(bool),
    Scan(bool),
    Pair(String),
    Connect(String),
    Disconnect(String),
    Forget(String),
    Reply { id: u64, value: Option<String> },
    CancelPairing,
    ClearError,
}

static STATE: LazyLock<Mutex<Snapshot>> = LazyLock::new(|| Mutex::new(Snapshot::default()));
static COMMANDS: Mutex<Option<mpsc::Sender<Command>>> = Mutex::new(None);

pub fn snapshot() -> Snapshot {
    STATE.lock().unwrap().clone()
}

pub fn submit(command: Command) {
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    if let Command::Reply { id, value } = &command {
        bluez::reply(*id, value.clone());
        return;
    }
    if let Some(sender) = COMMANDS.lock().unwrap().as_ref() {
        let _ = sender.send(command);
    }
}

pub fn start() {
    #[cfg(all(target_os = "linux", feature = "desktop"))]
    {
        let mut commands = COMMANDS.lock().unwrap();
        if commands.is_none() {
            let (sender, receiver) = mpsc::channel();
            *commands = Some(sender);
            std::thread::spawn(move || bluez::run(receiver));
        }
    }
}

#[cfg(all(target_os = "linux", feature = "desktop"))]
mod bluez;
