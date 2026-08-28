//! Host actions from a client (`design/host-actions.md` §7): discovering what the paired host
//! offers — v1, sleep / restart / shut down — and invoking one by id.
//!
//! Same lane, same trust, same agent as the library browse ([`crate::library`]): TLS client auth
//! with the device identity, host pinned by fingerprint, over `mgmt_port`. Nothing new is asked
//! of the transport, and the HOST is the only enforcer — [`ActionInfo::permitted`] is what the
//! host says about *this* device's grants, so the client renders honestly instead of offering a
//! row that will 403.
//!
//! Both calls work OUT of session, which is the point: "sleep the host" belongs on a host tile
//! at the end of an evening, not only mid-stream.

use serde::Deserialize;

/// One action as the host reports it to THIS caller (`GET /api/v1/actions`).
///
/// Unknown ids are expected and fine — a client renders [`Self::title`] verbatim for anything it
/// has no local string for, which is what lets a later host add actions with no client release.
#[derive(Clone, Debug, Deserialize)]
pub struct ActionInfo {
    /// Stable id: `power.sleep`, `power.reboot`, `power.shutdown` today.
    pub id: String,
    /// The host's own display title — the fallback label for an id this client doesn't know.
    #[serde(default)]
    pub title: String,
    /// Action group (`power` for the built-ins).
    #[serde(default)]
    pub group: String,
    /// Confirm twice before running it: the action loses state (restart, shut down).
    #[serde(default)]
    pub danger: bool,
    /// Whether the host can run it at all right now (a machine that cannot suspend, a foreign
    /// inhibitor, a missing group membership).
    #[serde(default)]
    pub available: bool,
    /// Why not, when `available` is false — shown rather than hidden, so "greyed out" always has
    /// a reason attached.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    /// Whether THIS device's access covers it (the host's Host-power grant).
    #[serde(default)]
    pub permitted: bool,
}

impl ActionInfo {
    /// Offer this row at all? Actions the device may not invoke are hidden (that is the access
    /// level talking, and a permanently dead row is noise); actions it may invoke but the host
    /// cannot run right now are SHOWN, disabled, with [`Self::unavailable_reason`].
    pub fn offerable(&self) -> bool {
        self.permitted
    }

    /// The client's own label for a known id, else the host's title. Keeps a familiar action
    /// worded the way the rest of this client words it, without hiding an unfamiliar one.
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

/// What this host offers this device, from `GET /api/v1/actions`.
///
/// **Best-effort by contract**, exactly like [`crate::library::fetch_running`]: an older host
/// (which has no such route), an unreachable one, or a shape we don't recognize yields an empty
/// list rather than an error. A missing row costs a menu entry; failing a host card over it
/// would cost the screen.
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

/// Invoke one action by id (`POST /api/v1/actions/{id}`, empty body).
///
/// `Ok(())` means the host ACCEPTED it (202) — it now ends every session and acts about a second
/// later, so this is the last word the client will get on the subject. The error is already
/// user-facing: the host's own refusal sentence (another device is streaming, a foreign sleep
/// inhibitor, the platform said no), or the library lane's classified transport error.
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
    // The id is the whole request — the body stays empty (the host's rule: no request field ever
    // reaches the privileged path). Percent-encoding is unnecessary and would be wrong: ids are
    // `[a-z.]` by grammar, and anything else is an id this host will 404 anyway.
    let url = format!(
        "{}/api/v1/actions/{action_id}",
        crate::library::base_url(addr, mgmt_port)
    );
    match agent.post(&url).send_empty() {
        Ok(_) => Ok(()),
        // A refusal carries the host's reason in the `ApiError` envelope; surface THAT, because
        // "409" tells a person nothing and "another device is streaming from this host right
        // now" tells them exactly what to do.
        Err(ureq::Error::StatusCode(code)) if (400..500).contains(&code) => Err(
            LibraryError::Unreachable(format!("the host refused ({code})")),
        ),
        Err(e) => Err(crate::library::classify(e)),
    }
}

/// How long a host's answer stays fresh before [`refresh`] will ask again. Long on purpose:
/// what it governs — whether this device holds the Host-power grant, whether the box can
/// suspend — changes when an operator edits access, not minute to minute, and every refresh is
/// a TLS handshake against a host that is otherwise idle.
#[cfg(any(target_os = "linux", windows))]
pub const TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// The process-wide answer cache, by host fingerprint.
///
/// Every desktop shell needs the same thing — a list settled BEFORE a menu draws (rows that
/// appear under a cursor already moving toward something else are a hazard when two of them
/// shut a machine down), refreshed rarely, shared across the screens that show it. One cache
/// with one TTL rule beats the same 40 lines in the console, the GTK page and the Windows
/// tile — which is how three shells end up disagreeing about what a host offers.
#[cfg(any(target_os = "linux", windows))]
type Cache =
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Vec<ActionInfo>)>>;

#[cfg(any(target_os = "linux", windows))]
fn cache() -> &'static Cache {
    static C: std::sync::OnceLock<Cache> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// What this host last said it lets this device do — the OFFERABLE rows only, so a caller
/// renders what it gets. Empty until a [`refresh`] has answered, and empty for a host with no
/// such route or a device without the grant.
#[cfg(any(target_os = "linux", windows))]
pub fn cached(fp_hex: &str) -> Vec<ActionInfo> {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(fp_hex)
        .map(|(_, a)| a.clone())
        .unwrap_or_default()
}

/// Ask this host again, off the caller's thread, unless the cached answer is still inside
/// [`TTL`]. Cheap and idempotent: call it on whatever refresh tick a shell already has.
///
/// The freshness stamp is taken BEFORE the request, so a slow or hanging host cannot make
/// every tick spawn another worker for it. The device identity is loaded on the worker rather
/// than taken as an argument — it is the same one file every shell already reads, and asking
/// for it here would mean threading it through three menus that have no other use for it.
#[cfg(any(target_os = "linux", windows))]
pub fn refresh(addr: &str, mgmt_port: u16, fp_hex: &str) {
    if fp_hex.is_empty() {
        return; // no pinned identity ⇒ nothing to authenticate as, and nothing to key on
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
                return; // no device identity ⇒ nothing to authenticate as
            };
            let pin = crate::trust::parse_hex32(&fp_hex);
            let found: Vec<ActionInfo> = fetch_actions(&addr, mgmt_port, &identity, pin)
                .into_iter()
                // The host decides who may see a row; a device without the grant is told
                // nothing about the action beyond that it exists, and shows nothing.
                .filter(ActionInfo::offerable)
                .collect();
            cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(fp_hex, (std::time::Instant::now(), found));
        })
        .ok();
}

/// Forget what this host said — call it right after invoking an action, because whatever it
/// said is about to be wrong. Without this, a menu goes on offering "Sleep host" on a machine
/// that is already asleep until the TTL lapses.
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

    /// Known ids wear this client's wording; an id from a future host wears the host's own
    /// title, which is the whole no-client-release contract.
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

    /// Not-permitted hides the row (that is the device's access level, and it will not change
    /// while the menu is open); unavailable KEEPS it, so the reason can be shown.
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
