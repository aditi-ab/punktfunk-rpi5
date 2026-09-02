//! Client half of host power actions (`design/host-actions.md`): list what the paired
//! host offers this device, then invoke one by id.
//!
//! Same mTLS lane as [`crate::library`]: device identity, fingerprint pin, `mgmt_port`.
//! [`ActionInfo::permitted`] is the host's grant for this device; ungranted rows are
//! omitted rather than offered as a 403. Both calls work out of session so a host tile
//! can sleep the box without a stream.

use serde::Deserialize;

/// One row from `GET /api/v1/actions` for this device.
///
/// Unknown ids are expected: render [`Self::title`] verbatim so a later host can add
/// actions without a client release.
#[derive(Clone, Debug, Deserialize)]
pub struct ActionInfo {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub group: String,
    /// Confirm first: the action drops host state (reboot, shutdown).
    #[serde(default)]
    pub danger: bool,
    /// Host can run it now. False for no-suspend, a foreign inhibitor, missing group.
    #[serde(default)]
    pub available: bool,
    /// Why `available` is false; shown on a disabled row, never used to hide it.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// Host-power grant for this device. False hides the row ([`Self::offerable`]).
    #[serde(default)]
    pub permitted: bool,
}

impl ActionInfo {
    /// Ungranted rows are omitted. Granted-but-unavailable rows stay, disabled, with [`Self::unavailable_reason`].
    pub fn offerable(&self) -> bool {
        self.permitted
    }

    /// Local wording for known ids; [`Self::title`] for anything else so a new host action still shows.
    pub fn label(&self) -> &str {
        match self.id.as_str() {
            "power.sleep" => "Sleep host",
            "power.reboot" => "Restart host",
            "power.shutdown" => "Shut down host",
            _ => &self.title,
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
#[derive(Deserialize)]
struct ActionList {
    #[serde(default)]
    actions: Vec<ActionInfo>,
}

/// `GET /api/v1/actions`. Empty on any miss, never an error — same contract as [`crate::library::fetch_running`].
///
/// An older host has no such route. A missing menu row is cheaper than failing the host card.
#[cfg(any(target_os = "linux", windows))]
pub fn fetch_actions(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Vec<ActionInfo> {
    let Ok(agent) = crate::library::agent(identity, pin) else {
        return Vec::new();
    };
    let url = format!(
        "{}/api/v1/actions",
        crate::library::base_url(addr, mgmt_port)
    );
    let Ok(mut resp) = agent.get(&url).call() else {
        return Vec::new();
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return Vec::new();
    };
    serde_json::from_str::<ActionList>(&body)
        .map(|l| l.actions)
        .unwrap_or_default()
}

/// `POST /api/v1/actions/{id}` with an empty body.
///
/// `Ok(())` is 202 Accepted: the host then ends every session and acts ~1 s later.
/// 4xx becomes [`crate::library::LibraryError::Unreachable`]; other failures go through [`crate::library::classify`].
#[cfg(any(target_os = "linux", windows))]
pub fn invoke(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    action_id: &str,
) -> Result<(), crate::library::LibraryError> {
    use crate::library::LibraryError;
    let agent = crate::library::agent(identity, pin)?;
    // Empty body: no request field reaches the privileged path. Do not percent-encode; ids are `[a-z.]`.
    let url = format!(
        "{}/api/v1/actions/{action_id}",
        crate::library::base_url(addr, mgmt_port)
    );
    match agent.post(&url).send_empty() {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => Err(
            LibraryError::Unreachable(format!("the host refused ({code})")),
        ),
        Err(e) => Err(crate::library::classify(e)),
    }
}

/// 300 s. Grant and suspend-capability change when an operator edits access, not
/// minute-to-minute; each refresh is a TLS handshake against an idle host.
#[cfg(any(target_os = "linux", windows))]
pub const TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// Process-wide, keyed by host fingerprint.
///
/// Settled before a menu draws: a row that appears under a cursor already moving toward
/// it can shut the machine down. One cache so the console, GTK, and Windows tiles agree.
#[cfg(any(target_os = "linux", windows))]
type Cache =
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Vec<ActionInfo>)>>;

#[cfg(any(target_os = "linux", windows))]
fn cache() -> &'static Cache {
    static C: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// Offerable rows last stored for this fingerprint. Empty until [`refresh`] answers, and for no-route / no-grant hosts.
#[cfg(any(target_os = "linux", windows))]
pub fn cached(fp_hex: &str) -> Vec<ActionInfo> {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(fp_hex)
        .map(|(_, a)| a.clone())
        .unwrap_or_default()
}

/// Spawn a worker unless the cache is still inside [`TTL`]. Idempotent; call it on any shell tick.
///
/// The freshness stamp is taken before the request so a hang cannot spawn another worker
/// every tick. Identity is loaded on the worker so menus do not have to thread it through.
#[cfg(any(target_os = "linux", windows))]
pub fn refresh(addr: &str, mgmt_port: u16, fp_hex: &str) {
    if fp_hex.is_empty() {
        return; // empty fingerprint cannot authenticate or key the cache
    }
    {
        let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
        match c.get_mut(fp_hex) {
            Some(entry) if entry.0.elapsed() < TTL => return,
            Some(entry) => entry.0 = std::time::Instant::now(),
            None => {
                c.insert(fp_hex.to_string(), (std::time::Instant::now(), Vec::new()));
            }
        }
    }
    let (addr, fp_hex) = (addr.to_string(), fp_hex.to_string());
    std::thread::Builder::new()
        .name("punktfunk-hostactions".into())
        .spawn(move || {
            let Ok(identity) = crate::trust::load_or_create_identity() else {
                return;
            };
            let pin = crate::trust::parse_hex32(&fp_hex);
            let found: Vec<ActionInfo> = fetch_actions(&addr, mgmt_port, &identity, pin)
                .into_iter()
                .filter(ActionInfo::offerable)
                .collect();
            cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(fp_hex, (std::time::Instant::now(), found));
        })
        .ok();
}

/// Drop the cache after [`invoke`]: otherwise the menu still offers Sleep until [`TTL`] lapses on an already-asleep host.
#[cfg(any(target_os = "linux", windows))]
pub fn invalidate(fp_hex: &str) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(fp_hex);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_prefer_local_wording_and_fall_back_to_the_host() {
        let mk = |id: &str, title: &str| ActionInfo {
            id: id.into(),
            title: title.into(),
            group: "power".into(),
            danger: false,
            available: true,
            unavailable_reason: None,
            permitted: true,
        };
        assert_eq!(mk("power.sleep", "Sleep host").label(), "Sleep host");
        assert_eq!(mk("power.reboot", "whatever").label(), "Restart host");
        assert_eq!(
            mk("plugin:vpn:toggle", "Toggle the VPN").label(),
            "Toggle the VPN"
        );
    }

    #[test]
    fn permission_hides_but_unavailability_only_disables() {
        let mut a = ActionInfo {
            id: "power.sleep".into(),
            title: "Sleep host".into(),
            group: "power".into(),
            danger: false,
            available: false,
            unavailable_reason: Some("this machine does not support sleep".into()),
            permitted: true,
        };
        assert!(a.offerable(), "unavailable actions are shown with a reason");
        a.permitted = false;
        assert!(!a.offerable(), "an ungranted action is not offered at all");
    }
}
