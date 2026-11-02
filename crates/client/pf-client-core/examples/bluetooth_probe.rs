//! Read-only adapter probe; --scan also holds a discovery session for five seconds.

use pf_client_core::bluetooth::{self, Command};
use std::time::Duration;

fn main() {
    bluetooth::start();
    for _ in 0..20 {
        if bluetooth::snapshot().available {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let initial = bluetooth::snapshot();
    assert!(initial.available, "Bluetooth adapter unavailable");
    println!("adapter available; powered={}", initial.powered);
    if std::env::args().any(|arg| arg == "--scan") {
        bluetooth::submit(Command::Scan(true));
        std::thread::sleep(Duration::from_secs(5));
        let scanning = bluetooth::snapshot();
        bluetooth::submit(Command::Scan(false));
        std::thread::sleep(Duration::from_secs(2));
        assert!(
            scanning.discovering,
            "discovery did not start: {:?}",
            scanning.error
        );
        assert!(!bluetooth::snapshot().discovering, "discovery did not stop");
        println!(
            "discovery started and stopped; {} devices",
            scanning.devices.len()
        );
    }
}
