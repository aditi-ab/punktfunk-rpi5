//! Shared native (`punktfunk/1`) pairing state — the on-demand arming PIN (with expiry) plus the
//! persistent paired-clients store. One [`NativePairing`] handle is shared by the punktfunk/1 QUIC
//! accept loop ([`crate::m3`]) and the management API ([`crate::mgmt`]), so an operator can **arm
//! pairing and read the PIN from the web console** instead of the service log.
//!
//! The PIN direction is inherent to the SPAKE2 ceremony: the *host* mints the PIN and the *client*
//! enters it (the client needs it to build its first message). So the UI **displays** the PIN —
//! armed on demand for a short window — rather than accepting one.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The host's paired punktfunk/1 clients: `~/.config/punktfunk/punktfunk1-paired.json`.
/// (Separate from GameStream pairing, which has its own store and ceremony.)
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PairedClients {
    pub clients: Vec<PairedClient>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PairedClient {
    pub name: String,
    /// Hex SHA-256 of the client's certificate.
    pub fingerprint: String,
}

impl PairedClients {
    fn contains(&self, fp_hex: &str) -> bool {
        self.clients
            .iter()
            .any(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
    }
}

struct PairedState {
    path: PathBuf,
    clients: PairedClients,
}

/// The current arming window. `pin == None` ⇒ disarmed. `expires_at == None` ⇒ armed with no
/// expiry (the CLI `--allow-pairing` flag); `Some(t)` ⇒ a web-armed window that auto-disarms.
#[derive(Default)]
struct Armed {
    pin: Option<String>,
    expires_at: Option<Instant>,
}

/// Shared native-pairing state: the arming PIN window + the persistent trust store.
pub struct NativePairing {
    arm: Mutex<Armed>,
    paired: Mutex<PairedState>,
}

/// A snapshot for the management API / web console.
pub struct NativePairingStatus {
    pub armed: bool,
    /// The PIN to display while armed (the operator reads it; the user enters it on the client).
    pub pin: Option<String>,
    /// Seconds left in a timed window (`None` = armed with no expiry, e.g. the CLI flag).
    pub expires_in_secs: Option<u64>,
    pub paired_clients: u32,
}

fn default_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME unset")?;
    Ok(PathBuf::from(home).join(".config/punktfunk/punktfunk1-paired.json"))
}

fn load(path: &std::path::Path) -> PairedClients {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(state: &PairedState) -> Result<()> {
    if let Some(dir) = state.path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Atomic replace: a crash/full-disk mid-write must not truncate the trust store (which would
    // silently lock out every paired client on a --require-pairing host). Temp + rename.
    let tmp = state.path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&state.clients)?)?;
    std::fs::rename(&tmp, &state.path)?;
    Ok(())
}

fn random_pin() -> String {
    use rand::Rng;
    format!("{:04}", rand::thread_rng().gen_range(0..10_000u32))
}

impl NativePairing {
    /// Load the trust store. `store_path = None` uses the default config path. If `arm_at_start`
    /// (the CLI `--allow-pairing`/`--require-pairing` flags), arm immediately with `fixed_pin`
    /// (or a fresh random PIN) and **no expiry** — back-compat with the headless CLI flow.
    pub fn load_with(
        store_path: Option<PathBuf>,
        fixed_pin: Option<String>,
        arm_at_start: bool,
    ) -> Result<NativePairing> {
        let path = match store_path {
            Some(p) => p,
            None => default_path()?,
        };
        let clients = load(&path);
        let arm = if arm_at_start {
            Armed {
                pin: Some(fixed_pin.unwrap_or_else(random_pin)),
                expires_at: None,
            }
        } else {
            Armed::default()
        };
        Ok(NativePairing {
            arm: Mutex::new(arm),
            paired: Mutex::new(PairedState { path, clients }),
        })
    }

    /// Arm pairing with a fresh random PIN, valid for `ttl`. Returns the PIN to display.
    pub fn arm(&self, ttl: Duration) -> String {
        let pin = random_pin();
        *self.arm.lock().unwrap() = Armed {
            pin: Some(pin.clone()),
            expires_at: Some(Instant::now() + ttl),
        };
        pin
    }

    /// Disarm pairing (no new ceremonies accepted).
    pub fn disarm(&self) {
        *self.arm.lock().unwrap() = Armed::default();
    }

    /// Expire a timed window if its deadline passed (called under the lock before any read).
    fn expire(arm: &mut Armed) {
        if let Some(t) = arm.expires_at {
            if Instant::now() >= t {
                *arm = Armed::default();
            }
        }
    }

    /// The current valid PIN, or `None` if disarmed/expired. The QUIC ceremony reads this
    /// per-attempt, so a window that lapsed mid-connection no longer pairs.
    pub fn current_pin(&self) -> Option<String> {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        arm.pin.clone()
    }

    /// A snapshot for the management API.
    pub fn status(&self) -> NativePairingStatus {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        let expires_in_secs = arm
            .expires_at
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs());
        NativePairingStatus {
            armed: arm.pin.is_some(),
            pin: arm.pin.clone(),
            expires_in_secs,
            paired_clients: self.paired.lock().unwrap().clients.clients.len() as u32,
        }
    }

    /// Is this client (hex SHA-256 fingerprint) in the paired set?
    pub fn is_paired(&self, fp_hex: &str) -> bool {
        self.paired.lock().unwrap().clients.contains(fp_hex)
    }

    /// Record a successful pairing (re-pairing the same fingerprint just updates the name).
    pub fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        let mut p = self.paired.lock().unwrap();
        p.clients.clients.retain(|c| c.fingerprint != fp_hex);
        p.clients.clients.push(PairedClient {
            name: name.to_string(),
            fingerprint: fp_hex.to_string(),
        });
        save(&p)
    }

    /// The paired clients (for the management API's device list).
    pub fn list(&self) -> Vec<PairedClient> {
        self.paired.lock().unwrap().clients.clients.clone()
    }

    /// Remove a paired client by fingerprint. Returns whether one was removed.
    pub fn remove(&self, fp_hex: &str) -> Result<bool> {
        let mut p = self.paired.lock().unwrap();
        let before = p.clients.clients.len();
        p.clients
            .clients
            .retain(|c| !c.fingerprint.eq_ignore_ascii_case(fp_hex));
        let removed = p.clients.clients.len() != before;
        if removed {
            save(&p)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        // A unique-ish temp path without Date/rand-in-test fuss: pid + addr of a local.
        let x = 0u8;
        std::env::temp_dir().join(format!(
            "pf-native-pair-{}-{}.json",
            std::process::id(),
            &x as *const _ as usize
        ))
    }

    #[test]
    fn arm_expire_and_pair() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();
        // Disarmed by default.
        assert!(np.current_pin().is_none());
        assert!(!np.status().armed);

        // Arm with a tiny TTL → a PIN appears, then expires.
        let pin = np.arm(Duration::from_millis(40));
        assert_eq!(pin.len(), 4);
        assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));
        assert!(np.status().armed);
        std::thread::sleep(Duration::from_millis(60));
        assert!(np.current_pin().is_none(), "window should have expired");
        assert!(!np.status().armed);

        // Pair / list / unpair.
        assert!(!np.is_paired("ab12"));
        np.add("Living Room", "AB12").unwrap();
        assert!(
            np.is_paired("ab12"),
            "fingerprint match is case-insensitive"
        );
        assert_eq!(np.list().len(), 1);
        assert_eq!(np.status().paired_clients, 1);
        assert!(np.remove("ab12").unwrap());
        assert!(!np.remove("ab12").unwrap());
        assert!(np.list().is_empty());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn cli_flag_arms_with_no_expiry() {
        let p = temp();
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), Some("1234".into()), true).unwrap();
        assert_eq!(np.current_pin().as_deref(), Some("1234"));
        let s = np.status();
        assert!(s.armed);
        assert_eq!(s.expires_in_secs, None, "CLI arming has no expiry");
        np.disarm();
        assert!(np.current_pin().is_none());
        let _ = std::fs::remove_file(&p);
    }
}
