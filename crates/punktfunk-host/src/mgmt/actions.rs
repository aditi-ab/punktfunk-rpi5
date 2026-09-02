//! `/api/v1/actions` — discovery of what this host can run, filtered per caller, and the
//! id-only invoke. See `design/host-actions.md`. v1 ships three `power.*` built-ins; later
//! host- or plugin-provided actions reuse these two routes.
//!
//! Admin bearer: both routes, everything permitted. Paired streaming cert: both routes, but
//! invoke re-reads `effective(fp, now)` and demands `GRANT_POWER` on that request. Plugin
//! token: neither — power is an operator hook, not a shared-token capability.
//!
//! Invoke is a trigger: the id selects a fixed host-side behavior, the body is empty, and no
//! request field reaches the privileged path. On accept: `202` → typed `HostPower` close of
//! every session → ~1 s so the reply flushes before the NIC goes away → act.

use super::auth::AuthLane;
use super::shared::*;
use crate::gamestream::tls::PeerCertFingerprint;
use crate::power::PowerVerb;
use axum::Extension;
use std::sync::atomic::{AtomicBool, Ordering};

/// Built-in: a stable id (`<group>.<verb>`; `plugin:<id>:<verb>` is reserved) bound to a
/// fixed executor. The registry is code so a request cannot add to it.
struct Builtin {
    id: &'static str,
    /// English fallback. Clients localize known ids and use this for unknown ones.
    title: &'static str,
    /// Two-press confirm hint. Reboot/shutdown lose state; sleep is reversible.
    danger: bool,
    verb: PowerVerb,
}

const BUILTINS: [Builtin; 3] = [
    Builtin {
        id: "power.sleep",
        title: "Sleep host",
        danger: false,
        verb: PowerVerb::Sleep,
    },
    Builtin {
        id: "power.reboot",
        title: "Restart host",
        danger: true,
        verb: PowerVerb::Reboot,
    },
    Builtin {
        id: "power.shutdown",
        title: "Shut down host",
        danger: true,
        verb: PowerVerb::Shutdown,
    },
];

/// One action as the caller sees it (`GET /actions`).
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionInfo {
    /// Invoke path parameter (`power.sleep`, …).
    #[schema(example = "power.sleep")]
    pub id: String,
    /// Clients localize known ids and fall back to this for unknown ones.
    pub title: String,
    pub group: String,
    /// Double-confirm hint: reboot/shutdown lose state.
    pub danger: bool,
    /// Platform probe. A VM that cannot S3 lists sleep as unavailable rather than a dead switch.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Whether THIS caller may invoke it (admin: always; cert: live `GRANT_POWER` bit).
    pub permitted: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionList {
    pub actions: Vec<ActionInfo>,
}

/// Admin always; a paired cert iff its live mask (re-read now — expiry- and edit-aware)
/// carries [`punktfunk_core::quic::GRANT_POWER`]. Any other lane: no.
fn power_permitted(st: &MgmtState, lane: AuthLane, fp: Option<&str>) -> bool {
    match lane {
        AuthLane::Admin => true,
        AuthLane::Cert => fp.is_some_and(|fp| {
            st.native
                .as_ref()
                .and_then(|n| n.effective(fp, unix_now()))
                .is_some_and(|mask| mask & punktfunk_core::quic::GRANT_POWER != 0)
        }),
        AuthLane::Plugin | AuthLane::Public => false,
    }
}

/// Host wall clock, unix seconds — the clock stored access deadlines are expressed in.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// List host actions
///
/// Per-caller view: platform availability plus whether this caller may invoke each one.
/// Admin: everything permitted. Paired cert: the device's live Host-power grant. Unknown
/// ids still render with the server-supplied title.
#[utoipa::path(
    get,
    path = "/actions",
    tag = "actions",
    operation_id = "listActions",
    responses(
        (status = OK, description = "The actions, per-caller", body = ActionList),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn list_actions(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    fp: Option<Extension<PeerCertFingerprint>>,
) -> Json<ActionList> {
    let fp = fp.as_ref().and_then(|e| e.0 .0.as_deref());
    let permitted = power_permitted(&st, lane, fp);
    // D-Bus round trips on Linux — off the async worker, all three in one hop.
    let probed = tokio::task::spawn_blocking(|| BUILTINS.map(|b| crate::power::probe(b.verb)))
        .await
        .expect("power probe task panicked");
    let actions = BUILTINS
        .iter()
        .zip(probed)
        .map(|(b, avail)| ActionInfo {
            id: b.id.into(),
            title: b.title.into(),
            group: "power".into(),
            danger: b.danger,
            available: crate::power::supported() && avail.available,
            unavailable_reason: if crate::power::supported() {
                avail.reason
            } else {
                Some("not supported on this host platform".into())
            },
            permitted,
        })
        .collect();
    Json(ActionList { actions })
}

/// One action in flight host-wide (`409` otherwise). The actions end the conversation, so
/// this is all the rate limiting v1 needs.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Once per (fingerprint, action) per boot — a retrying client must not turn the host log
/// into the DoS (`GrantDrops`).
fn log_denial_once(fp: &str, action: &str, device: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static LOGGED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let mut set = LOGGED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if set.insert((fp.to_string(), action.to_string())) {
        tracing::info!(
            device,
            fingerprint = fp,
            action,
            "denied a host action — this device's access lacks the Host power grant \
             (further denials of this pair are silent this boot)"
        );
    }
}

/// Invoke a host action
///
/// Id-only, empty body: nothing in the request reaches the privileged path. On `202` the
/// host ends every session (typed HostPower close), waits ~1 s so this response flushes,
/// then acts. Paired-cert callers need Host power and are `409` while another device's
/// session is live; the admin console is never blocked. One action at a time host-wide.
#[utoipa::path(
    post,
    path = "/actions/{id}",
    tag = "actions",
    operation_id = "invokeAction",
    params(("id" = String, Path, description = "Action id (`power.sleep`, `power.reboot`, `power.shutdown`)")),
    responses(
        (status = ACCEPTED, description = "Accepted — sessions are being ended and the action follows in about a second"),
        (status = FORBIDDEN, description = "This caller's access does not include this action (no Host power grant)", body = ApiError),
        (status = NOT_FOUND, description = "Unknown action id", body = ApiError),
        (status = CONFLICT, description = "Refused: an action is already in flight, another device's session is live (cert lane), or the platform said no (a foreign sleep inhibitor, a second local user, …)", body = ApiError),
        (status = NOT_IMPLEMENTED, description = "This host platform has no executor for it (macOS host)", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn invoke_action(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    fp: Option<Extension<PeerCertFingerprint>>,
    Path(id): Path<String>,
) -> Response {
    let Some(builtin) = BUILTINS.iter().find(|b| b.id == id) else {
        return api_error(StatusCode::NOT_FOUND, "unknown action id");
    };
    let fp = fp.as_ref().and_then(|e| e.0 .0.as_deref());
    let device = fp.and_then(|fp| {
        st.native.as_ref().and_then(|n| {
            n.list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(fp))
                .map(|c| crate::events::DeviceRef {
                    name: c.name,
                    fingerprint: c.fingerprint,
                    plane: crate::events::Plane::Native,
                })
        })
    });
    if !power_permitted(&st, lane, fp) {
        let device_name = device.as_ref().map(|d| d.name.as_str()).unwrap_or("");
        log_denial_once(fp.unwrap_or(""), builtin.id, device_name);
        return api_error(
            StatusCode::FORBIDDEN,
            "this device's access does not include host power — ask the host's operator to \
             enable the Host power grant",
        );
    }
    if !crate::power::supported() {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "host power actions are not supported on this host platform",
        );
    }
    // Another device's LIVE session blocks a cert-lane invoke; your own does not. Admin is
    // never blocked. A GameStream stream is always another device: its cert is never native.
    if lane == AuthLane::Cert {
        let others_native = crate::session_status::other_client_live(fp.unwrap_or(""));
        let gamestream = st.app.streaming.load(Ordering::SeqCst);
        if others_native || gamestream {
            return api_error(
                StatusCode::CONFLICT,
                "blocked: another device is streaming from this host right now",
            );
        }
    }
    let verb = builtin.verb;
    let avail = tokio::task::spawn_blocking(move || crate::power::probe(verb))
        .await
        .expect("power probe task panicked");
    if !avail.available {
        return api_error(
            StatusCode::CONFLICT,
            &format!(
                "blocked: {}",
                avail.reason.as_deref().unwrap_or("the platform said no")
            ),
        );
    }
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return api_error(
            StatusCode::CONFLICT,
            "a host action is already in flight — the host is on its way down",
        );
    }
    let invoker = device
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "the host console".into());
    tracing::info!(action = builtin.id, invoked_by = %invoker, "host action accepted");
    crate::events::emit(crate::events::EventKind::ActionInvoked {
        id: builtin.id.into(),
        device: device.clone(),
        outcome: "accepted".into(),
    });
    // 202 must flush before the NIC goes away. Typed close now; quit-flavored stop catches
    // anonymous sessions; the compat plane tears down on its own path.
    let id_owned: String = builtin.id.into();
    let app = st.app.clone();
    tokio::spawn(async move {
        crate::power::set_closing(true);
        crate::session_status::stop_all_quit();
        let _ = app.quit_session("host power action");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // Displays first: going under on a still-up display is not a transient.
        drain_displays().await;
        // Drop our suspend veto before asking logind — we never hold -ignore-inhibit rights.
        crate::sleep_inhibit::release_now();
        let outcome = tokio::task::spawn_blocking(move || crate::power::act(verb))
            .await
            .unwrap_or_else(|e| Err(format!("executor task panicked: {e}")));
        match outcome {
            // Reboot/shutdown ends this process shortly; sleep resumes here on wake.
            Ok(()) => tracing::info!(action = %id_owned, "host power action handed to the OS"),
            Err(e) => {
                tracing::warn!(action = %id_owned, error = %e, "host power action FAILED");
                crate::events::emit(crate::events::EventKind::ActionInvoked {
                    id: id_owned,
                    device,
                    outcome: format!("failed: {e}"),
                });
            }
        }
        crate::power::set_closing(false);
        IN_FLIGHT.store(false, Ordering::SeqCst);
    });
    StatusCode::ACCEPTED.into_response()
}

/// Wait for signaled sessions to drop their virtual displays before going under anyway.
/// Longer than the 1.5 s handshake release grace, short enough a wedged teardown cannot
/// strand the sleep.
const DISPLAY_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
const DISPLAY_DRAIN_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// Lingering slots to release now, and how many displays we are still waiting on. Pinned
/// displays are exempt from both — see [`drain_displays`]. Pure so the exemption is
/// testable; the registry is a process global.
fn drain_plan<'a>(displays: impl Iterator<Item = (&'a str, u64)>) -> (Vec<u64>, usize) {
    let (mut release, mut waiting) = (Vec::new(), 0usize);
    for (state, slot) in displays {
        match state {
            "lingering" => {
                release.push(slot);
                waiting += 1;
            }
            // Active = still tearing down; wait. Pinned = deliberate; leave it.
            "pinned" => {}
            _ => waiting += 1,
        }
    }
    (release, waiting)
}

/// Tear virtual displays down before the box goes under.
///
/// [`crate::session_status::stop_all_quit`] marks teardown deliberate so the display should
/// skip linger, but that drop is async and the 1 s reply-flush grace is shorter than the
/// 1.5 s the same wait gets in `native/handshake.rs`.
///
/// Going under on a still-up display is not a transient. The linger deadline is
/// `std::time::Instant`; on Linux that clock does not advance while suspended, so the box
/// wakes with the stale display standing and its window unspent.
///
/// [`crate::vdisplay::registry::release`] refuses ACTIVE displays, so the poll is the wait:
/// each tick sweeps `lingering`, and we return once nothing but pinned displays is left.
///
/// A pinned display stays. `KeepAlive::Forever` means until host shutdown or
/// `POST /display/release`; force-releasing it here would kill the nested gamescope session
/// and its game on every sleep — the thing the operator pinned it to avoid.
async fn drain_displays() {
    let deadline = std::time::Instant::now() + DISPLAY_DRAIN_BUDGET;
    loop {
        let snap = crate::vdisplay::registry::snapshot();
        let (release, waiting) =
            drain_plan(snap.displays.iter().map(|d| (d.state.as_str(), d.slot)));
        let pinned = snap.displays.len() - waiting;
        if waiting == 0 {
            if pinned > 0 {
                tracing::info!(
                    pinned,
                    "pinned display(s) left standing across the power action (keep-alive is \
                     `forever`) — free them with POST /display/release if a wake lands dark"
                );
            }
            return;
        }
        // Before the release so a slot that refuses to clear cannot spin past the budget.
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                displays = waiting,
                "virtual display(s) still up at the power-action budget — going under anyway; \
                 a wake may land on a stale display"
            );
            return;
        }
        if !release.is_empty() {
            // `release` tears the display down inline (gamescope + topology/DPMS restore), so
            // not on a runtime worker — same reason `power::act` is spawned blocking.
            let _ = tokio::task::spawn_blocking(move || {
                for slot in release {
                    crate::vdisplay::registry::release(Some(slot));
                }
            })
            .await;
        }
        tokio::time::sleep(DISPLAY_DRAIN_TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{drain_displays, drain_plan, DISPLAY_DRAIN_BUDGET};

    /// No display registered: drain must cost nothing. An inverted emptiness check would
    /// add the whole budget to every power action.
    #[tokio::test]
    async fn drain_displays_returns_at_once_when_no_display_is_up() {
        let t0 = std::time::Instant::now();
        drain_displays().await;
        assert!(
            t0.elapsed() < DISPLAY_DRAIN_BUDGET / 2,
            "drain burned the budget with no displays up: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn lingering_is_released_active_is_waited_out_pinned_is_left_alone() {
        let (release, waiting) =
            drain_plan([("lingering", 1u64), ("active", 2), ("pinned", 3)].into_iter());
        assert_eq!(release, vec![1], "only a lingering display is released");
        assert_eq!(
            waiting, 2,
            "active + lingering are waited on, pinned is not"
        );
    }

    /// Force-releasing a pin would kill the nested gamescope session and its game on every
    /// sleep. A box with only pinned displays must drain clean, releasing none of them.
    #[test]
    fn a_pinned_only_box_drains_clean_and_releases_nothing() {
        let (release, waiting) = drain_plan([("pinned", 7u64), ("pinned", 8)].into_iter());
        assert!(release.is_empty(), "a pin must survive a power action");
        assert_eq!(waiting, 0, "pinned displays must not hold the drain open");
    }
}
