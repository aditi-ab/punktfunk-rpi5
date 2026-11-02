//! The driver store as `pnputil /enum-drivers` lists it, read once per process on its own
//! thread, for a pad's attach diagnosis.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long [`driver_store_inventory`] waits for pnputil. Only [`diagnose_blocking`]
/// waits, on its own thread. pnputil routinely exceeds a couple of seconds on a
/// busy driver store; 30 s is enough to report the inventory instead of "still enumerating".
const INVENTORY_WAIT: Duration = Duration::from_secs(30);

/// `pnputil /enum-drivers`, lower-cased, once per process. Failure-path only.
/// Query runs on its own thread so a wedged pnputil is not re-run per pad.
/// `None` = still running past [`INVENTORY_WAIT`], or failed; a late result still caches.
fn driver_store_inventory() -> Option<&'static str> {
    static INV: OnceLock<String> = OnceLock::new();
    static SPAWN: std::sync::Once = std::sync::Once::new();
    SPAWN.call_once(|| {
        std::thread::spawn(|| {
            let pnputil = pf_paths::system32("pnputil.exe");
            let inv = std::process::Command::new(&pnputil)
                .arg("/enum-drivers")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase())
                .unwrap_or_default();
            let _ = INV.set(inv);
        });
    });
    let deadline = Instant::now() + INVENTORY_WAIT;
    loop {
        if let Some(inv) = INV.get() {
            return Some(inv);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `None` = pnputil unavailable, failed, or still enumerating.
pub(super) fn driver_store_has(inf: &str) -> Option<bool> {
    let inv = driver_store_inventory()?;
    if inv.is_empty() {
        return None;
    }
    Some(inv.contains(&inf.to_ascii_lowercase()))
}
